//! Repository checkout on the runner host.

use std::path::Path;

use anyhow::{Result, bail};
use rust_afariano_proto::{CI_GIT_USER, JobSpec};
use tokio::process::Command;

use crate::joblog::JobLog;
use crate::proc::{self, Kill, Stop, Stopper};

/// Answers git's username/password prompts from env vars that only the git
/// process sees, so the token is never on disk or in argv.
const ASKPASS: &str = "#!/bin/sh\ncase \"$1\" in\n  Username*) printf '%s\\n' \"$RUST_AFARIANO_GIT_USERNAME\" ;;\n  *) printf '%s\\n' \"$RUST_AFARIANO_GIT_PASSWORD\" ;;\nesac\n";

pub enum Checkout {
    Done,
    Stopped(Stop),
}

struct Git<'a> {
    dir: &'a Path,
    askpass: &'a Path,
    token: &'a str,
    env: &'a [(String, String)],
}

impl Git<'_> {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(self.dir)
            .args(["-c", "credential.helper="])
            .arg("-c")
            .arg(format!("safe.directory={}", self.dir.display()))
            .args(args);
        for (k, v) in self.env {
            c.env(k, v);
        }
        c.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", self.askpass)
            .env("RUST_AFARIANO_GIT_USERNAME", CI_GIT_USER)
            .env("RUST_AFARIANO_GIT_PASSWORD", self.token);
        c
    }

    async fn run(&self, args: &[&str], log: &JobLog, stopper: &Stopper) -> Result<Result<(), Stop>> {
        let exit = proc::run_logged(self.cmd(args), log, stopper, Kill::ProcessGroup).await?;
        if let Some(stop) = exit.stopped {
            return Ok(Err(stop));
        }
        if exit.code != Some(0) {
            bail!("git {} failed (exit {:?})", args.first().unwrap_or(&""), exit.code);
        }
        Ok(Ok(()))
    }

    async fn ok(&self, args: &[&str]) -> bool {
        let mut c = self.cmd(args);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).stdin(std::process::Stdio::null());
        matches!(c.status().await, Ok(s) if s.success())
    }
}

/// Fetch and check out `spec.git.sha` into `dir`. `env` are the job's plain
/// variables (e.g. `GIT_LFS_SKIP_SMUDGE`).
pub async fn checkout(
    spec: &JobSpec,
    dir: &Path,
    tmp: &Path,
    env: &[(String, String)],
    log: &JobLog,
    stopper: &Stopper,
) -> Result<Checkout> {
    let g = &spec.git;
    let strategy = g.strategy.as_str();
    if strategy == "none" {
        log.line("Skipping Git repository setup (GIT_STRATEGY=none)");
        return Ok(Checkout::Done);
    }
    if !is_hex_sha(&g.sha) {
        bail!("invalid commit sha {:?}", g.sha);
    }
    if strategy == "clone" && dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;

    let askpass = tmp.join("askpass.sh");
    crate::job::write_file(&askpass, ASKPASS.as_bytes(), 0o700)?;
    let git = Git { dir, askpass: &askpass, token: &spec.token, env };

    // The project path, not the host directory: logs are shared with everyone
    // who can see the repository and should not reveal the runner's file system.
    if dir.join(".git").is_dir() {
        log.line(&format!("Reusing the Git checkout of {}", spec.project_path));
    } else {
        log.line(&format!("Initialized an empty Git checkout for {}", spec.project_path));
        if let Err(stop) = git.run(&["init", "-q"], log, stopper).await? {
            return Ok(Checkout::Stopped(stop));
        }
    }
    if let Err(stop) = git.run(&["config", "remote.origin.url", &g.url], log, stopper).await? {
        return Ok(Checkout::Stopped(stop));
    }

    let depth = g.depth;
    if depth > 0 {
        log.line(&format!("Fetching changes with git depth set to {depth}..."));
    } else {
        log.line("Fetching changes...");
    }
    let refspec = if g.ref_is_tag {
        format!("+refs/tags/{0}:refs/tags/{0}", g.ref_name)
    } else {
        format!("+refs/heads/{0}:refs/remotes/origin/{0}", g.ref_name)
    };
    let depth_arg = format!("--depth={depth}");
    let mut fetch: Vec<&str> = vec!["fetch", "--prune", "--no-recurse-submodules", "--quiet"];
    if depth > 0 {
        fetch.push(&depth_arg);
    } else if dir.join(".git").join("shallow").exists() {
        fetch.push("--unshallow");
    }
    fetch.push("origin");

    let mut by_ref = fetch.clone();
    by_ref.push(&refspec);
    if let Err(stop) = git.run(&by_ref, log, stopper).await? {
        return Ok(Checkout::Stopped(stop));
    }
    let commit = format!("{}^{{commit}}", g.sha);
    if !git.ok(&["cat-file", "-e", &commit]).await {
        // The ref moved past this commit (or depth is too small): ask for it directly.
        let mut by_sha = fetch.clone();
        by_sha.push(&g.sha);
        if let Err(stop) = git.run(&by_sha, log, stopper).await? {
            return Ok(Checkout::Stopped(stop));
        }
    }

    let short = &g.sha[..8.min(g.sha.len())];
    log.line(&format!("Checking out {short} as detached HEAD (ref is {})...", g.ref_name));
    for args in [&["checkout", "-f", "-q", "--detach", g.sha.as_str()][..], &["clean", "-ffdx", "-q"][..]] {
        if let Err(stop) = git.run(args, log, stopper).await? {
            return Ok(Checkout::Stopped(stop));
        }
    }
    Ok(Checkout::Done)
}

fn is_hex_sha(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
