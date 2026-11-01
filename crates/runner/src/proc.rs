//! Child processes whose output goes to the job log, with timeout and cancel.

use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::process::Command;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::joblog::JobLog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Timeout,
    Cancel,
}

/// Fires on cancel or when the deadline passes.
#[derive(Clone)]
pub struct Stopper {
    pub cancel: CancellationToken,
    pub deadline: Instant,
}

impl Stopper {
    pub fn new(cancel: CancellationToken, timeout: Duration) -> Self {
        Self { cancel, deadline: Instant::now() + timeout }
    }

    pub async fn wait(&self) -> Stop {
        tokio::select! {
            _ = self.cancel.cancelled() => Stop::Cancel,
            _ = tokio::time::sleep_until(self.deadline) => Stop::Timeout,
        }
    }

    pub fn check(&self) -> Option<Stop> {
        if self.cancel.is_cancelled() {
            Some(Stop::Cancel)
        } else if Instant::now() >= self.deadline {
            Some(Stop::Timeout)
        } else {
            None
        }
    }
}

/// How to stop a running child.
pub enum Kill {
    /// The child leads its own process group: TERM the group, then KILL.
    ProcessGroup,
    /// `docker rm -f <name>` (the attached CLI then exits by itself).
    Container { docker: crate::docker::Docker, name: String },
}

#[derive(Debug, Clone, Copy)]
pub struct Exit {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stopped: Option<Stop>,
}

/// Run `cmd` with stdout+stderr merged into `log`, stdin from /dev/null.
pub async fn run_logged(mut cmd: Command, log: &JobLog, stopper: &Stopper, kill: Kill) -> Result<Exit> {
    let (reader, writer) = std::io::pipe().context("creating output pipe")?;
    cmd.stdin(Stdio::null()).stdout(writer.try_clone().context("cloning pipe")?).stderr(writer);
    if matches!(kill, Kill::ProcessGroup) {
        cmd.process_group(0);
    }
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("starting process")?;
    // The Command holds the pipe's write ends: drop it so EOF can arrive.
    drop(cmd);
    let pid = child.id();

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let reader_log = log.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => reader_log.write(&buf[..n]),
            }
        }
        let _ = done_tx.send(());
    });

    let mut stopped = None;
    let status = tokio::select! {
        s = child.wait() => s?,
        stop = stopper.wait() => {
            stopped = Some(stop);
            terminate(&mut child, pid, &kill).await;
            child.wait().await?
        }
    };

    if matches!(kill, Kill::ProcessGroup) {
        // Background processes left by the script would keep the pipe open.
        if let Some(pid) = pid {
            signal_group(pid, libc::SIGKILL);
        }
    }
    // Let the reader drain what is buffered; a leftover daemon holding the
    // pipe must not hang the job.
    let _ = tokio::time::timeout(Duration::from_secs(3), done_rx).await;

    Ok(Exit { code: status.code(), stopped })
}

async fn terminate(child: &mut tokio::process::Child, pid: Option<u32>, kill: &Kill) {
    match kill {
        Kill::ProcessGroup => {
            if let Some(pid) = pid {
                signal_group(pid, libc::SIGTERM);
                if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                    signal_group(pid, libc::SIGKILL);
                }
            } else {
                let _ = child.start_kill();
            }
        }
        Kill::Container { docker, name } => {
            docker.remove_container(name).await;
            if tokio::time::timeout(Duration::from_secs(15), child.wait()).await.is_err() {
                let _ = child.start_kill();
            }
        }
    }
}

fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: kill(2) with a negative pid signals the process group; no memory is touched.
    unsafe {
        libc::kill(-(pid as libc::pid_t), sig);
    }
}

/// Run a short helper command, capturing output (not logged). Errors include stderr.
pub async fn capture(mut cmd: Command) -> Result<String> {
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    let out = cmd.output().await.context("starting process")?;
    if !out.status.success() {
        anyhow::bail!("exit {}: {}", out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
