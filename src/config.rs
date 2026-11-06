//! Configuration file (TOML). Unknown keys are rejected so that a typo stops
//! the service instead of silently running on defaults.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_PATH: &str = "/etc/baby-mcp/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Public origin, e.g. https://baby.example.com. Its host is the passkey
    /// relying party, so it is effectively permanent.
    pub origin: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Directory holding the database.
    #[serde(default = "default_state")]
    pub state: PathBuf,
    #[serde(default = "default_tz")]
    pub tz: String,
    /// https hosts OAuth clients may redirect to (localhost is always allowed).
    #[serde(default = "default_redirect_hosts")]
    pub oauth_redirect_hosts: Vec<String>,
    /// WHO weight-for-age table (not shipped); default `<state>/who-weight-for-age.tsv`.
    pub growth_table: Option<PathBuf>,
    /// First-month weight-change percentiles (not shipped); default `<state>/newborn-weight-change.tsv`.
    pub weight_change_table: Option<PathBuf>,
    pub baby: Baby,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baby {
    pub name: String,
    /// Date and time of birth, e.g. 2026-01-02T03:04:00+01:00.
    pub birth: toml::value::Datetime,
    pub birth_weight_g: Option<f64>,
    /// "female" or "male": selects the WHO growth standard.
    pub sex: Option<crate::growth::Sex>,
    /// "vaginal" or "cesarean": selects the first-month weight-change reference.
    pub delivery: Option<crate::growth::Delivery>,
    /// The number of feeds per 24 h to plan for at least (the NVK minimum
    /// applies regardless), e.g. 7.
    pub aim_feeds_per_day: Option<f64>,
    /// Minutes before a feed to start the routine (diaper first), e.g. 15.
    pub feed_prep_minutes: Option<i64>,
}

fn default_listen() -> String {
    "127.0.0.1:8095".into()
}
fn default_state() -> PathBuf {
    "/var/lib/baby-mcp".into()
}
fn default_tz() -> String {
    "Europe/Amsterdam".into()
}
fn default_redirect_hosts() -> Vec<String> {
    vec!["claude.ai".into(), "claude.com".into()]
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.check()?;
        Ok(cfg)
    }

    fn check(&self) -> Result<(), String> {
        let o = self.origin.trim_end_matches('/');
        if !o.starts_with("https://") || o["https://".len()..].contains('/') {
            return Err(format!("origin must be like https://host, not {:?}", self.origin));
        }
        Ok(())
    }

    pub fn origin(&self) -> &str {
        self.origin.trim_end_matches('/')
    }

    pub fn host(&self) -> &str {
        let rest = &self.origin()["https://".len()..];
        rest.split(':').next().unwrap_or(rest)
    }

    pub fn growth_table_path(&self) -> PathBuf {
        self.growth_table.clone().unwrap_or_else(|| self.state.join("who-weight-for-age.tsv"))
    }

    pub fn weight_change_table_path(&self) -> PathBuf {
        self.weight_change_table.clone().unwrap_or_else(|| self.state.join("newborn-weight-change.tsv"))
    }

    pub fn db_path(&self) -> PathBuf {
        self.state.join("baby.sqlite")
    }

    /// Birth as Unix seconds; a time without offset is local time.
    pub fn birth(&self, clock: &crate::time::Clock) -> Option<i64> {
        clock.parse(&self.baby.birth.to_string(), 0).ok()
    }
}
