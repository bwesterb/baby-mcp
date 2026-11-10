//! Admin commands, run on the server as the service user, e.g.
//! `sudo -u baby-mcp baby-mcp user add bas "Bas"`.

use crate::auth;
use crate::config::Config;
use crate::db::Db;
use crate::entries::{self as en};
use crate::time::{Clock, now};
use clap::Subcommand;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum UserCmd {
    /// Create a user and print a passkey enrollment link.
    Add {
        name: String,
        display_name: String,
        /// Only sees the charts, up to yesterday (say a grandparent).
        #[arg(long)]
        viewer: bool,
    },
    /// List users.
    List,
    /// Set a user's role: parent (everything) or viewer (only the charts).
    Role { name: String, role: String },
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Manage users.
    #[command(subcommand)]
    User(UserCmd),
    /// Print a new single-use passkey enrollment link for a user.
    Enroll { name: String },
    /// List passkeys, OAuth clients and active tokens.
    Passkeys,
    /// Remove a passkey (by id from `passkeys`) and the user's refresh tokens.
    RemovePasskey { id: i64 },
    /// Invalidate all access and refresh tokens and web sessions.
    RevokeAll,
    /// List suggestions from agents (open ones unless --all).
    Suggestions {
        #[arg(long)]
        all: bool,
    },
    /// Mark a suggestion done.
    Done {
        id: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Mark a suggestion rejected.
    Reject {
        id: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Print the current status (as the `status` MCP tool returns it) as JSON.
    Status,
    /// Send a test notification to all of a user's devices.
    PushTest { name: String },
    /// Write everything (entries with all versions, users, suggestions) as JSON to stdout.
    Export,
    /// Import the kraamweek from a Healthsoft portal medical JSON file.
    ImportHealthsoft {
        file: std::path::PathBuf,
        /// The user the entries are attributed to.
        #[arg(long = "as")]
        user: String,
        #[arg(long)]
        force: bool,
    },
}

/// Refuse to run as root: SQLite would create files the service cannot open.
fn not_root() -> Result<(), String> {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        if s.lines().any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(2) == Some("0")) {
            return Err("run admin commands as the service user, e.g. `sudo -u baby-mcp baby-mcp ...`".into());
        }
    }
    Ok(())
}

fn enroll_link(cfg: &Config, db: &Db, user_id: i64) -> Result<String, String> {
    let tok = db.with(|c| auth::new_enroll_token(c, user_id)).map_err(en::err)?;
    Ok(format!("{}/enroll?token={tok}", cfg.origin()))
}

pub fn run(cfg: &Config, cmd: Cmd) -> Result<(), String> {
    not_root()?;
    let db = Db::open(&cfg.db_path())?;
    let clock = Clock::new(&cfg.tz)?;
    match cmd {
        Cmd::User(UserCmd::Add { name, display_name, viewer }) => {
            let role = if viewer { "viewer" } else { "parent" };
            let id = db
                .with(|c| {
                    c.execute(
                        "INSERT INTO user (name, display_name, created_at, role) VALUES (?1, ?2, ?3, ?4)",
                        params![name, display_name, now(), role],
                    )?;
                    Ok(c.last_insert_rowid())
                })
                .map_err(|e| format!("adding user {name:?}: {e}"))?;
            println!("Created {role} {name} (id {id}). Enroll a passkey within 30 minutes, single use:\n\n  {}\n", enroll_link(cfg, &db, id)?);
        }
        Cmd::User(UserCmd::Role { name, role }) => {
            if !matches!(role.as_str(), "parent" | "viewer") {
                return Err(format!("role must be parent or viewer, not {role:?}"));
            }
            let n = db.with(|c| c.execute("UPDATE user SET role = ?1 WHERE name = ?2", params![role, name])).map_err(en::err)?;
            if n == 0 {
                return Err(format!("no user {name:?}"));
            }
            println!("{name} is now a {role}.");
        }
        Cmd::User(UserCmd::List) => {
            db.with(|c| {
                let mut st = c.prepare("SELECT id, name, display_name, (SELECT count(*) FROM passkey p WHERE p.user_id = u.id), role FROM user u ORDER BY id")?;
                let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?)))?;
                for r in rows {
                    let (id, n, d, k, role) = r?;
                    println!("{id:>3}  {n:<16} {d:<24} {role:<7} {k} passkey(s)");
                }
                Ok(())
            })
            .map_err(en::err)?;
        }
        Cmd::Enroll { name } => {
            let (id, _) = db.with(|c| Ok(en::user_by_name(c, &name))).map_err(en::err)??.ok_or(format!("no user {name:?}"))?;
            println!("Enroll a passkey for {name} within 30 minutes, single use:\n\n  {}\n", enroll_link(cfg, &db, id)?);
        }
        Cmd::Passkeys => {
            db.with(|c| {
                let mut st = c.prepare(
                    "SELECT p.id, u.name, p.name, p.created_at, p.last_used FROM passkey p JOIN user u ON u.id = p.user_id ORDER BY p.id",
                )?;
                let rows = st.query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?))
                })?;
                for r in rows {
                    let (id, u, n, created, used) = r?;
                    println!(
                        "{id:>3}  {u:<12} {n:<24} enrolled {}  last used {}",
                        clock.fmt(created),
                        used.map(|t| clock.fmt(t)).unwrap_or("never".into())
                    );
                }
                let clients: i64 = c.query_row("SELECT count(*) FROM oauth_client", [], |r| r.get(0))?;
                let tokens: i64 = c.query_row("SELECT count(*) FROM refresh_token WHERE expires > ?1", [now()], |r| r.get(0))?;
                println!("{clients} OAuth client(s), {tokens} active refresh token(s)");
                Ok(())
            })
            .map_err(en::err)?;
        }
        Cmd::RemovePasskey { id } => {
            let user: Option<i64> = db
                .with(|c| {
                    let u = c.query_row("SELECT user_id FROM passkey WHERE id = ?1", [id], |r| r.get(0)).optional()?;
                    if let Some(u) = u {
                        c.execute("DELETE FROM passkey WHERE id = ?1", [id])?;
                        c.execute("DELETE FROM refresh_token WHERE user_id = ?1", [u])?;
                        c.execute("DELETE FROM session WHERE user_id = ?1", [u])?;
                    }
                    Ok(u)
                })
                .map_err(en::err)?;
            match user {
                Some(u) => println!("Removed passkey {id}; user {u} is logged out everywhere (access tokens expire within an hour)."),
                None => return Err(format!("no passkey {id}")),
            }
        }
        Cmd::RevokeAll => {
            db.with(|c| {
                auth::rotate_signing_key(c)?;
                c.execute("DELETE FROM refresh_token", [])?;
                c.execute("DELETE FROM session", [])?;
                c.execute("DELETE FROM oauth_client", [])
            })
            .map_err(en::err)?;
            println!("All tokens, sessions and client registrations revoked; passkeys are kept.");
        }
        Cmd::Suggestions { all } => {
            db.with(|c| {
                let w = if all { "" } else { "WHERE s.status = 'open'" };
                let mut st = c.prepare(&format!(
                    "SELECT s.id, s.at, u.display_name, s.status, s.text, s.note FROM suggestion s JOIN user u ON u.id = s.by {w} ORDER BY s.id"
                ))?;
                let rows = st.query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?))
                })?;
                for r in rows {
                    let (id, at, by, status, text, note) = r?;
                    println!("#{id} {} by {by} [{status}]\n  {}", clock.fmt(at), text.replace('\n', "\n  "));
                    if let Some(n) = note {
                        println!("  note: {n}");
                    }
                    println!();
                }
                Ok(())
            })
            .map_err(en::err)?;
        }
        Cmd::Done { id, note } => set_status(&db, id, "done", note)?,
        Cmd::Reject { id, note } => set_status(&db, id, "rejected", note)?,
        Cmd::Status => {
            // The same overview the MCP tool gives, including growth and feeding guidance.
            let app = crate::app::App::new(cfg.clone())?;
            let s = app.db.with(|c| Ok(crate::status::status_json(c, &app.clock, &app.baby(), now()))).map_err(en::err)??;
            println!("{}", serde_json::to_string_pretty(&s).unwrap());
        }
        Cmd::PushTest { name } => {
            let app = crate::app::App::new(cfg.clone())?;
            let id: i64 = app
                .db
                .with(|c| c.query_row("SELECT id FROM user WHERE name = ?1", [&name], |r| r.get(0)).optional())
                .map_err(en::err)?
                .ok_or_else(|| format!("no user {name}"))?;
            let n = crate::notify::Notice {
                kind: "test",
                key: String::new(),
                title: format!("🔔 {}: test", app.cfg.baby.name),
                body: "Notifications work on this device.".into(),
                url: "/notifications".into(),
                ttl: 600,
                by: None,
            };
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            let sent = rt.block_on(crate::notify::deliver(&app, &app.push_client, id, &n, "test"))?;
            println!("accepted by {sent} device(s); failures are listed on /notifications");
        }
        Cmd::Export => {
            let out = db
                .with(|c| Ok((|| -> Result<Value, String> {
                    // The only backup: fail loudly rather than export less.
                    let names = en::user_names(c)?;
                    let all = en::list(c, i64::MIN, i64::MAX, true)?;
                    let mut entries = vec![];
                    for e in &all {
                        let versions: Vec<Value> = en::history(c, e.entry_id)?
                            .iter()
                            .map(|v| json!({
                                "version": v.version, "at": v.at, "duration_s": v.duration_s, "text": v.text,
                                "fields": v.fields, "needs_attention_by": v.attention_by,
                                "needs_attention_why": v.attention_why, "deleted": v.deleted,
                                "changed_at": v.changed_at, "changed_by": names.get(&v.changed_by),
                                "superseded_at": v.superseded_at,
                            }))
                            .collect();
                        entries.push(json!({"entry_id": e.entry_id, "versions": versions}));
                    }
                    let mut st = c.prepare("SELECT s.id, s.at, u.name, s.text, s.status, s.note FROM suggestion s JOIN user u ON u.id = s.by").map_err(en::err)?;
                    let sugg: Vec<Value> = st
                        .query_map([], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "at": r.get::<_, i64>(1)?, "by": r.get::<_, String>(2)?,
                            "text": r.get::<_, String>(3)?, "status": r.get::<_, String>(4)?, "note": r.get::<_, Option<String>>(5)?}))).map_err(en::err)?
                        .collect::<rusqlite::Result<_>>().map_err(en::err)?;
                    Ok(json!({"exported_at": now(), "users": names, "entries": entries, "suggestions": sugg}))
                })()))
                .map_err(en::err)??;
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        Cmd::ImportHealthsoft { file, user, force } => {
            let raw = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let medical: Value = serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", file.display()))?;
            let n = db
                .with(|c| {
                    Ok((|| {
                        let (uid, _) = en::user_by_name(c, &user)?.ok_or(format!("no user {user:?}"))?;
                        let tx = c.unchecked_transaction().map_err(en::err)?;
                        let n = crate::import::import(&tx, &clock, uid, &medical, force)?;
                        tx.commit().map_err(en::err)?;
                        Ok::<_, String>(n)
                    })())
                })
                .map_err(en::err)??;
            println!("Imported {n} entries.");
        }
    }
    Ok(())
}

fn set_status(db: &Db, id: i64, status: &str, note: Option<String>) -> Result<(), String> {
    let n = db
        .with(|c| c.execute("UPDATE suggestion SET status = ?1, note = coalesce(?2, note) WHERE id = ?3", params![status, note, id]))
        .map_err(en::err)?;
    if n == 0 {
        return Err(format!("no suggestion #{id}"));
    }
    println!("Suggestion #{id} marked {status}.");
    Ok(())
}
