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

const INSTRUCTIONS: &str = r#"A baby log. Each entry is one moment (a feed, a diaper, a measurement, or several at once) with a start time, an optional duration, the text as said, and structured fields (their meanings are in the `log` tool's schema).

Logging ("drank 60 ml of pumped milk and had a wet diaper"):
- One `log` per moment, with everything that happened then; events at different times are separate entries.
- Text: the user's own words (any language), or a short faithful paraphrase of a conversation.
- Fields: fill in every field the user mentions. Leave out what wasn't said; null means deliberately nothing ("no bottle this time"). Sleep itself isn't tracked, only the side the baby is put down facing.
- After logging a feed, say which side to put her down facing: `status` sleep_facing.assumed_current (it counts that feed).
- Times are local; a bare clock time means today, or yesterday if that would be in the future. Tell the user the time you logged.
- Partial information ("she's drinking now, I'll tell you how much later"): log it now with needs_attention_by=["user"] and say in needs_attention_why what is missing. The parents see it on the status page and get a notification, so nothing is lost if this conversation is forgotten. Complete it later with `update` and clear the attention.
- If `log` reports a likely duplicate (someone else may already have logged it), ask before logging again with confirm=true.

Corrections: `update`, `delete` and `undelete` by entry id; `history` shows every earlier version.
Entries typed on the web form wait for an agent (`attention` by agent): extract the fields, set `at` from the text if it names a time (the stored time is when it was typed), and clear the attention.

Answering: use `status` for anything about now: what's next, when and how much. Its feeding guidance is a rule of thumb; a plan from the midwife, nurse or doctor that the user mentions takes precedence, so say so when they differ, and mention that intake at the breast isn't measured. For "when to feed", give the time to start (diaper first) and the feed time; put warnings first. Use `stats` for per-day totals, `entries` for the log, `sql` for anything else.
If the user wants something the tools can't do (or only awkwardly), do what you can and call `suggest`."#;

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

fn tools(tz: &str) -> Value {
    let id = json!({"type": "integer", "description": "entry id"});
    json!([
        {"name": "log", "title": "Log an entry",
         "description": "Add an entry for one moment. Refuses a likely duplicate (an entry within 15 minutes of the same kind: feed, diaper, …) unless confirm=true.",
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
         "description": "Change an entry. Fields are merged: given keys are set, keys in `remove` become unknown again.",
         "inputSchema": {"type": "object", "required": ["entry_id"], "properties": {
            "entry_id": id,
            "at": {"type": "string", "description": "new start time: HH:MM (on the entry's own day), YYYY-MM-DD HH:MM or RFC 3339"},
            "duration_min": {"type": ["number", "null"], "description": "final duration in minutes; null clears it"},
            "text": {"type": "string", "description": "replaces the text"},
            "fields": {"type": "object", "description": "the same fields as in `log` (null = deliberately nothing)"},
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
         "description": "Entries in a time range, oldest first. Default: today (local). At most `limit` entries: with more, truncated is true and next_from is where to continue.",
         "inputSchema": {"type": "object", "properties": {
            "from": {"type": "string", "description": "YYYY-MM-DD or a time; default start of today"},
            "to": {"type": "string", "description": "YYYY-MM-DD (exclusive day) or a time; default now"},
            "limit": {"type": "integer", "minimum": 1, "maximum": ENTRIES_MAX, "description": "default 200"},
            "include_deleted": {"type": "boolean"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "history", "title": "Entry history", "description": "All versions of one entry, oldest first.",
         "inputSchema": {"type": "object", "required": ["entry_id"], "properties": {"entry_id": id}},
         "annotations": {"readOnlyHint": true}},
        {"name": "attention", "title": "Entries needing attention",
         "description": "Entries waiting for a parent and/or an agent, with why.",
         "inputSchema": {"type": "object", "properties": {
            "by": {"enum": ["user", "agent", "any"], "description": "default any"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "status", "title": "Current status",
         "description": "The current situation: last feed, diapers, temperature and weight, with how long ago; next_feed_estimate (last feed + median recent gap, capped by the feeding plan; a top-up within 1h30 counts with its feed (at most 2 h), also in feed counts; due_in or overdue_by; diapers aren't predicted: they're checked at feeds); next_feed_start (the feed moved up by the parents' preparation time, diaper first); wet_diaper_warning (fewer wet diapers in 24 h than usual for the age); feeding_guidance: daily target (first-week build-up, then 150 ml/kg), last 24 h against it, the next amount and max_gap, minimum (NVK minimum feeds per 24 h and longest pause, with the rule), aim_feeds_per_day, next_feed.latest_at / latest_start_at (latest_reason: pause or count), breast_side (which breast to start with), warnings (close to the longest pause or to too few feeds); sleep_facing.next (side to put the baby down on after the next feed, assuming alternation after each feed since the last logged side; put_down_now: the side after the feed just given, while none is logged); bath (the day the next is due, due.on, reminded that evening at due.at, overdue after that day; with or without soap: two a week, alternating); vitamins.to_give_today (with the rule); WHO weight percentiles, weight change against newborns with the same delivery mode, birth weight regained; today's and last-24 h totals; entries needing attention.",
         "inputSchema": {"type": "object", "properties": {}}, "annotations": {"readOnlyHint": true}},
        {"name": "stats", "title": "Per-day statistics",
         "description": "Per local day: feeds, bottle ml by kind, breast minutes, diapers, spit-ups, temperature range, weight vs birth and WHO percentile, the longest gap between feeds the night before.",
         "inputSchema": {"type": "object", "properties": {
            "from": {"type": "string", "description": "YYYY-MM-DD, default 7 days ago"},
            "to": {"type": "string", "description": "YYYY-MM-DD (inclusive), default today"}}},
         "annotations": {"readOnlyHint": true}},
        {"name": "fields", "title": "Field registry", "description": "The structured fields entries can have, with types, units and allowed values.",
         "inputSchema": {"type": "object", "properties": {}}, "annotations": {"readOnlyHint": true}},
        {"name": "sql", "title": "Read-only SQL",
         "description": format!("Run one read-only SQLite query for questions the other tools don't answer. Table `entry` (current versions; deleted=0 for live ones): entry_id, at (Unix seconds UTC), duration_s, text, fields (JSON), needs_attention_by, needs_attention_why, deleted, changed_at, changed_by, plus one column per registry field (e.g. bottle_formula_ml, wet). `entry_old` has earlier versions. Times: use datetime(at, 'unixepoch') (UTC; local is {tz}). At most 500 rows."),
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
    // The tools wait on the database (the sql tool up to 5 seconds): on a
    // blocking thread, not one of the server's async workers.
    let res = tokio::task::spawn_blocking(move || match req.as_array() {
        Some(batch) => {
            let out: Vec<Value> = batch.iter().filter_map(|r| one(&app, user, r)).collect();
            (!out.is_empty()).then(|| json!(out))
        }
        None => one(&app, user, &req),
    })
    .await;
    match res {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("tool call failed: {e}")).into_response(),
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
            "protocolVersion": protocol_version(req["params"]["protocolVersion"].as_str()),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "baby-mcp", "version": crate::html::VERSION},
            "instructions": format!("This log is for {}.\n\n{INSTRUCTIONS}", app.cfg.baby.name),
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools(&app.cfg.tz)}),
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

/// Refuse a time more than 15 minutes ahead: a mistyped day or year would
/// otherwise become the "last feed" until someone notices.
fn not_future(at: i64, now: i64) -> Result<i64, String> {
    if at > now + 15 * 60 {
        return Err("that time is in the future; give the time it happened".into());
    }
    Ok(at)
}

/// How many entries `entries` returns by default, and at most: a wide range
/// would otherwise put the whole log in the agent's context.
const ENTRIES_DEFAULT: usize = 200;
const ENTRIES_MAX: usize = 500;

/// MCP versions this server speaks, newest first. A client asking for one
/// gets it; otherwise the newest (the client then decides).
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

fn protocol_version(asked: Option<&str>) -> &'static str {
    PROTOCOL_VERSIONS.iter().find(|v| Some(**v) == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0])
}

fn call(app: &App, user: i64, name: &str, a: &Value) -> Result<Value, String> {
    let clock = &app.clock;
    let t = now();
    match name {
        "log" => {
            let text = a["text"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("text is required")?;
            let at = not_future(clock.parse(a["at"].as_str().unwrap_or(""), t)?, t)?;
            let fields = fields_arg(a)?;
            let by = attention_arg(a)?.unwrap_or(0);
            let why = a["needs_attention_why"].as_str().map(str::trim).filter(|w| !w.is_empty()).map(str::to_string).filter(|_| by != 0);
            if by != 0 && why.is_none() {
                return Err("needs_attention_why is required with needs_attention_by: say what is missing".into());
            }
            let n = NewEntry { at, duration_s: duration_arg(a)?.flatten(), text: text.into(), fields, attention_by: by, attention_why: why };
            app.db.run(|c| {
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
            })
        }
        "update" => {
            let id = arg_i64(a, "entry_id")?;
            let fields = fields_arg(a)?;
            app.db.run(|c| {
                let mut e = en::get(c, id)?;
                if let Some(at) = a["at"].as_str() {
                    e.at = not_future(clock.parse_on(at, t, Some(e.at))?, t)?;
                }
                if let Some(d) = duration_arg(a)? {
                    e.duration_s = d;
                }
                if let Some(text) = a["text"].as_str() {
                    e.text = Some(text.trim()).filter(|t| !t.is_empty()).ok_or("text cannot be empty")?.to_string();
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
            })
        }
        "delete" | "undelete" => {
            let id = arg_i64(a, "entry_id")?;
            app.db.run(|c| {
                let mut e = en::get(c, id)?;
                let want = name == "delete";
                if e.deleted != want {
                    e.deleted = want;
                    e = en::save(c, user, &e, t)?;
                }
                Ok(json!({name: true, "entry": en::view(&e, clock, &en::user_names(c)?)}))
            })
        }
        "entries" => {
            let from = range_start(app, a["from"].as_str(), clock.day_start(t), t)?;
            let to = range_end(app, a["to"].as_str(), t + 1, t)?;
            app.db.run(|c| {
                let names = en::user_names(c)?;
                let mut list = en::list(c, from, to, a["include_deleted"].as_bool().unwrap_or(false))?;
                let limit = a["limit"].as_u64().map_or(ENTRIES_DEFAULT, |l| (l as usize).clamp(1, ENTRIES_MAX));
                let next = list.get(limit).map(|e| e.at);
                list.truncate(limit);
                let mut out = json!({"from": clock.fmt(from), "to": clock.fmt(to), "truncated": next.is_some(),
                                     "entries": list.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>()});
                if let Some(t) = next {
                    out["next_from"] = json!(clock.fmt(t));
                }
                Ok(out)
            })
        }
        "history" => {
            let id = arg_i64(a, "entry_id")?;
            app.db.run(|c| {
                let names = en::user_names(c)?;
                Ok(json!({"versions": en::history(c, id)?.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>()}))
            })
        }
        "attention" => {
            let mask = match a["by"].as_str().unwrap_or("any") {
                "user" => BY_USER,
                "agent" => BY_AGENT,
                _ => BY_USER | BY_AGENT,
            };
            app.db.run(|c| {
                let names = en::user_names(c)?;
                Ok(json!({"entries": en::attention(c, mask)?.iter().map(|e| en::view(e, clock, &names)).collect::<Vec<_>>()}))
            })
        }
        "status" => app.db.run(|c| crate::status::status_json(c, clock, &app.baby(), t)),
        "stats" => {
            let today = clock.day_start(t);
            let from = match a["from"].as_str() { Some(d) => clock.date_start(d)?, None => clock.day_start(today - 6 * 86400 + 3600) };
            let to = match a["to"].as_str() { Some(d) => clock.next_day_start(clock.date_start(d)?), None => t + 1 };
            if to - from > 400 * 86400 {
                return Err("range too long; at most about a year".into());
            }
            let days = app.db.run(|c| en::stats(c, clock, &app.baby(), from, to))?;
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
            // Table-valued functions, to look inside the fields JSON.
            "json_each" | "json_tree" => Authorization::Allow,
            // An empty column name is a read of no column, as in count(*).
            "user" if matches!(column_name, "id" | "name" | "display_name" | "") => Authorization::Allow,
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

#[cfg(test)]
mod size_tests {
    /// Keep what the agent reads at the start of a session short.
    #[test]
    fn instructions_and_tools_stay_small() {
        let tools = super::tools("Europe/Amsterdam").to_string();
        println!("instructions {} bytes, tools {} bytes", super::INSTRUCTIONS.len(), tools.len());
        assert!(super::INSTRUCTIONS.len() < 3000, "instructions grew to {} bytes", super::INSTRUCTIONS.len());
        assert!(tools.len() < 9000, "tool definitions grew to {} bytes", tools.len());
        assert!(tools.contains("local is Europe/Amsterdam"));
    }
}
