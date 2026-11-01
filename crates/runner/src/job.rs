//! Runs one job: prepare → git → (services) → script → after_script → cleanup.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rust_afariano_proto::{FailureReason, JobOutcome, JobResult, JobSpec, ServiceSpec};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Executor, PullPolicy};
use crate::docker::{self, Docker};
use crate::git::{self, Checkout};
use crate::joblog::{JobLog, MIN_MASK_LEN, Masker};
use crate::proc::{self, Exit, Kill, Stop, Stopper};
use crate::script;

const AFTER_SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
const SERVICE_WAIT_SECS: u32 = 30;

/// What the runner knows about itself.
#[derive(Debug, Clone)]
pub struct RunnerMeta {
    pub name: String,
    pub tags: Vec<String>,
}

/// Masker for the job's masked variables.
pub fn masker_for(spec: &JobSpec) -> Masker {
    Masker::new(spec.variables.iter().filter(|v| v.masked).map(|v| v.value.as_str()))
}

pub fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(data)?;
    // The umask may have narrowed the mode.
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    write_file(path, data, 0o600)
}

fn create_dir(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// `namespace/name` with plain components only.
fn check_project_path(p: &str) -> Result<()> {
    let path = Path::new(p);
    if p.is_empty() || path.is_absolute() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("invalid project path {p:?}");
    }
    Ok(())
}

fn valid_var_name(k: &str) -> bool {
    !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !k.as_bytes()[0].is_ascii_digit()
}

fn set_var(vars: &mut Vec<(String, String)>, k: &str, v: impl Into<String>) {
    vars.retain(|(key, _)| key != k);
    vars.push((k.to_string(), v.into()));
}

/// Where things live, as seen by the job (container or host paths).
struct Paths {
    project_host: PathBuf,
    tmp_host: PathBuf,
    project: String,
    builds: String,
    files: String,
}

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub meta: &'a RunnerMeta,
    pub docker: &'a Docker,
    pub slot: usize,
}

/// Run the job; never panics on job errors, everything ends up in the result
/// and the log.
pub async fn execute(
    ctx: &Ctx<'_>,
    spec: &JobSpec,
    log: &JobLog,
    cancel: CancellationToken,
    local_dir: Option<PathBuf>,
) -> JobResult {
    let stopper = Stopper::new(cancel, Duration::from_secs(spec.timeout_secs.max(1)));
    let exec = ctx.cfg.executor;
    log.line(&format!("\x1b[1mRunning with rust-afariano-runner {}\x1b[0;m", env!("CARGO_PKG_VERSION")));
    log.line(&format!("  on {} ({})", ctx.meta.name, crate::platform()));
    log.step(&format!("Preparing the \"{}\" executor", exec.as_str()));

    let tmp = match tmp_dir(ctx, spec) {
        Ok(t) => t,
        Err(e) => return system_failure(log, &e),
    };
    let mut cleanup = Cleanup::new(ctx.docker, spec.id, &tmp);
    let result = run_steps(ctx, spec, log, &stopper, local_dir, &tmp, &mut cleanup).await;
    log.step("Cleaning up project directory and file based variables");
    cleanup.run(exec).await;

    let result = match result {
        Ok(r) => r,
        Err(e) => system_failure(log, &e),
    };
    match (result.outcome, result.failure_reason) {
        (JobOutcome::Success, _) => log.success("Job succeeded"),
        (JobOutcome::Canceled, _) => log.error("Job canceled"),
        (JobOutcome::Failed, Some(FailureReason::JobTimeout)) => {
            log.error(&format!("Job failed: execution took longer than {}", human_duration(spec.timeout_secs)))
        }
        (JobOutcome::Failed, _) => match result.exit_code {
            Some(c) => log.error(&format!("Job failed: exit code {c}")),
            None => log.error("Job failed"),
        },
    }
    result
}

fn tmp_dir(ctx: &Ctx<'_>, spec: &JobSpec) -> Result<PathBuf> {
    let root = ctx.cfg.builds_dir().join("tmp");
    // Other host users must not reach file variables.
    create_dir(&root, 0o700)?;
    let tmp = root.join(format!("{}-job-{}", ctx.slot, spec.id));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    // Readable inside containers that run as another user.
    create_dir(&tmp, 0o755)?;
    create_dir(&tmp.join("files"), 0o755)?;
    Ok(tmp)
}

fn system_failure(log: &JobLog, e: &anyhow::Error) -> JobResult {
    log.error(&format!("{e:#}"));
    JobResult { outcome: JobOutcome::Failed, exit_code: None, failure_reason: Some(FailureReason::RunnerSystemFailure) }
}

fn stopped(stop: Stop) -> JobResult {
    match stop {
        Stop::Cancel => JobResult { outcome: JobOutcome::Canceled, exit_code: None, failure_reason: None },
        Stop::Timeout => {
            JobResult { outcome: JobOutcome::Failed, exit_code: None, failure_reason: Some(FailureReason::JobTimeout) }
        }
    }
}

fn from_exit(exit: Exit) -> JobResult {
    if let Some(stop) = exit.stopped {
        return stopped(stop);
    }
    match exit.code {
        Some(0) => JobResult { outcome: JobOutcome::Success, exit_code: Some(0), failure_reason: None },
        code => JobResult {
            outcome: JobOutcome::Failed,
            exit_code: code,
            failure_reason: Some(FailureReason::ScriptFailure),
        },
    }
}

async fn run_steps(
    ctx: &Ctx<'_>,
    spec: &JobSpec,
    log: &JobLog,
    stopper: &Stopper,
    local_dir: Option<PathBuf>,
    tmp: &Path,
    cleanup: &mut Cleanup<'_>,
) -> Result<JobResult> {
    check_project_path(&spec.project_path)?;
    let exec = ctx.cfg.executor;
    let builds_root = ctx.cfg.builds_dir();
    let slot_dir = builds_root.join(ctx.slot.to_string());
    let project_host = match local_dir {
        Some(d) => std::fs::canonicalize(&d).with_context(|| format!("--dir {}", d.display()))?,
        None => slot_dir.join(&spec.project_path),
    };
    let paths = match exec {
        Executor::Docker => Paths {
            project: format!("/builds/{}", spec.project_path),
            builds: "/builds".into(),
            files: "/rust-afariano/files".into(),
            project_host,
            tmp_host: tmp.to_path_buf(),
        },
        Executor::Shell => Paths {
            project: project_host.display().to_string(),
            builds: slot_dir.display().to_string(),
            files: tmp.join("files").display().to_string(),
            project_host,
            tmp_host: tmp.to_path_buf(),
        },
    };

    // Variables: file variables become files; runner variables last.
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut plain: Vec<(String, String)> = Vec::new();
    for v in &spec.variables {
        if !valid_var_name(&v.key) {
            log.warn(&format!("skipping variable with invalid name {:?}", v.key));
            continue;
        }
        if v.masked && v.value.len() < MIN_MASK_LEN && !v.value.is_empty() {
            log.warn(&format!("variable {} is too short to be masked", v.key));
        }
        if v.file {
            write_file(&tmp.join("files").join(&v.key), v.value.as_bytes(), 0o644)?;
            set_var(&mut vars, &v.key, format!("{}/{}", paths.files, v.key));
        } else {
            set_var(&mut vars, &v.key, v.value.clone());
            set_var(&mut plain, &v.key, v.value.clone());
        }
    }
    set_var(&mut vars, "CI_PROJECT_DIR", paths.project.clone());
    set_var(&mut vars, "CI_BUILDS_DIR", paths.builds.clone());
    set_var(&mut vars, "CI_SERVER", "yes");
    set_var(&mut vars, "CI_RUNNER_DESCRIPTION", ctx.meta.name.clone());
    set_var(&mut vars, "CI_RUNNER_TAGS", serde_json::to_string(&ctx.meta.tags).unwrap_or_default());
    set_var(&mut vars, "CI_RUNNER_VERSION", env!("CARGO_PKG_VERSION"));
    set_var(&mut vars, "CI_RUNNER_EXECUTABLE_ARCH", crate::platform());
    match exec {
        Executor::Docker => set_var(&mut vars, "CI_DISPOSABLE_ENVIRONMENT", "true"),
        Executor::Shell => set_var(&mut vars, "CI_SHARED_ENVIRONMENT", "true"),
    }

    let main: Vec<String> = spec.before_script.iter().chain(spec.script.iter()).cloned().collect();
    write_file(&tmp.join("script.sh"), script::generate(&main, &paths.project).as_bytes(), 0o644)?;
    if !spec.after_script.is_empty() {
        write_file(&tmp.join("after.sh"), script::generate(&spec.after_script, &paths.project).as_bytes(), 0o644)?;
    }

    // Docker: start pulling and services before the checkout? GitLab pulls
    // first; so do we, so a bad image fails before any git work.
    let mut docker_job = None;
    if exec == Executor::Docker {
        let image = spec.image.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| ctx.cfg.docker.default_image.clone());
        log.line(&format!("Using docker image {image}"));
        ctx.docker.ensure_image(&image, ctx.cfg.docker.pull_policy, log).await?;
        docker_job = Some(DockerJob::new(ctx, spec, &paths, image));
    }

    log.step("Getting source from Git repository");
    std::fs::create_dir_all(&paths.project_host)?;
    match git::checkout(spec, &paths.project_host, tmp, &plain, log, stopper).await? {
        Checkout::Done => {}
        Checkout::Stopped(stop) => return Ok(stopped(stop)),
    }
    if let Some(stop) = stopper.check() {
        return Ok(stopped(stop));
    }

    let result = match &docker_job {
        Some(dj) => {
            if let Err(stop) = dj.start_services(log, &vars, stopper, cleanup).await? {
                return Ok(stopped(stop));
            }
            log.step("Executing \"step_script\" stage of the job script");
            from_exit(dj.run_script("script.sh", "", &vars, log, stopper, cleanup).await?)
        }
        None => {
            log.step("Executing \"step_script\" stage of the job script");
            from_exit(run_shell(ctx.cfg, &paths, "script.sh", &vars, log, stopper).await?)
        }
    };

    if !spec.after_script.is_empty() {
        log.step("Running after_script");
        let status = match result.outcome {
            JobOutcome::Success => "success",
            JobOutcome::Failed => "failed",
            JobOutcome::Canceled => "canceled",
        };
        let mut after_vars = vars.clone();
        set_var(&mut after_vars, "CI_JOB_STATUS", status);
        let after_stop = Stopper::new(CancellationToken::new(), AFTER_SCRIPT_TIMEOUT);
        let exit = match &docker_job {
            Some(dj) => dj.run_script("after.sh", "-after", &after_vars, log, &after_stop, cleanup).await,
            None => run_shell(ctx.cfg, &paths, "after.sh", &after_vars, log, &after_stop).await,
        };
        match exit {
            Ok(e) if e.stopped == Some(Stop::Timeout) => log.warn("after_script timed out"),
            Ok(e) if e.code != Some(0) => log.warn(&format!("after_script failed (exit {:?}); ignored", e.code)),
            Ok(_) => {}
            Err(e) => log.warn(&format!("after_script could not run: {e:#}")),
        }
    }
    Ok(result)
}

async fn run_shell(
    cfg: &Config,
    paths: &Paths,
    script_name: &str,
    vars: &[(String, String)],
    log: &JobLog,
    stopper: &Stopper,
) -> Result<Exit> {
    let shell =
        cfg.shell.shell.clone().unwrap_or_else(|| if which("bash").is_some() { "bash".into() } else { "sh".into() });
    let mut cmd = Command::new(&shell);
    cmd.arg(paths.tmp_host.join(script_name)).current_dir(&paths.project_host).envs(vars.iter().map(|(k, v)| (k, v)));
    proc::run_logged(cmd, log, stopper, Kill::ProcessGroup).await
}

pub fn which(bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// `--entrypoint` flags and the command prefix for an entrypoint override.
fn entrypoint_args(ep: &Option<Vec<String>>) -> (Vec<String>, Vec<String>) {
    match ep {
        None => (vec![], vec![]),
        Some(v) if v.iter().all(|s| s.is_empty()) => (vec!["--entrypoint".into(), String::new()], vec![]),
        Some(v) => (vec!["--entrypoint".into(), v[0].clone()], v[1..].to_vec()),
    }
}

/// Container name, spec and variables of a started service.
type StartedService<'s> = (String, &'s ServiceSpec, Vec<(String, String)>);

struct DockerJob<'a> {
    ctx: &'a Ctx<'a>,
    spec: &'a JobSpec,
    image: String,
    base: String,
    label: String,
    network: String,
    /// `-v` arguments shared by the job and its services.
    mounts: Vec<String>,
    volumes: Vec<String>,
    tmp_host: PathBuf,
    workdir: String,
}

impl<'a> DockerJob<'a> {
    fn new(ctx: &'a Ctx<'a>, spec: &'a JobSpec, paths: &Paths, image: String) -> Self {
        let base = format!("rust-afariano-job-{}", spec.id);
        let mut mounts = Vec::new();
        let mut volumes = Vec::new();
        for (n, v) in ctx.cfg.docker.volumes.iter().enumerate() {
            if v.starts_with('/') && v.contains(':') {
                // host:container[:mode] bind mount
                mounts.push(v.clone());
            } else {
                let name = format!("{base}-v{n}");
                mounts.push(format!("{name}:{v}"));
                volumes.push(name);
            }
        }
        mounts.push(format!("{}:{}", paths.project_host.display(), paths.project));
        Self {
            ctx,
            spec,
            image,
            label: format!("rust-afariano.job={}", spec.id),
            network: base.clone(),
            base,
            mounts,
            volumes,
            tmp_host: paths.tmp_host.clone(),
            workdir: paths.project.clone(),
        }
    }

    fn common_args(&self) -> Vec<String> {
        let mut a = vec!["--network".to_string(), self.network.clone(), "--label".to_string(), self.label.clone()];
        if self.ctx.cfg.docker.privileged {
            a.push("--privileged".into());
        }
        for m in &self.mounts {
            a.push("-v".into());
            a.push(m.clone());
        }
        a
    }

    async fn start_services(
        &self,
        log: &JobLog,
        vars: &[(String, String)],
        stopper: &Stopper,
        cleanup: &mut Cleanup<'_>,
    ) -> Result<Result<(), Stop>> {
        let docker = self.ctx.docker;
        docker.create_network(&self.network, &self.label).await?;
        cleanup.network = Some(self.network.clone());
        cleanup.volumes = self.volumes.clone();

        let mut started: Vec<StartedService<'_>> = Vec::new();
        for (i, svc) in self.spec.services.iter().enumerate() {
            log.line(&format!("Starting service {} ...", svc.name));
            docker.ensure_image(&svc.name, self.ctx.cfg.docker.pull_policy, log).await?;
            let mut aliases = docker::derive_aliases(&svc.name);
            for a in svc.aliases.iter().flat_map(|a| docker::split_aliases(a)) {
                if !aliases.contains(&a) {
                    aliases.push(a);
                }
            }
            let mut svc_vars = vars.to_vec();
            for v in &svc.variables {
                set_var(&mut svc_vars, &v.key, v.value.clone());
            }
            let name = format!("{}-svc-{i}", self.base);
            let env = docker::env_args(&svc_vars, &self.tmp_host.join(format!("env-svc-{i}")))?;
            let (ep_flags, cmd_prefix) = entrypoint_args(&svc.entrypoint);
            let mut cmd = docker.cmd();
            cmd.args(["run", "-d", "--name", &name]).args(self.common_args());
            for a in &aliases {
                cmd.args(["--network-alias", a]);
            }
            cmd.args(&env.args).args(&ep_flags).arg(&svc.name).args(&cmd_prefix);
            if let Some(c) = &svc.command {
                cmd.args(c);
            }
            cmd.envs(env.cli_env.iter().map(|(k, v)| (k, v)));
            proc::capture(cmd).await.with_context(|| format!("starting service {}", svc.name))?;
            started.push((name, svc, svc_vars));
        }

        if started.is_empty() {
            return Ok(Ok(()));
        }
        log.line(&format!("Waiting for services to be up and running (timeout {SERVICE_WAIT_SECS} seconds)..."));
        let helper = &self.ctx.cfg.docker.helper_image;
        let helper_ok = match self.ctx.cfg.docker.pull_policy {
            PullPolicy::Never => docker.image_exists(helper).await,
            _ => {
                if docker.image_exists(helper).await {
                    true
                } else {
                    let mut c = docker.cmd();
                    c.args(["pull", "--quiet", helper]);
                    proc::capture(c).await.is_ok()
                }
            }
        };
        if !helper_ok {
            log.warn(&format!("helper image {helper} unavailable; not waiting for services"));
            return Ok(Ok(()));
        }
        for (name, svc, svc_vars) in &started {
            let ports: Vec<u16> =
                match svc_vars.iter().find(|(k, _)| k == "HEALTHCHECK_TCP_PORT").and_then(|(_, v)| v.parse().ok()) {
                    Some(p) => vec![p],
                    None => docker.exposed_ports(&svc.name).await,
                };
            if ports.is_empty() {
                continue;
            }
            let list = ports.iter().map(u16::to_string).collect::<Vec<_>>().join(" ");
            let probe = format!(
                "for i in $(seq 1 {SERVICE_WAIT_SECS}); do for p in {list}; do nc -z -w 1 {name} $p && exit 0; done; sleep 1; done; exit 1"
            );
            let mut c = docker.cmd();
            c.args(["run", "--rm", "--network", &self.network, "--label", &self.label])
                .arg(helper)
                .args(["sh", "-c", &probe]);
            let ok = tokio::select! {
                r = proc::capture(c) => r.is_ok(),
                stop = stopper.wait() => return Ok(Err(stop)),
            };
            if !ok {
                log.warn(&format!(
                    "Service {} probably didn't start properly: no TCP port of {list} answered within {SERVICE_WAIT_SECS}s",
                    svc.name
                ));
                if !docker.is_running(name).await {
                    log.warn(&format!("Service {} is not running. Its last log lines:", svc.name));
                }
                log.write(docker.logs_tail(name, 30).await.as_bytes());
            }
        }
        Ok(Ok(()))
    }

    async fn run_script(
        &self,
        script_name: &str,
        suffix: &str,
        vars: &[(String, String)],
        log: &JobLog,
        stopper: &Stopper,
        cleanup: &mut Cleanup<'_>,
    ) -> Result<Exit> {
        let docker = self.ctx.docker;
        let name = format!("{}{suffix}", self.base);
        let env = docker::env_args(vars, &self.tmp_host.join(format!("env-job{suffix}")))?;
        let image_ep = self.spec.image.as_ref().and_then(|i| i.entrypoint.clone());
        let (ep_flags, cmd_prefix) = entrypoint_args(&image_ep);
        let mut cmd = docker.cmd();
        cmd.args(["run", "--name", &name])
            .args(self.common_args())
            .arg("-v")
            .arg(format!("{}:/rust-afariano", self.tmp_host.display()))
            .args(["-w", &self.workdir])
            .args(&env.args)
            .args(&ep_flags)
            .arg(&self.image)
            .args(&cmd_prefix)
            .args(["sh", "-c", script::DETECT_SHELL])
            .arg(format!("/rust-afariano/{script_name}"))
            .envs(env.cli_env.iter().map(|(k, v)| (k, v)));
        // A network must exist even without services.
        if cleanup.network.is_none() {
            docker.create_network(&self.network, &self.label).await?;
            cleanup.network = Some(self.network.clone());
            cleanup.volumes = self.volumes.clone();
        }
        let exit =
            proc::run_logged(cmd, log, stopper, Kill::Container { docker: docker.clone(), name: name.clone() }).await?;
        docker.remove_container(&name).await;
        Ok(exit)
    }
}

/// Everything a job leaves behind, removed whatever happened.
struct Cleanup<'a> {
    docker: &'a Docker,
    label: String,
    network: Option<String>,
    volumes: Vec<String>,
    tmp: PathBuf,
}

impl<'a> Cleanup<'a> {
    fn new(docker: &'a Docker, job_id: i64, tmp: &Path) -> Self {
        Self {
            docker,
            label: format!("rust-afariano.job={job_id}"),
            network: None,
            volumes: Vec::new(),
            tmp: tmp.to_path_buf(),
        }
    }

    async fn run(&mut self, exec: Executor) {
        if exec == Executor::Docker {
            self.docker.remove_job_containers(&self.label).await;
            if let Some(n) = self.network.take() {
                self.docker.remove_network(&n).await;
            }
            for v in self.volumes.drain(..) {
                self.docker.remove_volume(&v).await;
            }
        }
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

pub fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_path_validation() {
        assert!(check_project_path("team/website").is_ok());
        assert!(check_project_path("../etc").is_err());
        assert!(check_project_path("/abs").is_err());
        assert!(check_project_path("a/../b").is_err());
        assert!(check_project_path("").is_err());
    }

    #[test]
    fn var_names() {
        assert!(valid_var_name("CI_JOB_ID"));
        assert!(valid_var_name("_x1"));
        assert!(!valid_var_name("1abc"));
        assert!(!valid_var_name("A-B"));
        assert!(!valid_var_name(""));
    }

    #[test]
    fn entrypoints() {
        assert_eq!(entrypoint_args(&None), (vec![], vec![]));
        assert_eq!(entrypoint_args(&Some(vec![])), (vec!["--entrypoint".to_string(), String::new()], vec![]));
        assert_eq!(
            entrypoint_args(&Some(vec![String::new()])),
            (vec!["--entrypoint".to_string(), String::new()], vec![])
        );
        assert_eq!(
            entrypoint_args(&Some(vec!["/bin/sh".into(), "-c".into()])),
            (vec!["--entrypoint".to_string(), "/bin/sh".to_string()], vec!["-c".to_string()])
        );
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(1800), "30m");
        assert_eq!(human_duration(5400), "1h 30m");
        assert_eq!(human_duration(0), "0s");
    }
}
