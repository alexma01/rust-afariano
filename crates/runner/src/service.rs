//! `install` / `uninstall`: launchd LaunchAgent (macOS) or systemd unit (Linux).
//!
//! The default config file gets the plain service name. Any other config file
//! (`--config`) gets a service named after its runner, so several runners can live
//! on one host, e.g. a docker and a shell executor on the same Mac.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use crate::config::{self, CONFIG_ENV};

const LABEL: &str = "io.github.frank10gm.rust-afariano-runner";
/// Label used by early builds; `install`/`uninstall` remove such an agent.
const LEGACY_LABEL: &str = "com.hartwellbridge.rust-afariano-runner";
const UNIT_DIR: &str = "/etc/systemd/system";
const UNIT_NAME: &str = "rust-afariano-runner";
/// Running jobs get `shutdown_timeout_secs` (default 600) on stop; give the
/// service manager a bit more before it kills us.
const STOP_TIMEOUT_SECS: u64 = 660;

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let s = exe.display().to_string();
    if s.contains("/target/") {
        eprintln!(
            "warning: installing {s}, a build directory; copy the binary somewhere stable \
             (e.g. /usr/local/bin) and run install from there"
        );
    }
    Ok(exe)
}

/// Names of one runner's service: launchd label, systemd unit and log file.
#[derive(Debug, PartialEq, Eq)]
pub struct Service {
    /// `None` for the default config file, else the sanitized runner name.
    suffix: Option<String>,
}

impl Service {
    /// The service for `config_path`: plain for the built-in default config file
    /// (whatever `$RUST_AFARIANO_RUNNER_CONFIG` says), else suffixed with `runner_name`.
    pub fn for_config(config_path: &Path, default_path: &Path, runner_name: &str) -> Result<Self> {
        if same_file(config_path, default_path) {
            return Ok(Self { suffix: None });
        }
        let suffix: String = runner_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
            .collect();
        let suffix = suffix.trim_matches(|c| c == '-' || c == '.').to_string();
        if suffix.is_empty() {
            bail!("the runner name {runner_name:?} cannot name a service: set `name` in {}", config_path.display());
        }
        Ok(Self { suffix: Some(suffix) })
    }

    fn with_suffix(&self, base: &str, sep: char) -> String {
        match &self.suffix {
            None => base.to_string(),
            Some(s) => format!("{base}{sep}{s}"),
        }
    }

    pub fn label(&self) -> String {
        self.with_suffix(LABEL, '.')
    }

    pub fn unit_name(&self) -> String {
        self.with_suffix(UNIT_NAME, '-')
    }

    fn unit_path(&self) -> PathBuf {
        Path::new(UNIT_DIR).join(format!("{}.service", self.unit_name()))
    }

    fn log_path(&self, home: &Path) -> PathBuf {
        home.join("Library/Logs").join(format!("{}.log", self.with_suffix(UNIT_NAME, '-')))
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The config file used when neither `--config` nor `$RUST_AFARIANO_RUNNER_CONFIG` is given.
fn builtin_config_path() -> PathBuf {
    config::select_config_path(None, config::is_root(), &config::home_dir())
}

fn require_config(path: &Path) -> Result<config::Config> {
    if !path.exists() {
        bail!("{} does not exist: run `rust-afariano-runner register --url … --token …` first", path.display());
    }
    config::Config::load(path)
}

pub fn install(config_path: &Path) -> Result<()> {
    let cfg = require_config(config_path)?;
    let config_path = config_path.canonicalize()?;
    let service = Service::for_config(&config_path, &builtin_config_path(), &cfg.name)?;
    let exe = current_exe()?;
    if cfg!(target_os = "macos") {
        install_launchd(&exe, &config_path, &service)
    } else if cfg!(target_os = "linux") {
        install_systemd(&exe, &config_path, &service)
    } else {
        bail!("install supports macOS and Linux only")
    }
}

pub fn uninstall(config_path: &Path) -> Result<()> {
    let default = builtin_config_path();
    let service = if same_file(config_path, &default) {
        Service { suffix: None }
    } else {
        // The runner name in the file names the service.
        let cfg = require_config(config_path)?;
        Service::for_config(config_path, &default, &cfg.name)?
    };
    if cfg!(target_os = "macos") {
        let label = service.label();
        let plist = plist_path(&label);
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{label}", uid())]).status();
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        if service.suffix.is_none() {
            remove_legacy_agent()?;
        }
        println!("Removed {}", plist.display());
        Ok(())
    } else if cfg!(target_os = "linux") {
        if !config::is_root() {
            bail!("uninstall needs root (sudo)");
        }
        let unit = service.unit_path();
        let _ = Command::new("systemctl").args(["disable", "--now", &service.unit_name()]).status();
        if unit.exists() {
            std::fs::remove_file(&unit)?;
        }
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        println!("Removed {}", unit.display());
        Ok(())
    } else {
        bail!("uninstall supports macOS and Linux only")
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn plist_path(label: &str) -> PathBuf {
    config::home_dir().join("Library/LaunchAgents").join(format!("{label}.plist"))
}

/// Stops and deletes a LaunchAgent installed under the old label, if any.
fn remove_legacy_agent() -> Result<()> {
    let plist = plist_path(LEGACY_LABEL);
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{LEGACY_LABEL}", uid())])
        .stderr(std::process::Stdio::null())
        .status();
    if plist.exists() {
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        println!("Removed the old LaunchAgent {}", plist.display());
    }
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn launchd_plist(exe: &Path, config: &Path, home: &Path, service: &Service) -> String {
    let label = xml_escape(&service.label());
    let log = service.log_path(home);
    let e = |p: &Path| xml_escape(&p.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>{CONFIG_ENV}</key>
    <string>{config}</string>
  </dict>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Standard</string>
  <key>ExitTimeOut</key>
  <integer>{STOP_TIMEOUT_SECS}</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        exe = e(exe),
        config = e(config),
        home = e(home),
        log = e(&log),
    )
}

fn install_launchd(exe: &Path, config: &Path, service: &Service) -> Result<()> {
    if config::is_root() {
        bail!(
            "on macOS run install as the user that runs Docker Desktop, not root: \
             the runner is a LaunchAgent in that user's session"
        );
    }
    let home = config::home_dir();
    if service.suffix.is_none() {
        remove_legacy_agent()?;
    }
    let label = service.label();
    let plist = plist_path(&label);
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::create_dir_all(home.join("Library/Logs"))?;
    std::fs::write(&plist, launchd_plist(exe, config, &home, service))
        .with_context(|| format!("writing {}", plist.display()))?;
    let domain = format!("gui/{}", uid());
    let log = service.log_path(&home);
    let service = format!("{domain}/{label}");
    // Replace a running copy, if any.
    let _ = Command::new("launchctl").args(["bootout", &service]).status();
    let _ = Command::new("launchctl").args(["enable", &service]).status();
    let st =
        Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist).status().context("running launchctl")?;
    if !st.success() {
        bail!(
            "launchctl bootstrap {domain} failed ({st}); the user must be logged in to the GUI \
             session (automatic login) for a LaunchAgent"
        );
    }
    println!("Installed {}", plist.display());
    println!("Logs: {}", log.display());
    println!("Status: launchctl print {service}");
    Ok(())
}

pub fn systemd_unit(exe: &Path, config: &Path) -> String {
    format!(
        "[Unit]
Description=rust-afariano runner
After=network-online.target docker.service
Wants=network-online.target

[Service]
ExecStart={exe} run
Environment={CONFIG_ENV}={config}
Restart=always
RestartSec=5
KillMode=mixed
TimeoutStopSec={STOP_TIMEOUT_SECS}

[Install]
WantedBy=multi-user.target
",
        exe = exe.display(),
        config = config.display(),
    )
}

fn install_systemd(exe: &Path, config: &Path, service: &Service) -> Result<()> {
    if !config::is_root() {
        bail!("install needs root on Linux (sudo rust-afariano-runner install)");
    }
    let unit = service.unit_path();
    let name = service.unit_name();
    std::fs::write(&unit, systemd_unit(exe, config)).with_context(|| format!("writing {}", unit.display()))?;
    for args in [&["daemon-reload"][..], &["enable", &name][..], &["restart", &name][..]] {
        let st = Command::new("systemctl").args(args).status()?;
        if !st.success() {
            bail!("systemctl {} failed ({st})", args.join(" "));
        }
    }
    println!("Installed {}", unit.display());
    println!("Logs: journalctl -u {name} -f");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_paths_and_path_env() {
        let p = launchd_plist(
            Path::new("/usr/local/bin/rust-afariano-runner"),
            Path::new("/Users/runner/.config/rust-afariano-runner/config.toml"),
            Path::new("/Users/runner"),
            &Service { suffix: None },
        );
        assert!(p.contains("<string>/usr/local/bin/rust-afariano-runner</string>"));
        assert!(p.contains("/usr/local/bin:/opt/homebrew/bin"));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains(&format!("<string>{LABEL}</string>")));
        assert!(p.contains("/Users/runner/Library/Logs/rust-afariano-runner.log"));
        assert!(p.contains(&format!("<key>{CONFIG_ENV}</key>")));
    }

    #[test]
    fn default_config_keeps_plain_names() {
        let d = Path::new("/Users/r/.config/rust-afariano-runner/config.toml");
        let s = Service::for_config(d, d, "mac-studio").unwrap();
        assert_eq!(s.label(), LABEL);
        assert_eq!(s.unit_name(), "rust-afariano-runner");
        assert_eq!(s.unit_path(), PathBuf::from("/etc/systemd/system/rust-afariano-runner.service"));
        assert_eq!(s.log_path(Path::new("/Users/r")), PathBuf::from("/Users/r/Library/Logs/rust-afariano-runner.log"));
    }

    #[test]
    fn other_config_is_named_after_the_runner() {
        let d = Path::new("/Users/r/.config/rust-afariano-runner/config.toml");
        let c = Path::new("/Users/r/.config/rust-afariano-runner/shell.toml");
        let s = Service::for_config(c, d, "mac studio/shell").unwrap();
        assert_eq!(s.label(), format!("{LABEL}.mac-studio-shell"));
        assert_eq!(s.unit_name(), "rust-afariano-runner-mac-studio-shell");
        assert_eq!(s.unit_path(), PathBuf::from("/etc/systemd/system/rust-afariano-runner-mac-studio-shell.service"));
        assert_eq!(
            s.log_path(Path::new("/Users/r")),
            PathBuf::from("/Users/r/Library/Logs/rust-afariano-runner-mac-studio-shell.log")
        );
        let p = launchd_plist(Path::new("/bin/r"), c, Path::new("/Users/r"), &s);
        assert!(p.contains(&format!("<string>{LABEL}.mac-studio-shell</string>")));
        assert!(p.contains("rust-afariano-runner-mac-studio-shell.log"));
        assert!(Service::for_config(c, d, "../").is_err());
    }

    #[test]
    fn unit_has_exec_and_env() {
        let u = systemd_unit(
            Path::new("/usr/local/bin/rust-afariano-runner"),
            Path::new("/etc/rust-afariano-runner/config.toml"),
        );
        assert!(u.contains("ExecStart=/usr/local/bin/rust-afariano-runner run"));
        assert!(u.contains("Environment=RUST_AFARIANO_RUNNER_CONFIG=/etc/rust-afariano-runner/config.toml"));
    }
}
