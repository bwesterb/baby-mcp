//! Web UI for people: status, charts and a capture form that works without
//! an agent. Server-rendered; a passkey session is required.

use crate::app::App;
use crate::auth::{self, User, same_origin, session_user, to_login};
use crate::entries::{self as en, BY_AGENT, DayStats, NewEntry};
use crate::html::{self, Page, esc};
use crate::time::now;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn require(app: &App, headers: &HeaderMap, here: &str) -> Result<User, Response> {
    session_user(app, headers).ok_or_else(|| to_login(here))
}

fn stat(label: &str, value: &str, sub: &str) -> String {
    format!(
        r#"<div class="stat"><div class="l">{}</div><div class="v">{}</div><div class="mut">{}</div></div>"#,
        esc(label),
        esc(value),
        esc(sub)
    )
}

fn feed_summary(f: &Map<String, Value>) -> String {
    let mut parts = vec![];
    let n = |k: &str| f.get(k).and_then(Value::as_f64).filter(|x| *x > 0.0);
    let bottle: Vec<String> = [("bottle_breastmilk_ml", "milk"), ("bottle_formula_ml", "formula")]
        .iter()
        .filter_map(|(k, what)| n(k).map(|x| format!("{x:.0} ml {what}")))
        .collect();
    if !bottle.is_empty() {
        parts.push(format!("🍼 {}", bottle.join(" + ")));
    }
    let (l, r) = (n("breast_left_min").unwrap_or(0.0), n("breast_right_min").unwrap_or(0.0));
    if l + r > 0.0 {
        parts.push(match (l > 0.0, r > 0.0) {
            (true, true) => format!("🤱 L{l:.0}′ R{r:.0}′"),
            (true, false) => format!("🤱 L{l:.0}′"),
            _ => format!("🤱 R{r:.0}′"),
        });
    } else if f.get("latch").and_then(Value::as_str).is_some_and(|l| l != "none") {
        parts.push("🤱".into());
    }
    parts.join(" · ")
}

fn side_arrow(side: &str) -> &'static str {
    if side == "left" { "⬅️" } else { "➡️" }
}

fn stool_emoji(color: &str) -> &'static str {
    match color {
        "yellow" => "🟡",
        "green" => "🟢",
        "brown" => "🟤",
        "meconium" => "⚫",
        "transitional" => "🟠",
        _ => "⚪",
    }
}

/// Everything an entry records, as compact emoji shorthand.
pub(crate) fn entry_summary(e: &en::Entry) -> String {
    let mut what = vec![feed_summary(&e.fields)];
    if e.is("wet") {
        what.push("💧".into());
    }
    if e.is("dirty") {
        what.push(format!("💩{}", e.str("stool_color").map(stool_emoji).unwrap_or("")));
    }
    if e.is("vomit") {
        what.push("🤢".into());
    }
    if let Some(p) = e.num("pump_ml") {
        what.push(format!("🥛 {p:.0} ml pumped"));
    }
    if let Some(t) = e.num("temp_c") {
        what.push(format!("🌡️ {t:.1}"));
    }
    if let Some(w) = e.num("weight_g") {
        what.push(format!("⚖️ {w:.0} g"));
    }
    if let Some(side) = e.str("sleep_facing") {
        what.push(format!("😴{}", side_arrow(side)));
    }
    match (e.is("vitamin_d"), e.is("vitamin_k")) {
        (true, true) => what.push("💊 D+K".into()),
        (true, false) => what.push("💊 D".into()),
        (false, true) => what.push("💊 K".into()),
        _ => {}
    }
    match e.str("bath") {
        Some("with_soap") => what.push("🛁🧼".into()),
        Some("without_soap") => what.push("🛁".into()),
        Some("sponge") => what.push("🧽".into()),
        _ => {}
    }
    what.retain(|s| !s.is_empty());
    what.join(" · ")
}

/// One "what's next" tile: the expected time (or that it is overdue), and
/// small, when it last happened.
struct Next {
    title: &'static str,
    at: Option<String>, // "YYYY-MM-DD HH:MM"
    html: String,
}

fn next_tile(title: &'static str, last: &Value, last_detail: &str, next: &Value) -> Next {
    next_tile_extra(title, last, last_detail, next, "")
}

/// Like `next_tile`, with an extra line (already HTML) under the estimate.
fn next_tile_extra(title: &'static str, last: &Value, last_detail: &str, next: &Value, extra: &str) -> Next {
    let last_line = match last.as_object() {
        Some(o) => {
            let ago = o.get("ago").and_then(Value::as_str).unwrap_or("");
            let at = o.get("at").and_then(Value::as_str).map(|a| &a[11..]).unwrap_or("");
            let detail = if last_detail.is_empty() { String::new() } else { format!(" · {last_detail}") };
            format!("last {ago} ago · {at}{detail}")
        }
        None => "nothing logged yet".into(),
    };
    let at = next["at"].as_str().map(str::to_string);
    let (when, sub, overdue) = match &at {
        None => ("—".to_string(), "not enough data yet".to_string(), false),
        Some(a) => match (next["due_in"].as_str(), next["overdue_by"].as_str()) {
            (_, Some(d)) => (format!("~{}", &a[11..]), format!("⏰ {d} overdue"), true),
            (Some(d), _) => (format!("~{}", &a[11..]), format!("in {d}"), false),
            _ => (format!("~{}", &a[11..]), String::new(), false),
        },
    };
    let html = format!(
        r#"<div class="next{}"><div class="t">{}</div><div class="v">{}</div><div class="{}">{}</div>{extra}<div class="last">{}</div></div>"#,
        if overdue { " overdue" } else { "" },
        esc(title),
        esc(&when),
        if overdue { "warn" } else { "mut" },
        esc(&sub),
        esc(&last_line)
    );
    Next { title, at, html }
}

/// "🍼 about 80 ml" (the suggested amount for the next feed) for the feed tile.
fn amount_line(g: &Value) -> String {
    let mut s = match g["next_feed"]["amount_ml"].as_f64() {
        Some(ml) => format!(r#"<div class="side">🍼 about {ml:.0} ml</div>"#),
        None => String::new(),
    };
    if let (Some(side), Some(why)) = (g["breast_side"]["start"].as_str(), g["breast_side"]["reason"].as_str()) {
        s += &format!(r#"<div class="side">🤱 start {} <span class="mut">({})</span></div>"#, esc(side), esc(why));
    }
    for w in g["warnings"].as_array().into_iter().flatten() {
        if let Some(t) = w["text"].as_str() {
            s += &format!(r#"<div class="side warn">⚠️ {}</div>"#, esc(t));
        }
    }
    if let Some(latest) = g["next_feed"]["latest_at"].as_str().filter(|l| l.len() >= 16) {
        match g["next_feed"]["latest_start_at"].as_str().filter(|l| l.len() >= 16) {
            Some(start) => s += &format!(
                r#"<div class="side">⏳ start by {} <span class="mut">(feed by {} · {})</span></div>"#,
                esc(&start[11..16]),
                esc(&latest[11..16]),
                esc(g["next_feed"]["latest_reason"].as_str().unwrap_or(""))
            ),
            None => s += &format!(
                r#"<div class="side">⏳ at the latest {} <span class="mut">({})</span></div>"#,
                esc(&latest[11..16]),
                esc(g["next_feed"]["latest_reason"].as_str().unwrap_or(""))
            ),
        }
    }
    s
}

/// "😴 face ➡️ right" (the next side to put her down on) for the feed tile.
fn sleep_line(v: &Value) -> String {
    let Some(next) = v["next"].as_str() else { return String::new() };
    let assumed = match v["feeds_since"].as_u64() {
        Some(n) if n > 0 => format!(
            r#" <span class="mut">(assumed: alternated after {n} feed{} since the last logged side)</span>"#,
            if n == 1 { "" } else { "s" }
        ),
        _ => String::new(),
    };
    format!(r#"<div class="side">😴 face {} {}{assumed}</div>"#, side_arrow(next), esc(next))
}

/// The vitamins tile: what is still to give today (orange from 18:00), or
/// "tomorrow" when all were given; "starts …" before day 8. None when no
/// vitamin applies any more.
fn vitamins_tile(clock: &crate::time::Clock, v: &Value, now: i64) -> Option<Next> {
    let active: Vec<&str> = [("k", "K"), ("d", "D")].iter().filter(|(k, _)| v[*k]["active"] == true).map(|(_, l)| *l).collect();
    let from = v["from"].as_str()?;
    let until: Vec<String> = [("k", "K"), ("d", "D")]
        .iter()
        .filter_map(|(k, l)| v[*k]["until"].as_str().map(|u| format!("{l} until {}", clock.parse(&format!("{u} 12:00"), now).map(|t| clock.zoned(t).strftime("%-d %b %Y").to_string()).unwrap_or_default())))
        .collect();
    let remind = v["remind_at"].as_str().unwrap_or("").to_string();
    let to_give: Vec<&str> = v["to_give_today"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let (when, sub, at, overdue) = if active.is_empty() {
        let start = clock.date_start(from).ok()?;
        if start <= now {
            return None; // past 4 years
        }
        (clock.zoned(start).strftime("%a %-d %b").to_string(), "starts (day 8)".to_string(), format!("{from} 00:00"), false)
    } else if !to_give.is_empty() {
        let late = clock.parse(&remind, now).is_ok_and(|r| now >= r);
        let what = to_give.iter().map(|l| format!("vitamin {l}")).collect::<Vec<_>>().join(" + ");
        ("today".to_string(), format!("{}{what} still to give", if late { "⏰ " } else { "" }), remind.clone(), late)
    } else {
        let tomorrow = clock.parse(&remind, now).map(|r| clock.fmt(r + 86400)).unwrap_or_default();
        ("tomorrow".to_string(), format!("✓ {} given today", active.join(" + ")), tomorrow, false)
    };
    let html = format!(
        r#"<div class="next{}"><div class="t">💊 Vitamins</div><div class="v">{}</div><div class="{}">{}</div><div class="last">{}</div></div>"#,
        if overdue { " overdue" } else { "" },
        esc(&when),
        if overdue { "warn" } else { "mut" },
        esc(&sub),
        esc(&until.join(" · "))
    );
    Some(Next { title: "💊 Vitamins", at: Some(at), html })
}

/// The bath tile: when the next bath is due (as a day, not a clock time) and
/// whether to use soap (the other kind than last time).
fn bath_tile(clock: &crate::time::Clock, b: &Value, now: i64) -> Next {
    let day = |at: &str| match clock.parse(at, now) {
        Ok(t) if clock.day(t) <= clock.day(now) => "today".to_string(),
        Ok(t) if clock.day(t) == clock.day(now + 86400) => "tomorrow".to_string(),
        Ok(t) => clock.zoned(t).strftime("%a %-d %b").to_string(),
        Err(_) => at.to_string(),
    };
    let kind = |k: &str| match k {
        "with_soap" => "🧼 with soap",
        "without_soap" => "🛁 without soap",
        _ => "🧽 sponge",
    };
    let d = &b["due"];
    let never = d["never"].as_bool() == Some(true);
    let (when, sub, overdue) = if never {
        ("—".to_string(), "nothing logged yet".to_string(), false)
    } else if let Some(o) = d["overdue_by"].as_str() {
        ("now".to_string(), format!("⏰ {o} overdue"), true)
    } else {
        (d["at"].as_str().map(day).unwrap_or_default(), format!("in {}", d["due_in"].as_str().unwrap_or("")), false)
    };
    let next = match b["next_kind"].as_str() {
        Some(k) => format!(r#"<div class="side">next: {}</div>"#, kind(k)),
        None => String::new(),
    };
    let last = match b["last"].as_object() {
        Some(o) => format!(
            "last {} ago · {}",
            o.get("ago").and_then(Value::as_str).unwrap_or(""),
            kind(o.get("value").and_then(Value::as_str).unwrap_or(""))
        ),
        None => "two a week, alternating with and without soap".into(),
    };
    let html = format!(
        r#"<div class="next{}"><div class="t">🛁 Bath</div><div class="v">{}</div><div class="{}">{}</div>{next}<div class="last">{}</div></div>"#,
        if overdue { " overdue" } else { "" },
        esc(&when),
        if overdue { "warn" } else { "mut" },
        esc(&sub),
        esc(&last)
    );
    Next { title: "🛁 Bath", at: if never { None } else { d["at"].as_str().map(str::to_string) }, html }
}

/// "What's next", soonest (or most overdue) first; kinds without an estimate last.
fn whats_next(mut tiles: Vec<Next>) -> String {
    tiles.sort_by(|a, b| match (&a.at, &b.at) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => a.title.cmp(b.title),
    });
    let inner: String = tiles.iter().map(|t| t.html.as_str()).collect();
    format!(r#"<div class="card"><h2>What's next</h2><div class="nexts">{inner}</div></div>"#)
}

/// Totals for a day (or part of one), plus that day's weight if any.
fn totals_card(heading: &str, t: &en::Totals, weight: Option<(f64, Option<String>)>) -> String {
    let mut s = format!(r#"<div class="card">{heading}<div class="grid">"#);
    let vomit = if t.vomit > 0 { format!("🤢 ×{}", t.vomit) } else { String::new() };
    s += &stat("Feeds", &t.feeds.to_string(), &format!("🤱 {}′ at the breast", t.breast_min));
    s += &stat("🍼 Bottle", &format!("{:.0} ml", t.total_ml), &format!("{:.0} milk + {:.0} formula", t.breastmilk_ml, t.formula_ml));
    let split = format!("💧 {} wet · 💩 {} dirty{}", t.wet, t.dirty, if vomit.is_empty() { String::new() } else { format!(" · {vomit}") });
    s += &stat("Diapers", &t.diapers.to_string(), &split);
    if t.pump_ml > 0.0 {
        s += &stat("🥛 Pumped", &format!("{:.0} ml", t.pump_ml), "");
    }
    if let Some((w, vs)) = weight {
        s += &stat("⚖️ Weight", &format!("{w:.0} g"), vs.as_deref().unwrap_or(""));
    }
    s + "</div></div>"
}

/// A day's entries, newest first.
fn log_card(heading: &str, entries: &[en::Entry], loggers: &HashMap<i64, String>, clock: &crate::time::Clock, empty: &str) -> String {
    let mut s = format!(r#"<div class="card">{heading}<table>"#);
    for e in entries.iter().rev() {
        let flag = if e.attention_by != 0 {
            let (u, a) = (e.attention_by & en::BY_USER != 0, e.attention_by & BY_AGENT != 0);
            format!(r#" <span title="{}">{}</span>"#, esc(&attention_label(u, a)), attention_emoji(u, a))
        } else {
            String::new()
        };
        let who = loggers.get(&e.entry_id).map(|n| format!(r#"<div class="sub">{}</div>"#, esc(n))).unwrap_or_default();
        let edited = if e.version > 1 { format!(r#"<div class="sub">✎ v{}</div>"#, e.version) } else { String::new() };
        s += &format!(
            "<tr><td class=\"time\"><a href=\"/entry/{}\">{}</a>{flag}{who}{edited}</td><td>{}{}</td></tr>",
            e.entry_id,
            esc(&clock.fmt_time(e.at)),
            esc(&e.text),
            sub_line(&entry_summary(e))
        );
    }
    if entries.is_empty() {
        s += &format!(r#"<tr><td class="mut">{}</td></tr>"#, esc(empty));
    }
    s + "</table></div>"
}

/// The smaller second line under an entry's text (nothing when empty).
fn sub_line(s: &str) -> String {
    if s.is_empty() { String::new() } else { format!(r#"<div class="sub">{}</div>"#, esc(s)) }
}

fn attention_emoji(user: bool, agent: bool) -> &'static str {
    match (user, agent) {
        (true, true) => "🙋🤖",
        (true, false) => "🙋",
        (false, true) => "🤖",
        _ => "",
    }
}

fn attention_label(user: bool, agent: bool) -> String {
    match (user, agent) {
        (true, true) => "needs you and Claude".into(),
        (true, false) => "needs you".into(),
        (false, true) => "waiting for Claude".into(),
        _ => String::new(),
    }
}

fn weight_of(entries: &[en::Entry], baby: &en::Baby) -> Option<(f64, Option<String>)> {
    let e = entries.iter().filter(|e| e.num("weight_g").is_some()).last()?;
    let w = e.num("weight_g")?;
    let mut sub = vec![];
    if let Some((p, _)) = baby.weight_percentile(w, e.at) {
        sub.push(crate::growth::label(p));
    }
    match baby.change_percentile(w, e.at) {
        Some((c, _, label)) => sub.push(format!("{c:+.1}% vs birth ({label} for {})", baby.delivery.map(|d| d.describe()).unwrap_or(""))),
        None => {
            if let Some(b) = baby.birth_weight_g.filter(|b| *b > 0.0) {
                sub.push(format!("{:+.1}% vs birth", 100.0 * (w - b) / b));
            }
        }
    }
    Some((w, (!sub.is_empty()).then(|| sub.join(" · "))))
}

pub async fn status(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let user = match require(&app, &headers, "/") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let t = now();
    let clock = &app.clock;
    let res = app.db.with(|c| {
        Ok((|| {
            let s = en::status(c, clock, &app.baby(), t)?;
            let today = en::list(c, clock.day_start(t), t + 1, false)?;
            let loggers = en::loggers(c, &today)?;
            Ok::<_, String>((s, today, loggers))
        })())
    });
    let (s, today, loggers) = match res {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };

    let mut body = format!(
        r#"<div class="card"><h1>{}</h1><p class="mut">{}{}</p></div>"#,
        esc(&app.cfg.baby.name),
        s["age"].as_str().map(|a| format!("{a} old · ")).unwrap_or_default(),
        esc(s["now"].as_str().unwrap_or(""))
    );

    let feed_what = s["last_feed"]["value"]["fields"].as_object().map(feed_summary).unwrap_or_default();
    let stool = s["last_dirty_diaper"]["value"].as_str().map(|c| format!("{} {c}", stool_emoji(c))).unwrap_or_default();
    let feed_warning = s["feeding_guidance"]["warnings"].as_array().is_some_and(|w| !w.is_empty());
    // With a preparation time, the tile shows when to start (diaper first).
    let start = &s["next_feed_start"];
    let (feed_next, prep_line) = match (start["at"].as_str(), start["feed_at"].as_str()) {
        (Some(_), Some(f)) if start["prep"] != "0m" => (start, format!(r#"<div class="side">🧷 diaper first; feed ~{}</div>"#, esc(&f[11..16]))),
        _ => (&s["next_feed_estimate"], String::new()),
    };
    let mut feed = next_tile_extra("🍼 Feed", &s["last_feed"], &feed_what, feed_next,
            &format!("{prep_line}{}{}", amount_line(&s["feeding_guidance"]), sleep_line(&s["sleep_facing"])));
    if feed_warning {
        feed.html = feed.html.replacen(r#"<div class="next">"#, r#"<div class="next overdue">"#, 1);
    }
    let mut tiles = vec![
        feed,
        {
            let warning = s["wet_diaper_warning"]["text"].as_str().map(|t| format!(r#"<div class="side warn">⚠️ {}</div>"#, esc(t))).unwrap_or_default();
            let mut tile = next_tile_extra("💧 Wet diaper", &s["last_wet_diaper"], "", &s["next_wet_diaper_estimate"], &warning);
            if !warning.is_empty() {
                tile.html = tile.html.replacen(r#"<div class="next">"#, r#"<div class="next overdue">"#, 1);
            }
            tile
        },
        next_tile("💩 Dirty diaper", &s["last_dirty_diaper"], &stool, &s["next_dirty_diaper_estimate"]),
        bath_tile(clock, &s["bath"], t),
    ];
    tiles.extend(vitamins_tile(clock, &s["vitamins"], t));
    body += &whats_next(tiles);

    let yesterday = clock.day(clock.day_start(t) - 3600);
    let weight = s["last_weight"]["value"].as_f64().map(|w| {
        let mut sub = vec![];
        if let Some(l) = s["weight_percentile"]["label"].as_str() {
            sub.push(l.to_string());
        }
        let change = &s["weight_change_percentile"];
        if let (Some(c), Some(l)) = (change["percent_change"].as_f64(), change["label"].as_str()) {
            let mode = app.cfg.baby.delivery.map(|d| d.describe()).unwrap_or("");
            sub.push(format!("{c:+.1}% vs birth ({l} for {mode})"));
        } else if let Some(v) = s["weight_vs_birth"].as_str().and_then(|v| v.split('(').nth(1)) {
            sub.push(format!("{} vs birth", v.trim_end_matches(')')));
        }
        if s["birth_weight_regained"]["regained"] == true {
            sub.push("✅ birth weight regained".into());
        }
        (w, (!sub.is_empty()).then(|| sub.join(" · ")))
    });
    body += &totals_card(
        &format!(r#"<div class="head"><h2>Today</h2><a href="/day?date={yesterday}">← Yesterday</a></div>"#),
        &en::totals(&today),
        weight,
    );

    // Needs attention.
    if let Some(list) = s["needs_attention"].as_array().filter(|l| !l.is_empty()) {
        body += r#"<div class="card"><h2 class="warn">Needs attention</h2><table>"#;
        for e in list {
            let by: Vec<&str> = e["needs_attention"]["by"].as_array().map(|v| v.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            body += &format!(
                "<tr><td class=\"time\"><a href=\"/entry/{}\">{}</a> <span title=\"{}\">{}</span></td><td>{}{}</td></tr>",
                e["id"].as_i64().unwrap_or(0),
                esc(&e["at"].as_str().unwrap_or("")[5..]),
                esc(&attention_label(by.contains(&"user"), by.contains(&"agent"))),
                attention_emoji(by.contains(&"user"), by.contains(&"agent")),
                esc(e["text"].as_str().unwrap_or("")),
                sub_line(e["needs_attention"]["why"].as_str().unwrap_or(""))
            );
        }
        body += "</table>";
        if list.iter().any(|e| e["needs_attention"]["by"].as_array().is_some_and(|v| v.iter().any(|x| x == "agent"))) {
            body += r#"<p class="mut">🤖 waiting for Claude: ask Claude to “process the entries that need attention”. 🙋 needs you.</p>"#;
        }
        body += "</div>";
    }

    body += &log_card("<h2>Today's log</h2>", &today, &loggers, clock, "Nothing logged yet today.");
    html::render(Page { title: "Status · baby-mcp", body, nav: Some(&user.display_name), refresh: Some(60) })
}

/// A past (or the current) day: totals and the full log, with navigation.
pub async fn day(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let user = match require(&app, &headers, "/day") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let t = now();
    let clock = &app.clock;
    let today = clock.day_start(t);
    let date = q.get("date").cloned().unwrap_or_else(|| clock.day(today - 3600));
    let start = match clock.date_start(&date) {
        Ok(s) => s,
        Err(e) => return html::error_page(StatusCode::BAD_REQUEST, &e),
    };
    if start >= today {
        return Redirect::to("/").into_response();
    }
    let end = clock.next_day_start(start);
    let entries = match app.db.with(|c| Ok(en::list(c, start, end, false))) {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    let prev = clock.day(start - 3600);
    let next = if end >= today { r#"<a href="/">Today →</a>"#.to_string() } else { format!(r#"<a href="/day?date={}">{} →</a>"#, clock.day(end), &clock.day(end)[5..]) };
    let title = clock.zoned(start).strftime("%A %-d %B %Y").to_string();
    let age = app.baby().birth.map(|b| format!("day {} of life", (start - clock.day_start(b)) / 86400 + 1)).unwrap_or_default();
    let mut body = format!(
        r#"<div class="card"><div class="head"><a href="/day?date={prev}">← {}</a><h1>{}</h1>{next}</div><p class="mut">{}</p></div>"#,
        &prev[5..],
        esc(&title),
        esc(&age)
    );
    body += &totals_card("<h2>Totals</h2>", &en::totals(&entries), weight_of(&entries, &app.baby()));
    let loggers = app.db.with(|c| Ok(en::loggers(c, &entries))).ok().and_then(Result::ok).unwrap_or_default();
    body += &log_card("<h2>Log</h2>", &entries, &loggers, clock, "Nothing logged this day.");
    html::render(Page { title: &format!("{title} · baby-mcp"), body, nav: Some(&user.display_name), refresh: Some(60) })
}

// ------------------------------------------------------------- charts

/// Diagonal stripes of the wet (blue) and dirty (brown) colours, for diapers
/// that were both; used as `fill="url(#wet-dirty)"`.
const STRIPES: &str = r##"<defs><pattern id="wet-dirty" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="6" height="6" fill="#4a7fb5"/><rect width="3" height="6" fill="#a5531d"/></pattern></defs>"##;

/// Stacked bars per day. `projection` is the expected total for the last day
/// (today) by midnight; it is drawn as a dashed extension of that bar.
/// A round step for about `n` grid lines up to `max` (1, 2 or 5 × 10^k, at least 1).
fn grid_step(max: f64, n: f64) -> f64 {
    let raw = (max / n).max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    step.max(1.0)
}

fn bars(title: &str, days: &[DayStats], series: &[(&str, &str, fn(&DayStats) -> f64)], unit: &str, projection: Option<f64>) -> String {
    let (w, h, pad) = (640.0, 180.0, 40.0);
    let data_max = days
        .iter()
        .map(|d| series.iter().map(|(_, _, f)| f(d)).sum::<f64>())
        .chain(projection)
        .fold(0.0, f64::max)
        .max(1.0);
    // Round grid lines; the top line is the scale.
    let step = grid_step(data_max, 5.0);
    let max = (data_max / step).ceil() * step;
    let bw = (w - pad) / days.len().max(1) as f64;
    let mut svg = format!(r#"<svg viewBox="0 0 {w} {}" width="100%" role="img" aria-label="{}">"#, h + 20.0, esc(title));
    svg += STRIPES;
    let mut v = 0.0;
    while v <= max + 1e-9 {
        let y = h - v / max * (h - 10.0);
        let label = if v + step > max + 1e-9 && !unit.is_empty() { format!("{v:.0} {unit}") } else { format!("{v:.0}") };
        svg += &format!(
            r#"<line x1="{pad}" x2="{w}" y1="{y:.1}" y2="{y:.1}" stroke="currentColor" stroke-opacity="{}"/><text x="{:.1}" y="{:.1}" text-anchor="end">{label}</text>"#,
            if v == 0.0 { ".3" } else { ".1" },
            pad - 4.0,
            y + 4.0
        );
        v += step;
    }
    for (i, d) in days.iter().enumerate() {
        let x = pad + i as f64 * bw + bw * 0.15;
        let mut y = h;
        for (_, color, f) in series {
            let v = f(d);
            let bh = v / max * (h - 10.0);
            y -= bh;
            svg += &format!(r#"<rect x="{x:.1}" y="{y:.1}" width="{:.1}" height="{bh:.1}" fill="{color}"><title>{} {v:.0}</title></rect>"#, bw * 0.7, d.date);
        }
        if let Some(p) = projection.filter(|p| i + 1 == days.len() && *p > 0.0) {
            let total: f64 = series.iter().map(|(_, _, f)| f(d)).sum();
            if p > total {
                let top = h - p / max * (h - 10.0);
                svg += &format!(
                    r#"<rect x="{x:.1}" y="{top:.1}" width="{:.1}" height="{:.1}" fill="none" stroke="currentColor" stroke-opacity=".55" stroke-dasharray="3 3"><title>expected by midnight at the last 24 hours' rate: ~{p:.0}</title></rect><text x="{:.1}" y="{:.1}" text-anchor="middle">~{p:.0}</text>"#,
                    bw * 0.7, y - top, x + bw * 0.35, (top - 3.0).max(9.0)
                );
            }
        }
        if i % 2 == 0 || days.len() <= 8 {
            svg += &format!(r#"<text x="{:.1}" y="{}" text-anchor="middle">{}</text>"#, x + bw * 0.35, h + 14.0, &d.date[5..]);
        }
    }
    svg += "</svg>";
    let legend: String = series
        .iter()
        .map(|(n, c, _)| {
            format!(
                r#"<svg class="swatch" width="11" height="11">{STRIPES}<rect width="11" height="11" fill="{c}"/></svg> {} "#,
                esc(n)
            )
        })
        .collect::<String>()
        + if projection.is_some() { " · dashed box: expected by midnight at the last 24 hours' rate" } else { "" };
    format!(r#"<div class="card"><h2>{}</h2>{svg}<p class="mut">{legend}</p></div>"#, esc(title))
}

/// Today's value so far plus the last 24 hours' total scaled to the part of
/// the day still to come (`rest`, 0–1).
fn expected_by_midnight(so_far: f64, last_24h: f64, rest: f64) -> f64 {
    so_far + last_24h * rest.clamp(0.0, 1.0)
}

/// Usual newborn temperature range shown on the chart (°C).
const TEMP_RANGE: (f64, f64) = (36.5, 37.5);

/// Temperature readings over time, with the usual range as a band.
fn temp_chart(clock: &crate::time::Clock, temps: &[(i64, f64)], from: i64, to: i64) -> String {
    if temps.is_empty() {
        return String::new();
    }
    let (lo, hi) = temps.iter().fold((TEMP_RANGE.0 - 0.5, TEMP_RANGE.1 + 0.5), |(a, b), (_, v)| (a.min(*v - 0.2), b.max(*v + 0.2)));
    let (lo, hi) = ((lo * 2.0).floor() / 2.0, (hi * 2.0).ceil() / 2.0);
    let (w, h, l, b) = (640.0, 200.0, 44.0, 20.0);
    let x = |t: i64| l + (t - from) as f64 / (to - from).max(1) as f64 * (w - l - 8.0);
    let y = |v: f64| 8.0 + (hi - v) / (hi - lo) * (h - b - 8.0);
    let mut svg = format!(r#"<svg viewBox="0 0 {w} {h}" width="100%" role="img" aria-label="Temperature">"#);
    svg += &format!(
        r##"<rect x="{l}" y="{:.1}" width="{:.1}" height="{:.1}" fill="#2f6f5e" fill-opacity=".12"/>"##,
        y(TEMP_RANGE.1), w - l - 8.0, y(TEMP_RANGE.0) - y(TEMP_RANGE.1)
    );
    let mut v = lo;
    while v <= hi + 1e-9 {
        svg += &format!(r##"<line x1="{l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke="currentColor" stroke-opacity=".08"/><text x="2" y="{:.1}">{v:.1}°</text>"##, w - 8.0, y(v), y(v), y(v) + 4.0);
        v += 0.5;
    }
    // A label per day, at local midnight.
    let mut d = clock.day_start(from);
    while d <= to {
        if d >= from {
            let label = clock.zoned(d).strftime("%-d/%-m").to_string();
            svg += &format!(r##"<line x1="{:.1}" x2="{:.1}" y1="8" y2="{}" stroke="currentColor" stroke-opacity=".06"/><text x="{:.1}" y="{}" text-anchor="middle">{label}</text>"##, x(d), x(d), h - b, x(d), h - 4.0);
        }
        d = clock.next_day_start(d);
    }
    let line: Vec<String> = temps.iter().map(|(t, v)| format!("{:.1},{:.1}", x(*t), y(*v))).collect();
    svg += &format!(r##"<polyline points="{}" fill="none" stroke="#6b6b70" stroke-opacity=".5" stroke-width="1.5"/>"##, line.join(" "));
    for (t, v) in temps {
        let out = *v < TEMP_RANGE.0 || *v > TEMP_RANGE.1;
        svg += &format!(
            r##"<circle cx="{:.1}" cy="{:.1}" r="3.5" fill="{}"><title>{} {v:.1} °C</title></circle>"##,
            x(*t), y(*v), if out { "#c0503e" } else { "#2f6f5e" }, clock.fmt(*t)
        );
    }
    svg += "</svg>";
    format!(
        r#"<div class="card"><h2>🌡️ Temperature</h2>{svg}<p class="mut">Band: the usual range for a newborn, {:.1}–{:.1} °C; readings outside it in red. A reference, not a diagnosis.</p></div>"#,
        TEMP_RANGE.0, TEMP_RANGE.1
    )
}

/// Percentage weight change since birth over the first month, against the
/// percentiles for newborns with the same delivery mode.
fn change_chart(baby: &en::Baby, weights: &[(i64, f64)], now: i64) -> String {
    let (Some(birth), Some(bw), Some(d), Some(table)) = (baby.birth, baby.birth_weight_g, baby.delivery, baby.change.as_ref()) else {
        return String::new();
    };
    let span = table.max_day(d);
    let age = (now - birth) as f64 / 86400.0;
    if age > span + 7.0 {
        return String::new(); // past the first month: only the WHO chart applies
    }
    let mut pts: Vec<(f64, f64)> = vec![(0.0, 0.0)];
    pts.extend(weights.iter().filter(|(t, _)| *t > birth).map(|(t, g)| ((t - birth) as f64 / 86400.0, 100.0 * (g - bw) / bw)).filter(|(x, _)| *x <= span));
    let show = (age.ceil() + 3.0).clamp(14.0, span);
    let days: Vec<f64> = (0..=(show * 4.0) as i64).map(|i| i as f64 / 4.0).collect();
    let curves: Vec<Vec<f64>> = days.iter().filter_map(|x| table.at(d, *x)).collect();
    let col = |k: usize| -> Vec<(f64, f64)> { days.iter().zip(&curves).map(|(x, v)| (*x, v[k])).collect() };
    let n = table.percentiles.len();
    let (lo, hi) = curves.iter().flatten().chain(pts.iter().map(|p| &p.1)).fold((f64::MAX, f64::MIN), |(a, b), &y| (a.min(y), b.max(y)));
    let (lo, hi) = ((lo - 1.0).floor(), (hi + 1.0).ceil());
    let (w, h, l, b) = (640.0, 220.0, 40.0, 20.0);
    let x = |d: f64| l + d / show * (w - l - 8.0);
    let y = |v: f64| 8.0 + (hi - v) / (hi - lo) * (h - b - 8.0);
    let line = |c: &[(f64, f64)]| c.iter().map(|(a, v)| format!("{:.1},{:.1}", x(*a), y(*v))).collect::<Vec<_>>().join(" ");
    let band = |lower: &[(f64, f64)], upper: &[(f64, f64)]| {
        let mut v = lower.to_vec();
        v.extend(upper.iter().rev());
        line(&v)
    };
    let mut svg = format!(r#"<svg viewBox="0 0 {w} {h}" width="100%" role="img" aria-label="Weight change since birth">"#);
    let step = if hi - lo > 20.0 { 5.0 } else { 2.0 };
    let mut v = (lo / step).ceil() * step;
    while v <= hi {
        let strong = if v == 0.0 { ".35" } else { ".08" };
        svg += &format!(r##"<line x1="{l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke="currentColor" stroke-opacity="{strong}"/><text x="2" y="{:.1}">{v:+.0}%</text>"##, w - 8.0, y(v), y(v), y(v) + 4.0);
        v += step;
    }
    let mut dd = 0.0;
    while dd <= show {
        let anchor = if x(dd) > w - 30.0 { "end" } else if dd == 0.0 { "start" } else { "middle" };
        svg += &format!(r#"<text x="{:.1}" y="{}" text-anchor="{anchor}">{}</text>"#, x(dd), h - 4.0, if dd == 0.0 { "birth".into() } else { format!("day {dd:.0}") });
        dd += 7.0;
    }
    // Bands: outermost pair, then the next pair inwards; the median dashed.
    if n >= 7 {
        svg += &format!(r##"<polygon points="{}" fill="#4a7fb5" fill-opacity=".10"/>"##, band(&col(0), &col(n - 1)));
        svg += &format!(r##"<polygon points="{}" fill="#4a7fb5" fill-opacity=".14"/>"##, band(&col(2), &col(n - 3)));
    }
    let mid = table.percentiles.iter().position(|p| *p == 50.0).unwrap_or(n / 2);
    svg += &format!(r##"<polyline points="{}" fill="none" stroke="#4a7fb5" stroke-opacity=".7" stroke-dasharray="4 4"/>"##, line(&col(mid)));
    svg += &format!(r##"<polyline points="{}" fill="none" stroke="#c0503e" stroke-width="2"/>"##, line(&pts));
    for (a, v) in pts.iter().skip(1) {
        let p = table.percentile(d, *a, *v).map(|p| format!(" · {}", table.label(p))).unwrap_or_default();
        svg += &format!(r##"<circle cx="{:.1}" cy="{:.1}" r="3.5" fill="#c0503e"><title>day {a:.1}: {v:+.1}%{p}</title></circle>"##, x(*a), y(*v));
    }
    svg += "</svg>";
    let lbl = |k: usize| format!("P{:.0}", table.percentiles[k]);
    format!(
        r#"<div class="card"><h2>📉 Weight change since birth</h2>{svg}<p class="mut">Red: her weighings. Bands: {}–{} and {}–{}, dashed: median, for newborns after {} (Paul et al., Pediatrics 2016, all feeding types; read from the published figure, so approximate, especially in the first two days). A reference, not a diagnosis.</p></div>"#,
        lbl(0), lbl(n - 1), lbl(2), lbl(n - 3), d.describe()
    )
}

/// Weight against age since birth, over the WHO weight-for-age bands
/// (P3–P97, P15–P85, median) when the sex is configured.
fn growth_chart(baby: &en::Baby, weights: &[(i64, f64)], now: i64) -> String {
    use crate::growth::age_days;
    let Some(birth) = baby.birth else { return String::new() };
    // Weighings logged before the birth time (e.g. a birth weight entered under
    // the day report's time) are left out; the birth weight comes from the config.
    let mut pts: Vec<(f64, f64)> = weights.iter().filter(|(t, _)| *t >= birth).map(|(t, g)| ((t - birth) as f64 / 86400.0, *g)).collect();
    if let Some(bw) = baby.birth_weight_g {
        pts.insert(0, (0.0, bw));
    }
    if pts.is_empty() {
        return String::new();
    }
    let span = (age_days(birth, now) + 2).max(14);
    let curve = |p: f64| -> Vec<(f64, f64)> {
        match (baby.sex, &baby.growth) {
            (Some(sex), Some(t)) => (0..=span).filter_map(|d| t.grams_at_percentile(sex, d, p).map(|g| (d as f64, g))).collect(),
            _ => vec![],
        }
    };
    let (p3, p15, p50, p85, p97) = (curve(3.0), curve(15.0), curve(50.0), curve(85.0), curve(97.0));
    let all_y = pts.iter().map(|p| p.1).chain(p3.iter().chain(&p97).map(|p| p.1));
    let (lo, hi) = all_y.fold((f64::MAX, f64::MIN), |(a, b), y| (a.min(y), b.max(y)));
    let (lo, hi) = (((lo - 100.0) / 500.0).floor() * 500.0, ((hi + 100.0) / 500.0).ceil() * 500.0);
    let (w, h, l, b) = (640.0, 220.0, 44.0, 20.0);
    let x = |d: f64| l + d / span as f64 * (w - l - 8.0);
    let y = |g: f64| 8.0 + (hi - g) / (hi - lo) * (h - b - 8.0);
    let line = |c: &[(f64, f64)]| c.iter().map(|(d, g)| format!("{:.1},{:.1}", x(*d), y(*g))).collect::<Vec<_>>().join(" ");
    let band = |lower: &[(f64, f64)], upper: &[(f64, f64)]| {
        let mut v: Vec<(f64, f64)> = lower.to_vec();
        v.extend(upper.iter().rev());
        line(&v)
    };
    let mut svg = format!(r#"<svg viewBox="0 0 {w} {h}" width="100%" role="img" aria-label="Weight against the WHO growth standard">"#);
    // Grid: every 500 g (1 kg once the range is wide), every week for the first
    // ten weeks, then every month.
    let step = if hi - lo > 4000.0 { 1000.0 } else { 500.0 };
    let mut g = (lo / step).ceil() * step;
    while g <= hi {
        let label = if step >= 1000.0 { format!("{:.0} kg", g / 1000.0) } else { format!("{:.1} kg", g / 1000.0) };
        svg += &format!(r##"<line x1="{l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke="currentColor" stroke-opacity=".08"/><text x="2" y="{:.1}">{label}</text>"##, w - 8.0, y(g), y(g), y(g) + 4.0);
        g += step;
    }
    let ticks: Vec<(f64, String)> = if span <= 70 {
        (0..=span / 7).map(|k| ((k * 7) as f64, if k == 0 { "birth".into() } else { format!("{k}w") })).collect()
    } else {
        let month = 365.25 / 12.0;
        let every = if span as f64 > 15.0 * month { 2 } else { 1 };
        (0..=(span as f64 / month) as i64).step_by(every).map(|m| (m as f64 * month, if m == 0 { "birth".into() } else { format!("{m}m") })).collect()
    };
    for (d, label) in ticks {
        let anchor = if d == 0.0 { "start" } else { "middle" };
        svg += &format!(r#"<text x="{:.1}" y="{}" text-anchor="{anchor}">{label}</text>"#, x(d), h - 4.0);
    }
    if !p50.is_empty() {
        svg += &format!(r##"<polygon points="{}" fill="#2f6f5e" fill-opacity=".10"/>"##, band(&p3, &p97));
        svg += &format!(r##"<polygon points="{}" fill="#2f6f5e" fill-opacity=".14"/>"##, band(&p15, &p85));
        svg += &format!(r##"<polyline points="{}" fill="none" stroke="#2f6f5e" stroke-opacity=".6" stroke-dasharray="4 4"/>"##, line(&p50));
    }
    svg += &format!(r##"<polyline points="{}" fill="none" stroke="#c0503e" stroke-width="2"/>"##, line(&pts));
    for (d, g) in &pts {
        let pct = baby.weight_percentile(*g, birth + (*d * 86400.0) as i64).map(|(p, _)| format!(" · {}", crate::growth::label(p))).unwrap_or_default();
        svg += &format!(r##"<circle cx="{:.1}" cy="{:.1}" r="3.5" fill="#c0503e"><title>day {:.0}: {g:.0} g{pct}</title></circle>"##, x(*d), y(*g), d.floor());
    }
    svg += "</svg>";
    let legend = if p50.is_empty() {
        r#"To compare with the WHO growth standard, set <code>sex</code> in the config and put the WHO weight-for-age table on the server (see data/README.md)."#.to_string()
    } else {
        r#"Red: weighings. Bands: WHO weight-for-age P3–P97 and P15–P85; dashed: median. A reference, not a diagnosis."#.to_string()
    };
    format!(r#"<div class="card"><h2>⚖️ Growth</h2>{svg}<p class="mut">{legend}</p></div>"#)
}

/// Per full day since birth: bottle intake per kg (top) and weight gained per
/// kg (bottom), with the fitted baseline when the data allow one.
fn per_kg_chart(days: &[crate::feeding::PerKgDay]) -> String {
    use crate::feeding::{FIT_FROM_DAY, fit};
    if days.iter().all(|d| d.intake.is_none() && d.growth.is_none()) {
        return String::new();
    }
    let fitted = fit(days);
    let (w, l, r) = (640.0, 40.0, 8.0);
    let (top_h, gap, bot_h, axis) = (130.0, 26.0, 90.0, 18.0);
    let h = top_h + gap + bot_h + axis;
    let bw = (w - l - r) / days.len() as f64;
    let cx = |i: usize| l + (i as f64 + 0.5) * bw;
    let nice = |v: f64, step: f64| ((v / step).ceil() * step).max(step);

    // Top: intake, ml/kg/day.
    let imax = nice(days.iter().filter_map(|d| d.intake).chain(fitted.as_ref().ok().map(|f| f.baseline)).fold(0.0, f64::max) * 1.05, 50.0);
    let yi = |v: f64| 8.0 + (imax - v) / imax * (top_h - 8.0);
    // Bottom: growth, g/kg/day, around zero.
    let (glo, ghi) = days.iter().filter_map(|d| d.growth).fold((0.0f64, 0.0f64), |(a, b), g| (a.min(g), b.max(g)));
    let (glo, ghi) = (-nice(-glo, 10.0), nice(ghi, 10.0));
    let y0 = top_h + gap;
    let yg = |v: f64| y0 + (ghi - v) / (ghi - glo) * bot_h;

    let mut svg = format!(r#"<svg viewBox="0 0 {w} {h}" width="100%" role="img" aria-label="Intake and growth per kg">"#);
    let mut v = 0.0;
    while v <= imax {
        svg += &format!(r#"<line x1="{l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke="currentColor" stroke-opacity=".08"/><text x="2" y="{:.1}">{v:.0}</text>"#, w - r, yi(v), yi(v), yi(v) + 4.0);
        v += if imax > 200.0 { 100.0 } else { 50.0 };
    }
    svg += &format!(r#"<text x="{}" y="{:.1}" opacity=".7">bottle ml per kg per day</text>"#, l + 4.0, 20.0);
    let mut v = glo;
    while v <= ghi + 1e-9 {
        let strong = if v == 0.0 { ".35" } else { ".08" };
        svg += &format!(r#"<line x1="{l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke="currentColor" stroke-opacity="{strong}"/><text x="2" y="{:.1}">{}</text>"#, w - r, yg(v), yg(v), yg(v) + 4.0, if v == 0.0 { "0".into() } else { format!("{v:+.0}") });
        v += 10.0;
    }
    svg += &format!(r#"<text x="{l}" y="{:.1}" opacity=".7">grams gained per kg per day</text>"#, y0 - 6.0);
    // The first week: intake is still building up.
    if let Some(i) = days.iter().position(|d| d.day_of_life >= FIT_FROM_DAY).or(Some(days.len())).filter(|i| *i > 0) {
        svg += &format!(r#"<rect x="{l}" y="0" width="{:.1}" height="{top_h}" fill="currentColor" fill-opacity=".04"/><text x="{:.1}" y="{:.1}" text-anchor="middle" opacity=".55">first week</text>"#, i as f64 * bw, l + i as f64 * bw / 2.0, top_h - 4.0);
    }
    if let Ok(f) = &fitted {
        svg += &format!(r##"<line x1="{l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke="#2f6f5e" stroke-opacity=".7" stroke-dasharray="5 4"><title>baseline ≈ {:.0} ml/kg/day</title></line>"##, w - r, yi(f.baseline), yi(f.baseline), f.baseline);
    }
    // Intake line with dots; hollow dots on days with (unmeasured) breastfeeding.
    let pts: Vec<String> = days.iter().enumerate().filter_map(|(i, d)| d.intake.map(|v| format!("{:.1},{:.1}", cx(i), yi(v)))).collect();
    svg += &format!(r##"<polyline points="{}" fill="none" stroke="#2f6f5e" stroke-width="2"/>"##, pts.join(" "));
    for (i, d) in days.iter().enumerate() {
        if let Some(v) = d.intake {
            let class = if d.breast { r#" class="hollow""# } else { r##" fill="#2f6f5e""## };
            let note = if d.breast { " (also at the breast, not counted)" } else { "" };
            svg += &format!(r##"<circle cx="{:.1}" cy="{:.1}" r="3.5"{class} stroke="#2f6f5e" stroke-width="1.5"><title>{} (day {}): {v:.0} ml/kg{note}</title></circle>"##, cx(i), yi(v), d.date, d.day_of_life);
        }
        if let Some(g) = d.growth {
            let (a, b) = (yg(g.max(0.0)), yg(g.min(0.0)));
            let color = if g >= 0.0 { "#2f6f5e" } else { "#c0503e" };
            svg += &format!(r#"<rect x="{:.1}" y="{a:.1}" width="{:.1}" height="{:.1}" fill="{color}" fill-opacity=".8"><title>{} (day {}): {g:+.1} g/kg</title></rect>"#, cx(i) - bw * 0.3, bw * 0.6, (b - a).max(0.5), d.date, d.day_of_life);
        }
        if i % 2 == 0 || days.len() <= 8 {
            svg += &format!(r#"<text x="{:.1}" y="{}" text-anchor="middle">{}</text>"#, cx(i), h - 4.0, &d.date[5..]);
        }
    }
    svg += "</svg>";
    let estimate = match &fitted {
        Ok(f) => format!(
            "Baseline (dashed): ~{:.0} ml/kg a day at zero growth, plus ~{:.1} ml per gram gained ({} days).",
            f.baseline, f.cost, f.days
        ),
        Err(why) => format!("No baseline yet: {}.", esc(why)),
    };
    format!(
        r#"<div class="card"><h2>⚖️🍼 Per kg</h2>{svg}<p class="mut">Per full day; weights interpolated between weighings. Hollow: also at the breast (not measured). {estimate}</p></div>"#
    )
}

pub async fn charts(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let user = match require(&app, &headers, "/charts") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let t = now();
    let from = app.clock.day_start(t - 13 * 86400);
    let days = match app.db.with(|c| Ok(en::stats(c, &app.clock, &app.baby(), from, t + 1))) {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    let baby = app.baby();
    let weights: Vec<(i64, f64)> = match app.db.with(|c| Ok(en::list(c, baby.birth.unwrap_or(0), t + 1, false))) {
        Ok(Ok(es)) => es.iter().filter_map(|e| e.num("weight_g").map(|g| (e.at, g))).collect(),
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    // The last 14 days, but not before birth.
    let temp_from = app.clock.day_start(t - 13 * 86400).max(baby.birth.map(|b| app.clock.day_start(b)).unwrap_or(i64::MIN));
    let temps: Vec<(i64, f64)> = match app.db.with(|c| Ok(en::list(c, temp_from, t + 1, false))) {
        Ok(Ok(es)) => es.iter().filter_map(|e| e.num("temp_c").map(|v| (e.at, v))).collect(),
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    // Today's expected totals by midnight: so far today + the last 24 hours'
    // totals scaled to the rest of the day.
    let last24 = match app.db.with(|c| Ok(en::list(c, t - 86400, t + 1, false))) {
        Ok(Ok(es)) => en::totals(&es),
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    let rest = (app.clock.next_day_start(t) - t) as f64 / 86400.0;
    let today = days.last().filter(|d| d.date == app.clock.day(t)).map(|d| d.totals.clone());
    let project = |m: fn(&en::Totals) -> f64| today.as_ref().map(|td| expected_by_midnight(m(td), m(&last24), rest));
    let since_birth = match app.db.with(|c| Ok(en::list(c, baby.birth.unwrap_or(t), t + 1, false))) {
        Ok(Ok(es)) => es,
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    let per_kg = match (baby.birth, baby.birth_weight_g) {
        (Some(b), Some(bw)) => crate::feeding::per_kg_days(&app.clock, b, bw, &since_birth, t, 28),
        _ => vec![],
    };
    let mut body = String::new();
    body += &change_chart(&baby, &weights, t);
    body += &growth_chart(&baby, &weights, t);
    body += &per_kg_chart(&per_kg);
    body += &temp_chart(&app.clock, &temps, temp_from, t);
    body += &bars("Bottle per day", &days, &[
        ("breast milk", "#2f6f5e", |d| d.totals.breastmilk_ml),
        ("formula", "#c08a3e", |d| d.totals.formula_ml),
    ], "ml", project(|t| t.total_ml));
    body += &bars("Breast per day", &days, &[
        ("left", "#7b5ea7", |d| d.totals.breast_left_min as f64),
        ("right", "#d97a9c", |d| d.totals.breast_right_min as f64),
    ], "min", project(|t| t.breast_min as f64));
    body += &bars("Diapers per day", &days, &[
        ("wet", "#4a7fb5", |d| d.totals.wet_only as f64),
        ("wet + dirty", "url(#wet-dirty)", |d| d.totals.wet_and_dirty as f64),
        ("dirty", "#a5531d", |d| d.totals.dirty_only as f64),
    ], "", project(|t| t.diapers as f64));
    body += &bars("Feeds per day", &days, &[("feeds", "#6b6b70", |d| d.totals.feeds as f64)], "", project(|t| t.feeds as f64));
    html::render(Page { title: "Charts · baby-mcp", body, nav: Some(&user.display_name), refresh: Some(60) })
}

// ------------------------------------------------------------- capture

pub async fn add_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let user = match require(&app, &headers, "/add") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let body = format!(
        r#"<div class="card"><h1>Add</h1>
<form method="post" action="/add">
<p><textarea id="text" name="text" rows="4" required placeholder="What happened? e.g. 60 ml pumped milk and a wet diaper. Mention a time if it wasn't just now."></textarea></p>
<div class="row"><button class="primary">Save</button></div>
</form>
<p class="mut">Saved as you write it. Claude fills in the details later, including the time if you mention one; otherwise it is logged as now.</p></div>"#
    );
    html::render(Page { title: "Add · baby-mcp", body, nav: Some(&user.display_name), refresh: None })
}

#[derive(Deserialize)]
pub struct AddForm {
    #[serde(default)]
    text: String,
}

pub async fn add(State(app): State<Arc<App>>, headers: HeaderMap, Form(f): Form<AddForm>) -> Response {
    if !same_origin(&app, &headers) {
        return html::error_page(StatusCode::FORBIDDEN, "Cross-site request refused.");
    }
    let user = match require(&app, &headers, "/add") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let text = f.text.trim().to_string();
    if text.is_empty() {
        return Redirect::to("/add").into_response();
    }
    let t = now();
    // Always free text: an agent extracts the fields (and the time, if mentioned).
    let n = NewEntry {
        at: t - t % 60,
        duration_s: None,
        text,
        fields: Map::new(),
        attention_by: BY_AGENT,
        attention_why: Some("fields not extracted yet".to_string()),
    };
    match app.db.with(|c| Ok(en::insert(c, user.id, &n, t))) {
        Ok(Ok(_)) => Redirect::to("/").into_response(),
        Ok(Err(e)) => html::error_page(StatusCode::BAD_REQUEST, &e),
        Err(e) => html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    }
}

// ------------------------------------------------------------- home-screen app

/// The web app manifest (public: browsers fetch it without the session cookie).
pub async fn manifest(State(app): State<Arc<App>>) -> Response {
    let body = serde_json::json!({
        "name": app.cfg.baby.name,
        "short_name": app.cfg.baby.name,
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#f6f5f2",
        "theme_color": "#2f6f5e",
        "icons": [
            {"src": "/icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any"},
            {"src": "/icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any"},
            {"src": "/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable"},
        ],
    });
    ([(axum::http::header::CONTENT_TYPE, "application/manifest+json"), (axum::http::header::CACHE_CONTROL, "max-age=3600")], body.to_string()).into_response()
}

fn png(bytes: &'static [u8]) -> Response {
    ([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "max-age=86400")], bytes).into_response()
}

pub async fn icon_180() -> Response {
    png(include_bytes!("../static/icon-180.png"))
}

pub async fn icon_192() -> Response {
    png(include_bytes!("../static/icon-192.png"))
}

pub async fn elephant() -> Response {
    png(include_bytes!("../static/elephant-64.png"))
}

/// For Android's circular crop: the elephant within the inner 80%.
pub async fn icon_maskable() -> Response {
    png(include_bytes!("../static/icon-maskable-512.png"))
}

pub async fn icon_512() -> Response {
    png(include_bytes!("../static/icon-512.png"))
}

// ------------------------------------------------------------- one entry and its history

/// A field value for people: units, yes/no, words for choices.
fn field_value(name: &str, v: &Value) -> String {
    let unit = crate::registry::get(name).map(|f| f.unit).unwrap_or("");
    match v {
        Value::Bool(b) => (if *b { "yes" } else { "no" }).into(),
        Value::Null => "none".into(),
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            let num = if x.fract() == 0.0 { format!("{x:.0}") } else { format!("{x}") };
            if unit.is_empty() { num } else { format!("{num} {unit}") }
        }
        Value::String(s) => s.replace('_', " "),
        other => other.to_string(),
    }
}

/// What changed from one version to the next, as short lines.
fn changes(clock: &crate::time::Clock, old: &en::Entry, new: &en::Entry) -> Vec<String> {
    let mut out = vec![];
    if old.at != new.at {
        out.push(format!("time: {} → {}", clock.fmt(old.at), clock.fmt(new.at)));
    }
    if old.duration_s != new.duration_s {
        let d = |x: Option<i64>| x.map(crate::time::fmt_dur).unwrap_or("none".into());
        out.push(format!("duration: {} → {}", d(old.duration_s), d(new.duration_s)));
    }
    let order = |k: &str| crate::registry::FIELDS.iter().position(|f| f.name == k).unwrap_or(usize::MAX);
    let mut keys: Vec<&String> = old.fields.keys().chain(new.fields.keys()).collect();
    keys.sort_by_key(|k| (order(k), k.to_string()));
    keys.dedup();
    for k in keys {
        match (old.fields.get(k), new.fields.get(k)) {
            (None, Some(v)) => out.push(format!("+ {k}: {}", field_value(k, v))),
            (Some(v), None) => out.push(format!("− {k} (was {})", field_value(k, v))),
            (Some(a), Some(b)) if a != b => out.push(format!("{k}: {} → {}", field_value(k, a), field_value(k, b))),
            _ => {}
        }
    }
    if old.text != new.text {
        let was: String = old.text.chars().take(200).collect();
        out.push(format!("text changed; was: “{was}{}”", if old.text.chars().count() > 200 { "…" } else { "" }));
    }
    if (old.attention_by, &old.attention_why) != (new.attention_by, &new.attention_why) {
        match (new.attention_by, &new.attention_why) {
            (0, _) => out.push(format!("attention cleared{}", old.attention_why.as_ref().map(|w| format!(" (was: {w})")).unwrap_or_default())),
            (by, why) => {
                let (u, a) = (by & en::BY_USER != 0, by & BY_AGENT != 0);
                out.push(format!("{} {}{}", attention_emoji(u, a), attention_label(u, a), why.as_ref().map(|w| format!(": {w}")).unwrap_or_default()));
            }
        }
    }
    if old.deleted != new.deleted {
        out.push((if new.deleted { "deleted" } else { "restored" }).into());
    }
    out
}

/// One line of an entry's history, coloured: added green, removed red, a
/// change as the old value struck through and the new one green.
fn history_line(l: &str) -> String {
    if l.starts_with("+ ") || l == "restored" || l == "created" {
        return format!(r#"<div><span class="add">{}</span></div>"#, esc(l));
    }
    if l.starts_with("− ") || l == "deleted" {
        return format!(r#"<div><span class="del">{}</span></div>"#, esc(l));
    }
    if l.starts_with("text changed") || l.starts_with("attention cleared") {
        return format!(r#"<div class="note">{}</div>"#, esc(l));
    }
    if let Some((key, rest)) = l.split_once(": ") {
        if let Some((old, new)) = rest.split_once(" → ") {
            return format!(r#"<div>{}: <s class="del">{}</s> → <span class="add">{}</span></div>"#, esc(key), esc(old), esc(new));
        }
    }
    if l.starts_with('🙋') || l.starts_with('🤖') || l.contains("waiting for") {
        return format!(r#"<div class="att">{}</div>"#, esc(l));
    }
    format!("<div>{}</div>", esc(l))
}

pub async fn entry_page(State(app): State<Arc<App>>, headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<i64>) -> Response {
    let user = match require(&app, &headers, &format!("/entry/{id}")) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let clock = &app.clock;
    let (versions, names) = match app.db.with(|c| Ok((en::history(c, id), en::user_names(c)))) {
        Ok((Ok(v), Ok(n))) => (v, n),
        Ok((Err(_), _)) => return html::error_page(StatusCode::NOT_FOUND, "No such entry."),
        Ok((_, Err(e))) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    let cur = versions.last().unwrap();
    let day = clock.day(cur.at);
    let who = |u: i64| names.get(&u).cloned().unwrap_or_else(|| format!("user {u}"));
    let mut body = format!(
        r#"<div class="card"><h1>{} {}</h1><p class="mut"><a href="/day?date={day}">📅 {day}</a> · entry {id}{}</p><p>{}</p>{}"#,
        if cur.deleted { "🗑️" } else { "📝" },
        esc(&clock.fmt(cur.at)),
        if cur.deleted { " · deleted" } else { "" },
        esc(&cur.text),
        sub_line(&entry_summary(cur))
    );
    if cur.attention_by != 0 {
        let (u, a) = (cur.attention_by & en::BY_USER != 0, cur.attention_by & BY_AGENT != 0);
        body += &format!(r#"<p class="warn">{} {}{}</p>"#, attention_emoji(u, a), esc(&attention_label(u, a)),
            cur.attention_why.as_ref().map(|w| format!(": {}", esc(w))).unwrap_or_default());
    }
    if !cur.fields.is_empty() {
        body += "<table>";
        let order = |k: &str| crate::registry::FIELDS.iter().position(|f| f.name == k).unwrap_or(usize::MAX);
        let mut keys: Vec<&String> = cur.fields.keys().collect();
        keys.sort_by_key(|k| order(k));
        for k in keys {
            body += &format!(r#"<tr><td class="time">{}</td><td>{}</td></tr>"#, esc(k), esc(&field_value(k, &cur.fields[k])));
        }
        body += "</table>";
    }
    body += "</div>";
    body += &format!(r#"<div class="card"><h2>History · {} version{}</h2><table>"#, versions.len(), if versions.len() == 1 { "" } else { "s" });
    for (i, v) in versions.iter().enumerate().rev() {
        let lines = match i {
            0 => {
                let mut l = vec!["created".to_string()];
                if v.attention_by != 0 {
                    let (u, a) = (v.attention_by & en::BY_USER != 0, v.attention_by & BY_AGENT != 0);
                    l.push(format!("{} {}{}", attention_emoji(u, a), attention_label(u, a), v.attention_why.as_ref().map(|w| format!(": {w}")).unwrap_or_default()));
                }
                l
            }
            _ => changes(clock, &versions[i - 1], v),
        };
        let items: String = lines.iter().map(|l| history_line(l)).collect();
        body += &format!(
            r#"<tr><td class="time">v{}<div class="sub">{}</div></td><td class="hist">{}{}</td></tr>"#,
            v.version,
            esc(&clock.fmt(v.changed_at)),
            if items.is_empty() { r#"<div class="mut">no visible change</div>"#.to_string() } else { items },
            sub_line(&format!("by {}", who(v.changed_by)))
        );
    }
    body += "</table></div>";
    html::render(Page { title: &format!("Entry {id} · baby-mcp"), body, nav: Some(&user.display_name), refresh: None })
}

pub async fn not_found() -> Response {
    let _ = auth::SCOPE;
    html::error_page(StatusCode::NOT_FOUND, "Not found.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand() {
        let e = en::Entry {
            entry_id: 1, version: 1, at: 0, duration_s: None, text: String::new(),
            fields: serde_json::from_str(r#"{"bottle_breastmilk_ml": 30, "bottle_formula_ml": 30, "breast_left_min": 5,
                "latch": "with_aid", "wet": true, "dirty": true, "stool_color": "yellow", "temp_c": 36.8}"#).unwrap(),
            attention_by: 0, attention_why: None, deleted: false, changed_at: 0, changed_by: 0, superseded_at: None,
        };
        assert_eq!(entry_summary(&e), "🍼 30 ml milk + 30 ml formula · 🤱 L5′ · 💧 · 💩🟡 · 🌡️ 36.8");
        let bath = |v: &str| en::Entry { fields: serde_json::from_str(&format!(r#"{{"bath": "{v}"}}"#)).unwrap(), ..e.clone() };
        assert_eq!(entry_summary(&bath("with_soap")), "🛁🧼");
        assert_eq!(entry_summary(&bath("without_soap")), "🛁");
        assert_eq!(entry_summary(&bath("sponge")), "🧽");
    }

    #[test]
    fn projection() {
        assert_eq!(expected_by_midnight(3.0, 8.0, 0.5), 7.0);
        assert_eq!(expected_by_midnight(3.0, 8.0, 1.5), 11.0, "clamped to a whole day");
        let day = |d: &str, f: u32| DayStats { date: d.into(), totals: en::Totals { feeds: f, ..Default::default() },
            temp_min_c: None, temp_max_c: None, weight_g: None, weight_vs_birth: None, weight_percentile: None };
        let days = vec![day("2025-11-06", 8), day("2025-11-07", 3)];
        let html = bars("Feeds", &days, &[("feeds", "#000", |d| d.totals.feeds as f64)], "", Some(7.0));
        assert!(html.contains("stroke-dasharray") && html.contains(">~7<") && html.contains("expected by midnight"));
        let html = bars("Feeds", &days, &[("feeds", "#000", |d| d.totals.feeds as f64)], "", Some(2.0));
        assert!(!html.contains(">~2<"), "no projection below what is already there");
    }

    #[test]
    fn bath() {
        use serde_json::json;
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let now = clock.parse("2025-11-07 10:00", 0).unwrap();
        // Last bath with soap on 5 Nov 13:45 → due 9 Nov 01:45, next without soap.
        let b = json!({"due": {"at": "2025-11-10 01:45", "due_in": "2d15h"}, "next_kind": "without_soap",
                       "last": {"ago": "20h15m", "value": "with_soap"}});
        let t = bath_tile(&clock, &b, now);
        assert_eq!(t.at.as_deref(), Some("2025-11-10 01:45"));
        assert!(t.html.contains(">Mon 10 Nov<") && t.html.contains("in 2d15h") && t.html.contains("next: 🛁 without soap"), "{}", t.html);
        assert!(t.html.contains("last 20h15m ago · 🧼 with soap") && !t.html.contains("overdue"));
        let b = json!({"due": {"at": "2025-11-06 01:45", "overdue_by": "1d8h"}, "next_kind": "with_soap", "last": {"ago": "4d", "value": "without_soap"}});
        let t = bath_tile(&clock, &b, now);
        assert!(t.html.contains(r#"class="next overdue""#) && t.html.contains(">now<") && t.html.contains("next: 🧼 with soap"), "{}", t.html);
        // Nothing logged: no estimate, sorted last.
        let t = bath_tile(&clock, &json!({"due": {"never": true, "at": "2025-11-07 10:00"}, "next_kind": null, "last": null}), now);
        assert!(t.at.is_none() && t.html.contains("nothing logged yet") && !t.html.contains("next:"));
    }

    #[test]
    fn vitamins() {
        use serde_json::json;
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let v = |k: bool, d: bool, give: serde_json::Value| json!({"from": "2025-11-07", "remind_at": "2025-11-07 18:00", "to_give_today": give,
            "k": {"active": k, "until": "2026-01-22"}, "d": {"active": d, "until": "2029-10-30"}});
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let t = vitamins_tile(&clock, &v(true, true, json!(["K", "D"])), at("2025-11-07 10:00")).unwrap();
        assert!(t.html.contains(">today<") && t.html.contains("vitamin K + vitamin D still to give") && !t.html.contains("overdue"), "{}", t.html);
        assert!(t.html.contains("K until 22 Jan 2026 · D until 30 Oct 2029"));
        let t = vitamins_tile(&clock, &v(true, true, json!(["K"])), at("2025-11-07 18:30")).unwrap();
        assert!(t.html.contains(r#"class="next overdue""#) && t.html.contains("⏰ vitamin K still to give"), "{}", t.html);
        let t = vitamins_tile(&clock, &v(true, true, json!([])), at("2025-11-07 19:00")).unwrap();
        assert!(t.html.contains(">tomorrow<") && t.html.contains("✓ K + D given today") && t.at.as_deref() == Some("2025-11-08 18:00"));
        let t = vitamins_tile(&clock, &v(false, false, json!([])), at("2025-11-03 10:00")).unwrap();
        assert!(t.html.contains(">Fri 7 Nov<") && t.html.contains("starts"));
        assert!(vitamins_tile(&clock, &v(false, false, json!([])), at("2031-01-01 10:00")).is_none());
    }

    #[test]
    fn grid_steps() {
        assert_eq!(grid_step(662.0, 5.0), 200.0);
        assert_eq!(grid_step(420.0, 5.0), 100.0);
        assert_eq!(grid_step(10.0, 5.0), 2.0);
        assert_eq!(grid_step(3.0, 5.0), 1.0, "counts: whole numbers");
    }

    #[test]
    fn whats_next_order() {
        use serde_json::json;
        let html = whats_next(vec![
            next_tile("🍼 Feed", &json!({"ago": "1h", "at": "2025-11-07 13:00"}), "", &json!({"at": "2025-11-07 16:00", "due_in": "1h"})),
            next_tile("💩 Dirty diaper", &Value::Null, "", &Value::Null),
            next_tile("💧 Wet diaper", &json!({"ago": "3h", "at": "2025-11-07 11:00"}), "", &json!({"at": "2025-11-07 14:30", "overdue_by": "30m"})),
        ]);
        let (w, f, d) = (html.find("Wet").unwrap(), html.find("Feed").unwrap(), html.find("Dirty").unwrap());
        assert!(w < f && f < d, "overdue first, no estimate last");
        assert!(html.contains("⏰ 30m overdue") && html.contains("next overdue") && html.contains("nothing logged yet"));
    }
}

#[cfg(test)]
mod preview {
    #[test]
    #[ignore]
    fn render_growth_chart() {
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let birth = clock.parse("2025-10-31 16:40", 0).unwrap();
        let baby = crate::entries::Baby { name: "B".into(), birth: Some(birth), birth_weight_g: Some(3900.0), sex: Some(crate::growth::Sex::Female), growth: crate::growth::Table::for_tests(), delivery: Some(crate::growth::Delivery::Cesarean), change: crate::growth::WeightChange::for_tests(), aim_feeds_per_day: None, feed_prep_s: 0 };
        let w: Vec<(i64, f64)> = [("2025-11-01 09:00", 3740.0), ("2025-11-02 10:00", 3620.0), ("2025-11-03 09:30", 3590.0), ("2025-11-04 09:00", 3610.0), ("2025-11-05 11:00", 3650.0), ("2025-11-06 12:00", 3700.0)]
            .iter().map(|(t, g)| (clock.parse(t, 0).unwrap(), *g)).collect();
        if std::env::var("CHART").as_deref() == Ok("diapers") {
            let days: Vec<crate::entries::DayStats> = [("2025-11-02", 1, 2, 2), ("2025-11-03", 5, 1, 1), ("2025-11-04", 5, 2, 0), ("2025-11-05", 2, 7, 1), ("2025-11-06", 3, 6, 0), ("2025-11-07", 1, 1, 0)]
                .iter()
                .map(|(d, w, b, x)| crate::entries::DayStats {
                    date: d.to_string(),
                    totals: crate::entries::Totals { wet_only: *w, wet_and_dirty: *b, dirty_only: *x, ..Default::default() },
                    temp_min_c: None, temp_max_c: None, weight_g: None, weight_vs_birth: None, weight_percentile: None,
                })
                .collect();
            let html = super::bars("Diapers per day", &days, &[
                ("wet", "#4a7fb5", |d| d.totals.wet_only as f64),
                ("wet + dirty", "url(#wet-dirty)", |d| d.totals.wet_and_dirty as f64),
                ("dirty", "#a5531d", |d| d.totals.dirty_only as f64),
            ], "", Some(2.0 + 9.0 * 0.6));
            let svg = &html[html.find("<svg").unwrap()..html.find("</svg>").unwrap() + 6];
            let svg = svg.replacen("<svg ", r#"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="200" style="background:#fff;font:11px sans-serif" "#, 1).replace(r#" width="100%""#, "");
            std::fs::write(std::env::var("OUT").unwrap(), svg).unwrap();
            return;
        }
        if std::env::var("CHART").as_deref() == Ok("temp") {
            let temps: Vec<(i64, f64)> = std::env::var("TEMPS").unwrap().split(';').map(|p| {
                let (t, v) = p.split_once('=').unwrap();
                (clock.parse(t, 0).unwrap(), v.parse().unwrap())
            }).collect();
            let now = clock.parse("2025-11-07 10:00", 0).unwrap();
            let html = super::temp_chart(&clock, &temps, clock.day_start(now - 13 * 86400).max(clock.day_start(birth)), now);
            let svg = &html[html.find("<svg").unwrap()..html.find("</svg>").unwrap() + 6];
            let svg = svg.replacen("<svg ", r#"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="200" style="background:#fff;font:11px sans-serif" "#, 1).replace(r#" width="100%""#, "");
            std::fs::write(std::env::var("OUT").unwrap(), svg).unwrap();
            return;
        }
        let html = if std::env::var("CHART").as_deref() == Ok("change") {
            super::change_chart(&baby, &w, clock.parse("2025-11-07 01:00", 0).unwrap())
        } else {
            super::growth_chart(&baby, &w, clock.parse("2025-11-07 01:00", 0).unwrap())
        };
        for (t, g) in &w {
            if let Some((c, _, l)) = baby.change_percentile(*g, *t) {
                eprintln!("{} {g:.0} g {c:+.1}% {l}", clock.fmt(*t));
            }
        }
        let svg = &html[html.find("<svg").unwrap()..html.find("</svg>").unwrap() + 6];
        let svg = svg.replacen("<svg ", r#"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="220" style="background:#fff;font:11px sans-serif" "#, 1).replace(r#" width="100%""#, "");
        std::fs::write(std::env::var("OUT").unwrap(), svg).unwrap();
    }
}
