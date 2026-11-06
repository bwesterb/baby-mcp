//! MCP over streamable HTTP, stateless, JSON responses only: a small
//! hand-written JSON-RPC layer (initialize, ping, tools/list, tools/call).

use crate::app::App;
use crate::auth;
use crate::entries::{self as en, BY_AGENT, BY_USER, Entry, NewEntry};
use crate::registry::{self, FIELDS};
use crate::time::now;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::ValueRef;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DUPLICATE_WINDOW: i64 = 15 * 60;

const INSTRUCTIONS: &str = r#"A baby log. Each entry is one event or episode (a feed, a diaper, a measurement, or several at the same moment) with a start time, an optional duration, the text the user said, and structured fields from a fixed registry (see the `fields` tool).

Logging ("drank 60 ml of pumped milk and had a wet diaper"):
- Call `log` once per moment. Put everything that happened at that moment in one entry; split events at different times into separate entries.
- Text: the user's own words where possible (any language); when it was a conversation, a short faithful paraphrase.
- Fields: extract every registry field the user mentions. A key absent means unknown/not stated; a key with null means deliberately nothing ("no bottle this time"). false and 0 are ordinary values.
- Bottle milk: expressed breast milk in bottle_breastmilk_ml, formula in bottle_formula_ml (a mixed bottle has both). At the breast: breast_left_min/breast_right_min and/or latch.
- Sleeping position: "put her down facing left" → sleep_facing=left (the side her face is turned to; not the breast side). The parents alternate it after every feed; `status` gives the next side under sleep_facing, assuming they kept alternating after feeds logged since the last logged side (feeds_since, assumed_current); a logged side resets that.
- Bath: "bathed her with soap" → bath=with_soap; "in the bath, no soap" / "just water" → bath=without_soap; "washed with a sponge/washcloth" (no bath) → bath=sponge. Goal: two baths a week, alternating with and without soap; `status` gives under bath when the next is due and next_kind (soap or not).
- Vitamins: "gave vitamin D (and K)" → vitamin_d=true (vitamin_k=true). Dutch advice: K daily from day 8 until 12 weeks, D daily from day 8 until 4 years; `status` lists under vitamins.to_give_today what is still due today.
- Times are local; bare clock times mean today, or yesterday if that would be in the future. Report back the time you logged.
- If `log` reports a likely duplicate (someone else may have logged it already), ask the user before logging again with confirm=true.

Corrections: `update` or `delete` by entry id (from `entries`, `status` or `attention`); every earlier version is kept (`history`).
Partial information ("she's drinking now, I'll tell you how much later", "had a bottle, not sure how much"): log right away with what is known, and set needs_attention_by=["user"] with needs_attention_why saying exactly what is missing (e.g. "amount not given yet", "still feeding: add duration and amount"). The parents then see it as 🙋 on the status page and get a notification, so it isn't lost if this conversation is forgotten. When the rest comes, `update` the entry and clear the attention (needs_attention_by=[]).
Attention: entries logged on the web form without an agent are marked for an agent (needs_attention by agent); process them with `attention` and `update`: extract the fields, set `at` from the text if it mentions a time (the stored time is when it was typed), then clear the attention.
Sleep is not tracked; don't log sleeps.
How much and when: `status` has feeding_guidance (a daily target from age and weight, the last 24 hours against it, and a suggested amount for the next feed). It is a rule-of-thumb guide; a feeding plan from the nurse, midwife or doctor that the user mentions always takes precedence, and say so when they differ. Mention that intake at the breast isn't measured. It follows the NVK minimum for healthy term babies (feeding_guidance.minimum: at least 6 feeds per 24 h and pauses of at most 6 h until 14 days old and back above birth weight, then 5 and 8 h until 8 weeks, then 4 and 12 h); next_feed.latest_at is the latest time for the next feed under that minimum: the end of the longest pause or, if earlier, when the last 24 h would hold fewer than the minimum number of feeds (latest_reason says which). The parents aim for at least aim_feeds_per_day feeds (planned on top of the minimum); The parents start the routine (diaper first) some minutes before a feed (next_feed_start.prep): next_feed_start (and next_feed.latest_start_at) say when to start; when they ask when to feed, give both. feeding_guidance.breast_side suggests which breast to start with (the one used less over the last 24 h, else alternating). feeding_guidance.warnings says when it gets close to the longest pause or to only the minimum number of feeds in 24 h: mention those first.
Wet diapers: `status` has wet_diaper_warning when the last 24 hours had fewer than usual for her age (day of life up to day 5, then 6); mention it. Questions: `status` for the current situation (last feed and diapers, estimates of the next feed, wet and dirty diaper, today's totals), `stats` for per-day totals, `entries` for the raw log, `sql` for anything else.
If the user wants something these tools cannot do, or only awkwardly, do what you can and call `suggest` so the developer can add it."#;

fn field_schema() -> Value {
    let mut props = Map::new();
    for f in FIELDS {
        let mut s = match f.kind {
            registry::Kind::Number { min, max } => json!({"type": ["number", "null"], "minimum": min, "maximum": max}),
            registry::Kind::Integer { min, max } => json!({"type": ["integer", "null"], "minimum": min, "maximum": max}),
            registry::Kind::Bool => json!({"type": ["boolean", "null"]}),
            registry::Kind::Choice(o) => {
                let mut e: Vec<Value> = o.iter().map(|x| json!(x)).collect();
                e.push(Value::Null);
                json!({"enum": e})
            }
        };
        let unit = if f.unit.is_empty() { String::new() } else { format!(" ({})", f.unit) };
        s["description"] = json!(format!("{}{unit}", f.help));
        props.insert(f.name.into(), s);
    }
    json!({"type": "object", "properties": props, "additionalProperties": false,
           "description": "Structured fields. Omit what is unknown; null = deliberately nothing."})
}

fn attention_schema() -> Value {
    json!({"type": "array", "items": {"enum": ["user", "agent"]},
           "description": "who still needs to look at this entry (empty = nobody)"})
}

fn tools() -> Value {
    let id = json!({"type": "integer", "description": "entry id"});
    json!([
        {"name": "log", "title": "Log an entry",
         "description": "Add an entry: what happened at one moment, with structured fields. Refuses a likely duplicate (an entry within 15 minutes sharing a feed/diaper/… group) unless confirm=true. With only partial information, log now and set needs_attention_by=[\"user\"] with what is missing in needs_attention_why, so the parents finish it later.",
         "inputSchema": {"type": "object", "required": ["text"], "properties": {
            "text": {"type": "string", "description": "what the user said (or a faithful paraphrase)"},
            "at": {"type": "string", "description": "start time: HH:MM, YYYY-MM-DD HH:MM or RFC 3339; default now"},
            "duration_min": {"type": "number", "description": "duration in minutes, if known and final"},
            "fields": field_schema(),
            "needs_attention_by": attention_schema(),
            "needs_attention_why": {"type": "string", "description": "required with needs_attention_by: what is missing or needs checking"},
            "confirm": {"type": "boolean", "description": "log even if it looks like a duplicate"}}},
         "annotations": {"destructiveHint": false, "openWorldHint": false}},
        {"name": "update", "title": "Update an entry",
         "description": "Change an entry. Fields are merged: given keys are set (null = deliberately nothing), keys in `remove` become unknown again. Earlier versions are kept.",
         "inputSchema": {"type": "object", "required": ["entry_id"], "properties": {
            "entry_id": id,
            "at": {"type": "string"},
            "duration_min": {"type": ["number", "null"], "description": "final duration in minutes; null clears it"},
            "text": {"type": "string", "description": "replaces the text"},
            "fields": field_schema(),
            "remove": {"type": "array", "items": {"type": "string"}, "description": "field names to make unknown"},
            "needs_attention_by": attention_schema(),
            "needs_attention_why": {"type": "string"}}},
         "annotations": {"destructiveHint": true, "idempotentHint": true, "openWorldHint": false}},
        {"name": "delete", "title": "Delete an entry", "description": "Mark an entry deleted (it can be undeleted).",
         "inputSchema": {"type": "object", "required": ["entry_id"], "properties": {"entry_id": id}},
         "annotations": {"destructiveHint": true, "idempotentHint": true, "openWorldHint": false}},
        {"name": "undelete", "title": "Undelete an entry", "description": "Restore a deleted entry.",
         "inputSchema": {"type": "object", "required": ["entry_id"], "properties": {"entry_id": id}},
         "annotations": {"destructiveHint": false, "idempotentHint": true, "openWorldHint": false}},
        {"name": "entries", "title": "List entries",
         "description": "Entries in a time range, oldest first. Default: today (local).",
         "inputSchema": {"type": "object", "properties": {
            "from": {"type": "string", "description": "YYYY-MM-DD or a time; default start of today"},
            "to": {"type": "string", "description": "YYYY-MM-DD (exclusive day) or a time; default now"},
            "include_deleted": {"type": "boolean"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "history", "title": "Entry history", "description": "All versions of one entry, oldest first.",
         "inputSchema": {"type": "object", "required": ["entry_id"], "properties": {"entry_id": id}},
         "annotations": {"readOnlyHint": true}},
        {"name": "attention", "title": "Entries needing attention",
         "description": "Entries that still need a person and/or an agent to look at them (e.g. web entries whose fields are not extracted yet).",
         "inputSchema": {"type": "object", "properties": {
            "by": {"enum": ["user", "agent", "any"], "description": "default any"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "status", "title": "Current status",
         "description": "Last feed, diapers, temperature and weight with how long ago; weight percentile (WHO weight-for-age), birth-weight percentile and whether birth weight is regained; estimates of the next feed, wet diaper and dirty diaper (median recent gap; due_in or overdue_by); feeding_guidance (daily target from age and weight, last 24 h vs target, suggested next amount); today's and last 24 h totals; entries needing attention.",
         "inputSchema": {"type": "object", "properties": {}}, "annotations": {"readOnlyHint": true}},
        {"name": "stats", "title": "Per-day statistics",
         "description": "Per local day: feeds, ml of breast milk and formula, breast minutes, diapers, spit-ups, pumped ml, temperature range, weight vs birth weight and its WHO weight-for-age percentile.",
         "inputSchema": {"type": "object", "properties": {
            "from": {"type": "string", "description": "YYYY-MM-DD, default 7 days ago"},
            "to": {"type": "string", "description": "YYYY-MM-DD (inclusive), default today"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "fields", "title": "Field registry", "description": "The structured fields entries can have, with types, units and allowed values.",
         "inputSchema": {"type": "object", "properties": {}}, "annotations": {"readOnlyHint": true}},
        {"name": "sql", "title": "Read-only SQL",
         "description": "Run one read-only SQLite query for questions the other tools don't answer. Table `entry` (current versions; deleted=0 for live ones): entry_id, at (Unix seconds UTC), duration_s, text, fields (JSON), needs_attention_by, needs_attention_why, deleted, changed_at, changed_by, plus one column per registry field (e.g. bottle_formula_ml, wet). `entry_old` has earlier versions. Times: use datetime(at, 'unixepoch') (UTC; local is Europe/Amsterdam). At most 500 rows.",
         "inputSchema": {"type": "object", "required": ["query"], "properties": {"query": {"type": "string"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "suggest", "title": "Suggest a feature",
         "description": "Leave a suggestion for the developer when the user asked for something the tools could not do, or only awkwardly. Say what the user wanted, what was missing, and what you did instead.",
         "inputSchema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}},
         "annotations": {"destructiveHint": false, "openWorldHint": false}}
    ])
}

pub async fn handle(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let user = match auth::bearer_user(&app, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Json(rpc_error(Value::Null, -32700, "parse error")).into_response(),
    };
    if let Some(batch) = req.as_array() {
        let out: Vec<Value> = batch.iter().filter_map(|r| one(&app, user, r)).collect();
        return if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { Json(out).into_response() };
    }
    match one(&app, user, &req) {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

pub async fn not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "use POST").into_response()
}

fn rpc_error(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

fn one(app: &App, user: i64, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req["method"].as_str().unwrap_or("");
    let id = id?; // notifications get no response
    let result = match method {
        "initialize" => json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "baby-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": format!("This log is for {}.\n\n{INSTRUCTIONS}", app.cfg.baby.name),
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or("");
            let args = req["params"].get("arguments").cloned().unwrap_or(json!({}));
            let started = Instant::now();
            let r = call(app, user, name, &args);
            eprintln!("mcp: user {user} {name} {} in {:?}", if r.is_ok() { "ok" } else { "error" }, started.elapsed());
            match r {
                Ok(v) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap()}],
                    "structuredContent": if v.is_object() { v.clone() } else { json!({"result": v}) },
                }),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        _ => return Some(rpc_error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

// ------------------------------------------------------------- tools

fn arg_i64(a: &Value, k: &str) -> Result<i64, String> {
    a[k].as_i64().ok_or_else(|| format!("{k} is required (an integer)"))
}

fn attention_arg(a: &Value) -> Result<Option<i64>, String> {
    match a.get("needs_attention_by") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(v)) => {
            let mut by = 0;
            for x in v {
                by |= match x.as_str() {
                    Some("user") => BY_USER,
                    Some("agent") => BY_AGENT,
                    _ => return Err("needs_attention_by: use \"user\" and/or \"agent\"".into()),
                };
            }
            Ok(Some(by))
        }
        _ => Err("needs_attention_by must be a list".into()),
    }
}

fn fields_arg(a: &Value) -> Result<Map<String, Value>, String> {
    match a.get("fields") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => registry::validate_fields(m),
        _ => Err("fields must be an object".into()),
    }
}

fn duration_arg(a: &Value) -> Result<Option<Option<i64>>, String> {
    match a.get("duration_min") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => match v.as_f64() {
            Some(m) if (0.0..=7.0 * 1440.0).contains(&m) => Ok(Some(Some((m * 60.0).round() as i64))),
            _ => Err("duration_min must be minutes between 0 and a week".into()),
        },
    }
}

fn call(app: &App, user: i64, name: &str, a: &Value) -> Result<Value, String> {
    let clock = &app.clock;
    let t = now();
    match name {
        "log" => {
            let text = a["text"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("text is required")?;
            let at = clock.parse(a["at"].as_str().unwrap_or(""), t)?;
            let fields = fields_arg(a)?;
            let by = attention_arg(a)?.unwrap_or(0);
            let why = a["needs_attention_why"].as_str().map(str::trim).filter(|w| !w.is_empty()).map(str::to_string).filter(|_| by != 0);
            if by != 0 && why.is_none() {
                return Err("needs_attention_why is required with needs_attention_by: say what is missing".into());
            }
            let n = NewEntry { at, duration_s: duration_arg(a)?.flatten(), text: text.into(), fields, attention_by: by, attention_why: why };
            app.db.with(|c| {
                Ok((|| {
                    let names = en::user_names(c)?;
                    let probe = Entry {
                        entry_id: 0, version: 0, at, duration_s: None, text: String::new(), fields: n.fields.clone(),
                        attention_by: 0, attention_why: None, deleted: false, changed_at: 0, changed_by: 0, superseded_at: None,
                    };
                    let groups = probe.groups();
                    if !a["confirm"].as_bool().unwrap_or(false) && !groups.is_empty() {
                        let dups = en::nearby(c, at, DUPLICATE_WINDOW, &groups)?;
                        if !dups.is_empty() {
                            return Ok(json!({
                                "logged": false,
                                "warning": "possible duplicate: entries within 15 minutes with the same kind of event already exist. Ask the user; to log anyway, call log again with confirm=true.",
                                "existing": dups.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>(),
                            }));
                        }
                    }
                    let e = en::insert(c, user, &n, t)?;
                    Ok(json!({"logged": true, "entry": en::view(&e, clock, &names)}))
                })())
            }).map_err(en::err)?
        }
        "update" => {
            let id = arg_i64(a, "entry_id")?;
            let fields = fields_arg(a)?;
            app.db.with(|c| {
                Ok((|| {
                    let mut e = en::get(c, id)?;
                    if let Some(at) = a["at"].as_str() {
                        e.at = clock.parse(at, t)?;
                    }
                    if let Some(d) = duration_arg(a)? {
                        e.duration_s = d;
                    }
                    if let Some(text) = a["text"].as_str() {
                        e.text = text.trim().to_string();
                    }
                    e.fields.extend(fields);
                    if let Some(rm) = a["remove"].as_array() {
                        for k in rm.iter().filter_map(Value::as_str) {
                            e.fields.remove(k);
                        }
                    }
                    if let Some(by) = attention_arg(a)? {
                        e.attention_by = by;
                        e.attention_why = if by == 0 { None } else {
                            Some(a["needs_attention_why"].as_str().map(str::to_string)
                                .or(e.attention_why.clone()).ok_or("needs_attention_why is required")?)
                        };
                    } else if let Some(why) = a["needs_attention_why"].as_str() {
                        e.attention_why = Some(why.into());
                    }
                    let e = en::save(c, user, &e, t)?;
                    Ok(json!({"updated": true, "entry": en::view(&e, clock, &en::user_names(c)?)}))
                })())
            }).map_err(en::err)?
        }
        "delete" | "undelete" => {
            let id = arg_i64(a, "entry_id")?;
            app.db.with(|c| {
                Ok((|| {
                    let mut e = en::get(c, id)?;
                    let want = name == "delete";
                    if e.deleted != want {
                        e.deleted = want;
                        e = en::save(c, user, &e, t)?;
                    }
                    Ok(json!({name: true, "entry": en::view(&e, clock, &en::user_names(c)?)}))
                })())
            }).map_err(en::err)?
        }
        "entries" => {
            let from = range_start(app, a["from"].as_str(), clock.day_start(t), t)?;
            let to = range_end(app, a["to"].as_str(), t + 1, t)?;
            app.db.with(|c| {
                Ok((|| {
                    let names = en::user_names(c)?;
                    let list = en::list(c, from, to, a["include_deleted"].as_bool().unwrap_or(false))?;
                    Ok(json!({"from": clock.fmt(from), "to": clock.fmt(to),
                              "entries": list.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>()}))
                })())
            }).map_err(en::err)?
        }
        "history" => {
            let id = arg_i64(a, "entry_id")?;
            app.db.with(|c| {
                Ok((|| {
                    let names = en::user_names(c)?;
                    Ok(json!({"versions": en::history(c, id)?.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>()}))
                })())
            }).map_err(en::err)?
        }
        "attention" => {
            let mask = match a["by"].as_str().unwrap_or("any") {
                "user" => BY_USER,
                "agent" => BY_AGENT,
                _ => BY_USER | BY_AGENT,
            };
            app.db.with(|c| {
                Ok((|| {
                    let names = en::user_names(c)?;
                    Ok(json!({"entries": en::attention(c, mask)?.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>()}))
                })())
            }).map_err(en::err)?
        }
        "status" => app.db.with(|c| Ok(en::status(c, clock, &app.baby(), t))).map_err(en::err)?,
        "stats" => {
            let today = clock.day_start(t);
            let from = match a["from"].as_str() { Some(d) => clock.date_start(d)?, None => clock.day_start(today - 6 * 86400 + 3600) };
            let to = match a["to"].as_str() { Some(d) => clock.next_day_start(clock.date_start(d)?), None => t + 1 };
            if to - from > 400 * 86400 {
                return Err("range too long; at most about a year".into());
            }
            let days = app.db.with(|c| Ok(en::stats(c, clock, &app.baby(), from, to))).map_err(en::err)??;
            Ok(json!({"days": days}))
        }
        "fields" => Ok(json!({"fields": FIELDS.iter().map(|f| f.describe()).collect::<Vec<_>>(),
            "notes": "Absent = unknown/not stated; null = deliberately nothing. Booleans false and numbers 0 are ordinary values."})),
        "sql" => run_sql(app, a["query"].as_str().unwrap_or("")),
        "suggest" => {
            let text = a["text"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("text is required")?;
            app.db
                .with(|c| c.execute("INSERT INTO suggestion (at, by, text) VALUES (?1, ?2, ?3)", rusqlite::params![t, user, text]))
                .map_err(en::err)?;
            eprintln!("suggestion recorded");
            Ok(json!({"result": "Thanks, the suggestion was recorded for the developer."}))
        }
        _ => Err(format!("unknown tool {name:?}")),
    }
}

fn range_start(app: &App, s: Option<&str>, default: i64, now: i64) -> Result<i64, String> {
    match s {
        None => Ok(default),
        Some(s) if s.len() == 10 => app.clock.date_start(s),
        Some(s) => app.clock.parse(s, now),
    }
}

fn range_end(app: &App, s: Option<&str>, default: i64, now: i64) -> Result<i64, String> {
    match s {
        None => Ok(default),
        Some(s) if s.len() == 10 => Ok(app.clock.next_day_start(app.clock.date_start(s)?)),
        Some(s) => app.clock.parse(s, now),
    }
}

fn run_sql(app: &App, q: &str) -> Result<Value, String> {
    let q = q.trim().trim_end_matches(';');
    if q.is_empty() {
        return Err("query is required".into());
    }
    let mut c = crate::db::Db::open_read_only(&app.cfg.db_path()).map_err(en::err)?;
    // Only the log itself is readable: never passkeys, sessions, tokens or the
    // signing key, which live in the same database.
    c.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Read { table_name, column_name } => match table_name {
            "entry" | "entry_old" | "suggestion" | "sqlite_master" | "sqlite_schema" => Authorization::Allow,
            "user" if matches!(column_name, "id" | "name" | "display_name") => Authorization::Allow,
            _ => Authorization::Deny,
        },
        AuthAction::Select | AuthAction::Function { .. } | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Pragma { pragma_name, .. }
            if matches!(pragma_name, "table_info" | "table_xinfo" | "table_list") =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }))
    .map_err(en::err)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    c.progress_handler(10_000, Some(move || Instant::now() > deadline)).map_err(en::err)?;
    let mut st = c.prepare(q).map_err(|e| format!("sql: {e}"))?;
    if !st.readonly() {
        return Err("only read-only queries are allowed".into());
    }
    let cols: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
    let mut rows = st.query([]).map_err(|e| format!("sql: {e}"))?;
    let mut out = vec![];
    let mut truncated = false;
    while let Some(r) = rows.next().map_err(|e| format!("sql: {e}"))? {
        if out.len() == 500 {
            truncated = true;
            break;
        }
        let mut row = Map::new();
        for (i, name) in cols.iter().enumerate() {
            let v = match r.get_ref(i).map_err(|e| format!("sql: {e}"))? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(x) => json!(x),
                ValueRef::Real(x) => json!(x),
                ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                ValueRef::Blob(b) => json!(format!("<{} bytes>", b.len())),
            };
            row.insert(name.clone(), v);
        }
        out.push(Value::Object(row));
    }
    Ok(json!({"columns": cols, "rows": out, "truncated": truncated}))
}
