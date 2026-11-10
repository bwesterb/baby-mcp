//! The status: what is going on now and what is next. Computed once as a
//! typed `Status` with timestamps; the pages and the notifier read it as
//! is, and `to_json` gives what the `status` MCP tool (and the `status`
//! admin command) return.

use crate::entries::{
    self as en, Baby, Entry, Totals, BY_AGENT, BY_USER, ESTIMATE_BACK, FEED_SQL, TOP_UP_WITHIN, attention, feed_episodes,
    feed_gap, last_where, list, user_names, view,
};
use crate::feeding::Guidance;
use crate::time::{Clock, fmt_dur};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub now: i64,
    pub last_feed: Option<Entry>,
    pub next_feed: Option<FeedEstimate>,
    /// Seconds before a feed to start the routine (diaper first).
    pub prep_s: i64,
    pub last_wet: Option<Entry>,
    pub last_dirty: Option<Entry>,
    pub last_temp: Option<Entry>,
    pub last_weight: Option<Entry>,
    pub wet_warning: Option<WetWarning>,
    pub today: Totals,
    pub last_24h: Totals,
    /// Entries needing a parent or an agent, oldest first.
    pub attention: Vec<Entry>,
    /// When she was back at birth weight: None without birth (weight), Some(None) not yet.
    pub regained: Option<Option<i64>>,
    pub guidance: Option<Guidance>,
    pub bath: Bath,
    pub vitamins: Option<Vitamins>,
    pub sleep: Option<SleepFacing>,
    /// User names, for the entries in `to_json`.
    pub names: HashMap<i64, String>,
}

/// The next feed: the end of the last one (its last top-up) plus the usual gap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeedEstimate {
    pub at: i64,
    pub gap: i64,
    /// How many recent feeds the gap is from.
    pub n: usize,
    /// The usual gap was longer than the plan allows (aim, NVK minimum).
    pub capped: bool,
}

/// Fewer wet diapers in the last 24 hours than usual for her age.
#[derive(Debug, Clone, PartialEq)]
pub struct WetWarning {
    pub wet: usize,
    pub minimum: usize,
    pub text: String,
}

/// Baths: due on the day 3.5 days after the last real bath falls on (a
/// sponge wash doesn't count), alternating with and without soap; reminded
/// at `due_at` that evening, overdue once that day is over. Never logged =
/// due now.
#[derive(Debug, Clone, Default)]
pub struct Bath {
    pub last: Option<Entry>,
    /// The last bath of any kind, a sponge wash too.
    pub last_any: Option<Entry>,
    pub due_day: i64,
    pub due_at: i64,
    pub day_end: i64,
}

impl Bath {
    pub fn never(&self) -> bool {
        self.last.is_none()
    }
    /// How long the due day has been over.
    pub fn overdue(&self, now: i64) -> Option<i64> {
        (!self.never() && now >= self.day_end).then(|| now - self.day_end)
    }
    /// Due today (and not yet overdue).
    pub fn today(&self, now: i64) -> bool {
        !self.never() && now >= self.due_day && now < self.day_end
    }
    /// The other kind than the last.
    pub fn next_kind(&self) -> Option<&'static str> {
        match self.last.as_ref()?.str("bath") {
            Some("with_soap") => Some("without_soap"),
            _ => Some("with_soap"),
        }
    }
}

/// Vitamins (RIVM): which are due today and whether they were given.
#[derive(Debug, Clone)]
pub struct Vitamins {
    /// Day 8: the start of the day they start.
    pub from: i64,
    pub remind_at: i64,
    pub formula_yesterday: f64,
    pub k: Vitamin,
    pub d: Vitamin,
}

#[derive(Debug, Clone)]
pub struct Vitamin {
    pub key: &'static str,
    pub label: &'static str,
    pub active: bool,
    /// The start of the day after the last day.
    pub until: i64,
    pub given_today: bool,
    pub last: Option<Entry>,
    /// Vitamin K with enough formula yesterday.
    pub not_needed: bool,
}

impl Vitamins {
    pub fn all(&self) -> [&Vitamin; 2] {
        [&self.k, &self.d]
    }
    /// Still to give today: K, D.
    pub fn to_give(&self) -> Vec<&'static str> {
        self.all().iter().filter(|v| v.active && !v.given_today && !v.not_needed).map(|v| v.label).collect()
    }
}

/// Sleeping position: alternate the side the face is turned to after every
/// feed. Feeds after the last logged side are assumed to have been followed
/// by a put-down on the other side each time.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SleepFacing {
    pub last: String,
    pub last_at: i64,
    pub feeds_since: usize,
    /// The side she is assumed to lie on now, and the next.
    pub current: String,
    pub next: String,
    /// Right after a feed without a side logged she may not be down yet:
    /// `current` is the side for now.
    pub put_down_now: bool,
}

impl Status {
    /// When to start the next feed: the estimate less the preparation time.
    pub fn feed_start(&self) -> Option<i64> {
        self.next_feed.map(|f| f.at - self.prep_s)
    }

    /// Entries needing a parent (🙋).
    pub fn needs_user(&self) -> impl Iterator<Item = &Entry> {
        self.attention.iter().filter(|e| e.attention_by & BY_USER != 0)
    }

    /// As the `status` tool returns it.
    pub fn to_json(&self, clock: &Clock, baby: &Baby) -> Value {
        let now = self.now;
        let since = |e: &Option<Entry>, extra: Option<Value>| since(e, clock, now, extra);
        let field = |e: &Option<Entry>, k: &str| e.as_ref().and_then(|e| e.fields.get(k).cloned());
        let rel = |t: i64| if t >= now { ("due_in", fmt_dur(t - now)) } else { ("overdue_by", fmt_dur(now - t)) };
        let next_feed = self.next_feed.map(|f| {
            let (k, d) = rel(f.at);
            json!({
                "at": clock.fmt(f.at),
                "basis": if f.capped {
                    format!("last feed + the longest gap the feeding plan allows ({}: the aim or the NVK minimum number of feeds; the median of the last {} feeds is longer; a top-up within {} counts with its feed)", fmt_dur(f.gap), f.n, fmt_dur(TOP_UP_WITHIN))
                } else {
                    format!("last feed + median gap ({}) of the last {} feeds (a top-up within {} counts with its feed)", fmt_dur(f.gap), f.n, fmt_dur(TOP_UP_WITHIN))
                },
                k: d,
            })
        });
        let feed_start = self.next_feed.map(|f| {
            let start = f.at - self.prep_s;
            let (k, d) = rel(start);
            json!({"at": clock.fmt(start), "prep": fmt_dur(self.prep_s), "feed_at": clock.fmt(f.at), k: d})
        });
        let mut out = json!({
            "now": clock.fmt(now),
            "baby": baby.name,
            "last_feed": since(&self.last_feed, self.last_feed.as_ref().map(|e| view(e, clock, &self.names))),
            "next_feed_estimate": next_feed,
            "next_feed_start": feed_start,
            "last_wet_diaper": since(&self.last_wet, None),
            "wet_diaper_warning": self.wet_warning.as_ref().map(|w| json!({"wet_24h": w.wet, "minimum": w.minimum, "text": w.text})),
            "last_dirty_diaper": since(&self.last_dirty, field(&self.last_dirty, "stool_color")),
            "last_temperature": since(&self.last_temp, field(&self.last_temp, "temp_c")),
            "last_weight": since(&self.last_weight, field(&self.last_weight, "weight_g")),
            "today": self.today,
            "last_24h": self.last_24h,
            "needs_attention": self.attention.iter().map(|e| view(e, clock, &self.names)).collect::<Vec<_>>(),
            "feeding_guidance": self.guidance.as_ref().map(|g| g.to_json(clock)),
            "growth_note": "weight_percentile compares with the WHO Child Growth Standards (weight-for-age), which ignore the normal weight loss of the first days; weight_change_percentile compares the change since birth with newborns of the same delivery mode during the first month. References, not a diagnosis.",
        });
        if let Some(b) = baby.birth {
            out["age"] = json!(fmt_dur(now - b));
        }
        if let Some(bw) = baby.birth_weight_g {
            out["birth_weight_g"] = json!(bw);
        }
        if let Some((w, wt)) = self.last_weight.as_ref().and_then(|e| Some((e.num("weight_g")?, e.at))) {
            out["weight_vs_birth"] = json!(en::weight_vs_birth(w, baby.birth_weight_g));
            out["weight_percentile"] = baby.percentile_json(w, wt);
            out["weight_change_percentile"] = baby.change_json(w, wt);
        }
        if let (Some(bw), Some(b)) = (baby.birth_weight_g, baby.birth) {
            out["birth_weight_percentile"] = baby.percentile_json(bw, b);
        }
        if let (Some(r), Some(b)) = (self.regained, baby.birth) {
            out["birth_weight_regained"] = match r {
                Some(t) => json!({"regained": true, "on": clock.day(t), "age_days": crate::growth::age_days(b, t)}),
                None => json!({"regained": false}),
            };
        }
        let bath = &self.bath;
        let mut due = json!({"on": clock.day(bath.due_day), "at": clock.fmt(bath.due_at)});
        if bath.never() {
            due["never"] = json!(true);
        } else if let Some(o) = bath.overdue(now) {
            due["overdue_by"] = json!(fmt_dur(o));
        } else {
            if bath.today(now) {
                due["today"] = json!(true);
            }
            if bath.due_at > now {
                due["due_in"] = json!(fmt_dur(bath.due_at - now));
            }
        }
        out["bath"] = json!({
            "goal": en::BATH_GOAL,
            "last": since(&bath.last, field(&bath.last, "bath")),
            "last_any": since(&bath.last_any, field(&bath.last_any, "bath")),
            "due": due,
            "next_kind": bath.next_kind(),
        });
        if let Some(v) = &self.vitamins {
            let mut j = json!({"from": clock.day(v.from), "remind_at": clock.fmt(v.remind_at),
                               "rule": "vitamin K daily from day 8 until 12 weeks, not needed with at least 500 ml formula a day (judged on yesterday's total); vitamin D daily from day 8 until 4 years (RIVM)",
                               "to_give_today": v.to_give()});
            for x in v.all() {
                j[x.key] = json!({"active": x.active, "until": clock.day(x.until - 1), "given_today": x.given_today, "last": since(&x.last, None)});
                if x.not_needed {
                    j[x.key]["not_needed"] = json!(vitamin_k_reason(v.formula_yesterday));
                }
            }
            out["vitamins"] = j;
        }
        if let Some(s) = &self.sleep {
            let mut v = json!({"last": s.last, "last_at": clock.fmt(s.last_at), "feeds_since": s.feeds_since, "next": s.next,
                               "note": "alternate the side the face is turned to after every feed; feeds since the last logged side are assumed to have alternated"});
            if s.feeds_since > 0 {
                v["assumed_current"] = json!(s.current);
            }
            if s.put_down_now {
                v["put_down_now"] = json!(s.current);
            }
            out["sleep_facing"] = v;
        }
        out
    }
}

/// Why vitamin K is not needed today.
pub fn vitamin_k_reason(formula_yesterday: f64) -> String {
    format!("{formula_yesterday:.0} ml formula yesterday: with at least {:.0} ml a day, formula gives enough vitamin K", en::VITAMIN_K_FORMULA_ML)
}

fn since(e: &Option<Entry>, clock: &Clock, now: i64, extra: Option<Value>) -> Value {
    match e {
        None => Value::Null,
        Some(e) => {
            let mut v = json!({"at": clock.fmt(e.at), "ago": fmt_dur(now - e.at), "entry_id": e.entry_id});
            if let Some(x) = extra {
                v["value"] = x;
            }
            v
        }
    }
}

/// The next feed from feed episodes: the end of the last one plus the usual
/// gap, at most `max_gap` (what the feeding plan allows). The one estimate
/// of the next feed: tiles, notifications and the timeline use it.
pub fn estimate_feed(eps: &[(i64, i64)], max_gap: Option<i64>) -> Option<FeedEstimate> {
    let (usual, &(_, last)) = (feed_gap(eps)?, eps.last()?);
    let gap = max_gap.map_or(usual, |m| usual.min(m));
    Some(FeedEstimate { at: last + gap, gap, n: eps.len().min(9), capped: gap < usual })
}

/// Fewer wet diapers in the last 24 hours than usual for her age: at least as
/// many as the day of life up to day 5, then at least 6 (breastfeeding and
/// kraamzorg guidance). Only once she is a day old and while someone is
/// logging (an entry in the last 6 hours). `day` is the last 24 hours.
pub fn wet_warning(clock: &Clock, baby: &Baby, day: &[Entry], now: i64) -> Option<WetWarning> {
    let birth = baby.birth?;
    if now - birth < 86400 || !day.iter().any(|e| e.at > now - 6 * 3600) {
        return None;
    }
    let dol = crate::feeding::day_of_life(clock, birth, now);
    let min = dol.min(6) as usize;
    let wet = day.iter().filter(|e| e.is("wet")).count();
    if wet >= min {
        return None;
    }
    let rule = if dol >= 6 { "usually at least 6 from day 6".to_string() } else { format!("usually at least {min} on day {dol}") };
    Some(WetWarning { wet, minimum: min, text: format!("Only {wet} wet diaper{} in the last 24 h ({rule})", if wet == 1 { "" } else { "s" }) })
}

/// After a feed without a side logged, how long "put her down now facing …"
/// is shown (she may not be down yet).
const PUT_DOWN_WITHIN: i64 = 2 * 3600;

/// How far back `status` reads the log in one go (from the start of today):
/// the feeding guidance's three full days and more. Older "last" entries
/// (a weighing a week ago) are looked up on their own.
const WINDOW_DAYS: i64 = 4;

pub fn status(c: &Connection, clock: &Clock, baby: &Baby, now: i64) -> Result<Status, String> {
    // One read of the recent log; the parts below slice it.
    let start = clock.day_start(now) - WINDOW_DAYS * 86400;
    let window = list(c, start, i64::MAX, false)?;
    let upto_now = &window[..window.partition_point(|e| e.at <= now)];
    let since = |from: i64| &upto_now[upto_now.partition_point(|e| e.at < from)..];
    // Totals over [from, to) (a feed just before `from` only tells whether
    // the first ones are top-ups).
    let totals = |from: i64, to: i64| en::totals_from(since(from - en::FEED_LOOKBACK).iter().filter(|e| e.at < to), from);
    // The last matching entry: from the window, or else the database.
    let last = |pred: &dyn Fn(&Entry) -> bool, cond: &str| match window.iter().rev().find(|e| pred(e)) {
        Some(e) => Ok(Some(e.clone())),
        None => last_where(c, cond),
    };
    let has = |k: &'static str| move |e: &Entry| e.fields.get(k).is_some_and(|v| !v.is_null());

    let last_weight = last(&has("weight_g"), "weight_g IS NOT NULL")?;
    let regained = match (baby.birth_weight_g, baby.birth) {
        // Back at birth weight: the first weighing at or above it after birth.
        (Some(bw), Some(b)) => Some(en::query(c, "deleted = 0 AND weight_g >= ?2 AND at > ?1 ORDER BY at LIMIT 1", rusqlite::params![b, bw])?.pop().map(|e| e.at)),
        _ => None,
    };
    let guidance = crate::feeding::for_status(clock, baby, last_weight.as_ref().and_then(|e| Some((e.num("weight_g")?, e.at))), upto_now, now);

    let last_bath = last(&|e| matches!(e.str("bath"), Some("with_soap" | "without_soap")), "bath IN ('with_soap', 'without_soap')")?;
    let due_day = clock.day_start(last_bath.as_ref().map(|e| e.at + en::BATH_EVERY).unwrap_or(now));
    let bath = Bath {
        due_at: if last_bath.is_some() { clock.at_hour(due_day, en::BATH_REMIND_HOUR) } else { now },
        day_end: clock.next_day_start(due_day),
        due_day,
        last: last_bath,
        last_any: last(&has("bath"), "bath IS NOT NULL")?,
    };

    let vitamins = match baby.birth {
        None => None,
        Some(b) => {
            let today = clock.day_start(now);
            let from = en::day_plus(clock, b, jiff::Span::new().days(7));
            let formula_yesterday = totals(clock.day_start(today - 3600), today).formula_ml;
            let mut vs = vec![];
            for (key, field, label) in en::VITAMINS {
                let until = en::day_plus(clock, b, if key == "k" { jiff::Span::new().weeks(12) } else { jiff::Span::new().years(4) });
                let active = today >= from && today < until;
                let last = last(&|e| e.is(field), &format!("{field} = 1"))?;
                vs.push(Vitamin {
                    key,
                    label,
                    active,
                    until,
                    given_today: last.as_ref().is_some_and(|e| e.at >= today),
                    last,
                    not_needed: key == "k" && active && formula_yesterday >= en::VITAMIN_K_FORMULA_ML,
                });
            }
            let [k, d]: [Vitamin; 2] = vs.try_into().expect("two vitamins");
            Some(Vitamins { from, remind_at: clock.at_hour(today, en::VITAMIN_REMIND_HOUR), formula_yesterday, k, d })
        }
    };

    let mut sleep = None;
    let last_side = last(&has("sleep_facing"), "sleep_facing IS NOT NULL")?;
    if let Some((e, side)) = last_side.as_ref().and_then(|e| Some((e, e.str("sleep_facing")?))) {
        let other = |s: &str| if s == "left" { "right" } else { "left" }.to_string();
        // Feeds since (a top-up isn't a new feed: no new put-down).
        let from = e.at - en::FEED_LOOKBACK;
        let older;
        let es = if from >= start {
            since(from)
        } else {
            older = list(c, from, now + 1, false)?;
            &older[..]
        };
        let since: Vec<(i64, i64)> = feed_episodes(es).into_iter().filter(|f| f.0 > e.at).collect();
        let current = if since.len().is_multiple_of(2) { side.to_string() } else { other(side) };
        sleep = Some(SleepFacing {
            last: side.to_string(),
            last_at: e.at,
            feeds_since: since.len(),
            next: other(&current),
            current,
            put_down_now: since.last().is_some_and(|f| now - f.1 < PUT_DOWN_WITHIN),
        });
    }

    Ok(Status {
        now,
        last_feed: last(&Entry::is_feed, FEED_SQL)?,
        next_feed: estimate_feed(&feed_episodes(since(now - ESTIMATE_BACK)), guidance.as_ref().map(|g| g.max_gap)),
        prep_s: baby.feed_prep_s,
        last_wet: last(&|e| e.is("wet"), "wet = 1")?,
        last_dirty: last(&|e| e.is("dirty"), "dirty = 1")?,
        last_temp: last(&has("temp_c"), "temp_c IS NOT NULL")?,
        wet_warning: wet_warning(clock, baby, since(now - 24 * 3600), now),
        last_weight,
        today: totals(clock.day_start(now), now + 1),
        last_24h: totals(now - 24 * 3600, now + 1),
        attention: attention(c, BY_USER | BY_AGENT)?,
        regained,
        guidance,
        bath,
        vitamins,
        sleep,
        names: user_names(c)?,
    })
}

/// The status as the `status` tool returns it.
pub fn status_json(c: &Connection, clock: &Clock, baby: &Baby, now: i64) -> Result<Value, String> {
    Ok(status(c, clock, baby, now)?.to_json(clock, baby))
}

/// An entry at `at` with `fields`, for tests.
#[cfg(test)]
pub fn entry(at: i64, fields: Value) -> Entry {
    Entry {
        entry_id: 1, version: 1, at, duration_s: None, text: String::new(), fields: fields.as_object().cloned().unwrap_or_default(),
        attention_by: 0, attention_why: None, deleted: false, changed_at: at, changed_by: 1, superseded_at: None,
    }
}

/// Both vitamins from `from`, given today or not, for tests.
#[cfg(test)]
pub fn vitamins(clock: &Clock, from: &str, remind_at: i64, k: (bool, bool), d: (bool, bool)) -> Vitamins {
    let v = |key, label, (active, given_today): (bool, bool), until: &str| Vitamin {
        key, label, active, given_today, until: clock.date_start(until).unwrap() + 86400, last: None, not_needed: false,
    };
    Vitamins { from: clock.date_start(from).unwrap(), remind_at, formula_yesterday: 0.0, k: v("k", "K", k, "2026-01-22"), d: v("d", "D", d, "2029-10-30") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::{NewEntry, insert};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A status reads the recent log once: a few statements, not one per part.
    #[test]
    fn few_queries() {
        let (_d, db, u) = crate::entries::tests::setup();
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let now = at("2025-11-10 12:00");
        let baby = Baby { name: "B".into(), birth: Some(at("2025-10-31 16:40")), birth_weight_g: Some(3900.0), sex: None, growth: None,
                          delivery: None, change: None, aim_feeds_per_day: Some(7.0), feed_prep_s: 900 };
        let n = Arc::new(AtomicUsize::new(0));
        db.with(|c| {
            // A weighing and a temperature long ago; then five days of feeds,
            // diapers, vitamins and the sleep side, and a bath.
            let log = |t: i64, f: Value| insert(c, u, &NewEntry { at: t, duration_s: None, text: "x".into(),
                fields: f.as_object().cloned().unwrap(), attention_by: 0, attention_why: None }, now).unwrap();
            log(at("2025-11-01 10:00"), json!({"weight_g": 3700, "temp_c": 36.8}));
            for k in 0..40 {
                let t = at("2025-11-05 08:00") + k * 3 * 3600;
                if t < now {
                    log(t, json!({"bottle_formula_ml": 70, "wet": true, "dirty": k % 3 == 0, "sleep_facing": "left", "vitamin_d": k % 8 == 0, "vitamin_k": k % 8 == 0}));
                }
            }
            log(at("2025-11-08 19:00"), json!({"bath": "with_soap"}));
            let counter = n.clone();
            c.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, Some(move |_: rusqlite::trace::TraceEvent<'_>| {
                counter.fetch_add(1, Ordering::Relaxed);
            }))?;
            let s = status(c, &clock, &baby, now).unwrap();
            assert!(s.next_feed.is_some() && s.vitamins.is_some() && s.sleep.is_some() && s.last_temp.is_some());
            Ok(())
        }).unwrap();
        // The window, attention, names, birth weight regained, and the last
        // weighing and temperature (older than the window).
        assert_eq!(n.load(Ordering::Relaxed), 6);
    }
}
