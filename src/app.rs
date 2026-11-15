//! Shared application state.

use crate::auth::Mem;
use crate::config::Config;
use crate::db::Db;
use crate::entries::Baby;
use crate::time::Clock;
use std::sync::Mutex;
use webauthn_rs::Webauthn;

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub clock: Clock,
    pub wa: Webauthn,
    pub mem: Mutex<Mem>,
    pub growth: Option<std::sync::Arc<crate::growth::Table>>,
    pub change: Option<std::sync::Arc<crate::growth::WeightChange>>,
    pub push_client: reqwest::Client,
    /// Bumped on every change to the log (any writer in this process), and
    /// sent to open pages so they reload (`/events`).
    pub version: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub changes: tokio::sync::broadcast::Sender<u64>,
}

impl App {
    pub fn new(cfg: Config) -> Result<App, String> {
        let clock = Clock::new(&cfg.tz)?;
        let db = Db::open(&cfg.db_path())?;
        let fixed = db
            .with(|c| crate::auth::fix_cred_ids(c))
            .map_err(|e| e.to_string())?;
        if fixed > 0 {
            eprintln!("passkeys: stored the credential id of {fixed} passkey(s)");
        }
        let wa = crate::auth::build_webauthn(cfg.origin(), cfg.host())?;
        let path = cfg.growth_table_path();
        let growth = if path.exists() {
            Some(std::sync::Arc::new(crate::growth::Table::load(&path)?))
        } else {
            eprintln!(
                "note: {} not found; running without WHO growth percentiles",
                path.display()
            );
            None
        };
        let path = cfg.weight_change_table_path();
        let change = if path.exists() {
            Some(std::sync::Arc::new(crate::growth::WeightChange::load(
                &path,
            )?))
        } else {
            eprintln!(
                "note: {} not found; running without first-month weight-change percentiles",
                path.display()
            );
            None
        };
        // Every row written to the log, from MCP, the web or anything else
        // using this connection, tells the open pages.
        let version = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let (changes, _) = tokio::sync::broadcast::channel(16);
        {
            let (version, changes) = (version.clone(), changes.clone());
            db.with(|c| {
                c.update_hook(Some(
                    move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
                        if table == "entry" {
                            let v = version.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            let _ = changes.send(v);
                        }
                    },
                ))
            })
            .map_err(|e| e.to_string())?;
        }
        Ok(App {
            cfg,
            db,
            clock,
            wa,
            mem: Mutex::new(Mem::default()),
            growth,
            change,
            push_client: crate::notify::client(),
            version,
            changes,
        })
    }

    /// The in-memory state (ceremonies, codes); a panic elsewhere while it
    /// was held doesn't make it unusable (as with the database).
    pub fn mem(&self) -> std::sync::MutexGuard<'_, Mem> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn baby(&self) -> Baby {
        Baby {
            name: self.cfg.baby.name.clone(),
            birth: self.cfg.birth(&self.clock),
            birth_weight_g: self.cfg.baby.birth_weight_g,
            sex: self.cfg.baby.sex,
            growth: self.growth.clone(),
            delivery: self.cfg.baby.delivery,
            change: self.change.clone(),
            aim_feeds_per_day: self.cfg.baby.aim_feeds_per_day,
            feed_prep_s: self.cfg.baby.feed_prep_minutes.unwrap_or(0).max(0) * 60,
        }
    }
}
