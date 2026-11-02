//! rust-afariano server: git hosting (SSH + smart HTTP) and CI pipelines.

/// A query string built from constant column lists (never from input).
macro_rules! sql {
    ($($t:tt)*) => {
        sqlx::AssertSqlSafe(format!($($t)*))
    };
}

mod access;
mod accounts;
mod api;
mod app;
mod auth;
mod config;
mod error;
mod git;
mod git_http;
mod logs;
mod models;
mod ops;
mod pipelines;
mod push;
mod runner_api;
mod secrets;
mod ssh;
mod util;
mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

use crate::app::AppState;
use crate::config::Config;

fn main() -> anyhow::Result<()> {
    let cfg = Config::parse().validate()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_AFARIANO_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,russh=warn")),
        )
        .init();
    // Two workers are plenty for a handful of users and keep the footprint small.
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(8).enable_all().build()?;
    rt.block_on(run(cfg))
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let git = tokio::process::Command::new("git").arg("--version").output().await.context("`git` must be installed")?;
    tracing::info!("using {}", String::from_utf8_lossy(&git.stdout).trim());

    let db = PgPoolOptions::new()
        .max_connections(cfg.db_pool)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        .connect(&cfg.database_url)
        .await
        .context("connecting to Postgres")?;
    sqlx::migrate!("./migrations").run(&db).await.context("running migrations")?;

    let http_addr = cfg.http_addr;
    let ssh_enabled = !cfg.ssh_addr.trim().is_empty();
    let state = Arc::new(AppState::new(cfg, db)?);
    secrets::encrypt_legacy(&state.db, &state.cipher).await.context("encrypting CI/CD variables")?;
    accounts::open_setup_if_empty(&state).await?;

    if ssh_enabled {
        let s = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ssh::serve(s).await {
                tracing::error!("ssh server stopped: {e:#}");
            }
        });
    }
    tokio::spawn(pipelines::reaper(state.clone()));

    let app = web::router(state.clone());
    let listener = tokio::net::TcpListener::bind(http_addr).await.with_context(|| format!("binding {http_addr}"))?;
    tracing::info!("http listening on {http_addr}, public URL {}", state.cfg.public_url);
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
    tracing::info!("shutting down");
    // Log streams (SSE) and runner long-polls never end on their own; finish
    // in-flight work briefly, then exit so restarts stay fast. Runners retry.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tracing::info!("exiting with connections still open");
        std::process::exit(0);
    });
}
