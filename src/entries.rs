//! Entries: storage, versions, and the status and statistics derived from them.

use crate::registry::{self, Group};
use crate::time::{Clock, fmt_dur};
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

fn query(c: &Connection, sql_where: &str, p: impl rusqlite::Params) -> Result<Vec<Entry>, String> {
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
        let mut g: Vec<Group> = self
            .fields
            .keys()
            .filter_map(|k| registry::get(k).map(|f| f.group))
            .collect();
        g.sort_by_key(|g| *g as u8);
        g.dedup();
        g
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
    pub pump_ml: f64,
}

pub fn totals<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Totals {
    let mut t = Totals::default();
    for e in entries {
        if e.deleted {
            continue;
        }
        if e.is_feed() {
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
        t.pump_ml += e.num("pump_ml").unwrap_or(0.0);
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

    fn change_json(&self, grams: f64, t: i64) -> Value {
        match (self.change_percentile(grams, t), self.delivery) {
            (Some((c, p, label)), Some(d)) => json!({
                "percent_change": (c * 10.0).round() / 10.0, "percentile": p.round(), "label": label,
                "reference": format!("newborns after a {} birth (Paul et al., Pediatrics 2016; percentiles read from the published figure, approximate)",
                                     if d == crate::growth::Delivery::Cesarean { "cesarean" } else { "vaginal" }),
            }),
            _ => Value::Null,
        }
    }

    fn percentile_json(&self, grams: f64, t: i64) -> Value {
        match self.weight_percentile(grams, t) {
            Some((p, z)) => json!({"percentile": (p * 10.0).round() / 10.0, "label": crate::growth::label(p),
                                   "z": (z * 100.0).round() / 100.0}),
            None => Value::Null,
        }
    }
}

fn weight_vs_birth(w: f64, birth: Option<f64>) -> Option<String> {
    let b = birth.filter(|b| *b > 0.0)?;
    Some(format!("{:+.0} g ({:+.1}%)", w - b, 100.0 * (w - b) / b))
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

/// Estimates the next occurrence from event times (oldest first): the last
/// one plus the median of up to 8 recent gaps of at least `min_gap`.
fn estimate(clock: &Clock, times: &[i64], min_gap: i64, what: &str, now: i64) -> Value {
    let gaps = recent_gaps(times, min_gap);
    let (Some(m), Some(last)) = (median(gaps.clone()), times.last()) else {
        return Value::Null;
    };
    let at = last + m;
    let mut v = json!({
        "at": clock.fmt(at),
        "basis": format!("last {what} + median gap ({}) of the last {} {what}s", fmt_dur(m), gaps.len() + 1),
    });
    if at >= now {
        v["due_in"] = json!(fmt_dur(at - now));
    } else {
        v["overdue_by"] = json!(fmt_dur(now - at));
    }
    v
}

fn recent_gaps(times: &[i64], min_gap: i64) -> Vec<i64> {
    times.windows(2).map(|w| w[1] - w[0]).filter(|g| *g >= min_gap).rev().take(8).collect()
}

/// The typical gap between events (oldest first), as `estimate` uses it.
pub fn usual_gap(times: &[i64]) -> Option<i64> {
    median(recent_gaps(times, ESTIMATE_MIN_GAP))
}

/// Gaps shorter than this don't count for estimates (a feed in two parts).
const ESTIMATE_MIN_GAP: i64 = 30 * 60;

/// How far back the estimates look: feeds and wet diapers, dirty diapers.
pub const ESTIMATE_BACK: i64 = 36 * 3600;

/// Fewer wet diapers in the last 24 hours than usual for her age: at least as
/// many as the day of life up to day 5, then at least 6 (breastfeeding and
/// kraamzorg guidance). Only once she is a day old and while someone is
/// logging (an entry in the last 6 hours). `day` is the last 24 hours.
pub fn wet_warning(clock: &Clock, baby: &Baby, day: &[Entry], now: i64) -> Value {
    let Some(birth) = baby.birth else { return Value::Null };
    if now - birth < 86400 || !day.iter().any(|e| e.at > now - 6 * 3600) {
        return Value::Null;
    }
    let dol = crate::feeding::day_of_life(clock, birth, now);
    let min = dol.min(6) as usize;
    let wet = day.iter().filter(|e| e.is("wet")).count();
    if wet >= min {
        return Value::Null;
    }
    let rule = if dol >= 6 { "usually at least 6 from day 6".to_string() } else { format!("usually at least {min} on day {dol}") };
    json!({"wet_24h": wet, "minimum": min, "text": format!("Only {wet} wet diaper{} in the last 24 h ({rule})", if wet == 1 { "" } else { "s" })})
}

/// When to start the routine for the next feed (diaper first): the estimate
/// moved up by `prep` seconds, with due_in / overdue_by from that time.
fn feed_start(clock: &Clock, next: &Value, prep: i64, now: i64) -> Value {
    let Some(at) = next["at"].as_str().and_then(|a| clock.parse(a, now).ok()) else { return Value::Null };
    let start = at - prep;
    let mut v = json!({"at": clock.fmt(start), "prep": fmt_dur(prep), "feed_at": clock.fmt(at)});
    if start >= now {
        v["due_in"] = json!(fmt_dur(start - now));
    } else {
        v["overdue_by"] = json!(fmt_dur(now - start));
    }
    v
}

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
pub const VITAMIN_REMIND_HOUR: i64 = 18;

/// The start of the local day `span` after the local day of `t`.
fn day_plus(clock: &Clock, t: i64, span: jiff::Span) -> i64 {
    let d = clock.zoned(t).date().checked_add(span).expect("date in range");
    clock.date_start(&d.to_string()).expect("valid date")
}

/// Two baths a week, alternating with and without soap.
pub const BATH_EVERY: i64 = 84 * 3600;
pub const BATH_GOAL: &str = "two baths a week, alternating with and without soap: a bath is due 3.5 days after the last one (a sponge wash doesn't count), of the other kind than the last";

pub fn status(c: &Connection, clock: &Clock, baby: &Baby, now: i64) -> Result<Value, String> {
    let names = user_names(c)?;
    let last_feed = last_where(c, FEED_SQL)?;
    let last_wet = last_where(c, "wet = 1")?;
    let last_dirty = last_where(c, "dirty = 1")?;
    let last_temp = last_where(c, "temp_c IS NOT NULL")?;
    let last_weight = last_where(c, "weight_g IS NOT NULL")?;
    let recent = list(c, now - ESTIMATE_BACK, now + 1, false)?;
    let today = list(c, clock.day_start(now), now + 1, false)?;
    let day = list(c, now - 24 * 3600, now + 1, false)?;

    let times = |es: &[Entry], f: fn(&Entry) -> bool| es.iter().filter(|e| f(e)).map(|e| e.at).collect::<Vec<_>>();
    let next_feed = estimate(clock, &times(&recent, Entry::is_feed), ESTIMATE_MIN_GAP, "feed", now);

    let mut out = json!({
        "now": clock.fmt(now),
        "baby": baby.name,
        "last_feed": since(&last_feed, clock, now, last_feed.as_ref().map(|e| view(e, clock, &names))),
        "next_feed_estimate": next_feed,
        "next_feed_start": feed_start(clock, &next_feed, baby.feed_prep_s, now),
        "last_wet_diaper": since(&last_wet, clock, now, None),
        "wet_diaper_warning": wet_warning(clock, baby, &day, now),
        "last_dirty_diaper": since(&last_dirty, clock, now, last_dirty.as_ref().and_then(|e| e.fields.get("stool_color").cloned())),
        "last_temperature": since(&last_temp, clock, now, last_temp.as_ref().and_then(|e| e.fields.get("temp_c").cloned())),
        "last_weight": since(&last_weight, clock, now, last_weight.as_ref().and_then(|e| e.fields.get("weight_g").cloned())),
        "today": totals(&today),
        "last_24h": totals(&day),
        "needs_attention": attention(c, BY_USER | BY_AGENT)?.iter().map(|e| view(e, clock, &names)).collect::<Vec<_>>(),
    });
    if let Some(b) = baby.birth {
        out["age"] = json!(fmt_dur(now - b));
    }
    if let Some(bw) = baby.birth_weight_g {
        out["birth_weight_g"] = json!(bw);
    }
    if let Some(w) = last_weight.as_ref().and_then(|e| e.num("weight_g")) {
        out["weight_vs_birth"] = json!(weight_vs_birth(w, baby.birth_weight_g));
        let wt = last_weight.as_ref().map(|e| e.at).unwrap_or(now);
        out["weight_percentile"] = baby.percentile_json(w, wt);
        out["weight_change_percentile"] = baby.change_json(w, wt);
    }
    if let (Some(bw), Some(b)) = (baby.birth_weight_g, baby.birth) {
        out["birth_weight_percentile"] = baby.percentile_json(bw, b);
        // Back at birth weight: the first weighing at or above it after birth.
        let back = query(c, "deleted = 0 AND weight_g IS NOT NULL AND at > ?1 ORDER BY at", [b])?
            .into_iter()
            .find(|e| e.num("weight_g").is_some_and(|w| w >= bw));
        out["birth_weight_regained"] = match back {
            Some(e) => json!({"regained": true, "on": clock.day(e.at), "age_days": crate::growth::age_days(b, e.at)}),
            None => json!({"regained": false}),
        };
    }
    out["feeding_guidance"] = crate::feeding::for_status(c, clock, baby, last_weight.as_ref().and_then(|e| e.num("weight_g")), now)?;
    // Baths: due 3.5 days after the last real bath (a sponge wash doesn't
    // count), alternating with and without soap; never logged = due now.
    let last = last_where(c, "bath IN ('with_soap', 'without_soap')")?;
    let due_at = last.as_ref().map(|e| e.at + BATH_EVERY).unwrap_or(now);
    let mut due = json!({"at": clock.fmt(due_at)});
    if last.is_none() {
        due["never"] = json!(true);
    } else if due_at > now {
        due["due_in"] = json!(fmt_dur(due_at - now));
    } else {
        due["overdue_by"] = json!(fmt_dur(now - due_at));
    }
    let next_kind = match last.as_ref().and_then(|e| e.str("bath")) {
        Some("with_soap") => json!("without_soap"),
        Some(_) => json!("with_soap"),
        None => Value::Null,
    };
    let last_any = last_where(c, "bath IS NOT NULL")?;
    out["bath"] = json!({
        "goal": BATH_GOAL,
        "last": since(&last, clock, now, last.as_ref().and_then(|e| e.fields.get("bath").cloned())),
        "last_any": since(&last_any, clock, now, last_any.as_ref().and_then(|e| e.fields.get("bath").cloned())),
        "due": due,
        "next_kind": next_kind,
    });

    // Vitamins: which are due today and whether they were given.
    if let Some(b) = baby.birth {
        let today = clock.day_start(now);
        let from = day_plus(clock, b, jiff::Span::new().days(7));
        let mut v = json!({"from": clock.day(from), "remind_at": clock.fmt(today + VITAMIN_REMIND_HOUR * 3600),
                           "rule": "vitamin K daily from day 8 until 12 weeks, vitamin D daily from day 8 until 4 years (RIVM)"});
        let mut to_give = vec![];
        for (key, field, label) in VITAMINS {
            let until = day_plus(clock, b, if key == "k" { jiff::Span::new().weeks(12) } else { jiff::Span::new().years(4) });
            let active = today >= from && today < until;
            let last = last_where(c, &format!("{field} = 1"))?;
            let given_today = last.as_ref().is_some_and(|e| e.at >= today);
            if active && !given_today {
                to_give.push(label);
            }
            v[key] = json!({"active": active, "until": clock.day(until - 1), "given_today": given_today,
                            "last": since(&last, clock, now, None)});
        }
        v["to_give_today"] = json!(to_give);
        out["vitamins"] = v;
    }

    // Sleeping position: alternate the side the face is turned to after every
    // feed. Feeds after the last logged side are assumed to have been followed
    // by a put-down on the other side each time.
    if let Some(e) = last_where(c, "sleep_facing IS NOT NULL")? {
        if let Some(side) = e.str("sleep_facing") {
            let other = |s: &str| if s == "left" { "right" } else { "left" };
            let feeds_since = list(c, e.at + 1, now + 1, false)?.iter().filter(|x| x.is_feed()).count();
            let current = if feeds_since % 2 == 0 { side } else { other(side) };
            let mut v = json!({"last": side, "last_at": clock.fmt(e.at), "feeds_since": feeds_since, "next": other(current),
                               "note": "alternate the side the face is turned to after every feed; feeds since the last logged side are assumed to have alternated"});
            if feeds_since > 0 {
                v["assumed_current"] = json!(current);
            }
            out["sleep_facing"] = v;
        }
    }
    out["growth_note"] = json!("weight_percentile compares with the WHO Child Growth Standards (weight-for-age), which ignore the normal weight loss of the first days; weight_change_percentile compares the change since birth with newborns of the same delivery mode during the first month. References, not a diagnosis.");
    Ok(out)
}

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
}

/// Per-day statistics for the local days from `from` (a day start) up to `to`.
pub fn stats(c: &Connection, clock: &Clock, baby: &Baby, from: i64, to: i64) -> Result<Vec<DayStats>, String> {
    let all = list(c, from, to, false)?;
    let mut out = vec![];
    let mut start = clock.day_start(from);
    while start < to {
        let end = clock.next_day_start(start);
        let day: Vec<&Entry> = all.iter().filter(|e| e.at >= start && e.at < end).collect();
        let temps: Vec<f64> = day.iter().filter_map(|e| e.num("temp_c")).collect();
        let weight = day.iter().filter_map(|e| e.num("weight_g")).last();
        out.push(DayStats {
            date: clock.day(start),
            totals: totals(day.iter().copied()),
            temp_min_c: temps.iter().copied().reduce(f64::min),
            temp_max_c: temps.iter().copied().reduce(f64::max),
            weight_g: weight,
            weight_vs_birth: weight.and_then(|w| weight_vs_birth(w, baby.birth_weight_g)),
            weight_percentile: day
                .iter()
                .filter(|e| e.num("weight_g").is_some())
                .last()
                .and_then(|e| baby.weight_percentile(e.num("weight_g")?, e.at))
                .map(|(p, _)| crate::growth::label(p)),
        });
        start = end;
    }
    Ok(out)
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
        let e = |t: &str, wet: bool| Entry { entry_id: 0, version: 1, at: at(t), duration_s: None, text: String::new(),
            fields: fields(json!({"wet": wet})), attention_by: 0, attention_why: None, deleted: false, changed_at: 0, changed_by: 1, superseded_at: None };
        // Day 3 (2 Nov): at least 3; two wet diapers → warning.
        let day = [e("2025-11-02 02:00", true), e("2025-11-02 08:00", true), e("2025-11-02 11:00", false)];
        let w = wet_warning(&clock, &baby, &day, at("2025-11-02 12:00"));
        assert_eq!(w["text"], "Only 2 wet diapers in the last 24 h (usually at least 3 on day 3)");
        // Day 8: at least 6.
        let day: Vec<Entry> = (0..5).map(|k| e(&format!("2025-11-07 {:02}:00", 1 + k * 4), true)).collect();
        assert_eq!(wet_warning(&clock, &baby, &day, at("2025-11-07 22:00"))["minimum"], 6);
        let day: Vec<Entry> = (0..6).map(|k| e(&format!("2025-11-07 {:02}:00", 1 + k * 3), true)).collect();
        assert!(wet_warning(&clock, &baby, &day, at("2025-11-07 22:00")).is_null(), "six is enough");
        // Nobody logging for 6 hours: no warning.
        assert!(wet_warning(&clock, &baby, &day[..1], at("2025-11-07 22:00")).is_null());
        // First day: nothing yet.
        assert!(wet_warning(&clock, &baby, &[e("2025-10-31 20:00", false)], at("2025-10-31 22:00")).is_null());
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
            let s = status(c, &clock, &baby, now).unwrap();
            assert_eq!(s["last_feed"]["at"], "2025-11-06 09:00");
            assert_eq!(s["last_dirty_diaper"]["value"], "yellow");
            assert_eq!(s["weight_vs_birth"], "-240 g (-6.2%)");
            assert_eq!(s["birth_weight_regained"]["regained"], false);
            assert_eq!(s["sleep_facing"]["last"], "left");
            assert_eq!(s["sleep_facing"]["next"], "right");
            // Baths: with soap on 27 Oct 19:00 → due 31 Oct 07:00, so overdue, next
            // without soap; the later sponge wash doesn't count.
            assert_eq!(s["bath"]["due"]["at"], "2025-11-01 07:00");
            assert!(s["bath"]["due"]["overdue_by"].is_string());
            assert_eq!(s["bath"]["next_kind"], "without_soap");
            assert_eq!(s["bath"]["last"]["value"], "with_soap");
            assert_eq!(s["bath"]["last_any"]["value"], "sponge");
            // Vitamins: born 30 Oct → from 6 Nov; on 5 Nov not active yet.
            assert_eq!(s["vitamins"]["from"], "2025-11-07");
            assert_eq!(s["vitamins"]["k"]["until"], "2026-01-22");
            assert_eq!(s["vitamins"]["d"]["until"], "2029-10-30");
            assert_eq!(s["vitamins"]["to_give_today"], json!([]));
            let later = clock.parse("2025-11-07 19:00", 0).unwrap();
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 09:00", 0).unwrap(), duration_s: None, text: "vit D".into(),
                fields: fields(json!({"vitamin_d": true})), attention_by: 0, attention_why: None }, later).unwrap();
            let s2 = status(c, &clock, &baby, later).unwrap();
            assert_eq!(s2["vitamins"]["to_give_today"], json!(["K"]));
            assert_eq!(s2["vitamins"]["d"]["given_today"], true);
            assert_eq!(s2["vitamins"]["remind_at"], "2025-11-07 18:00");
            // Sleep side: left at 10:05 on 5 Nov; one feed after it → assumed right, next left.
            assert_eq!(s2["sleep_facing"]["feeds_since"], 0);
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 12:00", 0).unwrap(), duration_s: None, text: "60 ml".into(),
                fields: fields(json!({"bottle_formula_ml": 60})), attention_by: 0, attention_why: None }, later).unwrap();
            let s3 = status(c, &clock, &baby, later).unwrap();
            assert_eq!((s3["sleep_facing"]["feeds_since"].as_i64(), s3["sleep_facing"]["assumed_current"].as_str(), s3["sleep_facing"]["next"].as_str()),
                       (Some(1), Some("right"), Some("left")));
            insert(c, u, &NewEntry { at: clock.parse("2025-11-07 15:00", 0).unwrap(), duration_s: None, text: "70 ml".into(),
                fields: fields(json!({"bottle_formula_ml": 70})), attention_by: 0, attention_why: None }, later).unwrap();
            assert_eq!(status(c, &clock, &baby, later).unwrap()["sleep_facing"]["next"], "right", "two feeds: back to the logged side's opposite");
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
            Ok(())
        }).unwrap();
    }
}
