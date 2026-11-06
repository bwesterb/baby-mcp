//! One-off import of the kraamweek from a Healthsoft kraamzorg parent portal:
//! reads the JSON of `GET /client/dossiers/{id}/medical` from a file.

use crate::entries::{self as en, BY_AGENT, BY_USER, NewEntry};
use crate::time::Clock;
use rusqlite::Connection;
use serde_json::{Map, Value, json};

const BREAST_WORDS: &[&str] = &["gekolf", "afgekolf", "kolf", "moedermelk", "colostrum", "colestrum", "breast", "pumped", "expressed"];
const FORMULA_WORDS: &[&str] = &["formule", "kunstvoeding", "flesvoeding", "formula", " kv"];

fn kind_of(s: &str) -> Option<bool> {
    // Some(true) = breast milk, Some(false) = formula
    let s = s.to_lowercase();
    let b = BREAST_WORDS.iter().filter_map(|w| s.find(w)).min();
    let f = FORMULA_WORDS.iter().filter_map(|w| s.find(w)).min();
    match (b, f) {
        (Some(b), Some(f)) => Some(b < f),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        _ => None,
    }
}

/// How a MIXED feed's total splits into breast milk and formula.
#[derive(Debug, PartialEq)]
pub enum Split {
    /// (breast milk, formula) ml
    Known(f64, f64),
    /// The notes say nothing about amounts: the split was never recorded.
    NotRecorded,
    /// The notes mention amounts that don't add up: needs a closer look.
    Unclear,
}

/// Splits a MIXED feed's total using the notes, e.g. "30 ml formule 30 ml
/// gekolft", "20ml gekolft rest formule", or "30 ml gekolft" (the rest is
/// then formula, since the feed was mixed).
pub fn split_mixed(total: f64, notes: &str) -> Split {
    let lower = notes.to_lowercase();
    let bytes = lower.as_bytes();
    let (mut breast, mut formula) = (None::<f64>, None::<f64>);
    let mut amounts = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b',') {
                i += 1;
            }
            let num: f64 = lower[start..i].replace(',', ".").parse().unwrap_or(f64::NAN);
            let rest = lower[i..].trim_start();
            if let Some(after) = rest.strip_prefix("ml").or_else(|| rest.strip_prefix("cc")) {
                amounts += 1;
                let until = after.find(|c: char| c.is_ascii_digit()).unwrap_or(after.len());
                match kind_of(&after[..until]) {
                    Some(true) => breast = Some(breast.unwrap_or(0.0) + num),
                    Some(false) => formula = Some(formula.unwrap_or(0.0) + num),
                    None => {}
                }
            }
        } else {
            i += 1;
        }
    }
    let near = |a: f64, b: f64| (a - b).abs() < 0.5;
    match (breast, formula) {
        (Some(b), Some(f)) if near(b + f, total) => Split::Known(b, f),
        (Some(b), None) if b <= total + 0.5 => Split::Known(b, (total - b).max(0.0)),
        (None, Some(f)) if f <= total + 0.5 => Split::Known((total - f).max(0.0), f),
        (None, None) if amounts == 0 => Split::NotRecorded,
        _ => Split::Unclear,
    }
}

/// A bath mentioned in the notes.
#[derive(Debug, PartialEq)]
pub enum Bath {
    Kind(&'static str),
    /// A bath, but not whether with soap.
    Unknown,
}

/// Fields that only appear in the free-text notes: the side her face was
/// turned to when put down to sleep, and a bath (Dutch or English).
pub fn from_notes(notes: &str) -> (Option<&'static str>, Option<Bath>, Vec<&'static str>) {
    let lower = notes.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |ws: &[&str]| words.iter().any(|w| ws.contains(w));
    let phrase = |ps: &[&str]| ps.iter().any(|p| lower.contains(p));
    let face = has(&["face", "facing", "gezicht", "gezichtje", "hoofd", "hoofdje"]);
    let side = match (face, has(&["left", "links"]), has(&["right", "rechts"])) {
        (true, true, false) => Some("left"),
        (true, false, true) => Some("right"),
        _ => None,
    };
    let sponge = has(&["washand", "washandje", "spons", "sponge", "sponged", "washcloth"]);
    let bathed = has(&["bad", "badje", "badjes", "bath", "bathed", "badderen", "gebadderd", "gebaad"]);
    let bath = if bathed {
        if phrase(&["zonder zeep", "geen zeep", "without soap", "no soap", "alleen water", "only water", "just water"]) {
            Some(Bath::Kind("without_soap"))
        } else if has(&["zeep", "zeepje", "soap"]) {
            Some(Bath::Kind("with_soap"))
        } else {
            Some(Bath::Unknown)
        }
    } else if sponge {
        Some(Bath::Kind("sponge"))
    } else {
        None
    };
    // "vitamine D", "vit. K", "vitamine D en K"; plural without letters
    // ("vitamins given", "vitaminen gegeven") means the usual drops: D and K.
    let mut vitamins = vec![];
    if has(&["vitamine", "vitamines", "vitaminen", "vitamin", "vitamins", "vit"]) {
        let (d, k) = (has(&["d", "dk"]), has(&["k", "dk"]));
        let plural = has(&["vitamines", "vitaminen", "vitamins"]);
        if d || (plural && !k) {
            vitamins.push("vitamin_d");
        }
        if k || (plural && !d) {
            vitamins.push("vitamin_k");
        }
    }
    (side, bath, vitamins)
}

fn s<'a>(m: &'a Value, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Maps one portal measure to an entry.
pub fn map_measure(clock: &Clock, date: &str, m: &Value) -> Result<NewEntry, String> {
    let time = s(m, "time").ok_or("measure without time")?;
    let at = clock.parse(&format!("{date} {}", &time[..time.len().min(5)]), 0)?;
    let mut f = Map::new();
    let mut by = 0;
    let mut why = None;
    let notes = s(m, "notes").unwrap_or("");
    let mut extra = None;
    if let Some(ml) = m.get("feeding").and_then(Value::as_f64).filter(|x| *x > 0.0) {
        match s(m, "feedingType") {
            Some("BREAST") => {
                f.insert("bottle_breastmilk_ml".into(), json!(ml));
            }
            Some("FORMULA") => {
                f.insert("bottle_formula_ml".into(), json!(ml));
            }
            _ => match split_mixed(ml, notes) {
                Split::Known(b, fo) => {
                    if b > 0.0 {
                        f.insert("bottle_breastmilk_ml".into(), json!(b));
                    }
                    if fo > 0.0 {
                        f.insert("bottle_formula_ml".into(), json!(fo));
                    }
                }
                Split::NotRecorded => extra = Some(format!("bottle {ml} ml of breast milk and formula; split not recorded")),
                Split::Unclear => {
                    by = BY_AGENT;
                    why = Some(format!("split {ml} ml into breast milk / formula"));
                }
            },
        }
    }
    if let Some(l) = s(m, "breastType") {
        let v = match l {
            "YES_WITH_AID" => "with_aid",
            "YES_WITH_DIFFICULTY" => "with_difficulty",
            "YES_WITHOUT_DIFFICULTY" => "without_difficulty",
            _ => "none",
        };
        f.insert("latch".into(), json!(v));
    }
    for (src, dst) in [("breastFeedingLeft", "breast_left_min"), ("breastFeedingRight", "breast_right_min")] {
        if let Some(x) = m.get(src).and_then(Value::as_i64) {
            f.insert(dst.into(), json!(x));
        }
    }
    for (src, dst) in [("urine", "wet"), ("feces", "dirty"), ("vomited", "vomit")] {
        if let Some(b) = m.get(src).and_then(Value::as_bool) {
            f.insert(dst.into(), json!(b));
        }
    }
    if let Some(c) = s(m, "fecesColor") {
        f.insert("stool_color".into(), json!(c.to_lowercase()));
    }
    if let Some(t) = m.get("temperature").and_then(Value::as_f64) {
        f.insert("temp_c".into(), json!(t));
    }
    if let Some(w) = m.get("weight").and_then(Value::as_f64) {
        f.insert("weight_g".into(), json!(w));
    }
    let (side, bath, vitamins) = from_notes(notes);
    for v in vitamins {
        f.insert(v.into(), json!(true));
    }
    if let Some(side) = side {
        f.insert("sleep_facing".into(), json!(side));
    }
    match bath {
        Some(Bath::Kind(k)) => {
            f.insert("bath".into(), json!(k));
        }
        Some(Bath::Unknown) => {
            by |= BY_USER;
            let q = "bath: with or without soap?".to_string();
            why = Some(match why {
                Some(w) => format!("{w}; {q}"),
                None => q,
            });
        }
        None => {}
    }
    let f = crate::registry::validate_fields(&f)?;
    let who = if s(m, "portal") == Some("CLIENT") { "parents" } else { "caregiver" };
    let mut text = format!("Imported from the kraamzorg portal (logged by {who})");
    if !notes.is_empty() {
        text = format!("{text}: {notes}");
    }
    let mut extras: Vec<String> = extra.into_iter().collect();
    if let Some(t) = s(m, "temperatureType").filter(|_| m.get("temperature").is_some_and(|t| !t.is_null())) {
        extras.push(format!("{} temperature", t.to_lowercase()));
    }
    if let Some(n) = m.get("hotWaterBottles").and_then(Value::as_i64).filter(|n| *n > 0) {
        extras.push(format!("{n} hot-water bottle{}", if n == 1 { "" } else { "s" }));
    }
    if let Some(b) = m["breathingCheck"].as_array().filter(|a| !a.is_empty()) {
        let v: Vec<String> = b.iter().filter_map(Value::as_str).map(|x| x.to_lowercase().replace('_', " ")).collect();
        extras.push(format!("breathing check: {}", v.join(", ")));
    }
    if !extras.is_empty() {
        text = format!("{text} ({})", extras.join("; "));
    }
    Ok(NewEntry { at, duration_s: None, text, fields: f, attention_by: by, attention_why: why })
}

fn lower(v: &str) -> String {
    v.to_lowercase().replace('_', " ")
}

fn yes_no(v: &Value) -> Option<&'static str> {
    v.as_bool().map(|b| if b { "yes" } else { "no" })
}

/// The nurses' day report as one text entry at 12:00 (logged by caregiver):
/// notes, navel/fontanel, feeding flags and the topics discussed.
pub fn map_day_report(clock: &Clock, r: &Value) -> Result<Option<NewEntry>, String> {
    let date = s(r, "date").ok_or("day report without date")?;
    let mut parts = vec![];
    for (k, label) in [("careNotes", "Care"), ("detailNotes", "Details"), ("sleepNotes", "Sleep"), ("cryNotes", "Crying")] {
        if let Some(t) = s(r, k).map(str::trim).filter(|t| !t.is_empty()) {
            parts.push(format!("{label}: {t}"));
        }
    }
    let mut flags = vec![];
    if let Some(v) = s(r, "umbilicalState") {
        flags.push(format!("navel: {}", lower(v)));
    }
    if let Some(v) = s(r, "fontanelState") {
        flags.push(format!("fontanel: {}", lower(v)));
    }
    for (k, label) in [("roomingIn", "rooming-in"), ("reportsFeeding", "parents report feeds"), ("fedOnDemand", "fed on demand"), ("goodSuckingBehavior", "good sucking")] {
        if let Some(v) = yes_no(&r[k]) {
            flags.push(format!("{label}: {v}"));
        }
    }
    if !flags.is_empty() {
        parts.push(flags.join("; "));
    }
    let topics: Vec<&str> = ["controlCheckpoints", "evaluationCheckpoints"]
        .iter()
        .flat_map(|k| r[*k].as_array().into_iter().flatten())
        .filter(|c| c["checked"] == true || !(c["value"].is_null() || c["value"] == false || c["value"] == ""))
        .filter_map(|c| s(c, "name"))
        .collect();
    if !topics.is_empty() {
        parts.push(format!("Checked/discussed: {}", topics.join(", ")));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let day = r["day"].as_i64().map(|d| format!(" (day {d})")).unwrap_or_default();
    Ok(Some(NewEntry {
        at: clock.parse(&format!("{date} 12:00"), 0)?,
        duration_s: None,
        text: format!("Kraamzorg day report{day}, by caregiver:\n{}", parts.join("\n")),
        fields: Map::new(),
        attention_by: 0,
        attention_why: None,
    }))
}

/// Heel prick and hearing screening (repeated on every day report; once each).
fn screenings(clock: &Clock, reports: &[Value]) -> Result<Vec<NewEntry>, String> {
    let mut out = vec![];
    for (date_k, notes_k, label) in [("heelPrickDate", "heelPrickNotes", "Heel prick (hielprik)"), ("hearingScreeningDate", "hearingScreeningNotes", "Hearing screening")] {
        if let Some(r) = reports.iter().find(|r| s(r, date_k).is_some()) {
            let date = s(r, date_k).unwrap();
            let notes = s(r, notes_k).map(|n| format!(": {n}")).unwrap_or_default();
            out.push(NewEntry {
                at: clock.parse(&format!("{date} 12:00"), 0)?,
                duration_s: None,
                text: format!("{label} done{notes} (from the kraamzorg dossier)"),
                fields: Map::new(),
                attention_by: 0,
                attention_why: None,
            });
        }
    }
    Ok(out)
}

/// The birth details from the dossier as one text entry at the birth time.
fn birth_entry(clock: &Clock, b: &Value) -> Result<Option<NewEntry>, String> {
    let (Some(date), Some(time)) = (s(b, "birthDate"), s(b, "birthTime")) else { return Ok(None) };
    let mut parts = vec![];
    if let Some(w) = b["weight"].as_f64() {
        parts.push(format!("{w:.0} g"));
    }
    if let Some(l) = b["length"].as_f64() {
        parts.push(format!("{l} cm long"));
    }
    if let Some(h) = b["headCircumference"].as_f64() {
        parts.push(format!("head circumference {h} cm"));
    }
    if let Some(t) = s(b, "birthType") {
        parts.push(match t {
            "SECTION" => "caesarean section".to_string(),
            other => lower(other),
        });
    }
    if let Some(p) = s(b, "fetalPosition") {
        parts.push(format!("position: {}", lower(p)));
    }
    let apgar: Vec<String> = ["apgarScore1Min", "apgarScore5Min", "apgarScore10Min"].iter().map(|k| b[*k].as_i64().map(|v| v.to_string()).unwrap_or("–".into())).collect();
    if apgar.iter().any(|a| a != "–") {
        parts.push(format!("Apgar {} (1/5/10 min, as recorded)", apgar.join("/")));
    }
    if b["vitaminKAdministered"] == true {
        parts.push("vitamin K given at birth".into());
    }
    if let (Some(a), Some(z)) = (s(b, "startNutrition"), s(b, "endNutrition")) {
        parts.push(format!("feeding: {} at the start, {} at the end of the kraamweek", lower(a), lower(z)));
    }
    Ok(Some(NewEntry {
        at: clock.parse(&format!("{date} {}", &time[..time.len().min(5)]), 0)?,
        duration_s: None,
        text: format!("Born (from the kraamzorg dossier): {}", parts.join("; ")),
        fields: Map::new(),
        attention_by: 0,
        attention_why: None,
    }))
}

pub fn import(c: &Connection, clock: &Clock, user: i64, medical: &Value, force: bool) -> Result<usize, String> {
    let existing: i64 = c.query_row("SELECT count(*) FROM entry", [], |r| r.get(0)).map_err(en::err)?;
    if existing > 0 && !force {
        return Err(format!("the log already has {existing} entries; use --force to import anyway"));
    }
    let births = medical["births"].as_array().ok_or("no births in this file; expected /client/dossiers/{id}/medical")?;
    let mut n = 0;
    let now = crate::time::now();
    for b in births {
        let reports: Vec<Value> = b["dayReports"].as_array().cloned().unwrap_or_default();
        let mut entries = vec![];
        entries.extend(birth_entry(clock, b)?);
        for r in &reports {
            let date = s(r, "date").ok_or("day report without date")?;
            for m in r["measures"].as_array().into_iter().flatten() {
                entries.push(map_measure(clock, date, m)?);
            }
            entries.extend(map_day_report(clock, r)?);
        }
        entries.extend(screenings(clock, &reports)?);
        for e in entries {
            en::insert(c, user, &e, now)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_notes() {
        use Split::*;
        // Real notes from a kraamweek.
        assert_eq!(split_mixed(60.0, "30 ml formule 30 ml gekolft"), Known(30.0, 30.0));
        assert_eq!(split_mixed(50.0, "20ml gekolft rest formule"), Known(20.0, 30.0));
        assert_eq!(split_mixed(80.0, "60ml formule en 20ml gekolft"), Known(20.0, 60.0));
        assert_eq!(split_mixed(16.0, "1ml colostrum en rest formule"), Known(1.0, 15.0));
        assert_eq!(split_mixed(45.0, "1ml colestrum rest formule"), Known(1.0, 44.0));
        assert_eq!(split_mixed(31.0, "30ml formule en 1ml colestrum"), Known(1.0, 30.0));
        assert_eq!(split_mixed(10.0, "Even aangelegd. Valt in slaap. 10 ml kunstvoeding"), Known(0.0, 10.0));
        assert_eq!(split_mixed(16.0, "Klein beetje rood in luier. 6ml colestrum"), Known(6.0, 10.0));
        assert_eq!(split_mixed(55.0, "30ml gekolfde voeding"), Known(30.0, 25.0));
        assert_eq!(split_mixed(85.0, "30 ml gekolft"), Known(30.0, 55.0));
        assert_eq!(split_mixed(53.0, ""), NotRecorded);
        assert_eq!(split_mixed(26.0, "Hard huilend wakker. Leek vroeg honger te hebben."), NotRecorded);
        assert_eq!(split_mixed(50.0, "50 ml flesvoeding op\n10 ml gekolfd"), Unclear);
    }

    #[test]
    fn measure_mapping() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let m = json!({"time": "18:00:00", "feedingType": "MIXED", "breastType": "YES_WITH_AID", "feeding": 60,
                       "urine": true, "feces": true, "fecesColor": "YELLOW", "temperature": 36.8,
                       "notes": "30 ml formule 30 ml gekolft", "portal": "CLIENT"});
        let e = map_measure(&clock, "2025-11-04", &m).unwrap();
        assert_eq!(clock.fmt(e.at), "2025-11-04 18:00");
        assert_eq!(e.fields["bottle_breastmilk_ml"], json!(30.0));
        assert_eq!(e.fields["bottle_formula_ml"], json!(30.0));
        assert_eq!(e.fields["latch"], json!("with_aid"));
        assert_eq!(e.fields["stool_color"], json!("yellow"));
        assert_eq!(e.attention_by, 0);
        assert!(e.text.contains("parents"));
        let m2 = json!({"time": "21:00:00", "feedingType": "MIXED", "feeding": 53, "urine": true});
        let e2 = map_measure(&clock, "2025-11-03", &m2).unwrap();
        assert_eq!(e2.attention_by, 0);
        assert!(e2.text.contains("53 ml") && e2.text.contains("split not recorded"));
        assert!(!e2.fields.contains_key("bottle_formula_ml"));
        let m3 = json!({"time": "21:00:00", "feedingType": "MIXED", "feeding": 50, "notes": "50 ml flesvoeding op\n10 ml gekolfd"});
        assert_eq!(map_measure(&clock, "2025-11-03", &m3).unwrap().attention_by, BY_AGENT);
        // Notes-only fields.
        let m4 = json!({"time": "08:34:00", "notes": "Put down to sleep with face turned to the left.", "portal": "CLIENT"});
        assert_eq!(map_measure(&clock, "2025-11-07", &m4).unwrap().fields["sleep_facing"], json!("left"));
        let m5 = json!({"time": "13:45:00", "notes": "Badje gehad.", "urine": true});
        let e5 = map_measure(&clock, "2025-11-06", &m5).unwrap();
        assert_eq!((e5.attention_by, e5.attention_why.as_deref()), (BY_USER, Some("bath: with or without soap?")));
        assert!(!e5.fields.contains_key("bath"));
    }

    #[test]
    fn dossier_parts() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let m = json!({"time": "00:46:00", "temperature": 36.8, "temperatureType": "RECTAL", "hotWaterBottles": 2, "breathingCheck": ["SILENT"], "notes": "x"});
        let e = map_measure(&clock, "2025-11-03", &m).unwrap();
        assert!(e.text.ends_with("x (rectal temperature; 2 hot-water bottles; breathing check: silent)"), "{}", e.text);
        let r = json!({"date": "2025-11-05", "day": 6, "careNotes": " Vanaf vandaag 60cc. ", "detailNotes": "", "umbilicalState": "DRIED_UP",
                       "roomingIn": true, "fedOnDemand": false, "controlCheckpoints": [{"name": "Spruw bespreken", "checked": true}, {"name": "x", "checked": false}],
                       "heelPrickDate": "2025-11-05", "heelPrickNotes": "GL 1", "hearingScreeningDate": "2025-11-05", "hearingScreeningNotes": "beide oortjes goed"});
        let d = map_day_report(&clock, &r).unwrap().unwrap();
        assert_eq!(clock.fmt(d.at), "2025-11-05 12:00");
        assert_eq!(d.text, "Kraamzorg day report (day 6), by caregiver:\nCare: Vanaf vandaag 60cc.\nnavel: dried up; rooming-in: yes; fed on demand: no\nChecked/discussed: Spruw bespreken");
        assert!(map_day_report(&clock, &json!({"date": "2025-10-31"})).unwrap().is_none());
        let sc = screenings(&clock, &[r.clone(), r]).unwrap();
        assert_eq!(sc.len(), 2, "once each, not per day report");
        assert!(sc[1].text.contains("Hearing screening done: beide oortjes goed"));
        let b = json!({"birthDate": "2025-10-31", "birthTime": "16:40:00", "weight": 3900, "length": 50, "headCircumference": 38.0,
                       "birthType": "SECTION", "apgarScore1Min": 9, "apgarScore5Min": 10, "vitaminKAdministered": true});
        let be = birth_entry(&clock, &b).unwrap().unwrap();
        assert_eq!(clock.fmt(be.at), "2025-10-31 16:40");
        assert!(be.text.contains("3900 g; 50 cm long; head circumference 38 cm; caesarean section; Apgar 9/10/– (1/5/10 min, as recorded); vitamin K given at birth"), "{}", be.text);
    }

    #[test]
    fn notes_fields() {
        assert_eq!(from_notes("Gezichtje naar rechts gelegd"), (Some("right"), None, vec![]));
        assert_eq!(from_notes("Badje zonder zeep").1, Some(Bath::Kind("without_soap")));
        assert_eq!(from_notes("In bad, met zeepje").1, Some(Bath::Kind("with_soap")));
        assert_eq!(from_notes("Bathed, just water").1, Some(Bath::Kind("without_soap")));
        assert_eq!(from_notes("Gewassen met washandje").1, Some(Bath::Kind("sponge")));
        assert_eq!(from_notes("Badje gehad.").1, Some(Bath::Unknown));
        assert_eq!(from_notes("Badkamer opgeruimd, links aangelegd"), (None, None, vec![]), "no bath, no face");
        assert_eq!(from_notes("Klein beetje rood in luier"), (None, None, vec![]));
        assert_eq!(from_notes("Vitamine D en K gegeven").2, vec!["vitamin_d", "vitamin_k"]);
        assert_eq!(from_notes("vit. D gegeven").2, vec!["vitamin_d"]);
        assert_eq!(from_notes("Vitamins given, then a short breastfeed.").2, vec!["vitamin_d", "vitamin_k"]);
        assert_eq!(from_notes("vitaminen gegeven").2, vec!["vitamin_d", "vitamin_k"]);
        assert_eq!(from_notes("Vitamine gegeven").2, Vec::<&str>::new(), "singular without letter: unclear");
        assert_eq!(from_notes("D+K druppels").2, Vec::<&str>::new(), "no vitamin word: not guessed");
    }
}
