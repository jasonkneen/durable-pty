use crate::paths;
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "dev.durable-pty";

fn exe() -> Result<PathBuf> {
    Ok(std::fs::canonicalize(std::env::current_exe()?)?)
}

fn plist_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn unit_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".config/systemd/user/durable-pty.service")
}

pub fn install(http: Option<&str>) -> Result<()> {
    paths::ensure_home()?;
    let exe = exe()?;
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let log = paths::log_file();
    let http_args = |sep: &str| match http {
        Some(a) => format!("{sep}--http{sep}{a}"),
        None => format!("{sep}--no-http"),
    };
    if cfg!(target_os = "macos") {
        let p = plist_path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let http_xml = match http {
            Some(a) => format!("<string>--http</string><string>{a}</string>"),
            None => "<string>--no-http</string>".to_string(),
        };
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>serve</string>{http_xml}</array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>AbandonProcessGroup</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>{path}</string>
    <key>DPTY_HOME</key><string>{home}</string>
  </dict>
</dict></plist>
"#,
            exe = exe.display(),
            log = log.display(),
            home = paths::home().display(),
        );
        std::fs::write(&p, xml)?;
        let uid = unsafe { libc::getuid() };
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).output();
        let out = Command::new("launchctl").args(["bootstrap", &format!("gui/{uid}"), &p.to_string_lossy()]).output()?;
        if !out.status.success() {
            bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        println!("installed {} — starts at login and restarts if it exits", p.display());
    } else if cfg!(target_os = "linux") {
        let p = unit_path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let unit = format!(
            "[Unit]\nDescription=durable-pty session daemon\n\n[Service]\nExecStart={exe} serve{args}\nRestart=always\nRestartSec=1\nKillMode=process\nEnvironment=PATH={path}\nEnvironment=DPTY_HOME={home}\nStandardOutput=append:{log}\nStandardError=append:{log}\n\n[Install]\nWantedBy=default.target\n",
            exe = exe.display(),
            args = http_args(" "),
            home = paths::home().display(),
            log = log.display(),
        );
        std::fs::write(&p, unit)?;
        for a in [vec!["daemon-reload"], vec!["enable", "--now", "durable-pty.service"]] {
            let out = Command::new("systemctl").arg("--user").args(&a).output()?;
            if !out.status.success() {
                bail!("systemctl {} failed: {}", a.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        println!("installed {} — starts at login and restarts if it exits", p.display());
    } else {
        bail!("install is supported on macOS and Linux");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).output();
        let _ = std::fs::remove_file(plist_path());
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", "durable-pty.service"]).output();
        let _ = std::fs::remove_file(unit_path());
    }
    println!("uninstalled (sessions and history in {} are kept)", paths::home().display());
    Ok(())
}
