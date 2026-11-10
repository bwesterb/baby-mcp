//! baby-mcp: a baby log used mainly through Claude (MCP), with a small web UI.
//! See DESIGN.md.

mod admin;
mod app;
mod auth;
mod config;
mod db;
mod entries;
mod feeding;
mod growth;
mod html;
mod import;
mod mcp;
mod notifications;
mod notify;
mod push;
mod registry;
mod status;
#[cfg(test)]
mod tests;
mod time;
mod web;

use axum::Router;
use axum::routing::{get, post};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about = "A baby log for Claude (MCP) and the web")]
struct Cli {
    /// Configuration file.
    #[arg(long, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Run the server (the default).
    Serve,
    #[command(flatten)]
    Admin(admin::Cmd),
}

pub fn router(app: Arc<app::App>) -> Router {
    Router::new()
        .route("/", get(web::status))
        .route("/day", get(web::day))
        .route("/log", get(web::log_part))
        .route("/timeline", get(web::timeline_day))
        .route("/charts", get(web::charts))
        .route("/entry/{id}", get(web::entry_page))
        .route("/add", get(web::add_page).post(web::add))
        .route("/app.css", get(html::css))
        .route("/app.js", get(html::js))
        .route("/manifest.webmanifest", get(web::manifest))
        .route("/icon-180.png", get(web::icon_180))
        .route("/icon-192.png", get(web::icon_192))
        .route("/icon-512.png", get(web::icon_512))
        .route("/icon-maskable-512.png", get(web::icon_maskable))
        .route("/favicon.ico", get(web::icon_32))
        .route("/icon.svg", get(web::icon_svg))
        .route("/icon-32.png", get(web::icon_32))
        .route("/elephant.png", get(web::elephant))
        .route("/notifications", get(notifications::page))
        .route("/push/prefs", post(notifications::save_pref))
        .route("/notifications/remove", post(notifications::remove))
        .route("/push/subscribe", post(notifications::subscribe))
        .route("/push/unsubscribe", post(notifications::unsubscribe))
        .route("/push/test", post(notifications::test))
        .route("/push/badge", get(notifications::badge))
        .route("/push/message/{id}", get(notifications::message))
        .route("/sw.js", get(notifications::service_worker))
        .route("/login", get(auth::login_page))
        .route("/logout", post(auth::logout))
        .route("/enroll", get(auth::enroll_page))
        .route("/auth/login/begin", post(auth::login_begin))
        .route("/auth/login/finish", post(auth::login_finish))
        .route("/auth/enroll/begin", post(auth::enroll_begin))
        .route("/auth/enroll/finish", post(auth::enroll_finish))
        .route("/.well-known/oauth-protected-resource", get(auth::protected_resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(auth::protected_resource))
        .route("/.well-known/oauth-authorization-server", get(auth::authorization_server))
        .route("/oauth/register", post(auth::register).options(auth::preflight))
        .route("/oauth/token", post(auth::token).options(auth::preflight))
        .route("/oauth/authorize", get(auth::authorize).post(auth::authorize_decide))
        .route("/mcp", post(mcp::handle).get(mcp::not_allowed).delete(mcp::not_allowed))
        .fallback(web::not_found)
        .with_state(app)
}

fn main() {
    let cli = Cli::parse();
    let cfg = match config::Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let result = match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(cfg),
        Cmd::Admin(c) => admin::run(&cfg, c),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn serve(cfg: config::Config) -> Result<(), String> {
    let listen = cfg.listen.clone();
    let _ = html::APP_NAME.set(cfg.baby.name.clone());
    let app = Arc::new(app::App::new(cfg)?);
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&listen).await.map_err(|e| format!("{listen}: {e}"))?;
        eprintln!("baby-mcp {}: {} on {listen}", env!("CARGO_PKG_VERSION"), app.cfg.origin());
        tokio::spawn(notify::run(app.clone()));
        axum::serve(listener, router(app)).await.map_err(|e| e.to_string())
    })
}
