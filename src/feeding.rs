//! Feeding guidance: a daily intake target from age and weight, compared with
//! what was actually given, and a suggested amount for the next feed.
//!
//! The rules of thumb (no official guideline; see DESIGN.md):
//! - first week: per feed about 10 ml × day of life (start ~10 ml, +10 ml a day);
//! - after that: 150 ml per kg of body weight per day (range 150–180);
//!   the build-up applies until it reaches the per-kg level.
//!
//! Only bottle millilitres are known; intake at the breast is not measured.
//! The plan of the nurse, midwife or doctor always takes precedence.

use crate::entries::{Baby, Entry};
use crate::time::{Clock, fmt_dur};
use serde_json::{Value, json};

pub const ML_PER_KG: f64 = 150.0;
pub const ML_PER_KG_HIGH: f64 = 180.0;
const RAMP_ML_PER_DAY_OF_LIFE: f64 = 10.0;
const FEEDS_MAX: f64 = 10.0;
const FEEDS_DEFAULT: f64 = 8.0;
const CATCH_UP_MAX: f64 = 0.2; // at most +20% per feed to make up a shortfall


pub const NOTE: &str = "A guide from rules of thumb (first week about 10 ml × day of life per feed, then 150 ml/kg/day), not medical advice; the plan of the nurse, midwife or doctor always takes precedence. Only bottle millilitres are counted: what she drinks at the breast is not measured, so with breastfeeding the bottle total looks lower.";

fn median_f(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

fn round5(x: f64) -> f64 {
    (x / 5.0).round() * 5.0
}

fn bottle_ml(e: &Entry) -> f64 {
    e.num("bottle_breastmilk_ml").unwrap_or(0.0) + e.num("bottle_formula_ml").unwrap_or(0.0)
}

fn at_breast(e: &Entry) -> bool {
    e.num("breast_left_min").unwrap_or(0.0) + e.num("breast_right_min").unwrap_or(0.0) > 0.0
        || e.str("latch").is_some_and(|l| l != "none")
}

/// The NVK's minimum number of feeds per 24 hours and longest pause for a
/// healthy term baby ("Advies ten aanzien van wakker maken van pasgeborenen
/// voor voeding", NVK 2016). It assumes adequate growth and feeding behaviour.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Minimum {
    pub feeds: f64,
    pub max_pause: i64,
    pub rule: &'static str,
}

pub fn nvk_minimum(age_days: f64, birth_weight: Option<f64>, weight: f64) -> Minimum {
    let m = |feeds: f64, hours: i64, rule| Minimum { feeds, max_pause: hours * 3600, rule };
    let small = birth_weight.is_some_and(|b| b < 2500.0);
    if small && (age_days < 14.0 || weight < 2500.0) {
        return m(8.0, 3, "under 2500 g (or younger than 14 days): at least 8 feeds per 24 h, pauses of at most 3 h");
    }
    if small && weight < 3000.0 {
        return m(7.0, 6, "2500–3000 g: at least 7 feeds per 24 h, pauses of at most 6 h");
    }
    let regained = birth_weight.is_none_or(|b| weight > b);
    if !small && (age_days < 14.0 || !regained) {
        return m(6.0, 6, "until 14 days old and back above birth weight: at least 6 feeds per 24 h, pauses of at most 6 h");
    }
    if age_days < 56.0 {
        return m(5.0, 8, "2–8 weeks: at least 5 feeds per 24 h, pauses of at most 8 h");
    }
    m(4.0, 12, "from 8 weeks: at least 4 feeds per 24 h, pauses of at most 12 h")
}

/// Inputs, separated from the database for testing.
pub struct Inputs<'a> {
    pub now: i64,
    pub birth: i64,
    pub birth_weight_g: Option<f64>,
    /// Feeds per 24 h to plan for at least (on top of the NVK minimum).
    pub aim_feeds: Option<f64>,
    /// Seconds before a feed to start the routine (diaper first).
    pub prep_s: i64,
    /// Latest known weight in grams (a weighing, or else the birth weight).
    pub weight_g: f64,
    /// Non-deleted entries from at least the last three full days up to now, oldest first.
    pub entries: &'a [Entry],
}

/// The daily intake target (ml) for a day of life, weight and feeds per day:
/// the first-week build-up until it reaches 150 ml/kg; then 150 ml/kg with
/// the 150–180 ml/kg range.
pub fn daily_target(day_of_life: i64, weight_g: f64, feeds: f64) -> (f64, Option<(f64, f64)>) {
    let kg = weight_g / 1000.0;
    let ramp = RAMP_ML_PER_DAY_OF_LIFE * day_of_life as f64 * feeds;
    if ramp < ML_PER_KG * kg {
        (ramp, None)
    } else {
        (ML_PER_KG * kg, Some((ML_PER_KG * kg, ML_PER_KG_HIGH * kg)))
    }
}

/// Day of life in calendar days, as the kraamzorg counts: the birth day is day 1.
pub fn day_of_life(clock: &Clock, birth: i64, t: i64) -> i64 {
    ((clock.day_start(t) - clock.day_start(birth)) as f64 / 86400.0).round() as i64 + 1
}

/// Weight at time `t`, interpolated linearly between weighings (the birth
/// weight at birth). `None` before birth or after the last weighing.
pub fn interpolated_weight(weights: &[(i64, f64)], t: i64) -> Option<f64> {
    let first = weights.first()?;
    let last = weights.last()?;
    if t < first.0 || t > last.0 {
        return None;
    }
    let i = weights.partition_point(|w| w.0 <= t);
    if i >= weights.len() {
        return Some(last.1);
    }
    let (a, b) = (weights[i - 1], weights[i]);
    Some(a.1 + (b.1 - a.1) * (t - a.0) as f64 / (b.0 - a.0).max(1) as f64)
}

/// Weighings since birth, starting with the birth weight, sorted by time.
/// Several weighings at the same time are averaged.
pub fn weighings(birth: i64, birth_weight: f64, entries: &[Entry]) -> Vec<(i64, f64)> {
    let mut w: Vec<(i64, f64)> = vec![(birth, birth_weight)];
    let mut later: Vec<(i64, f64)> = entries.iter().filter(|e| e.at > birth).filter_map(|e| e.num("weight_g").map(|g| (e.at, g))).collect();
    later.sort_by_key(|x| x.0);
    for (t, g) in later {
        match w.last_mut() {
            Some(l) if l.0 == t => l.1 = (l.1 + g) / 2.0,
            _ => w.push((t, g)),
        }
    }
    w
}

/// One full local day since birth, per kg of the weight that day.
#[derive(Debug, Clone, PartialEq)]
pub struct PerKgDay {
    pub date: String,
    pub day_of_life: i64,
    /// Bottle millilitres per kg (`None` when nothing was logged).
    pub intake: Option<f64>,
    /// Weight gained that day per kg, from interpolated weighings (`None`
    /// when the day is not between two weighings).
    pub growth: Option<f64>,
    /// Whether she was also fed at the breast that day (not measured).
    pub breast: bool,
}

/// Per full local day since birth (the birth day and today are partial and
/// left out), up to `max_days` most recent days.
pub fn per_kg_days(clock: &Clock, birth: i64, birth_weight: f64, entries: &[Entry], now: i64, max_days: usize) -> Vec<PerKgDay> {
    let weights = weighings(birth, birth_weight, entries);
    let mut out = vec![];
    let mut start = clock.next_day_start(birth);
    let today = clock.day_start(now);
    while start < today {
        let end = clock.next_day_start(start);
        let day: Vec<&Entry> = entries.iter().filter(|e| e.at >= start && e.at < end).collect();
        let (w0, w1) = (interpolated_weight(&weights, start), interpolated_weight(&weights, end));
        // The weight to divide by: the middle of the day, else the last known.
        let kg = interpolated_weight(&weights, start + (end - start) / 2)
            .unwrap_or_else(|| weights.iter().rfind(|w| w.0 <= end).map(|w| w.1).unwrap_or(birth_weight))
            / 1000.0;
        let ml: f64 = day.iter().map(|e| bottle_ml(e)).sum();
        out.push(PerKgDay {
            date: clock.day(start),
            day_of_life: day_of_life(clock, birth, start),
            intake: (ml > 0.0).then(|| ml / kg),
            growth: w0.zip(w1).map(|(a, b)| (b - a) / kg),
            breast: day.iter().any(|e| at_breast(e)),
        });
        start = end;
    }
    let skip = out.len().saturating_sub(max_days);
    out.split_off(skip)
}

/// The days used for the fit: after the first week (when intake is still
/// building up rather than following need) with both numbers known.
pub const FIT_FROM_DAY: i64 = 8;
const FIT_MIN_DAYS: usize = 5;
const FIT_MIN_SPREAD: f64 = 5.0; // g/kg/day between the slowest and fastest day

/// A least-squares fit of `intake/kg = baseline + cost × growth/kg` over the
/// days after the first week: `baseline` is the bottle ml/kg/day at zero
/// growth, `cost` the extra ml per gram gained. `None` while there are too
/// few days, too little difference in growth between them, or the result
/// does not make sense (negative cost or baseline).
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    pub baseline: f64,
    pub cost: f64,
    pub days: usize,
}

pub fn fit(days: &[PerKgDay]) -> Result<Fit, String> {
    let pts: Vec<(f64, f64)> = days
        .iter()
        .filter(|d| d.day_of_life >= FIT_FROM_DAY)
        .filter_map(|d| Some((d.growth?, d.intake?)))
        .collect();
    if pts.len() < FIT_MIN_DAYS {
        return Err(format!(
            "needs {FIT_MIN_DAYS} weighed days from day {FIT_FROM_DAY} on ({} so far)",
            pts.len()
        ));
    }
    let n = pts.len() as f64;
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let (lo, hi) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
    if hi - lo < FIT_MIN_SPREAD || sxx == 0.0 {
        return Err(format!("growth hardly differed between the days ({lo:.0} to {hi:.0} g/kg)"));
    }
    let cost = sxy / sxx;
    let baseline = my - cost * mx;
    if cost <= 0.0 || baseline < 0.0 {
        return Err("no clear pattern yet".into());
    }
    Ok(Fit { baseline, cost, days: pts.len() })
}


pub fn guidance(clock: &Clock, i: &Inputs) -> Guidance {
    let dol = day_of_life(clock, i.birth, i.now);
    let kg = i.weight_g / 1000.0;

    // Feeds as episodes: a top-up within 1 h counts with its feed (up to 2 h).
    let eps = crate::entries::feed_episodes(i.entries.iter().filter(|e| e.at <= i.now));
    // Feeds per day: median over the last three full local days since birth.
    let today = clock.day_start(i.now);
    let mut per_day = vec![];
    let mut start = today;
    for _ in 0..3 {
        let prev = clock.day_start(start - 3600);
        if prev < clock.day_start(i.birth) {
            break;
        }
        let n = eps.iter().filter(|f| f.0 >= prev && f.0 < start).count();
        if n > 0 {
            per_day.push(n as f64);
        }
        start = prev;
    }
    let min = nvk_minimum((i.now - i.birth) as f64 / 86400.0, i.birth_weight_g, i.weight_g);
    let floor = min.feeds.max(i.aim_feeds.unwrap_or(0.0)).min(FEEDS_MAX);
    let feeds = median_f(per_day.clone()).map(|m| m.clamp(floor, FEEDS_MAX)).unwrap_or(FEEDS_DEFAULT.max(floor));

    // Daily target: the first-week build-up until it reaches the per-kg level.
    let (target, range) = daily_target(dol, i.weight_g, feeds);
    let basis = if range.is_none() {
        format!("first-week build-up: about {:.0} ml per feed on day {dol} × {feeds:.0} feeds", RAMP_ML_PER_DAY_OF_LIFE * dol as f64)
    } else {
        format!("{ML_PER_KG:.0} ml/kg/day × {kg:.2} kg")
    };

    // What was given in the last 24 hours.
    let last: Vec<&Entry> = i.entries.iter().filter(|e| e.at > i.now - 86400 && e.at <= i.now).collect();
    let given: f64 = last.iter().map(|e| bottle_ml(e)).sum();
    let feeds_24h = eps.iter().filter(|f| f.0 > i.now - 86400).count();
    let breast_24h = last.iter().filter(|e| at_breast(e)).count();
    let shortfall = target - given;

    // Next feed: an amount of target / feeds, plus part of a shortfall (at
    // most +20%). The longest gap the plan allows (the aim or the NVK
    // minimum number of feeds, and its longest pause) caps the estimate in
    // `status`; at the latest: the NVK's longest pause.
    let max_gap = ((86400.0 / floor) as i64).min(min.max_pause);
    // Feed times for the NVK limits: the starts (the cautious side); the next
    // feed after the end of the last one (its last top-up).
    let feed_times: Vec<i64> = eps.iter().map(|f| f.0).collect();
    let per_feed = target / feeds;
    let extra = (shortfall / feeds).clamp(0.0, per_feed * CATCH_UP_MAX);
    let next_amount = round5(per_feed + extra);

    let latest = (!feed_times.is_empty()).then(|| latest(&min, &feed_times));
    Guidance {
        day_of_life: dol,
        weight_g: i.weight_g,
        target,
        range,
        basis,
        feeds,
        per_day,
        per_feed,
        given,
        feeds_24h,
        breast_24h,
        shortfall,
        extra,
        amount_ml: next_amount,
        max_gap,
        aim: i.aim_feeds,
        latest,
        prep_s: i.prep_s,
        warnings: warnings(clock, &min, i.aim_feeds, &feed_times, i.birth, i.now),
        breast_side: breast_side(i.entries, i.now),
        min,
        growth_check: None,
    }
}

/// Feeding guidance (see `guidance`); `to_json` is what `status` returns.
#[derive(Debug, Clone, Default)]
pub struct Guidance {
    pub day_of_life: i64,
    pub weight_g: f64,
    pub target: f64,
    pub range: Option<(f64, f64)>,
    pub basis: String,
    pub feeds: f64,
    /// Feeds on each of the last full days (newest first), for the basis.
    pub per_day: Vec<f64>,
    pub per_feed: f64,
    /// Bottle millilitres, feeds and feeds at the breast in the last 24 hours.
    pub given: f64,
    pub feeds_24h: usize,
    pub breast_24h: usize,
    pub shortfall: f64,
    /// Towards the shortfall, per feed.
    pub extra: f64,
    /// The suggested amount for the next feed.
    pub amount_ml: f64,
    /// The longest gap between feeds the plan allows (aim, NVK minimum).
    pub max_gap: i64,
    pub aim: Option<f64>,
    pub min: Minimum,
    /// The latest next feed under the NVK minimum, and why.
    pub latest: Option<(i64, String)>,
    pub prep_s: i64,
    pub warnings: Vec<Warning>,
    pub breast_side: Option<BreastSide>,
    /// A weight change below P25 for the delivery mode.
    pub growth_check: Option<String>,
}

/// An NVK warning, with a key so a notification fires once.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Warning {
    pub kind: &'static str,
    pub key: String,
    pub text: String,
}

/// Which breast to start with, and why.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BreastSide {
    pub start: &'static str,
    pub reason: String,
    pub left_min_24h: f64,
    pub right_min_24h: f64,
}

impl Guidance {
    /// The latest time to start (with a preparation time).
    pub fn latest_start(&self) -> Option<i64> {
        self.latest.as_ref().filter(|_| self.prep_s > 0).map(|(t, _)| t - self.prep_s)
    }

    pub fn to_json(&self, clock: &Clock) -> Value {
        let mut out = json!({
            "day_of_life": self.day_of_life,
            "weight_g": self.weight_g,
            "target_ml_per_day": self.target.round(),
            "target_basis": self.basis,
            "feeds_per_day": self.feeds,
            "feeds_per_day_basis": if self.per_day.is_empty() { "default".to_string() } else {
                format!("median of the last {} full day(s): {:?}", self.per_day.len(), self.per_day.iter().map(|x| *x as i64).collect::<Vec<_>>())
            },
            "per_feed_ml": round5(self.per_feed),
            "last_24h": {"bottle_ml": self.given.round(), "feeds": self.feeds_24h, "feeds_at_breast": self.breast_24h,
                         "vs_target_ml": (-self.shortfall).round()},
            "next_feed": {"amount_ml": self.amount_ml, "max_gap": fmt_dur(self.max_gap),
                          "amount_basis": if self.extra > 0.0 {
                              format!("{:.0} ml per feed + {:.0} ml towards the last 24 hours' shortfall (at most +20%)", self.per_feed, self.extra)
                          } else { format!("{:.0} ml per feed", self.per_feed) }},
            "minimum": {"feeds_per_day": self.min.feeds, "max_pause": fmt_dur(self.min.max_pause), "rule": self.min.rule,
                        "source": "NVK, 'Advies ten aanzien van wakker maken van pasgeborenen voor voeding' (2016), for healthy term babies that grow and drink well"},
            "note": NOTE,
        });
        if let Some((lo, hi)) = self.range {
            out["range_ml_per_day"] = json!([lo.round(), hi.round()]);
        }
        if let Some(aim) = self.aim {
            out["aim_feeds_per_day"] = json!(aim);
        }
        if let Some((latest, why)) = &self.latest {
            out["next_feed"]["latest_at"] = json!(clock.fmt(*latest));
            out["next_feed"]["latest_reason"] = json!(why);
        }
        if let Some(t) = self.latest_start() {
            out["next_feed"]["latest_start_at"] = json!(clock.fmt(t));
        }
        out["warnings"] = self.warnings.iter().map(|w| json!({"kind": w.kind, "key": w.key, "text": w.text})).collect();
        if let Some(b) = &self.breast_side {
            out["breast_side"] = json!({"start": b.start, "reason": b.reason, "left_min_24h": b.left_min_24h, "right_min_24h": b.right_min_24h});
        }
        if let Some(g) = &self.growth_check {
            out["growth_check"] = json!(g);
        }
        out
    }
}

/// Which breast to start with: the side with fewer minutes over the last
/// 24 hours if that is clearly less (≥ 10 min and ≥ 20% of the total),
/// otherwise the other side than the one used most at the last breastfeed.
pub fn breast_side(entries: &[Entry], now: i64) -> Option<BreastSide> {
    let mins = |e: &Entry| (e.num("breast_left_min").unwrap_or(0.0), e.num("breast_right_min").unwrap_or(0.0));
    let recent: Vec<&Entry> = entries.iter().filter(|e| e.at > now - 86400 && e.at <= now).filter(|e| {
        let (l, r) = mins(e);
        l + r > 0.0
    }).collect();
    let last = recent.last()?;
    let (l, r) = recent.iter().map(|e| mins(e)).fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
    let (ll, lr) = mins(last);
    let (side, reason) = if (l - r).abs() >= 10.0_f64.max(0.2 * (l + r)) {
        (if l < r { "left" } else { "right" }, format!("24 h: L {l:.0}′ · R {r:.0}′"))
    } else if ll != lr {
        (if ll > lr { "right" } else { "left" }, format!("last: mostly {}", if ll > lr { "L" } else { "R" }))
    } else {
        return None;
    };
    Some(BreastSide { start: side, reason, left_min_24h: l, right_min_24h: r })
}

/// The latest time for the next feed under the NVK minimum: the longest pause
/// after the last feed, or earlier, the moment the last 24 hours would hold
/// fewer than the minimum number of feeds (the minimum-th most recent feed
/// turns 24 h old). `feeds` is sorted, oldest first, and not empty.
pub fn latest(min: &Minimum, feeds: &[i64]) -> (i64, String) {
    let last = *feeds.last().unwrap();
    let pause = last + min.max_pause;
    let n = min.feeds as usize;
    match feeds.len().checked_sub(n).map(|i| feeds[i] + 86400) {
        Some(count) if count < pause => (count, format!("≥ {n} feeds/24 h")),
        _ => (pause, format!("max {} pause", fmt_dur(min.max_pause))),
    }
}

/// How long before the NVK's longest pause the "almost" warning starts.
const WARN_BEFORE: i64 = 3600;

/// Warnings when the NVK minimum gets close: almost the longest pause since
/// the last feed, or (within the hour) fewer than the minimum number of feeds
/// in the last 24 hours. Each has a key so a notification fires once.
pub fn warnings(clock: &Clock, min: &Minimum, aim: Option<f64>, feeds: &[i64], birth: i64, now: i64) -> Vec<Warning> {
    let mut out = vec![];
    let hm = |t: i64| clock.fmt_time(t);
    let Some(&last) = feeds.last() else { return out };
    let limit = last + min.max_pause;
    // Stop after 3 h past the limit: then nobody is logging.
    if now >= limit - WARN_BEFORE && now < limit + 3 * 3600 {
        let text = if now < limit {
            format!("Almost {} since the last feed ({}); the NVK maximum is {}: feed by {}", fmt_dur(now - last), hm(last), fmt_dur(min.max_pause), hm(limit))
        } else {
            format!("{} since the last feed ({}): longer than the NVK maximum of {}", fmt_dur(now - last), hm(last), fmt_dur(min.max_pause))
        };
        out.push(Warning { kind: "pause", key: format!("pause:{last}"), text });
    }
    // Rolling 24 hours, once she is a day old and someone fed in that time.
    if now - birth >= 86400 && now - last < 86400 {
        let in_24h = feeds.iter().filter(|t| **t > now - 86400 && **t <= now).count();
        let in_23h = feeds.iter().filter(|t| **t > now - 23 * 3600 && **t <= now).count();
        let goal = aim.map(|a| format!(", aim {a:.0}")).unwrap_or_default();
        // Below the minimum, not at it: a steady minimum is fine.
        if (in_23h as f64) < min.feeds {
            let (text, key) = if in_24h > in_23h {
                // A feed leaves the 24-hour window within the hour.
                let leaving = *feeds.iter().find(|t| **t > now - 86400).unwrap();
                (format!("Within the hour only {in_23h} feeds in the last 24 h unless she feeds by {} (NVK minimum {:.0}{goal})", hm(leaving + 86400), min.feeds),
                 format!("count:{leaving}"))
            } else {
                (format!("Only {in_24h} feeds in the last 24 h (NVK minimum {:.0}{goal})", min.feeds), // Once per last feed: a lasting shortfall is not news every hour.
                 format!("count:last:{last}"))
            };
            out.push(Warning { kind: "count", key, text });
        }
    }
    out
}

/// Feeding guidance for the status, or None without birth and weight.
/// `last_weight` is the last weighing: grams and when.
/// `entries`: from at least four days before the start of today, up to now.
pub fn for_status(clock: &Clock, baby: &Baby, last_weight: Option<(f64, i64)>, entries: &[Entry], now: i64) -> Option<Guidance> {
    let (Some(birth), Some(weight)) = (baby.birth, last_weight.map(|w| w.0).or(baby.birth_weight_g)) else {
        return None;
    };
    let mut g = guidance(clock, &Inputs { now, birth, birth_weight_g: baby.birth_weight_g, aim_feeds: baby.aim_feeds_per_day, prep_s: baby.feed_prep_s, weight_g: weight, entries });
    // Growth as a check: flag a weight change below P25 for the delivery
    // mode, read at the age of the weighing (not today's).
    if let Some((w, at)) = last_weight
        && let Some((_, p, label)) = baby.change_percentile(w, at)
            && p < 25.0 {
                g.growth_check = Some(format!(
                    "Her weight change is at {label} for her delivery mode, below P25: worth discussing with the midwife or consultatiebureau."
                ));
            }
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::status::entry;

    #[test]
    fn nvk() {
        // Normal birth weight: 6 / 6 h until 14 days and above birth weight.
        assert_eq!(nvk_minimum(7.0, Some(3900.0), 3720.0).feeds, 6.0);
        assert_eq!(nvk_minimum(20.0, Some(3900.0), 3850.0).feeds, 6.0, "not back above birth weight yet");
        let m = nvk_minimum(20.0, Some(3900.0), 4100.0);
        assert_eq!((m.feeds, m.max_pause), (5.0, 8 * 3600));
        assert_eq!(nvk_minimum(10.0, Some(3900.0), 4100.0).feeds, 6.0, "under 14 days");
        assert_eq!(nvk_minimum(60.0, Some(3900.0), 5000.0).max_pause, 12 * 3600);
        // Small babies.
        assert_eq!(nvk_minimum(5.0, Some(2400.0), 2400.0).max_pause, 3 * 3600);
        assert_eq!(nvk_minimum(20.0, Some(2400.0), 2700.0).feeds, 7.0);
        assert_eq!(nvk_minimum(20.0, Some(2400.0), 3100.0).feeds, 5.0);
    }

    #[test]
    fn aim_and_warnings() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-31 16:40", 0).unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let min = nvk_minimum(8.0, Some(3900.0), 3720.0);
        // Feeds every 4 h (6 a day): with an aim of 7 the plan is 7 feeds, gap at most 24 h / 7.
        let es: Vec<Entry> = (0..12).map(|k| entry(at("2025-11-06 02:00") + k * 4 * 3600, json!({"bottle_formula_ml": 80}))).collect();
        let now = at("2025-11-07 23:30");
        let g = guidance(&clock, &Inputs { now, birth, birth_weight_g: Some(3900.0), aim_feeds: Some(7.0), prep_s: 0, weight_g: 3720.0, entries: &es }).to_json(&clock);
        assert_eq!(g["feeds_per_day"], 7.0);
        assert_eq!(g["next_feed"]["max_gap"], "3h26m", "24 h / 7");
        // Last feed 22:00 (k=11 → 2025-11-07 22:00); at 23:30 no pause warning yet.
        let feeds: Vec<i64> = es.iter().map(|e| e.at).collect();
        let w = warnings(&clock, &min, Some(7.0), &feeds, birth, at("2025-11-07 23:30"));
        // 6 feeds in the last 24 h (02:00 … 22:00 every 4 h): at the minimum, no warning.
        assert!(w.is_empty(), "{w:?}");
        // Without the 02:00 feed: 5, below it; keyed on the last feed, not the hour.
        let fewer: Vec<i64> = feeds.iter().copied().filter(|t| *t != at("2025-11-07 02:00")).collect();
        let w = warnings(&clock, &min, Some(7.0), &fewer, birth, at("2025-11-07 23:30"));
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].text, "Only 5 feeds in the last 24 h (NVK minimum 6, aim 7)");
        let later = warnings(&clock, &min, Some(7.0), &fewer, birth, at("2025-11-08 00:40"));
        assert_eq!(later.iter().find(|x| x.kind == "count").unwrap().key, w[0].key);
        // At 01:30 the 02:00 feed of the previous day leaves the window within the hour.
        let w = warnings(&clock, &min, Some(7.0), &feeds[..6], birth, at("2025-11-07 01:30"));
        assert_eq!(w[0].text, "Within the hour only 5 feeds in the last 24 h unless she feeds by 02:00 (NVK minimum 6, aim 7)");
        // Five hours after the last feed: almost the 6 h maximum.
        let w = warnings(&clock, &min, Some(7.0), &feeds, birth, at("2025-11-08 03:10"));
        let pause = w.iter().find(|x| x.kind == "pause").unwrap();
        assert_eq!(pause.text, "Almost 5h10m since the last feed (22:00); the NVK maximum is 6h00m: feed by 04:00");
        // Regular feeds every 3 h: no warnings.
        let regular: Vec<i64> = (0..8).map(|k| at("2025-11-07 00:30") + k * 3 * 3600).collect();
        assert!(warnings(&clock, &min, Some(7.0), &regular, birth, at("2025-11-07 23:59")).is_empty());
    }

    #[test]
    fn top_ups_count_with_their_feed() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-31 16:40", 0).unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        // Every 3 h from 02:00 to 20:00, then 23:00 with top-ups at 23:50 and
        // 00:40 (one feed: each within the hour, all within 2 hours).
        let mut es: Vec<Entry> = (0..7).map(|k| entry(at("2025-11-07 02:00") + k * 3 * 3600, json!({"bottle_formula_ml": 80}))).collect();
        for (t, ml) in [("2025-11-07 23:00", 50), ("2025-11-07 23:50", 30), ("2025-11-08 00:40", 40)] {
            es.push(entry(at(t), json!({"bottle_formula_ml": ml})));
        }
        let eps = crate::entries::feed_episodes(&es);
        assert_eq!(eps.len(), 8);
        assert_eq!(*eps.last().unwrap(), (at("2025-11-07 23:00"), at("2025-11-08 00:40")));
        // 70 minutes after a feed: a feed of its own.
        let apart = [entry(at("2025-11-09 08:00"), json!({"bottle_formula_ml": 60})), entry(at("2025-11-09 09:10"), json!({"bottle_formula_ml": 20}))];
        assert_eq!(crate::entries::feed_episodes(&apart).len(), 2);
        // Cluster feeding every 50 minutes: not one feed, but one every time
        // the feed so far would run past 2 hours (10:00, 12:30, 15:00).
        let cluster: Vec<Entry> = (0..7).map(|k| entry(at("2025-11-09 10:00") + k * 50 * 60, json!({"bottle_formula_ml": 40}))).collect();
        assert_eq!(crate::entries::feed_episodes(&cluster).len(), 3);
        assert_eq!(crate::entries::totals(&cluster).feeds, 3);
        assert_eq!(crate::entries::totals(&es).feeds, 8, "top-ups aren't feeds of their own");
        // The day from midnight: the top-up at 00:40 belongs to the 23:00 feed
        // of the day before.
        let t = crate::entries::totals_from(&es, at("2025-11-08 00:00"));
        assert_eq!((t.feeds, t.formula_ml), (0, 40.0));
        let now = at("2025-11-08 01:30");
        let g = guidance(&clock, &Inputs { now, birth, birth_weight_g: Some(3900.0), aim_feeds: None, prep_s: 0, weight_g: 3720.0, entries: &es }).to_json(&clock);
        assert_eq!(g["last_24h"]["feeds"], 8);
        // The next feed after the last top-up (00:40) + the usual gap (3 h);
        // the NVK pause from the start of the feed (23:00 + 6 h).
        assert_eq!(crate::status::estimate_feed(&eps, None).map(|f| clock.fmt(f.at)).as_deref(), Some("2025-11-08 03:40"));
        assert_eq!(g["next_feed"]["latest_at"], "2025-11-08 05:00");
    }

    #[test]
    fn latest_by_pause_or_count() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let min = nvk_minimum(8.0, Some(3900.0), 3720.0);
        // Six feeds in quick succession, then 11:07: the 6th most recent (yesterday 13:00)
        // turns 24 h old at 13:00 today, before 11:07 + 6 h.
        let f: Vec<i64> = ["2025-11-06 13:00", "2025-11-06 16:00", "2025-11-06 19:00", "2025-11-06 22:00", "2025-11-07 04:00", "2025-11-07 11:07"]
            .iter().map(|s| at(s)).collect();
        assert_eq!(latest(&min, &f), (at("2025-11-07 13:00"), "≥ 6 feeds/24 h".into()));
        // Regular 3-hourly feeds: the pause limit comes first.
        let f: Vec<i64> = (0..9).map(|k| at("2025-11-06 14:00") + k * 3 * 3600).collect();
        assert_eq!(latest(&min, &f), (at("2025-11-07 20:00"), "max 6h00m pause".into()));
        // Fewer than six feeds in total: only the pause.
        assert_eq!(latest(&min, &[at("2025-11-07 08:00")]).0, at("2025-11-07 14:00"));
    }

    #[test]
    fn breast_sides() {
        let now = 1_000_000;
        let e = |ago: i64, l: i64, r: i64| entry(now - ago, json!({"breast_left_min": l, "breast_right_min": r}));
        // Clear imbalance over 24 h: start with the side used less.
        let b = breast_side(&[e(20_000, 15, 5), e(10_000, 20, 5), e(3000, 6, 10)], now).unwrap();
        assert_eq!((b.start, b.reason.as_str()), ("right", "24 h: L 41′ · R 20′"));
        // Balanced: alternate after the last breastfeed (mostly right → start left).
        let b = breast_side(&[e(20_000, 10, 5), e(3000, 5, 10)], now).unwrap();
        assert_eq!(b.start, "left");
        // Nothing at the breast in 24 h, or perfectly even last time and balanced: no advice.
        assert!(breast_side(&[e(90_000, 10, 0)], now).is_none());
        assert!(breast_side(&[e(3000, 5, 5)], now).is_none());
    }

    #[test]
    fn first_week_build_up_then_per_kg() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-31 16:40", 0).unwrap();
        // Day 3, 3.6 kg: build-up 30 ml × 8 = 240 < 540 → build-up applies.
        let now = clock.parse("2025-11-02 12:00", 0).unwrap();
        let g = guidance(&clock, &Inputs { now, birth, birth_weight_g: Some(3900.0), aim_feeds: None, prep_s: 0, weight_g: 3600.0, entries: &[] }).to_json(&clock);
        assert_eq!(g["day_of_life"], 3);
        assert_eq!(g["target_ml_per_day"], 240.0);
        assert!(g["range_ml_per_day"].is_null());
        // Day 8, 3.68 kg, 8 feeds/day: build-up 80 × 8 = 640 ≥ 552 → 150 ml/kg.
        let now = clock.parse("2025-11-07 12:00", 0).unwrap();
        let mut es = vec![];
        // 8 feeds a day, every 3 h from 01:00, from 2 Nov up to the morning of 6 Nov.
        for d in 1..=5 {
            let day = clock.date_start(&format!("2025-11-0{}", d + 2)).unwrap();
            for k in 0..8 {
                es.push(entry(day + k * 3 * 3600 + 3600, json!({"bottle_formula_ml": 60})));
            }
        }
        let g = guidance(&clock, &Inputs { now, birth, birth_weight_g: Some(3900.0), aim_feeds: None, prep_s: 0, weight_g: 3680.0, entries: &es }).to_json(&clock);
        assert_eq!(g["feeds_per_day"], 8.0);
        assert_eq!(g["target_ml_per_day"], 552.0);
        assert_eq!(g["range_ml_per_day"], json!([552.0, 662.0]));
        assert_eq!(g["per_feed_ml"], 70.0);
        // Last 24 h: 8 feeds of 60 ml = 480, short 72 → +9 ml (cap 13.8) → 69+9=78 → 80.
        assert_eq!(g["last_24h"]["bottle_ml"], 480.0);
        assert_eq!(g["last_24h"]["vs_target_ml"], -72.0);
        assert_eq!(g["next_feed"]["amount_ml"], 80.0);
        let gap = crate::status::estimate_feed(&crate::entries::feed_episodes(&es), None).unwrap().gap;
        assert_eq!(fmt_dur(gap), "3h00m");
    }

    #[test]
    fn feeds_per_day_clamped_and_catch_up_capped() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-02 12:00", 0).unwrap();
        let now = clock.parse("2025-11-07 12:00", 0).unwrap();
        // 36 days old and above birth weight: NVK minimum 5 feeds, pauses up to 8 h.
        // Three days with 3 feeds each: clamped to 5 feeds/day; nothing given in 24 h.
        let mut es = vec![];
        for d in 3..=5 {
            let day = clock.date_start(&format!("2025-11-0{}", d + 1)).unwrap();
            for k in 0..3 {
                es.push(entry(day + k * 6 * 3600, json!({"bottle_formula_ml": 100})));
            }
        }
        let g = guidance(&clock, &Inputs { now, birth, birth_weight_g: Some(3900.0), aim_feeds: None, prep_s: 0, weight_g: 4000.0, entries: &es }).to_json(&clock);
        assert_eq!(g["feeds_per_day"], 5.0);
        assert_eq!(g["minimum"]["max_pause"], "8h00m");
        // 600 ml/day / 5 = 120 per feed; shortfall 600 → capped at +20% → 144 → 145.
        assert_eq!(g["target_ml_per_day"], 600.0);
        assert_eq!(g["next_feed"]["amount_ml"], 145.0);
        // Gaps of 6 h are capped at 24 h / 5 feeds; at the latest 8 h after the last feed.
        assert_eq!(g["next_feed"]["max_gap"], "4h48m");
        let est = crate::status::estimate_feed(&crate::entries::feed_episodes(&es), Some(4 * 3600 + 48 * 60)).unwrap();
        assert_eq!((fmt_dur(est.gap), fmt_dur(est.usual)), ("4h48m".to_string(), "6h00m".to_string()));
        // Only 3 feeds a day: the 5th most recent (4 Nov 06:00) turned 24 h old long ago,
        // so "at the latest" is already past.
        assert_eq!(g["next_feed"]["latest_at"], "2025-11-06 06:00");
        assert_eq!(g["next_feed"]["latest_reason"], "≥ 5 feeds/24 h");
    }

    #[test]
    fn per_kg_days_and_fit() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-31 16:40", 0).unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        // Weighed at midnight 1 Nov (3600) and 3 Nov (3700): +50 g on each of 1 and 2 Nov.
        let es = vec![
            entry(at("2025-11-02 00:00"), json!({"weight_g": 3600})),
            entry(at("2025-11-04 00:00"), json!({"weight_g": 3700})),
            entry(at("2025-11-02 12:00"), json!({"bottle_formula_ml": 360})),
            entry(at("2025-11-03 12:00"), json!({"bottle_breastmilk_ml": 100, "breast_left_min": 5})),
        ];
        let days = per_kg_days(&clock, birth, 3900.0, &es, at("2025-11-05 10:00"), 28);
        let dates: Vec<&str> = days.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, ["2025-11-01", "2025-11-02", "2025-11-03", "2025-11-04"], "full days only");
        let d = &days[1];
        assert_eq!(d.day_of_life, 3);
        assert!((d.intake.unwrap() - 360.0 / 3.625).abs() < 0.01, "{d:?}");
        assert!((d.growth.unwrap() - 50.0 / 3.625).abs() < 0.01, "{d:?}");
        assert!(!d.breast && days[2].breast);
        assert!(days[0].growth.unwrap() < 0.0, "losing weight on day 2");
        assert_eq!(days[3].growth, None, "after the last weighing");
        assert_eq!(days[3].intake, None);
        assert!(fit(&days).is_err(), "first week only");

        // Synthetic later days: intake = 80 + 6 × growth (per kg).
        let mk = |dol: i64, g: f64| PerKgDay { date: String::new(), day_of_life: dol, intake: Some(80.0 + 6.0 * g), growth: Some(g), breast: false };
        let f = fit(&[mk(5, 0.0), mk(8, 5.0), mk(9, 10.0), mk(10, 15.0), mk(11, 20.0), mk(12, 25.0)]).unwrap();
        assert_eq!(f.days, 5);
        assert!((f.baseline - 80.0).abs() < 1e-6 && (f.cost - 6.0).abs() < 1e-6, "{f:?}");
        assert!(fit(&[mk(8, 10.0), mk(9, 11.0), mk(10, 12.0), mk(11, 10.0), mk(12, 11.0)]).unwrap_err().contains("hardly differed"));
    }
}
