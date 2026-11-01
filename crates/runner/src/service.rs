//! `install` / `uninstall`: launchd LaunchAgent (macOS) or systemd unit (Linux).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use crate::config::{self, CONFIG_ENV};

const LABEL: &str = "io.github.frank10gm.rust-afariano-runner";
/// Label used by early builds; `install`/`uninstall` remove such an agent.
const LEGACY_LABEL: &str = "com.hartwellbridge.rust-afariano-runner";
const UNIT: &str = "/etc/systemd/system/rust-afariano-runner.service";
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

fn require_config(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("{} does not exist: run `rust-afariano-runner register --url … --token …` first", path.display());
    }
    config::Config::load(path)?;
    Ok(())
}

pub fn install(config_path: &Path) -> Result<()> {
    require_config(config_path)?;
    let config_path = config_path.canonicalize()?;
    let exe = current_exe()?;
    if cfg!(target_os = "macos") {
        install_launchd(&exe, &config_path)
    } else if cfg!(target_os = "linux") {
        install_systemd(&exe, &config_path)
    } else {
        bail!("install supports macOS and Linux only")
    }
}

pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        let uid = uid();
        let plist = plist_path(LABEL);
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).status();
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        remove_legacy_agent()?;
        println!("Removed {}", plist.display());
        Ok(())
    } else if cfg!(target_os = "linux") {
        if !config::is_root() {
            bail!("uninstall needs root (sudo)");
        }
        let _ = Command::new("systemctl").args(["disable", "--now", UNIT_NAME]).status();
        if Path::new(UNIT).exists() {
            std::fs::remove_file(UNIT)?;
        }
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        println!("Removed {UNIT}");
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

pub fn launchd_plist(exe: &Path, config: &Path, home: &Path) -> String {
    let log = home.join("Library/Logs/rust-afariano-runner.log");
    let e = |p: &Path| xml_escape(&p.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
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

fn install_launchd(exe: &Path, config: &Path) -> Result<()> {
    if config::is_root() {
        bail!(
            "on macOS run install as the user that runs Docker Desktop, not root: \
             the runner is a LaunchAgent in that user's session"
        );
    }
    let home = config::home_dir();
    remove_legacy_agent()?;
    let plist = plist_path(LABEL);
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::create_dir_all(home.join("Library/Logs"))?;
    std::fs::write(&plist, launchd_plist(exe, config, &home))
        .with_context(|| format!("writing {}", plist.display()))?;
    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{LABEL}");
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
    println!("Logs: {}", home.join("Library/Logs/rust-afariano-runner.log").display());
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

fn install_systemd(exe: &Path, config: &Path) -> Result<()> {
    if !config::is_root() {
        bail!("install needs root on Linux (sudo rust-afariano-runner install)");
    }
    std::fs::write(UNIT, systemd_unit(exe, config)).with_context(|| format!("writing {UNIT}"))?;
    for args in [&["daemon-reload"][..], &["enable", UNIT_NAME][..], &["restart", UNIT_NAME][..]] {
        let st = Command::new("systemctl").args(args).status()?;
        if !st.success() {
            bail!("systemctl {} failed ({st})", args.join(" "));
        }
    }
    println!("Installed {UNIT}");
    println!("Logs: journalctl -u {UNIT_NAME} -f");
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
        );
        assert!(p.contains("<string>/usr/local/bin/rust-afariano-runner</string>"));
        assert!(p.contains("/usr/local/bin:/opt/homebrew/bin"));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("/Users/runner/Library/Logs/rust-afariano-runner.log"));
        assert!(p.contains(&format!("<key>{CONFIG_ENV}</key>")));
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
