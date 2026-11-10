//! Web UI for people: status, charts and a capture form that works without
//! an agent. Server-rendered; a passkey session is required.

use crate::app::App;
use crate::auth::{self, User, same_origin, session_user, to_login};
use crate::entries::{self as en, BY_AGENT, DayStats, NewEntry};
use crate::html::{self, Page, esc};
use crate::status::{Bath, SleepFacing, Status, Vitamins};
use crate::time::fmt_dur;
use crate::time::now;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// The logged-in user; a viewer only on the charts (elsewhere: to the charts).
fn require(app: &App, headers: &HeaderMap, here: &str) -> Result<User, Response> {
    let user = session_user(app, headers).ok_or_else(|| to_login(here))?;
    if user.viewer && here != "/charts" {
        return Err(Redirect::to("/charts").into_response());
    }
    Ok(user)
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
    at: Option<i64>,
    html: String,
}

/// A tile with the time of the next one (`next`), when the last one was
/// (with a detail), and an extra line (already HTML) under the estimate;
/// orange when overdue or with `warn`.
fn next_tile_extra(clock: &crate::time::Clock, title: &'static str, last: Option<(i64, &str)>, next: Option<i64>, extra: &str, warn: bool, now: i64) -> Next {
    let hm = |t: i64| clock.zoned(t).strftime("%H:%M").to_string();
    let last_line = match last {
        Some((t, last_detail)) => {
            let detail = if last_detail.is_empty() { String::new() } else { format!(" · {last_detail}") };
            format!("last {} ago · {}{detail}", fmt_dur(now - t), hm(t))
        }
        None => "nothing logged yet".into(),
    };
    let (when, sub, overdue) = match next {
        None => ("—".to_string(), "not enough data yet".to_string(), false),
        Some(t) if t < now => (format!("~{}", hm(t)), format!("⏰ {} overdue", fmt_dur(now - t)), true),
        Some(t) => (format!("~{}", hm(t)), format!("in {}", fmt_dur(t - now)), false),
    };
    let html = format!(
        r#"<div class="next{}"><div class="t">{}</div><div class="v">{}</div><div class="{}">{}</div>{extra}<div class="last">{}</div></div>"#,
        if overdue || warn { " overdue" } else { "" },
        esc(title),
        esc(&when),
        if overdue { "warn" } else { "mut" },
        esc(&sub),
        esc(&last_line)
    );
    Next { title, at: next, html }
}

/// "🍼 about 80 ml" (the suggested amount for the next feed) for the feed tile.
fn amount_line(clock: &crate::time::Clock, g: &crate::feeding::Guidance) -> String {
    let hm = |t: i64| clock.zoned(t).strftime("%H:%M").to_string();
    let mut s = format!(r#"<div class="side">🍼 about {:.0} ml</div>"#, g.amount_ml);
    if let Some(b) = &g.breast_side {
        s += &format!(r#"<div class="side">🤱 start {} <span class="mut">({})</span></div>"#, esc(b.start), esc(&b.reason));
    }
    for w in &g.warnings {
        s += &format!(r#"<div class="side warn">⚠️ {}</div>"#, esc(&w.text));
    }
    if let Some((latest, why)) = &g.latest {
        match g.latest_start() {
            Some(start) => s += &format!(
                r#"<div class="side">⏳ start by {} <span class="mut">(feed by {} · {})</span></div>"#,
                esc(&hm(start)),
                esc(&hm(*latest)),
                esc(why)
            ),
            None => s += &format!(r#"<div class="side">⏳ at the latest {} <span class="mut">({})</span></div>"#, esc(&hm(*latest)), esc(why)),
        }
    }
    s
}

/// "😴 face ➡️ right" (the next side to put her down on) for the feed tile.
fn sleep_line(v: &SleepFacing) -> String {
    let next = &v.next;
    // Right after a feed without a side logged: the side for now first.
    if v.put_down_now {
        let now = &v.current;
        return format!(
            r#"<div class="side">😴 now {} {} <span class="mut">· next {} {}</span></div>"#,
            side_arrow(now),
            esc(now),
            side_arrow(next),
            esc(next)
        );
    }
    let assumed = match v.feeds_since {
        0 => String::new(),
        n => format!(
            r#" <span class="mut">(assumed: alternated after {n} feed{} since the last logged side)</span>"#,
            if n == 1 { "" } else { "s" }
        ),
    };
    format!(r#"<div class="side">😴 face {} {}{assumed}</div>"#, side_arrow(next), esc(next))
}

/// The timeline above "what's next": one long strip from the day of birth
/// to the hours ahead, wider than the screen. It opens with "now" at
/// TL_NOW_AT of the width (or on the day shown); the script loads the days
/// that scroll into view from `/timeline` and swaps the log below for the
/// day in view.
const TL_PAST: i64 = 48 * 3600;
const TL_FUTURE: i64 = 12 * 3600;
const TL_NOW_AT: f64 = 0.7;
/// Night in the timeline's band: from midnight until this hour.
const TL_NIGHT_UNTIL: i8 = 8;
const TL_PAD: f64 = 24.0;
const TL_H: f64 = 92.0;
/// Room under the strip for the bar marking the day the log shows.
const TL_ON_H: f64 = 6.0;

/// A zoom level (pinch): pixels per hour, and hours between hour labels and
/// between hour lines. The two widest levels summarise each day, and each
/// week (of life), instead.
struct Zoom {
    pxh: f64,
    label: i8,
    line: i8,
}
const TL_ZOOM: [Zoom; 4] = [
    Zoom { pxh: 38.0, label: 2, line: 1 },
    Zoom { pxh: 12.0, label: 6, line: 3 },
    Zoom { pxh: 3.0, label: 24, line: 24 },
    Zoom { pxh: 0.5, label: 24, line: 24 },
];
const TL_Z: usize = 0;
const TL_SUMMARY: usize = 2;
const TL_WEEK: usize = 3;
/// The cookie keeping the zoom level (so the page opens at it).
const TL_Z_COOKIE: &str = "tlz2";

/// The whole strip: from midnight of the first day to the hours ahead.
struct Track {
    from: i64,
    to: i64,
    z: usize,
}

impl Track {
    fn pxh(&self) -> f64 {
        TL_ZOOM[self.z].pxh
    }
    fn x(&self, t: i64) -> f64 {
        TL_PAD + (t - self.from) as f64 / 3600.0 * self.pxh()
    }
    fn width(&self) -> f64 {
        self.x(self.to) + TL_PAD
    }
}

fn zoom(headers: &HeaderMap) -> usize {
    auth::cookie(headers, TL_Z_COOKIE).and_then(|z| z.parse().ok()).filter(|z| *z < TL_ZOOM.len()).unwrap_or(TL_Z)
}

/// From the day of birth (or the first entry, or two days back), to the hours ahead.
fn track(c: &rusqlite::Connection, clock: &crate::time::Clock, baby: &en::Baby, now: i64, z: usize) -> Result<Track, String> {
    let first: Option<i64> = c.query_row("SELECT MIN(at) FROM entry WHERE deleted = 0", [], |r| r.get(0)).map_err(en::err)?;
    let start = [baby.birth, first, Some(now - TL_PAST)].into_iter().flatten().min().unwrap_or(now);
    Ok(Track { from: clock.day_start(start), to: now + TL_FUTURE, z })
}

/// The days of a track, as (start, end); today's runs on to the hours ahead.
fn tl_days(clock: &crate::time::Clock, tr: &Track, now: i64) -> Vec<(i64, i64)> {
    let today = clock.day_start(now);
    let mut out = vec![];
    let mut d = tr.from;
    while d < tr.to {
        if d >= today {
            out.push((d, tr.to));
            break;
        }
        let next = clock.next_day_start(d);
        out.push((d, next));
        d = next;
    }
    out
}

/// The days of a track overlapping `from..to`, rendered (today's with what's
/// expected, from the status `s`).
fn tl_render(clock: &crate::time::Clock, tr: &Track, entries: &[en::Entry], s: &Status, now: i64, from: i64, to: i64) -> String {
    let today = clock.day_start(now);
    tl_days(clock, tr, now)
        .into_iter()
        .filter(|(a, b)| *b > from && *a < to)
        .map(|(a, b)| tl_day(clock, tr, entries, (a >= today).then_some(s), now, a, b))
        .collect()
}

fn timeline(clock: &crate::time::Clock, tr: &Track, days: &str, now: i64, focus: Option<i64>) -> String {
    let list: Vec<String> = tl_days(clock, tr, now).iter().map(|(a, _)| format!("{}:{a}", clock.day(*a))).collect();
    let pxh: Vec<String> = TL_ZOOM.iter().map(|z| z.pxh.to_string()).collect();
    format!(
        r#"<div class="card timeline"><div class="tl-zoom"><button type="button" data-zoom="-1" aria-label="Zoom out" title="Zoom out">−</button><button type="button" data-zoom="1" aria-label="Zoom in" title="Zoom in">+</button></div><div class="tl-scroll" id="tl" data-from="{}" data-to="{}" data-now="{now}" data-today="{}" data-at="{TL_NOW_AT}" data-z="{}" data-pxh="{}" data-pad="{TL_PAD}" data-week-z="{TL_WEEK}" data-days="{}" data-focus="{}"><svg class="tl" width="{:.0}" height="{}" role="img" aria-label="Timeline: feeds, diapers and the rest, by day">{days}</svg></div></div>"#,
        tr.from,
        tr.to,
        clock.day(now),
        tr.z,
        pxh.join(","),
        list.join(","),
        focus.map(|f| f.to_string()).unwrap_or_default(),
        tr.width(),
        TL_H + TL_ON_H
    )
}

/// One day of the timeline, as an svg at its place in the track: emojis on
/// three lanes (feeds, diapers, the rest) over a day/night band with the
/// hours. Today's (`s` given) runs on into the hours ahead with what's
/// expected: faded, or overdue just after "now" with an orange ring. The
/// expected feed is when to start (diaper first). At the widest zoom, a
/// summary of the day instead.
fn tl_day(clock: &crate::time::Clock, tr: &Track, entries: &[en::Entry], s: Option<&Status>, now: i64, from: i64, to: i64) -> String {
    let zoom = &TL_ZOOM[tr.z];
    let h = TL_H;
    let x = |t: i64| (t - from) as f64 / 3600.0 * zoom.pxh;
    let w = x(to);
    let lanes = [26.0, 50.0, 74.0]; // feeds, diapers, other (text baselines)
    let hm = |t: i64| clock.zoned(t).strftime("%H:%M").to_string();
    let today = s.is_some();
    let mut svg = format!(r#"<svg x="{:.1}" width="{w:.1}" height="{h}" overflow="visible" data-day="{}">"#, tr.x(from), clock.day(from));
    if today {
        svg += &format!(r#"<rect x="{:.1}" y="4" width="{:.1}" height="{:.1}" class="future"/>"#, x(now), x(to) - x(now), h - 22.0);
    }
    // Night (00–08) and day as a band under the hour numbers (by week: the
    // week's first day draws it plain, under its label).
    let night = |t: i64| clock.zoned(t).hour() < TL_NIGHT_UNTIL;
    let mut seg = from;
    while seg < to && tr.z != TL_WEEK {
        let n = night(seg);
        let mut end = seg - seg.rem_euclid(3600) + 3600;
        while end < to && night(end) == n {
            end += 3600;
        }
        let end = end.min(to);
        svg += &format!(r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="17" class="{}"/>"#, x(seg), h - 17.0, x(end) - x(seg), if n { "night" } else { "day" });
        seg = end;
    }
    let shown: Vec<&en::Entry> = entries.iter().filter(|e| e.at >= from && e.at < to && e.at <= now).collect();
    let now_line = |svg: &mut String| {
        if today {
            *svg += &format!(r#"<line x1="{0:.1}" x2="{0:.1}" y1="2" y2="{1:.1}" class="now"/>"#, x(now), h - 16.0);
        }
    };

    if tr.z == TL_WEEK {
        // Weeks of life, drawn by their first day over the whole week: a
        // line at its start, "week N" in the band, and the averages per day
        // over its complete days (today's isn't over yet).
        now_line(&mut svg);
        let idx = ((from - tr.from) as f64 / 86400.0).round() as i64;
        if idx % 7 != 0 {
            return svg + "</svg>";
        }
        let mut end = from;
        for _ in 0..7 {
            end = clock.next_day_start(end);
        }
        let label = format!("week {}", idx / 7 + 1);
        let band_end = end.min(tr.to);
        svg += &format!(r#"<rect x="0" y="{:.1}" width="{:.1}" height="17" class="day"/>"#, h - 17.0, x(band_end));
        svg += &format!(r#"<line x1="0" x2="0" y1="6" y2="{:.1}" class="hour"/>"#, h - 18.0);
        // This week: over what has passed of it.
        let mid = x(from + (end.min(now) - from) / 2);
        svg += &format!(r#"<text x="{mid:.1}" y="{}" text-anchor="middle" class="hd">{label}</text>"#, h - 4.0);
        let today_start = clock.day_start(now);
        let (mut n, mut feeds, mut ml, mut wet, mut dirty) = (0u32, 0.0, 0.0, 0.0, 0.0);
        let mut d = from;
        while d < end && clock.next_day_start(d) <= today_start {
            let e = clock.next_day_start(d);
            let t = en::totals_from(entries.iter().filter(|x| x.at < e), d);
            (feeds, ml, wet, dirty) = (feeds + t.feeds as f64, ml + t.total_ml, wet + t.wet as f64, dirty + t.dirty as f64);
            n += 1;
            d = e;
        }
        if n == 0 {
            return svg + "</svg>";
        }
        let k = n as f64;
        let one = |v: f64| format!("{:.1}", v / k).trim_end_matches(".0").to_string();
        let title = format!("{label}, per day over {n} day{}: {} feeds, {:.0} ml bottle, {} wet, {} dirty", if n == 1 { "" } else { "s" }, one(feeds), ml / k, one(wet), one(dirty));
        for (lane, text) in [(0, format!("🍼{}", one(feeds))), (1, format!("💧{:.0} 💩{:.0}", wet / k, dirty / k)), (2, format!("{:.0} ml", ml / k))] {
            // This week: kept left of "now" (about 6 px a character, emojis 2).
            let half = text.chars().map(|ch| if ch.is_ascii() { 3.1 } else { 6.0 }).sum::<f64>();
            let cx = if end > now { mid.min(x(now) - 4.0 - half) } else { mid };
            svg += &format!(r#"<text x="{cx:.1}" y="{}" text-anchor="middle" class="sum w"><title>{}</title>{text}</text>"#, lanes[lane] - 3.0, esc(&title));
        }
        return svg + "</svg>";
    }

    if tr.z == TL_SUMMARY {
        // The day: a line at midnight, the date in the band, and its totals.
        let date = clock.zoned(from).strftime("%a %-d").to_string();
        svg += &format!(r#"<line x1="0" x2="0" y1="6" y2="{:.1}" class="hour"/>"#, h - 18.0);
        let noon = x(from + 12 * 3600).min(w / 2.0);
        svg += &format!(r#"<text x="{noon:.1}" y="{}" text-anchor="middle" class="hd">{date}</text>"#, h - 4.0);
        now_line(&mut svg);
        let t = en::totals_from(entries.iter().filter(|e| e.at < to && e.at <= now), from);
        let cx = if today { x(from + (now - from) / 2) } else { noon };
        let mut lines = vec![];
        if t.feeds > 0 {
            lines.push((0, format!("🍼{}", t.feeds)));
        }
        let diapers: Vec<String> = [(t.wet, "💧"), (t.dirty, "💩")].iter().filter(|(n, _)| *n > 0).map(|(n, e)| format!("{e}{n}")).collect();
        if !diapers.is_empty() {
            lines.push((1, diapers.join(" ")));
        }
        if t.total_ml > 0.0 {
            lines.push((2, format!("{:.0} ml", t.total_ml)));
        }
        let title = format!("{date}: {} feeds, {:.0} ml bottle, {} wet, {} dirty", t.feeds, t.total_ml, t.wet, t.dirty);
        for (lane, text) in lines {
            svg += &format!(r#"<text x="{cx:.1}" y="{}" text-anchor="middle" class="sum"><title>{}</title>{text}</text>"#, lanes[lane] - 3.0, esc(&title));
        }
        return svg + "</svg>";
    }

    // Hour lines and labels; midnight with the day ("Sun 9") instead of 00,
    // and an hour label that would overlap a day left out.
    let mut labels: Vec<(f64, String, bool, &str)> = vec![]; // x, text, is a day, class
    let mut hr = from + (3600 - from.rem_euclid(3600)) % 3600;
    while hr < to {
        let z = clock.zoned(hr);
        if z.hour() % zoom.line == 0 {
            svg += &format!(r#"<line x1="{0:.1}" x2="{0:.1}" y1="6" y2="{1:.1}" class="hour"/>"#, x(hr), h - 18.0);
        }
        if z.hour() % zoom.label == 0 {
            let day = z.hour() == 0;
            let text = if day { z.strftime("%a %-d").to_string() } else { z.strftime("%H").to_string() };
            labels.push((x(hr), text, day, if night(hr) { "hn" } else { "hd" }));
        }
        hr += 3600;
    }
    let half = |t: &str| t.chars().count() as f64 * 3.5; // about 7 px a character
    for (lx, text, day, class) in &labels {
        let overlaps = !day && labels.iter().any(|(dx, dt, d, _)| *d && (lx - dx).abs() < half(text) + half(dt) + 4.0);
        if !overlaps {
            svg += &format!(r#"<text x="{lx:.1}" y="{}" text-anchor="middle" class="{class}">{text}</text>"#, h - 4.0);
        }
    }
    now_line(&mut svg);
    // Smaller emojis when zoomed out.
    let (k, emo) = if zoom.pxh < 20.0 { (0.72, "emo s") } else { (1.0, "emo") };
    let mut put = |lane: usize, emoji: &str, t: i64, opacity: Option<&str>, ring: bool, title: String| {
        let (cx, cy) = (x(t), lanes[lane]);
        if ring {
            svg += &format!(r#"<circle cx="{cx:.1}" cy="{:.1}" r="{:.0}" class="ring"/>"#, cy - 6.0 * k, 11.0 * k);
        }
        svg += &format!(
            r#"<text x="{cx:.1}" y="{cy}" text-anchor="middle" class="{emo}"{}><title>{}</title>{emoji}</text>"#,
            opacity.map(|o| format!(r#" opacity="{o}""#)).unwrap_or_default(),
            esc(&title)
        );
    };
    for e in shown {
        let title = format!("{} {}", hm(e.at), entry_summary(e));
        if e.is_feed() {
            put(0, "🍼", e.at, None, false, title.clone());
        }
        if e.is("dirty") && e.is("wet") {
            // Both: the poo with a small drop at its top right.
            let both = format!(r#"💩<tspan x="{:.1}" y="{:.1}" class="badge">💧</tspan>"#, x(e.at) + 7.0 * k, lanes[1] - 8.0 * k);
            put(1, &both, e.at, None, false, title.clone());
        } else if e.is("dirty") {
            put(1, "💩", e.at, None, false, title.clone());
        } else if e.is("wet") {
            put(1, "💧", e.at, None, false, title.clone());
        }
        if e.is("vitamin_d") || e.is("vitamin_k") {
            put(2, "💊", e.at, None, false, title.clone());
        } else if e.str("bath").is_some() {
            put(2, "🛁", e.at, None, false, title.clone());
        } else if e.num("weight_g").is_some() {
            put(2, "⚖️", e.at, None, false, title.clone());
        } else if e.is("vomit") {
            put(2, "🤢", e.at, None, false, title.clone());
        }
    }
    let Some(s) = s else { return svg + "</svg>" };
    // What's expected: overdue ones just after now, with a ring.
    let soon = now + 8 * 60;
    let mut expect = |lane: usize, emoji: &str, at: Option<i64>, label: String, opacity: &str| {
        let Some(t) = at else { return };
        if t > to {
            return;
        }
        let overdue = t < now;
        put(lane, emoji, if overdue { soon } else { t }, if overdue { None } else { Some(opacity) }, overdue,
            format!("{} {label}{}", hm(t), if overdue { " (overdue)" } else { " (expected)" }));
    };
    // After the next one, more at the usual gap (the same rule as the
    // estimates), from the next one or, if that is overdue, from now: fainter.
    let later = |next: Option<i64>, gap: Option<i64>, shift: i64| -> Vec<i64> {
        let (Some(next), Some(gap)) = (next, gap) else { return vec![] };
        let mut out = vec![];
        let mut t = next.max(now) + gap;
        while t - shift <= to {
            out.push(t - shift);
            t += gap;
        }
        out
    };
    // The feed: when to start (diaper first), if there is a preparation time.
    let feed_at = s.next_feed.map(|f| f.at);
    let prep = s.prep_s.max(0);
    let feed_label = |t: i64| if prep > 0 { format!("start (feed ~{})", hm(t + prep)) } else { "feed".to_string() };
    expect(0, "🍼", feed_at.map(|t| t - prep), feed_at.map(|t| feed_label(t - prep)).unwrap_or_default(), ".45");
    let feed_gap = en::feed_gap(&en::feed_episodes(entries.iter().filter(|e| e.at > now - en::ESTIMATE_BACK && e.at <= now)));
    for t in later(feed_at, feed_gap, prep) {
        expect(0, "🍼", Some(t), feed_label(t), ".25");
    }
    if let Some(v) = s.vitamins.as_ref().filter(|v| !v.to_give().is_empty()) {
        expect(2, "💊", Some(v.remind_at), "vitamins".into(), ".45");
    }
    let bath = &s.bath;
    if !bath.never() {
        // Due today and past the reminder: still today, not overdue.
        expect(2, "🛁", Some(if bath.today(now) { bath.due_at.max(now + 60) } else { bath.due_at }), "bath".into(), ".45");
    }
    svg + "</svg>"
}

/// One day of the timeline (`?date=…&z=…`), for the script to place.
pub async fn timeline_day(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require(&app, &headers, "/") {
        return r;
    }
    let t = now();
    let clock = &app.clock;
    let z = q.get("z").and_then(|z| z.parse().ok()).filter(|z| *z < TL_ZOOM.len()).unwrap_or(TL_Z);
    let Some(start) = q.get("date").and_then(|d| clock.date_start(d).ok()) else {
        return (StatusCode::BAD_REQUEST, "bad date").into_response();
    };
    let res = app.db.with(|c| {
        Ok((|| {
            let tr = track(c, clock, &app.baby(), t, z)?;
            let Some((a, b)) = tl_days(clock, &tr, t).into_iter().find(|(a, _)| *a == start) else { return Ok(None) };
            let today = a >= clock.day_start(t);
            let s = if today { Some(crate::status::status(c, clock, &app.baby(), t)?) } else { None };
            // By week, a week's first day summarises the whole week.
            let until = if z == TL_WEEK { a + 8 * 86400 } else { b };
            let entries = en::list(c, a - en::ESTIMATE_BACK, until.min(t + 1), false)?;
            Ok::<_, String>(Some(tl_day(clock, &tr, &entries, s.as_ref(), t, a, b)))
        })())
    });
    match res {
        Ok(Ok(Some(svg))) => ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"), (axum::http::header::CACHE_CONTROL, "no-store")], svg).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "no such day").into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, en::err(e)).into_response(),
    }
}

/// One diaper tile: when the last diapers were, and the wet diaper warning.
/// Not predicted: diapers are checked at feeds, wet at nearly every one and
/// dirty at most, with nothing in the log telling which (backtested).
fn diaper_tile(s: &Status) -> Next {
    let kinds = [("💧", &s.last_wet), ("💩", &s.last_dirty)];
    let ago = |e: &en::Entry| fmt_dur(s.now - e.at);
    let latest = kinds.iter().filter_map(|k| k.1.as_ref()).map(|e| e.at).max();
    let when = latest.map(|t| format!("{} ago", fmt_dur(s.now - t))).unwrap_or_else(|| "—".to_string());
    // What the last diaper was; a kind from an earlier one with its time.
    let parts: Vec<String> = kinds
        .iter()
        .filter_map(|(emoji, last)| {
            let e = last.as_ref()?;
            let color = if *emoji == "💩" { e.str("stool_color").map(stool_emoji).unwrap_or("") } else { "" };
            Some(if Some(e.at) == latest { format!("{emoji}{color}") } else { format!("{emoji}{color} {} ago", ago(e)) })
        })
        .collect();
    let sub = if parts.is_empty() { "nothing logged yet".to_string() } else { parts.join(" · ") };
    let warning = s.wet_warning.as_ref().map(|w| format!(r#"<div class="side warn">⚠️ {}</div>"#, esc(&w.text))).unwrap_or_default();
    let html = format!(
        r#"<div class="next{}"><div class="t">🧷 Diaper</div><div class="v">{}</div><div class="mut">{}</div>{warning}<div class="last">checked at feeds</div></div>"#,
        if warning.is_empty() { "" } else { " overdue" },
        esc(&when),
        esc(&sub)
    );
    Next { title: "🧷 Diaper", at: None, html }
}

/// The vitamins tile: what is still to give today (orange from 18:00), or
/// "tomorrow" when all were given; "starts …" before day 8. None when no
/// vitamin applies any more.
fn vitamins_tile(clock: &crate::time::Clock, v: &Vitamins, now: i64) -> Option<Next> {
    // Not needed today (vitamin K with enough formula) counts as not active.
    let active: Vec<&str> = v.all().iter().filter(|x| x.active && !x.not_needed).map(|x| x.label).collect();
    let skipped: Vec<String> = v.all().iter().filter(|x| x.not_needed)
        .map(|x| format!("{} not needed today ({:.0} ml formula yesterday)", x.label, v.formula_yesterday))
        .collect();
    let until: Vec<String> = v.all().iter().map(|x| format!("{} until {}", x.label, clock.zoned(x.until - 1).strftime("%-d %b %Y"))).collect();
    let to_give = v.to_give();
    let tomorrow = v.remind_at + 86400;
    let (when, sub, at, overdue) = if active.is_empty() && !skipped.is_empty() {
        ("tomorrow".to_string(), "nothing to give today".to_string(), tomorrow, false)
    } else if active.is_empty() {
        if v.from <= now {
            return None; // past 4 years
        }
        (clock.zoned(v.from).strftime("%a %-d %b").to_string(), "starts (day 8)".to_string(), v.from, false)
    } else if !to_give.is_empty() {
        let late = now >= v.remind_at;
        let what = to_give.iter().map(|l| format!("vitamin {l}")).collect::<Vec<_>>().join(" + ");
        ("today".to_string(), format!("{}{what} still to give", if late { "⏰ " } else { "" }), v.remind_at, late)
    } else {
        ("tomorrow".to_string(), format!("✓ {} given today", active.join(" + ")), tomorrow, false)
    };
    let html = format!(
        r#"<div class="next{}"><div class="t">💊 Vitamins</div><div class="v">{}</div><div class="{}">{}</div><div class="last">{}</div></div>"#,
        if overdue { " overdue" } else { "" },
        esc(&when),
        if overdue { "warn" } else { "mut" },
        esc(&sub),
        esc(&skipped.iter().chain(until.iter()).cloned().collect::<Vec<_>>().join(" · "))
    );
    Some(Next { title: "💊 Vitamins", at: Some(at), html })
}

/// The bath tile: when the next bath is due (as a day, not a clock time) and
/// whether to use soap (the other kind than last time).
fn bath_tile(clock: &crate::time::Clock, b: &Bath, now: i64) -> Next {
    let day = |t: i64| {
        if clock.day(t) <= clock.day(now) {
            "today".to_string()
        } else if clock.day(t) == clock.day(now + 86400) {
            "tomorrow".to_string()
        } else {
            clock.zoned(t).strftime("%a %-d %b").to_string()
        }
    };
    let kind = |k: &str| match k {
        "with_soap" => "🧼 with soap",
        "without_soap" => "🛁 without soap",
        _ => "🧽 sponge",
    };
    // Due on a day: "today" all that day; overdue only once it's over.
    let (when, sub, overdue) = if b.never() {
        ("—".to_string(), "nothing logged yet".to_string(), false)
    } else if let Some(o) = b.overdue(now) {
        ("now".to_string(), format!("⏰ {} overdue", fmt_dur(o)), true)
    } else if b.today(now) {
        ("today".to_string(), String::new(), false)
    } else {
        (day(b.due_at), format!("in {}", fmt_dur(b.due_at - now)), false)
    };
    let next = match b.next_kind() {
        Some(k) => format!(r#"<div class="side">{}</div>"#, kind(k)),
        None => String::new(),
    };
    let sub = if sub.is_empty() { String::new() } else { format!(r#"<div class="{}">{}</div>"#, if overdue { "warn" } else { "mut" }, esc(&sub)) };
    let last = match &b.last {
        Some(e) => format!("last {} ago · {}", fmt_dur(now - e.at), kind(e.str("bath").unwrap_or(""))),
        None => "two a week, alternating with and without soap".into(),
    };
    let html = format!(
        r#"<div class="next{}"><div class="t">🛁 Bath</div><div class="v">{}</div>{sub}{next}<div class="last">{}</div></div>"#,
        if overdue { " overdue" } else { "" },
        esc(&when),
        esc(&last)
    );
    Next { title: "🛁 Bath", at: (!b.never()).then_some(b.due_at), html }
}

/// What's next, soonest (or most overdue) first; kinds without an estimate
/// last; under `head`.
fn whats_next(head: &str, mut tiles: Vec<Next>) -> String {
    tiles.sort_by(|a, b| match (&a.at, &b.at) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => a.title.cmp(b.title),
    });
    let inner: String = tiles.iter().map(|t| t.html.as_str()).collect();
    format!(r#"<div class="card">{head}<div class="nexts">{inner}</div></div>"#)
}

/// Totals for a day (or part of one), plus that day's weight if any.
fn totals_card(heading: &str, t: &en::Totals, weight: Option<(f64, Option<String>)>) -> String {
    let mut s = format!(r#"<div class="card">{heading}<div class="grid">"#);
    let vomit = if t.vomit > 0 { format!("🤢 ×{}", t.vomit) } else { String::new() };
    s += &stat("Feeds", &t.feeds.to_string(), &format!("🤱 {}′ at the breast", t.breast_min));
    s += &stat("🍼 Bottle", &format!("{:.0} ml", t.total_ml), &format!("{:.0} milk + {:.0} formula", t.breastmilk_ml, t.formula_ml));
    let split = format!("💧 {} wet · 💩 {} dirty{}", t.wet, t.dirty, if vomit.is_empty() { String::new() } else { format!(" · {vomit}") });
    s += &stat("Diapers", &t.diapers.to_string(), &split);
    if let Some((w, vs)) = weight {
        s += &stat("⚖️ Weight", &format!("{w:.0} g"), vs.as_deref().unwrap_or(""));
    }
    s + "</div></div>"
}

const AGENT_HINT: &str = r#"<p class="mut">🤖 waiting for Claude: ask Claude to “process the entries that need attention”. 🙋 needs you.</p>"#;

/// A day's entries, newest first; one needing attention with the reason.
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
            "<tr><td class=\"time\"><a href=\"/entry/{}\">{}</a>{flag}{who}{edited}</td><td>{}{}{}</td></tr>",
            e.entry_id,
            esc(&clock.fmt_time(e.at)),
            esc(&e.text),
            sub_line(&entry_summary(e)),
            e.attention_why.as_deref().filter(|_| e.attention_by != 0).map(|w| format!(r#"<div class="sub warn">{}</div>"#, esc(w))).unwrap_or_default()
        );
    }
    if entries.is_empty() {
        s += &format!(r#"<tr><td class="mut">{}</td></tr>"#, esc(empty));
    }
    s += "</table>";
    if entries.iter().any(|e| e.attention_by & BY_AGENT != 0) {
        s += AGENT_HINT;
    }
    s + "</div>"
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

/// The last weighing of `entries`, with how it compares (see `weight_line`).
fn weight_of(entries: &[en::Entry], baby: &en::Baby) -> Option<(f64, Option<String>)> {
    weight_line(entries.iter().filter(|e| e.num("weight_g").is_some()).last()?, baby, false)
}

/// A weighing: grams, and its percentile and change since birth (as of the
/// weighing); with `regained`, that she is back at birth weight.
fn weight_line(e: &en::Entry, baby: &en::Baby, regained: bool) -> Option<(f64, Option<String>)> {
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
    if regained {
        sub.push("✅ birth weight regained".into());
    }
    Some((w, (!sub.is_empty()).then(|| sub.join(" · "))))
}

pub async fn status(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let user = match require(&app, &headers, "/") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let t = now();
    let clock = &app.clock;
    let today_start = clock.day_start(t);
    // A past day in view (`?day=`): its log below the timeline, the timeline on it.
    let day = q.get("day").and_then(|d| clock.date_start(d).ok()).filter(|d| *d < today_start);
    let z = zoom(&headers);
    let res = app.db.with(|c| {
        Ok((|| {
            let s = crate::status::status(c, clock, &app.baby(), t)?;
            let tr = track(c, clock, &app.baby(), t, z)?;
            // The days around the opening view (the script loads the rest).
            let span = match z {
                TL_SUMMARY => 7 * 86400,
                TL_WEEK => 49 * 86400,
                _ => 0,
            };
            let (a, b) = match day {
                Some(d) => (d - 86400 - span, d + 2 * 86400 + span),
                None => (t - TL_PAST - span, tr.to),
            };
            let entries = en::list(c, a - en::ESTIMATE_BACK, t + 1, false)?;
            let days = tl_render(clock, &tr, &entries, &s, t, a, b);
            // At the week zoom: the week (of the day in view) instead.
            let log = if z == TL_WEEK { weeklog(&app, c, &tr, day.unwrap_or(today_start), t)? } else { daylog(&app, c, day.unwrap_or(today_start), t, Some(&s))? };
            Ok::<_, String>((s, tr, days, log))
        })())
    });
    let (s, tr, days, log) = match res {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };

    let baby = app.baby();
    let mut body = format!(
        r#"<div class="card top"><h1>{}</h1><span class="mut">{}{}</span></div>"#,
        esc(&app.cfg.baby.name),
        baby.birth.map(|b| format!("{} old · ", fmt_dur(t - b))).unwrap_or_default(),
        esc(&clock.fmt(t))
    );

    // Needs attention, first: today's and older.
    if !s.attention.is_empty() {
        body += r#"<div class="card"><h2 class="warn">Needs attention</h2><table>"#;
        for e in &s.attention {
            let (u, a) = (e.attention_by & en::BY_USER != 0, e.attention_by & BY_AGENT != 0);
            body += &format!(
                "<tr><td class=\"time\"><a href=\"/entry/{}\">{}</a> <span title=\"{}\">{}</span></td><td>{}{}</td></tr>",
                e.entry_id,
                esc(&clock.zoned(e.at).strftime("%m-%d %H:%M").to_string()),
                esc(&attention_label(u, a)),
                attention_emoji(u, a),
                esc(&e.text),
                e.attention_why.as_deref().map(|w| format!(r#"<div class="sub warn">{}</div>"#, esc(w))).unwrap_or_default()
            );
        }
        body += "</table>";
        if s.attention.iter().any(|e| e.attention_by & BY_AGENT != 0) {
            body += AGENT_HINT;
        }
        body += "</div>";
    }

    body += &timeline(clock, &tr, &days, t, day.map(|d| d + 12 * 3600));

    let feed_what = s.last_feed.as_ref().map(|e| feed_summary(&e.fields)).unwrap_or_default();
    // With a preparation time, the tile shows when to start (diaper first).
    let (feed_next, prep_line) = match s.next_feed {
        Some(f) if s.prep_s > 0 => (s.feed_start(), format!(r#"<div class="side">🧷 diaper first; feed ~{}</div>"#, esc(&clock.zoned(f.at).strftime("%H:%M").to_string()))),
        f => (f.map(|f| f.at), String::new()),
    };
    let extra = format!(
        "{prep_line}{}{}",
        s.guidance.as_ref().map(|g| amount_line(clock, g)).unwrap_or_default(),
        s.sleep.as_ref().map(sleep_line).unwrap_or_default()
    );
    // An NVK warning makes the tile orange, even when not overdue.
    let warn = s.guidance.as_ref().is_some_and(|g| !g.warnings.is_empty());
    let feed = next_tile_extra(clock, "🍼 Feed", s.last_feed.as_ref().map(|e| (e.at, feed_what.as_str())), feed_next, &extra, warn, t);
    let mut tiles = vec![
        feed,
        diaper_tile(&s),
        bath_tile(clock, &s.bath, t),
    ];
    tiles.extend(s.vitamins.as_ref().and_then(|v| vitamins_tile(clock, v, t)));
    // What's next is about now: hidden while a past day is in view. Its
    // heading is today's in the days' layout (prev · day · next), so the
    // links stay in place when stepping through days onto today.
    let yesterday = clock.day_start(today_start - 3600);
    let prev = format!("← {}", clock.zoned(yesterday).strftime("%a %-d"));
    let head = format!(
        r#"<div class="head"><a href="/?day={0}" data-day="{0}">{prev}</a><h2>Today</h2><span class="next-none" aria-hidden="true">{prev}</span></div>"#,
        clock.day(yesterday)
    );
    // At the week zoom the week is shown instead (keyed by its first day).
    let week = tr.z == TL_WEEK;
    let shown = if week { week_of(clock, &tr, day.unwrap_or(today_start)).1 } else { day.unwrap_or(today_start) };
    body += &format!(r#"<div class="now-only"{}>{}</div>"#, if day.is_some() || week { " hidden" } else { "" }, whats_next(&head, tiles));
    body += &format!(r#"<div id="daylog" data-day="{}">{log}</div>"#, clock.day(shown));
    html::render(Page { title: "Status · baby-mcp", body, nav: Some(&user.display_name), refresh: Some(60), viewer: false, footer_extra: String::new() })
}

/// The start of the day `n` days after the day of `t`.
fn add_days(clock: &crate::time::Clock, t: i64, n: i64) -> i64 {
    let d = clock.zoned(t).date().checked_add(jiff::Span::new().days(n)).expect("date in range");
    clock.date_start(&d.to_string()).expect("valid date")
}

/// The week (of life: from the track's first day, in sevens) a day is in:
/// its number (from 1) and its first day.
fn week_of(clock: &crate::time::Clock, tr: &Track, day: i64) -> (i64, i64) {
    let idx = ((clock.day_start(day) - tr.from) as f64 / 86400.0).round() as i64;
    let w = idx.max(0) / 7;
    (w + 1, add_days(clock, tr.from, 7 * w))
}

/// The week below the timeline at the week zoom: the averages per day over
/// its complete days, the weight change, and a row per day (a day's link
/// zooms in on it: `data-zoom`).
fn weeklog(app: &App, c: &rusqlite::Connection, tr: &Track, day: i64, now: i64) -> Result<String, String> {
    let clock = &app.clock;
    let today = clock.day_start(now);
    let (n, start) = week_of(clock, tr, day);
    let end = add_days(clock, start, 7);
    let link = |d: i64, text: &str| {
        let date = clock.day(d);
        let href = if d >= today { "/".to_string() } else { format!("/?day={date}") };
        format!(r#"<a href="{href}" data-day="{date}">{text}</a>"#)
    };
    let prev_text = format!("← week {}", n - 1);
    let prev = if n > 1 { link(add_days(clock, start, -7), &prev_text) } else { format!(r#"<span class="next-none" aria-hidden="true">{prev_text}</span>"#) };
    let next_text = format!("week {} →", n + 1);
    let next = if end <= today { link(end, &next_text) } else { format!(r#"<span class="next-none" aria-hidden="true">{next_text}</span>"#) };
    let last_day = add_days(clock, start, 6);
    let span = format!("{} – {}", clock.zoned(start).strftime("%a %-d %b"), clock.zoned(last_day).strftime("%a %-d %b"));
    let life = app.baby().birth.map(|b| {
        let d0 = (start - clock.day_start(b)) / 86400 + 1;
        format!(" · day {}–{} of life", d0, d0 + 6)
    }).unwrap_or_default();
    let title = if start <= today && today < end { format!("Week {n} (this week)") } else { format!("Week {n}") };
    let head = format!(r#"<div class="head">{prev}<h2>{title}</h2>{next}</div><p class="mut">{span}{life}</p>"#);

    // Per day: totals; the averages over the complete days.
    let mut rows = String::new();
    let (mut k, mut feeds, mut ml, mut wet, mut dirty, mut breast) = (0u32, 0u32, 0.0, 0u32, 0u32, 0i64);
    let mut d = start;
    while d < end && d <= today {
        let e = clock.next_day_start(d);
        let t = en::totals_between(c, d, e.min(now + 1))?;
        let complete = e <= today;
        if complete {
            (k, feeds, ml, wet, dirty, breast) = (k + 1, feeds + t.feeds, ml + t.total_ml, wet + t.wet, dirty + t.dirty, breast + t.breast_min);
        }
        let label = clock.zoned(d).strftime("%a %-d").to_string();
        rows += &format!(
            r#"<tr><td class="time"><a href="{}" data-day="{}" data-zoom="{TL_SUMMARY}">{label}</a>{}</td><td>🍼 {}</td><td>{:.0} ml</td><td>💧 {}</td><td>💩 {}</td></tr>"#,
            if d >= today { "/".to_string() } else { format!("/?day={}", clock.day(d)) },
            clock.day(d),
            if complete { "" } else { r#" <span class="mut">so far</span>"# },
            t.feeds, t.total_ml, t.wet, t.dirty
        );
        d = e;
    }
    let avg = |v: f64| if k == 0 { "—".to_string() } else { format!("{:.1}", v / k as f64).trim_end_matches(".0").to_string() };
    let per = if k == 0 { "no complete day yet".to_string() } else { format!("per day, over {k} complete day{}", if k == 1 { "" } else { "s" }) };
    let mut out = format!(r#"<div class="card">{head}<div class="grid">"#);
    out += &stat("Feeds", &avg(feeds as f64), &if k == 0 { String::new() } else { format!("🤱 {:.0}′ at the breast a day", breast as f64 / k as f64) });
    out += &stat("🍼 Bottle", &if k == 0 { "—".into() } else { format!("{:.0} ml", ml / k as f64) }, &per);
    out += &stat("Diapers", &format!("💧 {} · 💩 {}", avg(wet as f64), avg(dirty as f64)), "wet · dirty a day");
    // Weight: the last weighing in the week, against the last one before it.
    let weighed = |from: i64, to: i64| -> Result<Option<(i64, f64)>, String> {
        Ok(en::list(c, from, to, false)?.iter().rev().find_map(|e| e.num("weight_g").map(|w| (e.at, w))))
    };
    if let Some((_, w)) = weighed(start, end.min(now + 1))? {
        let before = weighed(tr.from - 86400, start)?;
        let sub = before.map(|(_, b)| format!("{:+.0} g since the week before", w - b)).unwrap_or_default();
        out += &stat("⚖️ Weight", &format!("{w:.0} g"), &sub);
    }
    out += "</div></div>";
    out += &format!(r#"<div class="card"><h2>Days</h2><table class="week">{rows}</table></div>"#);
    Ok(out)
}

/// The day below the timeline: today's totals and log so far (entries
/// needing attention are in their own card at the start, not repeated), or
/// a past day's, with links to the days around it (`data-day`: the script
/// scrolls the timeline there instead).
/// The log of one day; today's (with `s`, the status) under "Today so far".
fn daylog(app: &App, c: &rusqlite::Connection, start: i64, now: i64, s: Option<&Status>) -> Result<String, String> {
    let clock = &app.clock;
    let today = clock.day_start(now);
    let end = clock.next_day_start(start);
    let entries = en::list(c, start, end.min(now + 1), false)?;
    let loggers = en::loggers(c, &entries)?;
    let totals = en::totals_between(c, start, end.min(now + 1))?;
    let link = |d: i64, text: &str| {
        let date = clock.day(d);
        let href = if d >= today { "/".to_string() } else { format!("/?day={date}") };
        format!(r#"<a href="{href}" data-day="{date}">{text}</a>"#)
    };
    if start >= today {
        // The last weighing, whichever day it was.
        let weight = s.and_then(|s| weight_line(s.last_weight.as_ref()?, &app.baby(), s.regained.flatten().is_some()));
        let mut out = totals_card("<h2>Today so far</h2>", &totals, weight);
        let rest: Vec<en::Entry> = entries.iter().filter(|e| e.attention_by == 0).cloned().collect();
        out += &log_card("<h2>Today's log</h2>", &rest, &loggers, clock, if entries.is_empty() { "Nothing logged yet today." } else { "Only entries that need attention so far." });
        return Ok(out);
    }
    let prev = clock.day_start(start - 3600);
    let short = |d: i64| clock.zoned(d).strftime("%a %-d").to_string();
    let next = if end >= today { link(today, "Today →") } else { link(end, &format!("{} →", short(end))) };
    let title = clock.zoned(start).strftime("%a %-d %B").to_string();
    let age = app.baby().birth.map(|b| format!(r#"<p class="mut">day {} of life</p>"#, (start - clock.day_start(b)) / 86400 + 1)).unwrap_or_default();
    let head = format!(r#"<div class="head">{}<h2>{}</h2>{next}</div>{age}"#, link(prev, &format!("← {}", short(prev))), esc(&title));
    let mut out = totals_card(&head, &totals, weight_of(&entries, &app.baby()));
    out += &log_card("<h2>Log</h2>", &entries, &loggers, clock, "Nothing logged this day.");
    Ok(out)
}

/// The day log of one day (`?date=…`), for the script to swap in; with
/// `z` the week zoom, the week it is in.
pub async fn log_part(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require(&app, &headers, "/") {
        return r;
    }
    let t = now();
    let clock = &app.clock;
    let Some(start) = q.get("date").and_then(|d| clock.date_start(d).ok()).map(|d| d.min(clock.day_start(t))) else {
        return (StatusCode::BAD_REQUEST, "bad date").into_response();
    };
    let z = q.get("z").and_then(|z| z.parse().ok()).filter(|z| *z < TL_ZOOM.len()).unwrap_or(TL_Z);
    let res = app.db.with(|c| {
        Ok((|| {
            if z == TL_WEEK {
                let tr = track(c, clock, &app.baby(), t, z)?;
                return weeklog(&app, c, &tr, start, t);
            }
            let s = if start >= clock.day_start(t) { Some(crate::status::status(c, clock, &app.baby(), t)?) } else { None };
            daylog(&app, c, start, t, s.as_ref())
        })())
    });
    match res {
        Ok(Ok(h)) => ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"), (axum::http::header::CACHE_CONTROL, "no-store")], h).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, en::err(e)).into_response(),
    }
}

/// A day: the status page with that day in view (yesterday by default).
pub async fn day(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require(&app, &headers, "/day") {
        return r;
    }
    let clock = &app.clock;
    let today = clock.day_start(now());
    let date = q.get("date").cloned().unwrap_or_else(|| clock.day(today - 3600));
    match clock.date_start(&date) {
        Ok(s) if s < today => Redirect::to(&format!("/?day={date}")).into_response(),
        Ok(_) => Redirect::to("/").into_response(),
        Err(e) => html::error_page(StatusCode::BAD_REQUEST, &e),
    }
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
    // Today, when the last weighing is at least a day ago.
    if age <= show && pts.last().is_none_or(|p| age - p.0 >= 1.0) {
        svg += &today_marker(x(age), 8.0, h - b, w - 8.0);
    }
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

/// A dashed vertical line at today, labelled, from `top` to `bottom`.
fn today_marker(x: f64, top: f64, bottom: f64, right_edge: f64) -> String {
    let anchor = if x > right_edge - 40.0 { "end" } else { "start" };
    let dx = if anchor == "end" { -4.0 } else { 4.0 };
    format!(
        r#"<line x1="{x:.1}" x2="{x:.1}" y1="{top:.1}" y2="{bottom:.1}" class="chart-today"/><text x="{:.1}" y="{:.1}" text-anchor="{anchor}" class="chart-today-l">today</text>"#,
        x + dx,
        top + 10.0
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
    // Today, when the last weighing is at least a day ago.
    let age = (now - birth) as f64 / 86400.0;
    if pts.last().is_none_or(|p| age - p.0 >= 1.0) {
        svg += &today_marker(x(age), 8.0, h - b, w - 8.0);
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

pub async fn charts(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let mut user = match require(&app, &headers, "/charts") {
        Ok(u) => u,
        Err(r) => return r,
    };
    // A parent can look at the page as a viewer sees it (?as=viewer).
    let preview = !user.viewer && q.get("as").map(String::as_str) == Some("viewer");
    user.viewer |= preview;
    // A viewer sees complete days only: up to the start of today, without
    // today's running totals or what they are expected to reach.
    let real_now = now();
    let t = if user.viewer { app.clock.day_start(real_now) - 1 } else { real_now };
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
    let last24 = match app.db.with(|c| Ok(en::totals_between(c, t - 86400, t + 1))) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => return html::error_page(StatusCode::INTERNAL_SERVER_ERROR, &en::err(e)),
    };
    let rest = (app.clock.next_day_start(t) - t) as f64 / 86400.0;
    let today = days.last().filter(|d| d.date == app.clock.day(t)).map(|d| d.totals.clone());
    let project = |m: fn(&en::Totals) -> f64| today.as_ref().filter(|_| !user.viewer).map(|td| expected_by_midnight(m(td), m(&last24), rest));
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
    if user.viewer {
        body += r#"<p class="mut">Up to and including yesterday.</p>"#;
    }
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
    // Parents: look at the page as a viewer (say a grandparent) sees it.
    let mut view_as = String::new();
    if !user.viewer || preview {
        view_as = format!(
            r#"<div class="viewas"><label for="viewas">View as</label><select id="viewas"><option value="user"{}>user</option><option value="viewer"{}>viewer</option></select></div>"#,
            if preview { "" } else { " selected" },
            if preview { " selected" } else { "" }
        );
    }
    html::render(Page { title: "Charts · baby-mcp", body, nav: Some(&user.display_name), refresh: Some(60), viewer: user.viewer, footer_extra: view_as })
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
    html::render(Page { title: "Add · baby-mcp", body, nav: Some(&user.display_name), refresh: None, viewer: false, footer_extra: String::new() })
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

/// The tab icon: the app icon as SVG (crisp at any size) and a 32 px PNG.
pub async fn icon_svg() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml"), (axum::http::header::CACHE_CONTROL, "max-age=86400")], include_bytes!("../static/icon.svg").as_slice()).into_response()
}

pub async fn icon_32() -> Response {
    png(include_bytes!("../static/icon-32.png"))
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

/// One field as a short chip with its emoji: "🍼 55 ml formula", "💧 wet".
/// An explicit no (false, or null: deliberately nothing) is flagged, for a
/// muted chip: "💩 no dirty diaper".
fn field_chip(name: &str, v: &Value) -> (String, bool) {
    let (emoji, label) = match name {
        "sleep_facing" => ("😴", "facing"),
        "bottle_breastmilk_ml" => ("🍼", "breast milk"),
        "bottle_formula_ml" => ("🍼", "formula"),
        "breast_left_min" => ("🤱", "left"),
        "breast_right_min" => ("🤱", "right"),
        "latch" => ("🤱", "latch"),
        "wet" => ("💧", "wet diaper"),
        "dirty" => ("💩", "dirty diaper"),
        "stool_color" => (v.as_str().map(stool_emoji).filter(|e| !e.is_empty()).unwrap_or("💩"), "stool"),
        "vomit" => ("🤢", "spit-up"),
        "temp_c" => ("🌡️", ""),
        "weight_g" => ("⚖️", ""),
        "bath" => (if v.as_str() == Some("sponge") { "🧽" } else { "🛁" }, "bath"),
        "vitamin_k" => ("💊", "vitamin K"),
        "vitamin_d" => ("💊", "vitamin D"),
        _ => ("", name),
    };
    let label = if label.is_empty() { name } else { label };
    let text = match (name, v) {
        (_, Value::Bool(false)) | (_, Value::Null) => return (format!("{emoji} no {label}").trim().to_string(), true),
        (_, Value::Bool(true)) => format!("{emoji} {label}"),
        ("temp_c" | "weight_g", _) => format!("{emoji} {}", field_value(name, v)),
        ("breast_left_min" | "breast_right_min", _) => format!("{emoji} {label} {}", field_value(name, v)),
        ("sleep_facing", Value::String(s)) => format!("{emoji} facing {s} {}", side_arrow(s)),
        ("bath", Value::String(s)) if s == "sponge" => format!("{emoji} sponge wash"),
        ("bath", Value::String(_)) => format!("{emoji} bath {}", field_value(name, v)),
        ("latch", _) => format!("{emoji} latch {}", field_value(name, v)),
        ("stool_color", _) => format!("{emoji} {} stool", field_value(name, v)),
        (_, Value::Number(_)) => format!("{emoji} {} {label}", field_value(name, v)),
        _ => format!("{emoji} {label}: {}", field_value(name, v)),
    };
    (text.trim().to_string(), false)
}

/// What changed from one version to the next, as short lines.
fn changes(clock: &crate::time::Clock, old: &en::Entry, new: &en::Entry) -> Vec<Line> {
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
    let chip = |k: &str, v: &Value, class: &str| {
        format!(r#"<span class="chip {class}" title="{}">{}</span>"#, esc(&format!("{k}: {}", field_value(k, v))), esc(&field_chip(k, v).0))
    };
    let mut chips = vec![];
    for k in keys {
        match (old.fields.get(k), new.fields.get(k)) {
            (None, Some(v)) => chips.push(chip(k, v, "add")),
            (Some(v), None) => chips.push(chip(k, v, "del")),
            (Some(a), Some(b)) if a != b => chips.push(format!(r#"<span class="chg">{} → {}</span>"#, chip(k, a, "del"), chip(k, b, "add"))),
            _ => {}
        }
    }
    let mut out: Vec<Line> = out.into_iter().map(Line::Text).collect();
    if !chips.is_empty() {
        out.push(Line::Html(format!(r#"<div class="chips">{}</div>"#, chips.concat())));
    }
    if (old.attention_by, &old.attention_why) != (new.attention_by, &new.attention_why) {
        match (new.attention_by, &new.attention_why) {
            (0, _) => out.push(Line::Text(format!("attention cleared{}", old.attention_why.as_ref().map(|w| format!(" (was: {w})")).unwrap_or_default()))),
            (by, why) => {
                let (u, a) = (by & en::BY_USER != 0, by & BY_AGENT != 0);
                out.push(Line::Text(format!("{} {}{}", attention_emoji(u, a), attention_label(u, a), why.as_ref().map(|w| format!(": {w}")).unwrap_or_default())));
            }
        }
    }
    if old.deleted != new.deleted {
        out.push(Line::Text((if new.deleted { "deleted" } else { "restored" }).into()));
    }
    out
}

/// Words and the whitespace between them, as separate tokens.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let mut space = None;
    for (i, c) in s.char_indices() {
        let sp = c.is_whitespace();
        if space.is_some_and(|w| w != sp) {
            out.push(&s[start..i]);
            start = i;
        }
        space = Some(sp);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// The new text with the words removed from the old one struck through and
/// the added ones marked, by the longest common subsequence of words. A
/// long text (unlikely here) falls back to old and new as a whole.
fn word_diff(old: &str, new: &str) -> String {
    let (a, b) = (tokens(old), tokens(new));
    if a.len() * b.len() > 250_000 {
        return format!("<del>{}</del> <ins>{}</ins>", esc(old), esc(new));
    }
    // lcs[i][j]: of a[i..] and b[j..].
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    // Ops: 0 same, 1 removed, 2 added; runs merged, with whitespace between
    // two changes of the same kind folded into them.
    let mut ops: Vec<(u8, String)> = vec![];
    let mut push = |op: u8, t: &str| match ops.last_mut() {
        Some((o, s)) if *o == op => s.push_str(t),
        _ => ops.push((op, t.to_string())),
    };
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            push(0, a[i]);
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            push(1, a[i]);
            i += 1;
        } else {
            push(2, b[j]);
            j += 1;
        }
    }
    let mut out = String::new();
    for (op, t) in &ops {
        match op {
            1 => out += &format!("<del>{}</del>", esc(t)),
            2 => out += &format!("<ins>{}</ins>", esc(t)),
            _ => out += &esc(t),
        }
    }
    out
}

/// A line of an entry's history: text (coloured by `history_line`) or
/// ready HTML (the field chips).
enum Line {
    Text(String),
    Html(String),
}

/// One line of an entry's history, coloured: added green, removed red, a
/// change as the old value struck through and the new one green.
fn history_line(l: &str) -> String {
    if l == "restored" || l == "created" {
        return format!(r#"<div><span class="add">{}</span></div>"#, esc(l));
    }
    if l == "deleted" {
        return format!(r#"<div><span class="del">{}</span></div>"#, esc(l));
    }
    if l.starts_with("attention cleared") {
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
        r#"<div class="card"><h1>{} {}</h1><p class="mut"><a href="/?day={day}">📅 {day}</a> · entry {id}{}</p><p>{}</p>"#,
        if cur.deleted { "🗑️" } else { "📝" },
        esc(&clock.fmt(cur.at)),
        if cur.deleted { " · deleted" } else { "" },
        esc(&cur.text),
    );
    if cur.attention_by != 0 {
        let (u, a) = (cur.attention_by & en::BY_USER != 0, cur.attention_by & BY_AGENT != 0);
        body += &format!(r#"<p class="warn">{} {}{}</p>"#, attention_emoji(u, a), esc(&attention_label(u, a)),
            cur.attention_why.as_ref().map(|w| format!(": {}", esc(w))).unwrap_or_default());
    }
    if !cur.fields.is_empty() {
        let order = |k: &str| crate::registry::FIELDS.iter().position(|f| f.name == k).unwrap_or(usize::MAX);
        let mut keys: Vec<&String> = cur.fields.keys().collect();
        keys.sort_by_key(|k| order(k));
        body += r#"<div class="chips">"#;
        for k in keys {
            let (text, no) = field_chip(k, &cur.fields[k]);
            body += &format!(r#"<span class="chip{}" title="{}">{}</span>"#, if no { " no" } else { "" }, esc(&format!("{k}: {}", field_value(k, &cur.fields[k]))), esc(&text));
        }
        body += "</div>";
    }
    body += "</div>";
    body += &format!(r#"<div class="card"><h2>History · {} version{}</h2><table>"#, versions.len(), if versions.len() == 1 { "" } else { "s" });
    for (i, v) in versions.iter().enumerate().rev() {
        let lines = match i {
            0 => {
                // Created: as the change from an empty entry.
                let empty = en::Entry { fields: Default::default(), attention_by: 0, attention_why: None, deleted: false, ..v.clone() };
                let mut l = vec![Line::Text("created".to_string())];
                l.extend(changes(clock, &empty, v));
                l
            }
            _ => changes(clock, &versions[i - 1], v),
        };
        let mut items: String = lines
            .iter()
            .map(|l| match l {
                Line::Text(t) => history_line(t),
                Line::Html(h) => h.clone(),
            })
            .collect();
        if i > 0 && versions[i - 1].text != v.text {
            items += &format!(r#"<div class="tdiff">{}</div>"#, word_diff(&versions[i - 1].text, &v.text));
        }
        body += &format!(
            r#"<tr><td class="time">v{}<div class="sub">{}</div></td><td class="hist">{}{}</td></tr>"#,
            v.version,
            esc(&clock.fmt(v.changed_at)),
            if items.is_empty() { r#"<div class="mut">no visible change</div>"#.to_string() } else { items },
            sub_line(&format!("by {}", who(v.changed_by)))
        );
    }
    body += "</table></div>";
    html::render(Page { title: &format!("Entry {id} · baby-mcp"), body, viewer: false, footer_extra: String::new(), nav: Some(&user.display_name), refresh: None })
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
        use crate::status::entry;
        use serde_json::json;
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let now = at("2025-11-07 10:00");
        let due = |day: &str, last: Option<(&str, &str)>| {
            let d = clock.date_start(day).unwrap();
            Bath { last: last.map(|(t, k)| entry(at(t), json!({"bath": k}))), last_any: None, due_day: d, due_at: d + 18 * 3600, day_end: d + 86400 }
        };
        // Last bath with soap on 6 Nov 13:45 → due Mon 10 Nov, next without soap.
        let t = bath_tile(&clock, &due("2025-11-10", Some(("2025-11-06 13:45", "with_soap"))), now);
        assert_eq!(t.at, Some(at("2025-11-10 18:00")));
        assert!(t.html.contains(">Mon 10 Nov<") && t.html.contains("in 3d8h") && t.html.contains(r#"<div class="side">🛁 without soap</div>"#) && !t.html.contains("next:"), "{}", t.html);
        assert!(t.html.contains("last 20h15m ago · 🧼 with soap") && !t.html.contains("overdue"), "{}", t.html);
        let t = bath_tile(&clock, &due("2025-11-05", Some(("2025-11-03 10:00", "without_soap"))), now);
        assert!(t.html.contains(r#"class="next overdue""#) && t.html.contains(">now<") && t.html.contains(">🧼 with soap<") && t.html.contains("⏰ 1d10h overdue"), "{}", t.html);
        // Due today, before or after the evening reminder: "today", not overdue.
        let t = bath_tile(&clock, &due("2025-11-07", Some(("2025-11-04 10:00", "with_soap"))), now);
        assert!(t.html.contains(">today<") && !t.html.contains("in 8h") && !t.html.contains("overdue") && t.html.contains(">🛁 without soap<"), "{}", t.html);
        // Nothing logged: no estimate, sorted last.
        let t = bath_tile(&clock, &due("2025-11-07", None), now);
        assert!(t.at.is_none() && t.html.contains("nothing logged yet") && !t.html.contains(r#"class="side""#));
    }

    #[test]
    fn vitamins() {
        use crate::status::vitamins;
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        // Active (and given today or not) for K and D.
        let v = |k: (bool, bool), d: (bool, bool)| vitamins(&clock, "2025-11-07", at("2025-11-07 18:00"), k, d);
        let t = vitamins_tile(&clock, &v((true, false), (true, false)), at("2025-11-07 10:00")).unwrap();
        assert!(t.html.contains(">today<") && t.html.contains("vitamin K + vitamin D still to give") && !t.html.contains("overdue"), "{}", t.html);
        assert!(t.html.contains("K until 22 Jan 2026 · D until 30 Oct 2029"), "{}", t.html);
        let t = vitamins_tile(&clock, &v((true, false), (true, true)), at("2025-11-07 18:30")).unwrap();
        assert!(t.html.contains(r#"class="next overdue""#) && t.html.contains("⏰ vitamin K still to give"), "{}", t.html);
        let t = vitamins_tile(&clock, &v((true, true), (true, true)), at("2025-11-07 19:00")).unwrap();
        assert!(t.html.contains(">tomorrow<") && t.html.contains("✓ K + D given today") && t.at == Some(at("2025-11-08 18:00")));
        let t = vitamins_tile(&clock, &v((false, false), (false, false)), at("2025-11-03 10:00")).unwrap();
        assert!(t.html.contains(">Fri 7 Nov<") && t.html.contains("starts"));
        assert!(vitamins_tile(&clock, &v((false, false), (false, false)), at("2031-01-01 10:00")).is_none());
        // Vitamin K not needed (enough formula yesterday): only D, and why.
        let mut vk = v((true, false), (true, false));
        (vk.k.not_needed, vk.formula_yesterday) = (true, 520.0);
        let t = vitamins_tile(&clock, &vk, at("2025-11-07 10:00")).unwrap();
        assert!(t.html.contains("vitamin D still to give") && t.html.contains("K not needed today (520 ml formula yesterday)"), "{}", t.html);
        vk.d.given_today = true;
        let t = vitamins_tile(&clock, &vk, at("2025-11-07 19:00")).unwrap();
        assert!(t.html.contains("✓ D given today") && !t.html.contains("K + D"), "{}", t.html);
    }

    #[test]
    fn grid_steps() {
        assert_eq!(grid_step(662.0, 5.0), 200.0);
        assert_eq!(grid_step(420.0, 5.0), 100.0);
        assert_eq!(grid_step(10.0, 5.0), 2.0);
        assert_eq!(grid_step(3.0, 5.0), 1.0, "counts: whole numbers");
    }

    #[test]
    fn one_diaper_tile() {
        use crate::status::{WetWarning, entry};
        use serde_json::json;
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let now = at("2025-11-07 13:54");
        let s = Status {
            now,
            last_wet: Some(entry(at("2025-11-07 09:00"), json!({"wet": true}))),
            last_dirty: Some(entry(at("2025-11-07 03:08"), json!({"dirty": true, "stool_color": "yellow"}))),
            ..Default::default()
        };
        let t = diaper_tile(&s);
        assert!(t.at.is_none(), "not predicted");
        assert!(t.html.contains(">4h54m ago<") && t.html.contains(">💧 · 💩🟡 10h46m ago<") && t.html.contains(r#"class="next""#), "{}", t.html);
        // Both in the last diaper: just what it was.
        let one = entry(at("2025-11-07 12:54"), json!({"wet": true, "dirty": true, "stool_color": "yellow"}));
        let both = Status { now, last_wet: Some(one.clone()), last_dirty: Some(one), ..Default::default() };
        assert!(diaper_tile(&both).html.contains(">1h00m ago<") && diaper_tile(&both).html.contains(">💧 · 💩🟡<"), "{}", diaper_tile(&both).html);
        // The warning makes the tile orange.
        let mut s2 = s.clone();
        s2.wet_warning = Some(WetWarning { wet: 4, minimum: 6, text: "Only 4 wet diapers in the last 24 h (usually at least 6 from day 6)".into() });
        let t = diaper_tile(&s2);
        assert!(t.html.contains("⚠️ Only 4 wet") && t.html.contains(r#"class="next overdue""#), "{}", t.html);
        assert!(diaper_tile(&Status::default()).html.contains("nothing logged yet"));
    }

    #[test]
    fn field_chips() {
        use serde_json::json;
        assert_eq!(field_chip("bottle_formula_ml", &json!(55)), ("🍼 55 ml formula".into(), false));
        assert_eq!(field_chip("dirty", &json!(false)), ("💩 no dirty diaper".into(), true));
        assert_eq!(field_chip("bottle_formula_ml", &Value::Null), ("🍼 no formula".into(), true));
        assert_eq!(field_chip("wet", &json!(true)), ("💧 wet diaper".into(), false));
        assert_eq!(field_chip("breast_left_min", &json!(12)), ("🤱 left 12 min".into(), false));
        assert_eq!(field_chip("weight_g", &json!(3900)), ("⚖️ 3900 g".into(), false));
        assert_eq!(field_chip("bath", &json!("with_soap")), ("🛁 bath with soap".into(), false));
        for f in crate::registry::FIELDS {
            assert!(!field_chip(f.name, &json!(1)).0.is_empty(), "{}", f.name);
        }
    }

    #[test]
    fn today_marker_only_a_day_after_the_last_weighing() {
        let birth = 1_760_000_000;
        let baby = en::Baby { name: "B".into(), birth: Some(birth), birth_weight_g: Some(3900.0), sex: None, growth: None, delivery: None, change: None,
                              aim_feeds_per_day: None, feed_prep_s: 0 };
        let w = [(birth + 3 * 86400, 3700.0)];
        assert!(!growth_chart(&baby, &w, birth + 3 * 86400 + 12 * 3600).contains("chart-today"), "weighed today");
        assert!(growth_chart(&baby, &w, birth + 5 * 86400).contains("chart-today"), "two days later");
    }

    #[test]
    fn word_diffs() {
        assert_eq!(word_diff("drank 60 ml formula", "drank 80 ml formula and a wet diaper"),
                   "drank <del>60</del><ins>80</ins> ml formula<ins> and a wet diaper</ins>");
        assert_eq!(word_diff("same", "same"), "same");
        assert_eq!(word_diff("", "new <text>"), "<ins>new &lt;text&gt;</ins>");
        assert_eq!(word_diff("a b c", "a c"), "a <del>b </del>c");
    }

    #[test]
    fn timeline_strip() {
        use serde_json::json;
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let now = at("2025-11-07 14:00");
        let e = |t: &str, f: serde_json::Value| en::Entry { entry_id: 1, version: 1, at: at(t), duration_s: None, text: String::new(),
            fields: f.as_object().cloned().unwrap(), attention_by: 0, attention_why: None, deleted: false, changed_at: 0, changed_by: 1, superseded_at: None };
        let entries = [e("2025-11-05 09:00", json!({"bottle_formula_ml": 50})),
                       e("2025-11-07 07:00", json!({"bottle_formula_ml": 60})),
                       e("2025-11-07 10:30", json!({"bottle_formula_ml": 80, "wet": true, "dirty": true})),
                       e("2025-11-07 12:00", json!({"vitamin_d": true, "vitamin_k": true}))];
        let s = Status { next_feed: Some(crate::status::FeedEstimate { at: at("2025-11-07 13:30"), gap: 0, n: 0 }), ..Default::default() };
        let tr = Track { from: at("2025-11-05 00:00"), to: now + TL_FUTURE, z: TL_Z };
        let days = tl_days(&clock, &tr, now);
        assert_eq!(days, vec![(tr.from, at("2025-11-06 00:00")), (at("2025-11-06 00:00"), at("2025-11-07 00:00")), (at("2025-11-07 00:00"), tr.to)], "today runs on");
        let today = tl_day(&clock, &tr, &entries, Some(&s), now, days[2].0, days[2].1);
        assert!(today.starts_with(&format!(r#"<svg x="{:.1}" width="{:.1}""#, TL_PAD + 48.0 * 38.0, 26.0 * 38.0)) && today.contains(r#"data-day="2025-11-07""#), "{today}");
        assert_eq!(today.matches(">🍼</text>").count(), 6, "two logged feeds, one overdue, three later (until 02:00): {today}");
        assert!(today.contains(r#"opacity=".25"><title>17:30 feed"#), "the next after an overdue one: now + the usual gap, fainter: {today}");
        assert!(today.contains("10:30 🍼 80 ml formula") && today.contains("💩") && today.contains("💊"));
        assert!(today.contains(r#"💩<tspan x="406.0" y="42.0" class="badge">💧</tspan>"#), "wet and dirty: a drop on the poo: {today}");
        assert!(today.contains("13:30 feed (overdue)") && today.contains(r#"class="ring""#), "overdue feed ringed");
        assert!(!today.contains("diaper (expected)"), "diapers aren't predicted");
        assert!(today.contains(r#"class="hd">10<"#) && today.contains(r#"class="hn">04<"#) && today.contains(r#"class="night""#), "hours over the night band");
        assert!(today.contains(r#"x="0.0" y="88" text-anchor="middle" class="hn">Fri 7<"#) && !today.contains(r#">00<"#), "midnight: the day instead of 00");
        assert!(today.contains(r#"class="hn">Sat 8<"#) && today.contains(r#"class="now""#), "on into tomorrow, with now");
        // A past day: what was logged, nothing expected.
        let past = tl_day(&clock, &tr, &entries, None, now, days[0].0, days[0].1);
        assert!(past.contains("09:00 🍼 50 ml formula") && !past.contains("expected") && !past.contains(r#"class="now""#), "{past}");
        // With a preparation time the expected feed is the start.
        let mut s2 = s.clone();
        s2.next_feed = Some(crate::status::FeedEstimate { at: at("2025-11-07 15:30"), gap: 0, n: 0 });
        s2.prep_s = 15 * 60;
        let all = tl_render(&clock, &tr, &entries, &s2, now, now - TL_PAST, tr.to);
        assert!(all.contains("15:15 start (feed ~15:30) (expected)"), "{all}");
        assert!(all.contains("18:45 start (feed ~19:00)") && all.contains("01:45 start (feed ~02:00)") && !all.contains("feed ~05:30"), "{all}");
        assert_eq!(all.matches("<svg ").count(), 3, "the last 48 hours: three days");
        let page = timeline(&clock, &tr, &all, now, None);
        assert!(page.contains(&format!(r#"data-days="2025-11-05:{},2025-11-06:"#, tr.from)) && page.contains(&format!(r#"width="{:.0}""#, tr.width())), "{page}");
        // Zoomed out: smaller emojis, fewer labels; widest: a summary per day.
        let tr2 = Track { z: 1, ..tr };
        let out = tl_day(&clock, &tr2, &entries, Some(&s), now, days[2].0, days[2].1);
        assert!(out.contains(r#"class="emo s""#) && out.contains(r#"class="hd">12<"#) && !out.contains(r#"class="hd">10<"#), "{out}");
        // By week: the first day of a week of life summarises it, per day.
        let trw = Track { z: TL_WEEK, ..tr };
        let wk = tl_day(&clock, &trw, &entries, None, now, days[0].0, days[0].1);
        assert!(wk.contains(">week 1<") && wk.contains(">🍼0.5<") && wk.contains(">💧0 💩0<") && wk.contains(">25 ml<"), "two full days (5, 6 Nov), one feed of 50 ml: {wk}");
        let wk2 = tl_day(&clock, &trw, &entries, None, now, days[1].0, days[1].1);
        assert!(!wk2.contains("week") && !wk2.contains("class=\"sum\""), "not a week's first day: {wk2}");
        let tr3 = Track { z: TL_SUMMARY, ..tr };
        let sum = tl_day(&clock, &tr3, &entries, Some(&s), now, days[2].0, days[2].1);
        assert!(sum.contains(">🍼2<") && sum.contains(">💧1 💩1<") && sum.contains(">140 ml<") && sum.contains(">Fri 7<") && !sum.contains("expected"), "{sum}");
    }

    #[test]
    fn whats_next_order() {
        let clock = crate::time::Clock::new("Europe/Amsterdam").unwrap();
        let at = |s: &str| clock.parse(s, 0).unwrap();
        let now = at("2025-11-07 15:00");
        let html = whats_next("", vec![
            next_tile_extra(&clock, "🍼 Feed", Some((at("2025-11-07 13:00"), "")), Some(at("2025-11-07 16:00")), "", false, now),
            next_tile_extra(&clock, "💩 Dirty diaper", None, None, "", false, now),
            next_tile_extra(&clock, "💧 Wet diaper", Some((at("2025-11-07 11:00"), "")), Some(at("2025-11-07 14:30")), "", false, now),
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
