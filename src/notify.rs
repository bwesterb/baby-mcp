//! Push notifications: once a minute, work out what is due and push it to
//! every device of the users who want that kind.
//!
//! - `feed_soon`: 15 minutes before it's time to start the next feed (the
//!   estimate less the preparation time, diaper first: the feed tile's);
//! - `feed_overdue`: 30 minutes after the estimate (not after 3 hours: then
//!   nobody is logging);
//! - `feed_warning`: an NVK warning (close to the longest pause, or too few
//!   feeds in 24 hours);
//! - `vitamins`: from 18:00 when one wasn't given that day;
//! - `attention`: an entry needs the user (🙋) and was left unchanged for 20
//!   minutes (whoever just made it knows; finished in time, nothing is sent);
//! - `changes` (off by default): someone else logged, changed or deleted an entry.
//!
//! Each (user, kind, key) is sent once; the key is the last feed's time, the
//! warning's, the day, or the entry (and its reason or version).

use crate::app::App;
use crate::push::{self, Outcome, Subscription};
use crate::time::{Clock, now};
use rusqlite::{Connection, params};
use crate::status::Status;
use std::sync::Arc;

pub const KINDS: [(&str, &str, &str); 6] = [
    ("feed_soon", "Feed soon", "15 minutes before it's time to start (diaper first)"),
    ("feed_overdue", "Feed overdue", "30 minutes after the next feed was expected"),
    ("attention", "Entries need attention", "when an entry needing you (🙋) is left for 20 minutes"),
    ("vitamins", "Vitamins", "at 18:00 if vitamin K or D hasn't been given that day"),
    ("changes", "Any change", "when someone else logs, changes or deletes an entry"),
    ("feed_warning", "Feeding minimum", "when it gets close to the longest pause without a feed, or to only the minimum number of feeds in 24 hours (NVK)"),
];

const SOON: i64 = 15 * 60;
const OVERDUE: i64 = 30 * 60;
const STALE: i64 = 3 * 3600;
const ATTENTION_WINDOW: i64 = 15 * 60;
/// An entry needing the user notifies once it has been left this long.
const ATTENTION_QUIET: i64 = 20 * 60;

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
pub fn due(c: &Connection, clock: &Clock, status: &Status, name: &str, now: i64) -> Result<Vec<Notice>, String> {
    let mut out = vec![];
    let at = status.next_feed.map(|f| f.at);
    let last = status.last_feed.as_ref().map(|e| e.at);
    if let (Some(at), Some(last)) = (at, last) {
        let mut extra = vec![];
        if let Some(g) = &status.guidance {
            extra.push(format!("about {:.0} ml", g.amount_ml));
        }
        if let Some(b) = status.guidance.as_ref().and_then(|g| g.breast_side.as_ref()) {
            extra.push(format!("start {} breast", b.start));
        }
        if let Some(s) = &status.sleep {
            extra.push(format!("then face {}", s.next));
        }
        let extra = if extra.is_empty() { String::new() } else { format!(" · {}", extra.join(" · ")) };
        let hm = clock.fmt_time(at);
        let last_hm = clock.fmt_time(last);
        // With a preparation time (diaper first), "soon" counts from the start.
        let start = status.feed_start().unwrap_or(at);
        if now >= start - SOON && now < start {
            let body = if start < at {
                format!("Start around {} (diaper first), feed ~{hm} (last feed {last_hm}){extra}", clock.fmt_time(start))
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
    for w in status.guidance.iter().flat_map(|g| &g.warnings) {
        out.push(Notice {
            kind: "feed_warning",
            key: w.key.clone(),
            title: format!("⚠️ {name}: feeding"),
            body: w.text.clone(),
            url: "/".into(),
            ttl: 2 * 3600,
            by: None,
        });
    }
    let give = status.vitamins.as_ref().map(|v| v.to_give()).unwrap_or_default();
    if let (false, Some(remind)) = (give.is_empty(), status.vitamins.as_ref().map(|v| v.remind_at))
        && now >= remind {
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
    let quiet = crate::entries::query(
        c,
        "deleted = 0 AND needs_attention_by & ?1 != 0 AND changed_at >= ?2 AND changed_at <= ?3 ORDER BY at",
        params![crate::entries::BY_USER, now - ATTENTION_QUIET - ATTENTION_WINDOW, now - ATTENTION_QUIET],
    )?;
    for e in quiet {
        let (id, at, text, why) = (e.entry_id, e.at, e.text, e.attention_why);
        let short: String = text.chars().take(80).collect();
        out.push(Notice {
            kind: "attention",
            // Once per entry and reason: not again for each edit.
            key: format!("{id}:{}", why.as_deref().unwrap_or("")),
            title: format!("🙋 {name}: needs your attention"),
            body: match why {
                Some(w) => format!("{w} — {} {short}", clock.fmt_time(at)),
                None => format!("{} {short}", clock.fmt_time(at)),
            },
            url: format!("/?day={}", clock.day(at)),
            ttl: 6 * 3600,
            by: None,
        });
    }
    // Any change: every new version in the last 15 minutes, for the others.
    let names = crate::entries::user_names(c)?;
    for e in crate::entries::query(c, "changed_at >= ?1 ORDER BY changed_at", [now - ATTENTION_WINDOW])? {
        let id = e.entry_id;
        let who = names.get(&e.changed_by).cloned().unwrap_or_else(|| "someone".into());
        let verb = if e.deleted { "deleted" } else if e.version == 1 { "logged" } else { "changed" };
        let what = crate::web::entry_summary(&e);
        let what = if what.is_empty() { e.text.chars().take(80).collect() } else { what };
        out.push(Notice {
            kind: "changes",
            key: format!("{id}:{}", e.version),
            title: format!("✏️ {name}: {who} {verb} {}", clock.fmt_time(e.at)),
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
pub fn badge(status: &Status, now: i64) -> Vec<String> {
    let mut items = vec![];
    for e in status.needs_user() {
        items.push(format!("🙋 {}", e.attention_why.as_deref().unwrap_or("needs attention")));
    }
    // With a preparation time, "soon" and "overdue" count from the start.
    if let Some(w) = status.guidance.as_ref().and_then(|g| g.warnings.first()) {
        items.push(format!("⚠️ {}", w.text));
    } else if let Some(start) = status.feed_start() {
        if now > start {
            items.push("🍼 feed overdue".into());
        } else if start - now <= SOON {
            items.push("🍼 feed due soon".into());
        }
    }
    let give = status.vitamins.as_ref().map(|v| v.to_give()).unwrap_or_default();
    if !give.is_empty() && status.vitamins.as_ref().is_some_and(|v| now >= v.remind_at) {
        items.push(format!("💊 vitamin {} not given yet", give.join(" + ")));
    }
    if let Some(w) = &status.wet_warning {
        items.push(format!("💧 {}", w.text));
    }
    // The bath: from the evening it's due (with which kind), overdue after.
    let bath = &status.bath;
    let kind = match bath.next_kind() {
        Some("with_soap") => " (with soap)",
        Some("without_soap") => " (without soap)",
        _ => "",
    };
    if bath.overdue(now).is_some() {
        items.push(format!("🛁 bath overdue{kind}"));
    } else if bath.today(now) && now >= bath.due_at {
        items.push(format!("🛁 bath today{kind}"));
    }
    items
}

/// The badge items, and the notification groups that still apply: those with
/// a notice due now, and attention while an entry needs a parent. A shown
/// notification of any other group is stale (changes and tests are once).
pub fn current(app: &App, now: i64) -> Result<(Vec<String>, Vec<&'static str>), String> {
    let baby = app.baby();
    app.db
        .run(|c| {
            let status = crate::status::status(c, &app.clock, &baby, now)?;
            let mut groups: Vec<&'static str> = vec![];
            for n in due(c, &app.clock, &status, &baby.name, now)? {
                let g = group(n.kind);
                if !matches!(g, "changes" | "test") && !groups.contains(&g) {
                    groups.push(g);
                }
            }
            if status.needs_user().next().is_some() && !groups.contains(&"attention") {
                groups.push("attention");
            }
            Ok::<_, String>((badge(&status, now), groups))
        })
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
        .user_agent(concat!("baby-mcp/", env!("BABY_MCP_VERSION")))
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
        .run(|c| {
            let err = crate::entries::err;
            let status = crate::status::status(c, &app.clock, &baby, t)?;
            let notices = due(c, &app.clock, &status, &baby.name, t)?;
            let users: Vec<i64> = {
                let mut st = c.prepare("SELECT DISTINCT s.user_id FROM push_subscription s JOIN user u ON u.id = s.user_id WHERE u.role = 'parent'").map_err(err)?;
                st.query_map([], |r| r.get(0)).map_err(err)?.collect::<rusqlite::Result<_>>().map_err(err)?
            };
            // Per user and group: everything current, and whether any of it is new.
            let mut current: Vec<((i64, &'static str), Vec<Notice>, bool)> = vec![];
            for n in notices {
                for &u in &users {
                    if n.by == Some(u) || !wants(c, u, n.kind).map_err(err)? {
                        continue;
                    }
                    // Mark first: never twice, even if delivery fails.
                    let fresh = c.execute(
                        "INSERT OR IGNORE INTO push_sent (user_id, kind, key, at) VALUES (?1, ?2, ?3, ?4)",
                        params![u, n.kind, n.key, t],
                    ).map_err(err)? == 1;
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
            c.execute("DELETE FROM push_sent WHERE at < ?1", [t - 7 * 86400]).map_err(err)?;
            // A group with something new goes out as one notification with all of it.
            Ok(current.into_iter().filter(|x| x.2).map(|((u, g), ns, _)| (u, merge(g, &baby.name, ns))).collect())
        })?;
    // One failure must not hold back the others (they are marked sent).
    for (u, n) in jobs {
        if let Err(e) = deliver(app, client, u, &n, group(n.kind)).await {
            eprintln!("push to user {u}: {e}");
        }
    }
    Ok(())
}

pub async fn run(app: Arc<App>) {
    let client = client();
    let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
    // After a stall (a suspended machine, a slow tick) carry on; no burst.
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
    use crate::status::{Bath, FeedEstimate, SleepFacing, entry, vitamins};
    use serde_json::{Value, json};

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
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let status = Status {
            last_feed: Some(entry(at("2025-11-07 10:00"), json!({"bottle_formula_ml": 60}))),
            next_feed: Some(FeedEstimate { at: at("2025-11-07 13:00"), gap: 3 * 3600, n: 8, usual: 3 * 3600 }),
            guidance: Some(crate::feeding::Guidance { amount_ml: 90.0, ..Default::default() }),
            sleep: Some(SleepFacing { next: "right".into(), ..Default::default() }),
            ..Default::default()
        };
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
        prep.prep_s = 15 * 60;
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
        let later = due(&c, &clock, &status, "Eva", at("2025-11-07 13:01")).unwrap();
        let attention: Vec<&Notice> = later.iter().filter(|n| n.kind == "attention").collect();
        assert_eq!(attention.len(), 1, "left for 20 minutes: only the one needing the user");
        assert_eq!((attention[0].key.clone(), attention[0].url.as_str()), (format!("{}:check this", spit.entry_id), "/?day=2025-11-07"));
        // Any change: both new entries, each with its author (not sent to them).
        let changes: Vec<&Notice> = ns.iter().filter(|n| n.kind == "changes").collect();
        assert_eq!(changes.len(), 2);
        assert_eq!((changes[0].title.as_str(), changes[0].body.as_str(), changes[0].by), ("✏️ Eva: Anna logged 12:40", "🤢", Some(1)));
        assert_eq!(changes[1].title, "✏️ Eva: Tom logged 12:40");
        assert!(due(&c, &clock, &status, "Eva", at("2025-11-07 13:20")).unwrap().is_empty(), "only recent changes");
        // An edit starts the 20 minutes again; the same reason keeps the key.
        let keys = |t: &str| due(&c, &clock, &status, "Eva", at(t)).unwrap().into_iter().filter(|n| n.kind == "attention").map(|n| n.key).collect::<Vec<_>>();
        let mut edited = spit.clone();
        edited.text = "Spit up a lot, twice".into();
        crate::entries::save(&c, 1, &edited, at("2025-11-07 12:42")).unwrap();
        assert!(keys("2025-11-07 13:01").is_empty());
        assert_eq!(keys("2025-11-07 13:03"), [format!("{}:check this", spit.entry_id)]);
        // Finished within the 20 minutes: nothing is sent.
        let mut done = crate::entries::get(&c, spit.entry_id).unwrap();
        done.attention_by = 0;
        done.attention_why = None;
        crate::entries::save(&c, 1, &done, at("2025-11-07 12:45")).unwrap();
        assert!(keys("2025-11-07 13:06").is_empty());

        let vit = Status { vitamins: Some(vitamins(&clock, "2025-11-01", at("2025-11-07 18:00"), (true, false), (true, false))), ..Default::default() };
        assert!(due(&c, &clock, &vit, "Eva", at("2025-11-07 17:59")).unwrap().is_empty());
        let n = &due(&c, &clock, &vit, "Eva", at("2025-11-07 18:01")).unwrap()[0];
        assert_eq!((n.kind, n.key.as_str(), n.body.as_str()), ("vitamins", "2025-11-07", "Not given yet today: vitamin K and vitamin D"));
        let none = Status { vitamins: Some(vitamins(&clock, "2025-11-01", at("2025-11-07 18:00"), (true, true), (true, true))), ..Default::default() };
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
        let attn = |by: i64, why: &str| crate::entries::Entry { attention_by: by, attention_why: Some(why.into()), ..entry(at("2025-11-07 09:00"), json!({})) };
        let day = clock.date_start("2025-11-09").unwrap();
        let s = Status {
            attention: vec![attn(crate::entries::BY_USER, "bath: with or without soap?"), attn(crate::entries::BY_AGENT, "split")],
            next_feed: Some(FeedEstimate { at: at("2025-11-07 13:00"), gap: 3 * 3600, n: 8, usual: 3 * 3600 }),
            vitamins: Some(vitamins(&clock, "2025-11-01", at("2025-11-07 18:00"), (true, true), (true, false))),
            bath: Bath { last: Some(entry(at("2025-11-05 19:00"), json!({"bath": "with_soap"}))), last_any: None,
                         due_day: day, due_at: day + 18 * 3600, day_end: day + 86400 },
            ..Default::default()
        };
        assert_eq!(badge(&s, at("2025-11-07 12:50")), vec!["🙋 bath: with or without soap?", "🍼 feed due soon"]);
        assert_eq!(badge(&s, at("2025-11-07 12:30")).len(), 1, "feed not soon yet; agent-only entries don't count");
        // A bath due today: on the badge from the evening, with the kind.
        let mut sb = s.clone();
        let today = clock.date_start("2025-11-07").unwrap();
        (sb.bath.due_day, sb.bath.due_at, sb.bath.day_end) = (today, today + 18 * 3600, today + 86400);
        assert!(!badge(&sb, at("2025-11-07 17:59")).iter().any(|i| i.starts_with("🛁")), "not before the evening");
        assert!(badge(&sb, at("2025-11-07 18:00")).contains(&"🛁 bath today (without soap)".to_string()));
        let mut s2 = s.clone();
        s2.guidance = Some(crate::feeding::Guidance { warnings: vec![crate::feeding::Warning { text: "Almost 5h since the last feed".into(), ..Default::default() }], ..Default::default() });
        (s2.bath.due_day, s2.bath.due_at, s2.bath.day_end) = (today - 86400, today - 6 * 3600, today);
        let b = badge(&s2, at("2025-11-07 18:30"));
        assert_eq!(b, vec!["🙋 bath: with or without soap?", "⚠️ Almost 5h since the last feed", "💊 vitamin D not given yet", "🛁 bath overdue (without soap)"]);
    }
}
