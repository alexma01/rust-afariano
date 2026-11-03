//! rust-afariano-runner: polls a rust-afariano server and runs CI jobs with Docker or a shell.

mod client;
mod config;
mod docker;
mod git;
mod job;
mod joblog;
mod proc;
mod run;
mod script;
mod service;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::config::{Config, Executor};

#[derive(Parser)]
#[command(name = "rust-afariano-runner", version, about = "Runs rust-afariano CI jobs")]
struct Cli {
    /// Config file (default: $RUST_AFARIANO_RUNNER_CONFIG, /etc/rust-afariano-runner/config.toml
    /// as root, else ~/.config/rust-afariano-runner/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check a runner token with the server and write the config file.
    Register {
        /// Server URL, e.g. https://git.example.com
        #[arg(long)]
        url: String,
        /// Runner token (rfr_…) shown when the runner was created in the UI.
        #[arg(long)]
        token: String,
        #[arg(long, value_enum, default_value = "docker")]
        executor: Executor,
        /// Local name (default: the name given in the UI).
        #[arg(long)]
        name: Option<String>,
        /// Jobs at the same time.
        #[arg(long)]
        concurrent: Option<usize>,
        /// Run job and service containers privileged (needed for docker:dind).
        #[arg(long)]
        privileged: bool,
        /// Where checkouts live (absolute). Give each runner on one host its own.
        #[arg(long)]
        builds_dir: Option<PathBuf>,
    },
    /// Poll the server and run jobs until stopped.
    Run,
    /// Run as a service: launchd LaunchAgent (macOS) or systemd unit (Linux, root).
    /// With `--config` other than the default, the service is named after the runner,
    /// so several runners can run on one host.
    Install,
    /// Remove the service (of `--config`, when given).
    Uninstall,
    /// Debug: run a JobSpec JSON file without a server, log to stdout.
    ExecLocal {
        job: PathBuf,
        /// Use this directory as the project directory (with GIT_STRATEGY none).
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, value_enum)]
        executor: Option<Executor>,
        #[arg(long)]
        privileged: bool,
    },
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUST_AFARIANO_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let code = match rt.block_on(real_main(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn real_main(cli: Cli) -> Result<i32> {
    let path = cli.config.clone().unwrap_or_else(config::config_path);
    match cli.cmd {
        Cmd::Register { url, token, executor, name, concurrent, privileged, builds_dir } => {
            if !token.starts_with(rust_afariano_proto::RUNNER_TOKEN_PREFIX) {
                bail!("runner tokens start with {}", rust_afariano_proto::RUNNER_TOKEN_PREFIX);
            }
            let url = url.trim_end_matches('/').to_string();
            let mut cfg = if path.exists() { Config::load(&path)? } else { Config::local(executor) };
            cfg.url = url;
            cfg.token = token;
            cfg.executor = executor;
            if let Some(c) = concurrent {
                if c == 0 {
                    bail!("--concurrent must be at least 1");
                }
                cfg.concurrent = c;
            }
            if privileged {
                cfg.docker.privileged = true;
            }
            if let Some(dir) = builds_dir {
                if !dir.is_absolute() {
                    bail!("--builds-dir must be an absolute path");
                }
                cfg.builds_dir = Some(dir);
            }
            let client = client::Client::new(&cfg.url, &cfg.token)?;
            let info = client.verify(&run::hello(&cfg)).await?;
            cfg.name = name.unwrap_or(info.name.clone());
            cfg.save(&path)?;
            println!(
                "Registered runner #{} {:?} (tags {:?}{}).",
                info.id,
                info.name,
                info.tags,
                if info.run_untagged { ", runs untagged jobs" } else { "" }
            );
            println!("Config: {}", path.display());
            println!("Next: rust-afariano-runner install   (or `rust-afariano-runner run` in the foreground)");
            Ok(0)
        }
        Cmd::Run => {
            let cfg = Config::load(&path)?;
            run::run(cfg).await?;
            Ok(0)
        }
        Cmd::Install => {
            service::install(&path)?;
            Ok(0)
        }
        Cmd::Uninstall => {
            service::uninstall(&path)?;
            Ok(0)
        }
        Cmd::ExecLocal { job, dir, executor, privileged } => {
            let mut cfg =
                if path.exists() { Config::load(&path)? } else { Config::local(executor.unwrap_or(Executor::Docker)) };
            if let Some(e) = executor {
                cfg.executor = e;
            }
            if privileged {
                cfg.docker.privileged = true;
            }
            run::exec_local(cfg, &job, dir).await
        }
    }
}
