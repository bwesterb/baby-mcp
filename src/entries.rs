//! Entries: storage, versions, and the status and statistics derived from them.

use crate::registry::{self, Group};
use crate::time::Clock;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub const BY_USER: i64 = 1;
pub const BY_AGENT: i64 = 2;

#[derive(Debug, Clone)]
pub struct Entry {
    pub entry_id: i64,
    pub version: i64,
    pub at: i64,
    pub duration_s: Option<i64>,
    pub text: String,
    pub fields: Map<String, Value>,
    pub attention_by: i64,
    pub attention_why: Option<String>,
    pub deleted: bool,
    pub changed_at: i64,
    pub changed_by: i64,
    pub superseded_at: Option<i64>,
}

const COLS: &str = "entry_id, version, at, duration_s, text, fields, needs_attention_by, \
                    needs_attention_why, deleted, changed_at, changed_by";

/// SQL condition (over the generated columns) for "this entry is a feed".
pub const FEED_SQL: &str = "(coalesce(bottle_breastmilk_ml, 0) > 0 OR coalesce(bottle_formula_ml, 0) > 0 \
     OR coalesce(breast_left_min, 0) > 0 OR coalesce(breast_right_min, 0) > 0 \
     OR (latch IS NOT NULL AND latch != 'none'))";

fn from_row(r: &Row, superseded: bool) -> rusqlite::Result<Entry> {
    let fields: String = r.get(5)?;
    Ok(Entry {
        entry_id: r.get(0)?,
        version: r.get(1)?,
        at: r.get(2)?,
        duration_s: r.get(3)?,
        text: r.get(4)?,
        fields: serde_json::from_str(&fields).unwrap_or_default(),
        attention_by: r.get(6)?,
        attention_why: r.get(7)?,
        deleted: r.get::<_, i64>(8)? != 0,
        changed_at: r.get(9)?,
        changed_by: r.get(10)?,
        superseded_at: if superseded { r.get(11)? } else { None },
    })
}

pub(crate) fn query(c: &Connection, sql_where: &str, p: impl rusqlite::Params) -> Result<Vec<Entry>, String> {
    let sql = format!("SELECT {COLS} FROM entry WHERE {sql_where}");
    let mut st = c.prepare(&sql).map_err(err)?;
    let rows = st.query_map(p, |r| from_row(r, false)).map_err(err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(err)
}

pub fn err(e: rusqlite::Error) -> String {
    format!("database: {e}")
}

impl Entry {
    pub fn num(&self, name: &str) -> Option<f64> {
        self.fields.get(name).and_then(Value::as_f64)
    }
    pub fn is(&self, name: &str) -> bool {
        self.fields.get(name).and_then(Value::as_bool) == Some(true)
    }
    pub fn str(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }
    pub fn is_feed(&self) -> bool {
        ["bottle_breastmilk_ml", "bottle_formula_ml", "breast_left_min", "breast_right_min"]
            .iter()
            .any(|f| self.num(f).unwrap_or(0.0) > 0.0)
            || self.str("latch").is_some_and(|l| l != "none")
    }
    pub fn groups(&self) -> Vec<Group> {
        registry::groups(&self.fields)
    }
}

pub struct NewEntry {
    pub at: i64,
    pub duration_s: Option<i64>,
    pub text: String,
    pub fields: Map<String, Value>,
    pub attention_by: i64,
    pub attention_why: Option<String>,
}

pub fn insert(c: &Connection, user: i64, n: &NewEntry, now: i64) -> Result<Entry, String> {
    check_attention(n.attention_by, &n.attention_why)?;
    c.execute(
        "INSERT INTO entry (at, duration_s, text, fields, needs_attention_by, needs_attention_why, \
         changed_at, changed_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            n.at,
            n.duration_s,
            n.text,
            Value::Object(n.fields.clone()).to_string(),
            n.attention_by,
            n.attention_why,
            now,
            user
        ],
    )
    .map_err(err)?;
    get(c, c.last_insert_rowid())
}

fn check_attention(by: i64, why: &Option<String>) -> Result<(), String> {
    match (by, why) {
        (0, None) => Ok(()),
        (1..=3, Some(w)) if !w.trim().is_empty() => Ok(()),
        (0, Some(_)) => Err("needs_attention_why given but nobody needs to attend".into()),
        _ => Err("needs attention: give both who (user and/or agent) and why".into()),
    }
}

pub fn get(c: &Connection, id: i64) -> Result<Entry, String> {
    query(c, "entry_id = ?1", [id])?
        .pop()
        .ok_or_else(|| format!("no entry with id {id}"))
}

/// Stores `e` as the next version of its entry.
pub fn save(c: &Connection, user: i64, e: &Entry, now: i64) -> Result<Entry, String> {
    check_attention(e.attention_by, &e.attention_why)?;
    let n = c
        .execute(
            "UPDATE entry SET version = ?1, at = ?2, duration_s = ?3, text = ?4, fields = ?5, \
             needs_attention_by = ?6, needs_attention_why = ?7, deleted = ?8, changed_at = ?9, \
             changed_by = ?10 WHERE entry_id = ?11 AND version = ?12",
            params![
                e.version + 1,
                e.at,
                e.duration_s,
                e.text,
                Value::Object(e.fields.clone()).to_string(),
                e.attention_by,
                e.attention_why,
                e.deleted as i64,
                now,
                user,
                e.entry_id,
                e.version
            ],
        )
        .map_err(err)?;
    if n != 1 {
        return Err(format!("entry {} changed meanwhile; read it again", e.entry_id));
    }
    get(c, e.entry_id)
}

pub fn list(c: &Connection, from: i64, to: i64, deleted: bool) -> Result<Vec<Entry>, String> {
    let w = if deleted { "" } else { "deleted = 0 AND " };
    query(c, &format!("{w}at >= ?1 AND at < ?2 ORDER BY at, entry_id"), [from, to])
}

pub fn history(c: &Connection, id: i64) -> Result<Vec<Entry>, String> {
    let cur = get(c, id)?;
    let mut st = c
        .prepare(&format!("SELECT {COLS}, superseded_at FROM entry_old WHERE entry_id = ?1 ORDER BY version"))
        .map_err(err)?;
    let mut out = st
        .query_map([id], |r| from_row(r, true))
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    out.push(cur);
    Ok(out)
}

pub fn attention(c: &Connection, mask: i64) -> Result<Vec<Entry>, String> {
    query(c, "deleted = 0 AND (needs_attention_by & ?1) != 0 ORDER BY at", [mask])
}

/// Non-deleted entries within `window` seconds of `at` sharing a group.
pub fn nearby(c: &Connection, at: i64, window: i64, groups: &[Group]) -> Result<Vec<Entry>, String> {
    Ok(list(c, at - window, at + window + 1, false)?
        .into_iter()
        .filter(|e| e.groups().iter().any(|g| groups.contains(g)))
        .collect())
}


pub fn last_where(c: &Connection, cond: &str) -> Result<Option<Entry>, String> {
    Ok(query(c, &format!("deleted = 0 AND {cond} ORDER BY at DESC, entry_id DESC LIMIT 1"), [])?.pop())
}

// ------------------------------------------------------------- users

pub fn user_names(c: &Connection) -> Result<HashMap<i64, String>, String> {
    let mut st = c.prepare("SELECT id, display_name FROM user").map_err(err)?;
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(err)?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>().map_err(err)
}

/// Who first logged each entry (display names), by entry id: the author of
/// version 1, which for edited entries is in `entry_old`.
pub fn loggers(c: &Connection, entries: &[Entry]) -> Result<HashMap<i64, String>, String> {
    let names = user_names(c)?;
    let mut st = c.prepare("SELECT changed_by FROM entry_old WHERE entry_id = ?1 AND version = 1").map_err(err)?;
    let mut out = HashMap::new();
    for e in entries {
        let by = if e.version == 1 {
            e.changed_by
        } else {
            st.query_row([e.entry_id], |r| r.get(0)).unwrap_or(e.changed_by)
        };
        if let Some(n) = names.get(&by) {
            out.insert(e.entry_id, n.clone());
        }
    }
    Ok(out)
}

pub fn user_by_name(c: &Connection, name: &str) -> Result<Option<(i64, String)>, String> {
    c.query_row("SELECT id, display_name FROM user WHERE name = ?1", [name], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
    .map_err(err)
}

// ------------------------------------------------------------- views

pub fn attention_names(by: i64) -> Vec<&'static str> {
    let mut v = vec![];
    if by & BY_USER != 0 {
        v.push("user");
    }
    if by & BY_AGENT != 0 {
        v.push("agent");
    }
    v
}

pub fn view(e: &Entry, clock: &Clock, names: &HashMap<i64, String>) -> Value {
    let mut v = json!({
        "id": e.entry_id,
        "at": clock.fmt(e.at),
        "text": e.text,
        "fields": e.fields,
        "version": e.version,
        "changed": format!("{} by {}", clock.fmt(e.changed_at),
                           names.get(&e.changed_by).map(String::as_str).unwrap_or("?")),
    });
    if let Some(d) = e.duration_s {
        v["duration_min"] = json!((d + 30) / 60);
        v["until"] = json!(clock.fmt_time(e.at + d));
    }
    if e.attention_by != 0 {
        v["needs_attention"] = json!({"by": attention_names(e.attention_by), "why": e.attention_why});
    }
    if e.deleted {
        v["deleted"] = json!(true);
    }
    if let Some(s) = e.superseded_at {
        v["superseded"] = json!(clock.fmt(s));
    }
    v
}

// ------------------------------------------------------------- statistics

#[derive(Debug, Default, Serialize, Clone)]
pub struct Totals {
    pub feeds: u32,
    pub breastmilk_ml: f64,
    pub formula_ml: f64,
    pub total_ml: f64,
    pub breast_min: i64,
    pub breast_left_min: i64,
    pub breast_right_min: i64,
    /// Diapers that were wet (whether or not also dirty) and dirty (idem).
    pub wet: u32,
    pub dirty: u32,
    /// Each diaper counted once: wet only, dirty only, or both.
    pub diapers: u32,
    pub wet_only: u32,
    pub dirty_only: u32,
    pub wet_and_dirty: u32,
    pub vomit: u32,
}

/// Totals; feeds counted as episodes (a top-up counts with its feed). See
/// `totals_from` for a top-up of a feed just before the entries.
#[cfg(test)]
pub fn totals<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Totals {
    totals_from(entries, i64::MIN)
}

/// Totals of the entries from `from`; the earlier ones (up to FEED_LOOKBACK
/// before) only tell whether the first feeds are top-ups.
pub fn totals_from<'a>(entries: impl IntoIterator<Item = &'a Entry>, from: i64) -> Totals {
    let mut t = Totals::default();
    // The feed so far: its start and last entry.
    let mut feed: Option<(i64, i64)> = None;
    for e in entries {
        if e.deleted {
            continue;
        }
        let new_feed = e.is_feed() && !feed.is_some_and(|f| is_top_up(f, e.at));
        if e.is_feed() {
            feed = Some(if new_feed { (e.at, e.at) } else { (feed.map_or(e.at, |f| f.0), e.at) });
        }
        if e.at < from {
            continue;
        }
        if new_feed {
            t.feeds += 1;
        }
        t.breastmilk_ml += e.num("bottle_breastmilk_ml").unwrap_or(0.0);
        t.formula_ml += e.num("bottle_formula_ml").unwrap_or(0.0);
        let (l, r) = (e.num("breast_left_min").unwrap_or(0.0) as i64, e.num("breast_right_min").unwrap_or(0.0) as i64);
        t.breast_left_min += l;
        t.breast_right_min += r;
        t.breast_min += l + r;
        let (wet, dirty) = (e.is("wet"), e.is("dirty"));
        t.wet += wet as u32;
        t.dirty += dirty as u32;
        match (wet, dirty) {
            (true, true) => t.wet_and_dirty += 1,
            (true, false) => t.wet_only += 1,
            (false, true) => t.dirty_only += 1,
            _ => {}
        }
        t.diapers += (wet || dirty) as u32;
        t.vomit += e.is("vomit") as u32;
    }
    t.total_ml = t.breastmilk_ml + t.formula_ml;
    t
}

pub struct Baby {
    pub name: String,
    pub birth: Option<i64>,
    pub birth_weight_g: Option<f64>,
    pub sex: Option<crate::growth::Sex>,
    pub growth: Option<std::sync::Arc<crate::growth::Table>>,
    pub delivery: Option<crate::growth::Delivery>,
    pub change: Option<std::sync::Arc<crate::growth::WeightChange>>,
    pub aim_feeds_per_day: Option<f64>,
    /// Seconds before a feed to start the routine (diaper first).
    pub feed_prep_s: i64,
}

impl Baby {
    /// Weight-for-age percentile (WHO) of `grams` at time `t`.
    pub fn weight_percentile(&self, grams: f64, t: i64) -> Option<(f64, f64)> {
        let (sex, birth, table) = (self.sex?, self.birth?, self.growth.as_ref()?);
        if t < birth {
            return None;
        }
        let z = table.zscore(sex, crate::growth::age_days(birth, t), grams / 1000.0)?;
        Some((crate::growth::percentile(z), z))
    }

    /// Percentage weight change since birth at time `t`, and its percentile
    /// among newborns with the same delivery mode (first month only).
    pub fn change_percentile(&self, grams: f64, t: i64) -> Option<(f64, f64, String)> {
        let (birth, bw, d, table) = (self.birth?, self.birth_weight_g?, self.delivery?, self.change.as_ref()?);
        if t < birth || bw <= 0.0 {
            return None;
        }
        let change = 100.0 * (grams - bw) / bw;
        let p = table.percentile(d, (t - birth) as f64 / 86400.0, change)?;
        Some((change, p, table.label(p)))
    }

    pub(crate) fn change_json(&self, grams: f64, t: i64) -> Value {
        match (self.change_percentile(grams, t), self.delivery) {
            (Some((c, p, label)), Some(d)) => json!({
                "percent_change": (c * 10.0).round() / 10.0, "percentile": p.round(), "label": label,
                "reference": format!("newborns after a {} birth (Paul et al., Pediatrics 2016; percentiles read from the published figure, approximate)",
                                     if d == crate::growth::Delivery::Cesarean { "cesarean" } else { "vaginal" }),
            }),
            _ => Value::Null,
        }
    }

    pub(crate) fn percentile_json(&self, grams: f64, t: i64) -> Value {
        match self.weight_percentile(grams, t) {
            Some((p, z)) => json!({"percentile": (p * 10.0).round() / 10.0, "label": crate::growth::label(p),
                                   "z": (z * 100.0).round() / 100.0}),
            None => Value::Null,
        }
    }
}

pub(crate) fn weight_vs_birth(w: f64, birth: Option<f64>) -> Option<String> {
    let b = birth.filter(|b| *b > 0.0)?;
    Some(format!("{:+.0} g ({:+.1}%)", w - b, 100.0 * (w - b) / b))
}

/// A feed entry at most this long after the previous one is a top-up: part of
/// the same feed, as long as that feed started at most FEED_SPAN_MAX before.
pub const TOP_UP_WITHIN: i64 = 90 * 60;
/// The longest one feed lasts, top-ups included: frequent feeds (cluster
/// feeding) are separate feeds after this, not one ever longer feed.
pub const FEED_SPAN_MAX: i64 = 2 * 3600;
/// How far before a range to look to tell whether its first feeds are top-ups.
pub const FEED_LOOKBACK: i64 = FEED_SPAN_MAX;

/// Whether a feed entry at `at` tops up the feed `(start, last entry)`.
pub fn is_top_up(feed: (i64, i64), at: i64) -> bool {
    at - feed.1 <= TOP_UP_WITHIN && at - feed.0 <= FEED_SPAN_MAX
}

/// Feeds as episodes (start, end): feed entries within TOP_UP_WITHIN of the
/// previous one (and FEED_SPAN_MAX of the start) are top-ups of the same
/// feed. Entries oldest first.
pub fn feed_episodes<'a>(es: impl IntoIterator<Item = &'a Entry>) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = vec![];
    for e in es.into_iter().filter(|e| e.is_feed() && !e.deleted) {
        match out.last_mut() {
            Some(last) if is_top_up(*last, e.at) => last.1 = e.at,
            _ => out.push((e.at, e.at)),
        }
    }
    out
}

/// The usual gap between feeds: the median of up to 8 recent gaps from the end
/// of one feed (its last top-up) to the start of the next.
pub fn feed_gap(eps: &[(i64, i64)]) -> Option<i64> {
    median(eps.windows(2).map(|w| w[1].0 - w[0].1).filter(|g| *g >= ESTIMATE_MIN_GAP).rev().take(8).collect())
}

/// Gaps shorter than this don't count for the feed estimate.
const ESTIMATE_MIN_GAP: i64 = 30 * 60;

/// How far back the feed estimate looks.
pub const ESTIMATE_BACK: i64 = 36 * 3600;

fn median(mut v: Vec<i64>) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

/// Vitamins (RIVM): K daily from day 8 until 12 weeks, D daily from day 8
/// until 4 years. A reminder is due at 18:00 when one wasn't given that day.
pub const VITAMINS: [(&str, &str, &str); 2] = [("k", "vitamin_k", "K"), ("d", "vitamin_d", "D")];
/// No extra vitamin K with at least this much formula a day (formula has
/// enough); judged on the day before, so it holds for the whole day.
pub const VITAMIN_K_FORMULA_ML: f64 = 500.0;
pub const VITAMIN_REMIND_HOUR: i64 = 18;

/// The start of the local day `span` after the local day of `t`.
pub(crate) fn day_plus(clock: &Clock, t: i64, span: jiff::Span) -> i64 {
    let d = clock.zoned(t).date().checked_add(span).expect("date in range");
    clock.date_start(&d.to_string()).expect("valid date")
}

/// Two baths a week, alternating with and without soap.
pub const BATH_EVERY: i64 = 84 * 3600;
pub const BATH_GOAL: &str = "two baths a week, alternating with and without soap: a bath is due on the day 3.5 days after the last one (a sponge wash doesn't count), of the other kind than the last; reminded at 18:00 that day, overdue once the day is over";
/// The bath reminder: this hour on the day a bath is due.
pub const BATH_REMIND_HOUR: i64 = 18;

#[derive(Debug, Serialize)]
pub struct DayStats {
    pub date: String,
    #[serde(flatten)]
    pub totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_min_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_max_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_g: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_vs_birth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_percentile: Option<String>,
    /// The night that ended this morning: the longest stretch between feeds
    /// overlapping 00:00–05:00 (see `night_stretch`), in minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_night_gap_min: Option<i64>,
}

/// The night's hours: a stretch between feeds counts for the night when it
/// overlaps midnight to 05:00.
pub const NIGHT_UNTIL_HOUR: i8 = 5;
/// How far before a day to look for the feed a night stretch starts with.
pub const NIGHT_LOOKBACK: i64 = 24 * 3600;

/// The longest stretch between feeds (from the end of one, its last top-up,
/// to the start of the next) overlapping 00:00–05:00 of the day starting at
/// `day`. None without a feed before and after it (a stretch still going on
/// counts once she has fed). Episodes oldest first.
pub fn night_stretch(clock: &Clock, eps: &[(i64, i64)], day: i64) -> Option<i64> {
    let until = clock.zoned(day).date().at(NIGHT_UNTIL_HOUR, 0, 0, 0);
    let until = clock.civil(until).ok()?;
    eps.windows(2).map(|w| (w[0].1, w[1].0)).filter(|(a, b)| *a < until && *b > day).map(|(a, b)| b - a).max()
}

/// Per-day statistics for the local days from `from` (a day start) up to `to`.
pub fn stats(c: &Connection, clock: &Clock, baby: &Baby, from: i64, to: i64) -> Result<Vec<DayStats>, String> {
    Ok(stats_of(clock, baby, &list(c, from - NIGHT_LOOKBACK, to, false)?, from, to))
}

/// `stats` from entries already read: at least from NIGHT_LOOKBACK before
/// `from` up to `to`, oldest first.
pub fn stats_of(clock: &Clock, baby: &Baby, all: &[Entry], from: i64, to: i64) -> Vec<DayStats> {
    let eps = feed_episodes(all);
    let mut out = vec![];
    let mut start = clock.day_start(from);
    while start < to {
        let end = clock.next_day_start(start);
        let day: Vec<&Entry> = all.iter().filter(|e| e.at >= start && e.at < end).collect();
        let temps: Vec<f64> = day.iter().filter_map(|e| e.num("temp_c")).collect();
        let weight = day.iter().filter_map(|e| e.num("weight_g")).next_back();
        out.push(DayStats {
            date: clock.day(start),
            totals: totals_from(all.iter().filter(|e| e.at >= start - FEED_LOOKBACK && e.at < end), start),
            temp_min_c: temps.iter().copied().reduce(f64::min),
            temp_max_c: temps.iter().copied().reduce(f64::max),
            weight_g: weight,
            weight_vs_birth: weight.and_then(|w| weight_vs_birth(w, baby.birth_weight_g)),
            weight_percentile: day
                .iter().rfind(|e| e.num("weight_g").is_some())
                .and_then(|e| baby.weight_percentile(e.num("weight_g")?, e.at))
                .map(|(p, _)| crate::growth::label(p)),
            longest_night_gap_min: night_stretch(clock, &eps, start).map(|s| (s + 30) / 60),
        });
        start = end;
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::db::Db;

    pub fn setup() -> (tempfile::TempDir, Db, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.sqlite")).unwrap();
        let uid = db
            .with(|c| {
                c.execute("INSERT INTO user (name, display_name, created_at) VALUES ('t', 'Tester', 0)", [])?;
                Ok(c.last_insert_rowid())
            })
            .unwrap();
        (dir, db, uid)
    }

    fn fields(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn versions_and_generated_columns() {
        let (_d, db, u) = setup();
        db.with(|c| {
            let e = insert(c, u, &NewEntry {
                at: 1000, duration_s: None, text: "drank 60".into(),
                fields: fields(json!({"bottle_breastmilk_ml": 60, "wet": true, "dirty": null})),
                attention_by: 0, attention_why: None,
            }, 1000).unwrap();
            // generated columns, and the two kinds of none
            let (ml, dirty, dirty_type, temp_type): (f64, Option<i64>, Option<String>, Option<String>) = c.query_row(
                "SELECT bottle_breastmilk_ml, dirty, json_type(fields, '$.dirty'), json_type(fields, '$.temp_c') FROM entry",
                [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            assert_eq!(ml, 60.0);
            assert_eq!(dirty, None);
            assert_eq!(dirty_type.as_deref(), Some("null")); // deliberately nothing
            assert_eq!(temp_type, None); // unknown
            let mut e2 = e.clone();
            e2.fields.insert("bottle_breastmilk_ml".into(), json!(70));
            let e2 = save(c, u, &e2, 2000).unwrap();
            assert_eq!(e2.version, 2);
            let h = history(c, e.entry_id).unwrap();
            assert_eq!(h.len(), 2);
            assert_eq!(h[0].num("bottle_breastmilk_ml"), Some(60.0));
            assert_eq!(h[0].superseded_at, Some(2000));
            // stale write is refused
            assert!(save(c, u, &e, 3000).is_err());
            // real deletes are refused by the trigger
            assert!(c.execute("DELETE FROM entry", []).is_err());
            Ok(())
        }).unwrap();
    }

    #[test]
    fn wet_diapers() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-31 16:40", 0).unwrap();
        let baby = Baby { name: "B".into(), birth: Some(birth), birth_weight_g: None, sex: None, growth: None, delivery: None, change: None,
                          aim_feeds_per_day: None, feed_prep_s: 0 };
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let e = |t: &str, wet: bool| crate::status::entry(at(t), json!({"wet": wet}));
        // Day 3 (2 Nov): at least 3; two wet diapers → warning.
        let day = [e("2025-11-02 02:00", true), e("2025-11-02 08:00", true), e("2025-11-02 11:00", false)];
        let w = crate::status::wet_warning(&clock, &baby, &day, at("2025-11-02 12:00")).unwrap();
        assert_eq!(w.text, "Only 2 wet diapers in the last 24 h (usually at least 3 on day 3)");
        // Day 8: at least 6.
        let day: Vec<Entry> = (0..5).map(|k| e(&format!("2025-11-07 {:02}:00", 1 + k * 4), true)).collect();
        assert_eq!(crate::status::wet_warning(&clock, &baby, &day, at("2025-11-07 22:00")).unwrap().minimum, 6);
        let day: Vec<Entry> = (0..6).map(|k| e(&format!("2025-11-07 {:02}:00", 1 + k * 3), true)).collect();
        assert!(crate::status::wet_warning(&clock, &baby, &day, at("2025-11-07 22:00")).is_none(), "six is enough");
        // Nobody logging for 6 hours: no warning.
        assert!(crate::status::wet_warning(&clock, &baby, &day[..1], at("2025-11-07 22:00")).is_none());
        // First day: nothing yet.
        assert!(crate::status::wet_warning(&clock, &baby, &[e("2025-10-31 20:00", false)], at("2025-10-31 22:00")).is_none());
    }

    #[test]
    fn night_stretches() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let e = |t: &str| crate::status::entry(at(t), json!({"bottle_formula_ml": 60}));
        // 22:00 → 03:30 (top-up 04:00) → 07:00: the longest stretch overlapping
        // the night is 5h30, from the end of the 22:00 feed.
        let es = [e("2025-11-06 19:00"), e("2025-11-06 22:00"), e("2025-11-07 03:30"), e("2025-11-07 04:00"), e("2025-11-07 07:00"), e("2025-11-07 10:00")];
        let eps = feed_episodes(&es);
        assert_eq!(night_stretch(&clock, &eps, clock.date_start("2025-11-07").unwrap()), Some(5 * 3600 + 1800));
        // The night before: no feed before it in the log.
        assert_eq!(night_stretch(&clock, &eps, clock.date_start("2025-11-06").unwrap()), None);
        // Still sleeping (no feed after 22:00 yet): not known.
        assert_eq!(night_stretch(&clock, &eps[..2], clock.date_start("2025-11-07").unwrap()), None);
        let d = stats_of(&clock, &Baby { name: "B".into(), birth: None, birth_weight_g: None, sex: None, growth: None, delivery: None, change: None,
                                         aim_feeds_per_day: None, feed_prep_s: 0 }, &es, at("2025-11-07 00:00"), at("2025-11-07 12:00"));
        assert_eq!(d[0].longest_night_gap_min, Some(330));
    }

    #[test]
    fn totals_and_status() {
        let (_d, db, u) = setup();
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let now = clock.parse("2025-11-06 12:00", 0).unwrap();
        db.with(|c| {
            for (t, f) in [
                ("06:00", json!({"bottle_breastmilk_ml": 30, "bottle_formula_ml": 30, "wet": true})),
                ("09:00", json!({"bottle_formula_ml": 70, "dirty": true, "stool_color": "yellow", "latch": "with_aid", "breast_left_min": 5})),
                ("10:00", json!({"weight_g": 3660})),
                ("10:05", json!({"sleep_facing": "left"})),
            ] {
                insert(c, u, &NewEntry { at: clock.parse(t, now).unwrap(), duration_s: None, text: t.into(),
                    fields: fields(f), attention_by: 0, attention_why: None }, now).unwrap();
            }
            let all = list(c, 0, i64::MAX, false).unwrap();
            let t = totals(&all);
            assert_eq!((t.feeds, t.total_ml, t.breastmilk_ml, t.wet, t.dirty, t.breast_min), (2, 130.0, 30.0, 1, 1, 5));
            assert_eq!((t.breast_left_min, t.breast_right_min), (5, 0));
            assert_eq!((t.diapers, t.wet_only, t.dirty_only, t.wet_and_dirty), (2, 1, 1, 0));
            let both = Entry { fields: fields(json!({"wet": true, "dirty": true})), ..all[0].clone() };
            let t2 = totals([&all[0], &both]);
            assert_eq!((t2.diapers, t2.wet, t2.dirty, t2.wet_and_dirty), (2, 2, 1, 1), "a wet and dirty diaper counts once");
            let baby = Baby { name: "B".into(), birth: Some(clock.parse("2025-10-31 16:40", 0).unwrap()),
                              birth_weight_g: Some(3900.0), sex: Some(crate::growth::Sex::Female),
                              growth: crate::growth::Table::for_tests(), delivery: Some(crate::growth::Delivery::Cesarean),
                              change: crate::growth::WeightChange::for_tests(), aim_feeds_per_day: None, feed_prep_s: 0 };
            for (t, f) in [("2025-10-28 19:00", json!({"bath": "with_soap"})), ("2025-11-02 19:00", json!({"bath": "sponge"}))] {
                insert(c, u, &NewEntry { at: clock.parse(t, now).unwrap(), duration_s: None, text: t.into(),
                    fields: fields(f), attention_by: 0, attention_why: None }, now).unwrap();
            }
            let s = crate::status::status_json(c, &clock, &baby, now).unwrap();
            assert_eq!(s["last_feed"]["at"], "2025-11-06 09:00");
            assert_eq!(s["last_dirty_diaper"]["value"], "yellow");
            assert_eq!(s["weight_vs_birth"], "-240 g (-6.2%)");
            assert_eq!(s["birth_weight_regained"]["regained"], false);
            assert_eq!(s["sleep_facing"]["last"], "left");
            assert_eq!(s["sleep_facing"]["next"], "right");
            // Baths: with soap on 28 Oct 19:00 → 3.5 days on is 1 Nov, reminded at
            // 18:00 and overdue by 6 Nov; next without soap; the later sponge
            // wash doesn't count.
            assert_eq!((s["bath"]["due"]["on"].as_str(), s["bath"]["due"]["at"].as_str()), (Some("2025-11-01"), Some("2025-11-01 18:00")), "the day 3.5 days on, reminded that evening");
            assert!(s["bath"]["due"]["overdue_by"].is_string());
            assert_eq!(s["bath"]["next_kind"], "without_soap");
            assert_eq!(s["bath"]["last"]["value"], "with_soap");
            assert_eq!(s["bath"]["last_any"]["value"], "sponge");
            // Vitamins: born 31 Oct → from 7 Nov (day 8); on 6 Nov not active yet.
            assert_eq!(s["vitamins"]["from"], "2025-11-07");
            assert_eq!(s["vitamins"]["k"]["until"], "2026-01-22");
            assert_eq!(s["vitamins"]["d"]["until"], "2029-10-30");
            assert_eq!(s["vitamins"]["to_give_today"], json!([]));
            let later = clock.parse("2025-11-07 19:00", 0).unwrap();
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 09:00", 0).unwrap(), duration_s: None, text: "vit D".into(),
                fields: fields(json!({"vitamin_d": true})), attention_by: 0, attention_why: None }, later).unwrap();
            let s2 = crate::status::status_json(c, &clock, &baby, later).unwrap();
            assert_eq!(s2["vitamins"]["to_give_today"], json!(["K"]));
            assert_eq!(s2["vitamins"]["d"]["given_today"], true);
            assert_eq!(s2["vitamins"]["remind_at"], "2025-11-07 18:00");
            assert!(s2["vitamins"]["k"]["not_needed"].is_null());
            // Sleep side: left at 10:05 on 6 Nov, no feed since; one feed after it
            // → assumed right, next left.
            assert_eq!(s2["sleep_facing"]["feeds_since"], 0);
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 12:00", 0).unwrap(), duration_s: None, text: "60 ml".into(),
                fields: fields(json!({"bottle_formula_ml": 60})), attention_by: 0, attention_why: None }, later).unwrap();
            let s3 = crate::status::status_json(c, &clock, &baby, later).unwrap();
            assert_eq!((s3["sleep_facing"]["feeds_since"].as_i64(), s3["sleep_facing"]["assumed_current"].as_str(), s3["sleep_facing"]["next"].as_str()),
                       (Some(1), Some("right"), Some("left")));
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 15:00", 0).unwrap(), duration_s: None, text: "70 ml".into(),
                fields: fields(json!({"bottle_formula_ml": 70})), attention_by: 0, attention_why: None }, later).unwrap();
            assert_eq!(crate::status::status_json(c, &clock, &baby, later).unwrap()["sleep_facing"]["next"], "right", "two feeds: back to the logged side's opposite");
            // A top-up 20 minutes later is part of that feed: no new put-down.
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 15:20", 0).unwrap(), duration_s: None, text: "+20 ml".into(),
                fields: fields(json!({"bottle_formula_ml": 20})), attention_by: 0, attention_why: None }, later).unwrap();
            assert_eq!(crate::status::status_json(c, &clock, &baby, later).unwrap()["sleep_facing"]["next"], "right", "a top-up doesn't flip the side");
            // Right after a feed without a side: the side for now as well.
            let just = clock.parse("2025-11-07 16:00", 0).unwrap();
            let v = &crate::status::status_json(c, &clock, &baby, just).unwrap()["sleep_facing"];
            assert_eq!((v["put_down_now"].as_str(), v["next"].as_str()), (Some("left"), Some("right")), "{v}");
            assert!(crate::status::status_json(c, &clock, &baby, later).unwrap()["sleep_facing"]["put_down_now"].is_null(), "not hours later");
            if baby.growth.is_some() {
                assert_eq!(s["birth_weight_percentile"]["label"], "P92");
                assert!(s["weight_percentile"]["label"].as_str().unwrap().starts_with('P'));
            } else {
                assert!(s["weight_percentile"].is_null(), "no table, no percentile");
            }
            assert_eq!(s["next_feed_estimate"]["at"], "2025-11-06 12:00");
            assert_eq!(s["next_feed_estimate"]["due_in"], "0m");
            // Diapers are checked at feeds; they aren't predicted.
            assert!(s.get("next_wet_diaper_estimate").is_none() && s.get("next_dirty_diaper_estimate").is_none());
            let d = stats(c, &clock, &baby, clock.date_start("2025-11-05").unwrap(), now).unwrap();
            assert_eq!(d.len(), 2);
            assert_eq!(d[1].totals.total_ml, 130.0);
            // With 500 ml formula the day before, no vitamin K that day.
            for (t, ml) in [("2025-11-07 08:00", 250), ("2025-11-07 14:00", 250)] {
                insert(c, u, &NewEntry { at: clock.parse(t, 0).unwrap(), duration_s: None, text: "formula".into(),
                    fields: fields(json!({"bottle_formula_ml": ml})), attention_by: 0, attention_why: None }, later).unwrap();
            }
            let next_day = clock.parse("2025-11-08 10:00", 0).unwrap();
            let s3 = crate::status::status_json(c, &clock, &baby, next_day).unwrap();
            assert_eq!(s3["vitamins"]["to_give_today"], json!(["D"]), "{}", s3["vitamins"]);
            assert!(s3["vitamins"]["k"]["not_needed"].as_str().unwrap().contains(" ml formula yesterday: with at least 500 ml a day"));
            Ok(())
        }).unwrap();
    }
}
