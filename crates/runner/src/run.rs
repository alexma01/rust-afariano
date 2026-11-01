//! `run`: poll the server and execute jobs; `exec-local`: one job from a file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rust_afariano_proto::{JobOutcome, JobResult, JobSpec, RunnerHello};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::client::{Client, RequestError};
use crate::config::{Config, Executor};
use crate::docker::Docker;
use crate::job::{self, Ctx, RunnerMeta};
use crate::joblog::{self, JobLog, Sink};

pub fn hello(cfg: &Config) -> RunnerHello {
    RunnerHello {
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: crate::platform(),
        executor: cfg.executor.as_str().to_string(),
        hostname: crate::hostname(),
    }
}

/// SIGINT/SIGTERM once: `shutdown`; twice: `force` too.
fn spawn_signals(shutdown: CancellationToken, force: CancellationToken) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            return;
        };
        let mut count = 0;
        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            count += 1;
            if count == 1 {
                tracing::info!("shutdown requested: no new jobs; running jobs may finish");
                shutdown.cancel();
            } else {
                tracing::warn!("second signal: canceling running jobs");
                force.cancel();
            }
        }
    });
}

pub async fn run(cfg: Config) -> Result<()> {
    if cfg.url.is_empty() || cfg.token.is_empty() {
        bail!("config has no url/token: run `rust-afariano-runner register` first");
    }
    let cfg = Arc::new(cfg);
    let client = Client::new(&cfg.url, &cfg.token)?;
    let hello = hello(&cfg);
    let docker = Docker::default();
    let shutdown = CancellationToken::new();
    let force = CancellationToken::new();
    spawn_signals(shutdown.clone(), force.clone());

    if cfg.executor == Executor::Docker {
        match docker.version().await {
            Ok(v) => tracing::info!("docker server {v}"),
            Err(e) => tracing::warn!("docker is not reachable yet: {e:#}"),
        }
    }

    let info = loop {
        match client.verify(&hello).await {
            Ok(info) => break info,
            Err(e) => {
                if e.to_string().contains("rejected the runner token") {
                    return Err(e);
                }
                tracing::warn!("cannot reach {}: {e:#}; retrying in 10s", cfg.url);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    _ = shutdown.cancelled() => return Ok(()),
                }
            }
        }
    };
    let meta = Arc::new(RunnerMeta { name: info.name.clone(), tags: info.tags.clone() });
    tracing::info!(
        "runner #{} {:?} on {} — executor {}, tags {:?}{}, concurrent {}",
        info.id,
        info.name,
        cfg.url,
        cfg.executor.as_str(),
        info.tags,
        if info.run_untagged { " + untagged" } else { "" },
        cfg.concurrent
    );

    let mut workers = JoinSet::new();
    for slot in 0..cfg.concurrent {
        workers.spawn(worker(
            slot,
            cfg.clone(),
            meta.clone(),
            client.clone(),
            hello.clone(),
            docker.clone(),
            shutdown.clone(),
            force.clone(),
        ));
    }

    tokio::select! {
        _ = async { while workers.join_next().await.is_some() {} } => return Ok(()),
        _ = shutdown.cancelled() => {}
    }
    let limit = Duration::from_secs(cfg.shutdown_timeout_secs);
    tokio::select! {
        _ = async { while workers.join_next().await.is_some() {} } => return Ok(()),
        _ = tokio::time::sleep(limit) => {
            tracing::warn!("jobs still running after {}s: canceling them", limit.as_secs());
            force.cancel();
        }
        _ = force.cancelled() => {}
    }
    while workers.join_next().await.is_some() {}
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn worker(
    slot: usize,
    cfg: Arc<Config>,
    meta: Arc<RunnerMeta>,
    client: Client,
    hello: RunnerHello,
    docker: Docker,
    shutdown: CancellationToken,
    force: CancellationToken,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let res = tokio::select! {
            r = client.request_job(&hello) => r,
            _ = shutdown.cancelled() => return,
        };
        let wait = match res {
            Ok(None) => {
                backoff = Duration::from_secs(1);
                continue;
            }
            Ok(Some(spec)) => {
                backoff = Duration::from_secs(1);
                let ctx = Ctx { cfg: &cfg, meta: &meta, docker: &docker, slot };
                run_job(&ctx, &client, spec, &force).await;
                continue;
            }
            Err(RequestError::Unauthorized) => {
                tracing::error!("the server rejected the runner token; retrying in 60s");
                Duration::from_secs(60)
            }
            Err(RequestError::Transient(e)) => {
                tracing::warn!("job request failed: {e:#}; retrying in {}s", backoff.as_secs());
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                w
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

async fn run_job(ctx: &Ctx<'_>, client: &Client, spec: JobSpec, force: &CancellationToken) {
    tracing::info!(
        job = spec.id,
        slot = ctx.slot,
        "starting {} ({} @ {})",
        spec.name,
        spec.project_path,
        &spec.git.sha[..8.min(spec.git.sha.len())]
    );
    let cancel = force.child_token();
    let log = JobLog::new(job::masker_for(&spec));
    let uploader = joblog::spawn_uploader(
        log.clone(),
        Sink::Server { client: client.clone(), job_id: spec.id, token: spec.token.clone() },
        cancel.clone(),
    );
    let result = job::execute(ctx, &spec, &log, cancel, None).await;
    log.close();
    if tokio::time::timeout(Duration::from_secs(180), uploader).await.is_err() {
        tracing::warn!(job = spec.id, "log upload did not finish in time");
    }
    report(client, &spec, &result).await;
    tracing::info!(job = spec.id, "finished: {:?} {:?}", result.outcome, result.failure_reason);
}

async fn report(client: &Client, spec: &JobSpec, result: &JobResult) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let mut delay = Duration::from_secs(1);
    loop {
        match client.finish(spec.id, &spec.token, result).await {
            Ok(true) => return,
            Ok(false) => {
                tracing::warn!(job = spec.id, "server no longer knows the job; result dropped");
                return;
            }
            Err(e) if tokio::time::Instant::now() < deadline => {
                tracing::warn!(job = spec.id, "reporting the result failed: {e:#}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            Err(e) => {
                tracing::error!(job = spec.id, "giving up reporting the result: {e:#}");
                return;
            }
        }
    }
}

/// Run a `JobSpec` from a file with no server; log to stdout. Returns the exit code.
pub async fn exec_local(cfg: Config, job_file: &Path, dir: Option<PathBuf>) -> Result<i32> {
    let text = std::fs::read_to_string(job_file).with_context(|| format!("reading {}", job_file.display()))?;
    let spec: JobSpec = serde_json::from_str(&text).context("parsing the job JSON")?;
    let docker = Docker::default();
    let meta = RunnerMeta { name: cfg.name.clone(), tags: Vec::new() };
    let ctx = Ctx { cfg: &cfg, meta: &meta, docker: &docker, slot: 0 };
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    spawn_signals(shutdown.clone(), cancel.clone());
    {
        let (shutdown, cancel) = (shutdown.clone(), cancel.clone());
        tokio::spawn(async move {
            shutdown.cancelled().await;
            cancel.cancel();
        });
    }
    let log = JobLog::new(job::masker_for(&spec));
    let printer = joblog::spawn_uploader(log.clone(), Sink::Stdout, cancel.clone());
    let result = job::execute(&ctx, &spec, &log, cancel, dir).await;
    log.close();
    let _ = printer.await;
    eprintln!("{}", serde_json::to_string(&result)?);
    Ok(match result.outcome {
        JobOutcome::Success => 0,
        JobOutcome::Canceled => 130,
        JobOutcome::Failed => result.exit_code.filter(|c| *c != 0).unwrap_or(1),
    })
}
