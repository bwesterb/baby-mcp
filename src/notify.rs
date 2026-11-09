//! Push notifications: once a minute, work out what is due and push it to
//! every device of the users who want that kind.
//!
//! - `feed_soon`: 15 minutes before the next-feed estimate (the feed tile's);
//! - `feed_overdue`: 30 minutes after it (not after 3 hours: then nobody is logging);
//! - `attention`: an entry needs the user (🙋) and was left unchanged for 10
//!   minutes (whoever just made it knows; finished in time, nothing is sent).
//!
//! Each (user, kind, key) is sent once; the key is the last feed's time or
//! the entry and its version.

use crate::app::App;
use crate::push::{self, Outcome, Subscription};
use crate::time::{Clock, now};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::sync::Arc;

pub const KINDS: [(&str, &str, &str); 6] = [
    ("feed_soon", "Feed soon", "15 minutes before it's time to start (diaper first)"),
    ("feed_overdue", "Feed overdue", "30 minutes after the next feed was expected"),
    ("attention", "Entries need attention", "when an entry needing you (🙋) is left for 10 minutes"),
    ("vitamins", "Vitamins", "at 18:00 if vitamin K or D hasn't been given that day"),
    ("changes", "Any change", "when someone else logs, changes or deletes an entry"),
    ("feed_warning", "Feeding minimum", "when it gets close to the longest pause without a feed, or to only the minimum number of feeds in 24 hours (NVK)"),
];

const SOON: i64 = 15 * 60;
const OVERDUE: i64 = 30 * 60;
const STALE: i64 = 3 * 3600;
const ATTENTION_WINDOW: i64 = 15 * 60;
/// An entry needing the user notifies once it has been left this long.
const ATTENTION_QUIET: i64 = 10 * 60;

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: &'static str,
    pub key: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub ttl: u32,
    /// Who caused it: not sent to that user.
    pub by: Option<i64>,
}

/// What is due now, for everyone (before choices and deduplication).
pub fn due(c: &Connection, clock: &Clock, status: &Value, name: &str, now: i64) -> Result<Vec<Notice>, String> {
    let mut out = vec![];
    let at = status["next_feed_estimate"]["at"].as_str().and_then(|s| clock.parse(s, now).ok());
    let last = status["last_feed"]["at"].as_str().and_then(|s| clock.parse(s, now).ok());
    if let (Some(at), Some(last)) = (at, last) {
        let mut extra = vec![];
        if let Some(ml) = status["feeding_guidance"]["next_feed"]["amount_ml"].as_f64() {
            extra.push(format!("about {ml:.0} ml"));
        }
        if let Some(side) = status["feeding_guidance"]["breast_side"]["start"].as_str() {
            extra.push(format!("start {side} breast"));
        }
        if let Some(side) = status["sleep_facing"]["next"].as_str() {
            extra.push(format!("then face {side}"));
        }
        let extra = if extra.is_empty() { String::new() } else { format!(" · {}", extra.join(" · ")) };
        let hm = clock.zoned(at).strftime("%H:%M").to_string();
        let last_hm = clock.zoned(last).strftime("%H:%M").to_string();
        // With a preparation time (diaper first), "soon" counts from the start.
        let start = status["next_feed_start"]["at"].as_str().and_then(|s| clock.parse(s, now).ok()).unwrap_or(at);
        if now >= start - SOON && now < start {
            let body = if start < at {
                format!("Start around {} (diaper first), feed ~{hm} (last feed {last_hm}){extra}", clock.zoned(start).strftime("%H:%M"))
            } else {
                format!("Expected around {hm} (last feed {last_hm}){extra}")
            };
            out.push(Notice {
                kind: "feed_soon",
                key: last.to_string(),
                title: format!("🍼 {name}: feed soon"),
                body,
                url: "/".into(),
                ttl: (at - now + OVERDUE) as u32,
                by: None,
            });
        }
        if now >= at + OVERDUE && now < at + STALE {
            out.push(Notice {
                kind: "feed_overdue",
                key: last.to_string(),
                title: format!("⏰ {name}: feed overdue"),
                body: format!("Expected around {hm}; last feed {last_hm}{extra}"),
                url: "/".into(),
                ttl: 3600,
                by: None,
            });
        }
    }
    for w in status["feeding_guidance"]["warnings"].as_array().into_iter().flatten() {
        if let (Some(key), Some(text)) = (w["key"].as_str(), w["text"].as_str()) {
            out.push(Notice {
                kind: "feed_warning",
                key: key.to_string(),
                title: format!("⚠️ {name}: feeding"),
                body: text.to_string(),
                url: "/".into(),
                ttl: 2 * 3600,
                by: None,
            });
        }
    }
    let give: Vec<&str> = status["vitamins"]["to_give_today"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if let (false, Some(remind)) = (give.is_empty(), status["vitamins"]["remind_at"].as_str().and_then(|s| clock.parse(s, now).ok())) {
        if now >= remind && now < clock.next_day_start(now) {
            let what = give.iter().map(|l| format!("vitamin {l}")).collect::<Vec<_>>().join(" and ");
            out.push(Notice {
                kind: "vitamins",
                key: clock.day(now),
                title: format!("💊 {name}: vitamins"),
                body: format!("Not given yet today: {what}"),
                url: "/".into(),
                ttl: (clock.next_day_start(now) - now) as u32,
                by: None,
            });
        }
    }
    let mut st = c
        .prepare(
            "SELECT entry_id, version, at, text, needs_attention_why FROM entry \
             WHERE deleted = 0 AND needs_attention_by != 0 AND needs_attention_by & 1 AND changed_at >= ?1 AND changed_at <= ?2 ORDER BY at",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([now - ATTENTION_QUIET - ATTENTION_WINDOW, now - ATTENTION_QUIET], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?)))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, _version, at, text, why) = row.map_err(|e| e.to_string())?;
        let short: String = text.chars().take(80).collect();
        out.push(Notice {
            kind: "attention",
            // Once per entry and reason: not again for each edit.
            key: format!("{id}:{}", why.as_deref().unwrap_or("")),
            title: format!("🙋 {name}: needs your attention"),
            body: match why {
                Some(w) => format!("{w} — {} {short}", clock.zoned(at).strftime("%H:%M")),
                None => format!("{} {short}", clock.zoned(at).strftime("%H:%M")),
            },
            url: format!("/?day={}", clock.day(at)),
            ttl: 6 * 3600,
            by: None,
        });
    }
    // Any change: every new version in the last 15 minutes, for the others.
    let names = crate::entries::user_names(c)?;
    let ids: Vec<i64> = {
        let mut st = c.prepare("SELECT entry_id FROM entry WHERE changed_at >= ?1 ORDER BY changed_at").map_err(|e| e.to_string())?;
        st.query_map([now - ATTENTION_WINDOW], |r| r.get(0)).map_err(|e| e.to_string())?.collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())?
    };
    for id in ids {
        let e = crate::entries::get(c, id)?;
        let who = names.get(&e.changed_by).cloned().unwrap_or_else(|| "someone".into());
        let verb = if e.deleted { "deleted" } else if e.version == 1 { "logged" } else { "changed" };
        let what = crate::web::entry_summary(&e);
        let what = if what.is_empty() { e.text.chars().take(80).collect() } else { what };
        out.push(Notice {
            kind: "changes",
            key: format!("{id}:{}", e.version),
            title: format!("✏️ {name}: {who} {verb} {}", clock.zoned(e.at).strftime("%H:%M")),
            body: what,
            url: format!("/entry/{id}"),
            ttl: 3 * 3600,
            by: Some(e.changed_by),
        });
    }
    Ok(out)
}

/// What the app icon's badge counts: each entry needing the parents (🙋),
/// and one each for feeding (due within 15 minutes, overdue, or an NVK
/// warning), vitamins not given yet after 18:00, and a bath due today (from
/// 18:00) or overdue.
pub fn badge(clock: &Clock, status: &Value, now: i64) -> Vec<String> {
    let mut items = vec![];
    for e in status["needs_attention"].as_array().into_iter().flatten() {
        if e["needs_attention"]["by"].as_array().is_some_and(|b| b.iter().any(|x| x == "user")) {
            items.push(format!("🙋 {}", e["needs_attention"]["why"].as_str().unwrap_or("needs attention")));
        }
    }
    let g = &status["feeding_guidance"];
    // With a preparation time, "soon" and "overdue" count from the start.
    let next = if status["next_feed_start"]["at"].is_string() { &status["next_feed_start"] } else { &status["next_feed_estimate"] };
    if let Some(w) = g["warnings"].as_array().and_then(|w| w.first()).and_then(|w| w["text"].as_str()) {
        items.push(format!("⚠️ {w}"));
    } else if next["overdue_by"].is_string() {
        items.push("🍼 feed overdue".into());
    } else if next["at"].as_str().and_then(|a| clock.parse(a, now).ok()).is_some_and(|t| t - now <= SOON) {
        items.push("🍼 feed due soon".into());
    }
    let give: Vec<&str> = status["vitamins"]["to_give_today"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if !give.is_empty() && status["vitamins"]["remind_at"].as_str().and_then(|r| clock.parse(r, now).ok()).is_some_and(|r| now >= r) {
        items.push(format!("💊 vitamin {} not given yet", give.join(" + ")));
    }
    if let Some(w) = status["wet_diaper_warning"]["text"].as_str() {
        items.push(format!("💧 {w}"));
    }
    // The bath: from the evening it's due (with which kind), overdue after.
    let bath = &status["bath"];
    let kind = match bath["next_kind"].as_str() {
        Some("with_soap") => " (with soap)",
        Some("without_soap") => " (without soap)",
        _ => "",
    };
    if bath["due"]["overdue_by"].is_string() {
        items.push(format!("🛁 bath overdue{kind}"));
    } else if bath["due"]["today"] == true && bath["due"]["at"].as_str().and_then(|a| clock.parse(a, now).ok()).is_some_and(|t| now >= t) {
        items.push(format!("🛁 bath today{kind}"));
    }
    items
}

/// The badge for the current state.
/// The badge items, and the notification groups that still apply: those with
/// a notice due now, and attention while an entry needs a parent. A shown
/// notification of any other group is stale (changes and tests are once).
pub fn current(app: &App, now: i64) -> Result<(Vec<String>, Vec<&'static str>), String> {
    let baby = app.baby();
    app.db
        .with(|c| {
            Ok((|| {
                let status = crate::entries::status(c, &app.clock, &baby, now)?;
                let mut groups: Vec<&'static str> = vec![];
                for n in due(c, &app.clock, &status, &baby.name, now)? {
                    let g = group(n.kind);
                    if !matches!(g, "changes" | "test") && !groups.contains(&g) {
                        groups.push(g);
                    }
                }
                let needs_user = status["needs_attention"].as_array().into_iter().flatten()
                    .any(|e| e["needs_attention"]["by"].as_array().is_some_and(|b| b.iter().any(|x| x == "user")));
                if needs_user && !groups.contains(&"attention") {
                    groups.push("attention");
                }
                Ok::<_, String>((badge(&app.clock, &status, now), groups))
            })())
        })
        .map_err(|e| e.to_string())?
}

/// Kinds that are off until a user turns them on.
const OFF_BY_DEFAULT: [&str; 1] = ["changes"];

/// Notification groups: one notification per group on a device, a newer one
/// replacing the older (also the push "topic").
pub fn group(kind: &str) -> &'static str {
    match kind {
        "feed_soon" | "feed_overdue" | "feed_warning" => "feed",
        "attention" => "attention",
        "changes" => "changes",
        "vitamins" => "vitamins",
        "test" => "test",
        _ => "other",
    }
}

fn priority(kind: &str) -> u8 {
    match kind {
        "feed_warning" => 0,
        "feed_overdue" => 1,
        "feed_soon" => 2,
        _ => 3,
    }
}

/// The current notices of one group as one notification: the most important
/// first, one line each.
pub fn merge(group: &str, name: &str, mut ns: Vec<Notice>) -> Notice {
    ns.sort_by_key(|n| priority(n.kind));
    if ns.len() == 1 {
        return ns.remove(0);
    }
    let first = ns[0].clone();
    let mut lines: Vec<String> = vec![];
    let title = match group {
        "changes" => {
            let prefix = format!("✏️ {name}: ");
            for n in &ns {
                lines.push(format!("{}: {}", n.title.trim_start_matches(prefix.as_str()), n.body));
            }
            format!("✏️ {name}: {} changes", ns.len())
        }
        "attention" => {
            lines = ns.iter().map(|n| n.body.clone()).collect();
            format!("🙋 {name}: {} entries need your attention", ns.len())
        }
        _ => {
            for n in &ns {
                if !lines.contains(&n.body) {
                    lines.push(n.body.clone());
                }
            }
            first.title.clone()
        }
    };
    Notice {
        kind: first.kind,
        key: first.key.clone(),
        title,
        body: lines.join("\n"),
        url: if matches!(group, "changes" | "attention") { "/".into() } else { first.url.clone() },
        ttl: ns.iter().map(|n| n.ttl).max().unwrap_or(3600),
        by: None,
    }
}

/// A user's choice for a kind (on unless turned off, except OFF_BY_DEFAULT).
pub fn wants(c: &Connection, user_id: i64, kind: &str) -> rusqlite::Result<bool> {
    let v: Option<i64> = rusqlite::OptionalExtension::optional(c.query_row(
        "SELECT enabled FROM push_pref WHERE user_id = ?1 AND kind = ?2",
        params![user_id, kind],
        |r| r.get(0),
    ))?;
    Ok(v.unwrap_or(if OFF_BY_DEFAULT.contains(&kind) { 0 } else { 1 }) == 1)
}

pub fn subscriptions(c: &Connection, user_id: i64) -> rusqlite::Result<Vec<Subscription>> {
    let mut st = c.prepare("SELECT id, endpoint, p256dh, auth FROM push_subscription WHERE user_id = ?1")?;
    let rows = st.query_map([user_id], |r| Ok(Subscription { id: r.get(0)?, endpoint: r.get(1)?, p256dh: r.get(2)?, auth: r.get(3)? }))?;
    rows.collect()
}

pub fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("baby-mcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

/// Pushes a message to all of a user's devices; returns how many accepted it.
pub async fn deliver(app: &App, client: &reqwest::Client, user_id: i64, n: &Notice, topic: &str) -> Result<usize, String> {
    let t = now();
    let (vapid, subs, id) = app
        .db
        .with(|c| Ok((push::vapid(c)?, subscriptions(c, user_id)?, push::store_message(c, user_id, &n.title, &n.body, &n.url, topic, t)?)))
        .map_err(|e| e.to_string())?;
    let payload = serde_json::json!({ "m": id }).to_string();
    let mut ok = 0;
    for s in &subs {
        let outcome = push::send(client, &vapid, s, payload.as_bytes(), n.ttl, topic, app.cfg.origin(), t).await;
        let _ = app.db.with(|c| match &outcome {
            Outcome::Ok => c.execute("UPDATE push_subscription SET last_ok = ?1, last_error = NULL WHERE id = ?2", params![t, s.id]),
            Outcome::Gone => c.execute("DELETE FROM push_subscription WHERE id = ?1", [s.id]),
            Outcome::Failed(e) => c.execute("UPDATE push_subscription SET last_error = ?1 WHERE id = ?2", params![e, s.id]),
        });
        match outcome {
            Outcome::Ok => ok += 1,
            Outcome::Gone => eprintln!("push: subscription {} is gone; removed", s.id),
            Outcome::Failed(e) => eprintln!("push: subscription {}: {e}", s.id),
        }
    }
    Ok(ok)
}

/// One round: what is due, for whom, not sent before.
async fn tick(app: &App, client: &reqwest::Client) -> Result<(), String> {
    let t = now();
    let baby = app.baby();
    let jobs: Vec<(i64, Notice)> = app
        .db
        .with(|c| {
            let status = crate::entries::status(c, &app.clock, &baby, t).map_err(rusqlite::Error::InvalidParameterName)?;
            let notices = due(c, &app.clock, &status, &baby.name, t).map_err(rusqlite::Error::InvalidParameterName)?;
            let users: Vec<i64> = {
                let mut st = c.prepare("SELECT DISTINCT s.user_id FROM push_subscription s JOIN user u ON u.id = s.user_id WHERE u.role = 'parent'")?;
                st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            // Per user and group: everything current, and whether any of it is new.
            let mut current: Vec<((i64, &'static str), Vec<Notice>, bool)> = vec![];
            for n in notices {
                for &u in &users {
                    if n.by == Some(u) || !wants(c, u, n.kind)? {
                        continue;
                    }
                    // Mark first: never twice, even if delivery fails.
                    let fresh = c.execute(
                        "INSERT OR IGNORE INTO push_sent (user_id, kind, key, at) VALUES (?1, ?2, ?3, ?4)",
                        params![u, n.kind, n.key, t],
                    )? == 1;
                    let k = (u, group(n.kind));
                    match current.iter_mut().find(|x| x.0 == k) {
                        Some(x) => {
                            x.1.push(n.clone());
                            x.2 |= fresh;
                        }
                        None => current.push((k, vec![n.clone()], fresh)),
                    }
                }
            }
            c.execute("DELETE FROM push_sent WHERE at < ?1", [t - 7 * 86400])?;
            // A group with something new goes out as one notification with all of it.
            Ok(current.into_iter().filter(|x| x.2).map(|((u, g), ns, _)| (u, merge(g, &baby.name, ns))).collect())
        })
        .map_err(|e| e.to_string())?;
    for (u, n) in jobs {
        deliver(app, client, u, &n, group(n.kind)).await?;
    }
    Ok(())
}

pub async fn run(app: Arc<App>) {
    let client = client();
    let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        every.tick().await;
        if let Err(e) = tick(&app, &client).await {
            eprintln!("push: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn feed_notices() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("t.sqlite")).unwrap();
        db.with(|c| {
            c.execute_batch("INSERT INTO user (id, name, display_name, created_at) VALUES (1, 'anna', 'Anna', 0), (2, 'tom', 'Tom', 0)")?;
            Ok(())
        }).unwrap();
        let c = rusqlite::Connection::open(dir.path().join("t.sqlite")).unwrap();
        let status = json!({
            "last_feed": {"at": "2025-11-07 10:00"},
            "next_feed_estimate": {"at": "2025-11-07 13:00"},
            "feeding_guidance": {"next_feed": {"amount_ml": 90.0}},
            "sleep_facing": {"next": "right"},
        });
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let kinds = |t: &str| due(&c, &clock, &status, "Eva", at(t)).unwrap().into_iter().map(|n| n.kind).collect::<Vec<_>>();
        assert!(kinds("2025-11-07 12:40").is_empty());
        assert_eq!(kinds("2025-11-07 12:45"), ["feed_soon"]);
        assert!(kinds("2025-11-07 13:10").is_empty());
        assert_eq!(kinds("2025-11-07 13:30"), ["feed_overdue"]);
        assert!(kinds("2025-11-07 16:00").is_empty(), "stale: nobody is logging");
        let n = &due(&c, &clock, &status, "Eva", at("2025-11-07 12:50")).unwrap()[0];
        assert_eq!(n.body, "Expected around 13:00 (last feed 10:00) · about 90 ml · then face right");
        // With 15 minutes' preparation: "soon" from 12:30, saying when to start.
        let mut prep = status.clone();
        prep["next_feed_start"] = json!({"at": "2025-11-07 12:45", "feed_at": "2025-11-07 13:00", "prep": "15m"});
        assert!(due(&c, &clock, &prep, "Eva", at("2025-11-07 12:29")).unwrap().is_empty());
        let n = &due(&c, &clock, &prep, "Eva", at("2025-11-07 12:31")).unwrap()[0];
        assert_eq!(n.body, "Start around 12:45 (diaper first), feed ~13:00 (last feed 10:00) · about 90 ml · then face right");
        assert_eq!(n.key, at("2025-11-07 10:00").to_string());

        let new = |text: &str, f: Value, by: i64, attn: i64| {
            crate::entries::insert(&c, by, &crate::entries::NewEntry {
                at: at("2025-11-07 12:40"), duration_s: None, text: text.into(), fields: f.as_object().cloned().unwrap(),
                attention_by: attn, attention_why: (attn != 0).then(|| "check this".to_string()),
            }, at("2025-11-07 12:40")).unwrap()
        };
        let spit = new("Spit up a lot", json!({"vomit": true}), 1, 1);
        new("agent only", json!({}), 2, 2);
        let ns = due(&c, &clock, &status, "Eva", at("2025-11-07 12:44")).unwrap();
        assert!(!ns.iter().any(|n| n.kind == "attention"), "not while it was just made (whoever did knows)");
        let later = due(&c, &clock, &status, "Eva", at("2025-11-07 12:51")).unwrap();
        let attention: Vec<&Notice> = later.iter().filter(|n| n.kind == "attention").collect();
        assert_eq!(attention.len(), 1, "left for 10 minutes: only the one needing the user");
        assert_eq!((attention[0].key.clone(), attention[0].url.as_str()), (format!("{}:check this", spit.entry_id), "/?day=2025-11-07"));
        // Any change: both new entries, each with its author (not sent to them).
        let changes: Vec<&Notice> = ns.iter().filter(|n| n.kind == "changes").collect();
        assert_eq!(changes.len(), 2);
        assert_eq!((changes[0].title.as_str(), changes[0].body.as_str(), changes[0].by), ("✏️ Eva: Anna logged 12:40", "🤢", Some(1)));
        assert_eq!(changes[1].title, "✏️ Eva: Tom logged 12:40");
        assert!(due(&c, &clock, &status, "Eva", at("2025-11-07 13:10")).unwrap().is_empty(), "only recent changes");
        // An edit starts the 10 minutes again; the same reason keeps the key.
        let keys = |t: &str| due(&c, &clock, &status, "Eva", at(t)).unwrap().into_iter().filter(|n| n.kind == "attention").map(|n| n.key).collect::<Vec<_>>();
        let mut edited = spit.clone();
        edited.text = "Spit up a lot, twice".into();
        crate::entries::save(&c, 1, &edited, at("2025-11-07 12:42")).unwrap();
        assert!(keys("2025-11-07 12:51").is_empty());
        assert_eq!(keys("2025-11-07 12:53"), [format!("{}:check this", spit.entry_id)]);
        // Finished within the 10 minutes: nothing is sent.
        let mut done = crate::entries::get(&c, spit.entry_id).unwrap();
        done.attention_by = 0;
        done.attention_why = None;
        crate::entries::save(&c, 1, &done, at("2025-11-07 12:45")).unwrap();
        assert!(keys("2025-11-07 12:56").is_empty());

        let vit = json!({"vitamins": {"to_give_today": ["K", "D"], "remind_at": "2025-11-07 18:00"}});
        assert!(due(&c, &clock, &vit, "Eva", at("2025-11-07 17:59")).unwrap().is_empty());
        let n = &due(&c, &clock, &vit, "Eva", at("2025-11-07 18:01")).unwrap()[0];
        assert_eq!((n.kind, n.key.as_str(), n.body.as_str()), ("vitamins", "2025-11-07", "Not given yet today: vitamin K and vitamin D"));
        let none = json!({"vitamins": {"to_give_today": [], "remind_at": "2025-11-07 18:00"}});
        assert!(due(&c, &clock, &none, "Eva", at("2025-11-07 18:01")).unwrap().is_empty());
    }

    #[test]
    fn grouping() {
        let n = |kind: &'static str, title: &str, body: &str| Notice { kind, key: String::new(), title: title.into(), body: body.into(), url: "/".into(), ttl: 600, by: None };
        let feed = merge("feed", "Eva", vec![n("feed_soon", "🍼 Eva: feed soon", "Start around 12:45"), n("feed_warning", "⚠️ Eva: feeding", "Almost 6 h since the last feed")]);
        assert_eq!((feed.title.as_str(), feed.body.as_str()), ("⚠️ Eva: feeding", "Almost 6 h since the last feed\nStart around 12:45"));
        let ch = merge("changes", "Eva", vec![n("changes", "✏️ Eva: Tom logged 12:40", "🤢"), n("changes", "✏️ Eva: Ritu changed 12:00", "🍼 60 ml formula")]);
        assert_eq!((ch.title.as_str(), ch.body.as_str()), ("✏️ Eva: 2 changes", "Tom logged 12:40: 🤢\nRitu changed 12:00: 🍼 60 ml formula"));
        assert_eq!(merge("vitamins", "Eva", vec![n("vitamins", "💊", "x")]).body, "x", "one notice stays as it is");
        assert_eq!((group("feed_overdue"), group("feed_warning"), group("changes")), ("feed", "feed", "changes"));
    }

    #[test]
    fn badge_items() {
        let clock = Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let s = json!({
            "needs_attention": [
                {"needs_attention": {"by": ["user"], "why": "bath: with or without soap?"}},
                {"needs_attention": {"by": ["agent"], "why": "split"}},
            ],
            "next_feed_estimate": {"at": "2025-11-07 13:00", "due_in": "10m"},
            "feeding_guidance": {"warnings": []},
            "vitamins": {"to_give_today": ["D"], "remind_at": "2025-11-07 18:00"},
            "bath": {"due": {"due_in": "2d"}},
        });
        assert_eq!(badge(&clock, &s, at("2025-11-07 12:50")), vec!["🙋 bath: with or without soap?", "🍼 feed due soon"]);
        assert_eq!(badge(&clock, &s, at("2025-11-07 12:30")).len(), 1, "feed not soon yet; agent-only entries don't count");
        // A bath due today: on the badge from the evening, with the kind.
        let mut sb = s.clone();
        sb["bath"] = json!({"due": {"on": "2025-11-07", "at": "2025-11-07 18:00", "today": true}, "next_kind": "without_soap"});
        assert!(!badge(&clock, &sb, at("2025-11-07 17:59")).iter().any(|i| i.starts_with("🛁")), "not before the evening");
        assert!(badge(&clock, &sb, at("2025-11-07 18:00")).contains(&"🛁 bath today (without soap)".to_string()));
        let mut s2 = s.clone();
        s2["feeding_guidance"]["warnings"] = json!([{"text": "Almost 5h since the last feed"}]);
        s2["bath"]["due"] = json!({"overdue_by": "3h"});
        let b = badge(&clock, &s2, at("2025-11-07 18:30"));
        assert_eq!(b, vec!["🙋 bath: with or without soap?", "⚠️ Almost 5h since the last feed", "💊 vitamin D not given yet", "🛁 bath overdue"]);
    }
}
