//! System-wide SOCKS proxy management (macOS).
//!
//! While the tunnel runs we point every enabled network service at the local
//! SOCKS listener; on stop we restore exactly what was configured before. That
//! is what makes Safari, Chrome, the App Store and even plain `curl` (without
//! `-x`) use the bond with no manual setup, WireGuard-style.
//!
//! `networksetup` needs root for changes. We try it directly first (works when
//! the app is already privileged or when the user has granted it) and fall back
//! to a single macOS authorisation prompt for the whole batch.

#[allow(dead_code)]
const NETWORKSETUP: &str = "/usr/sbin/networksetup";

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Saved {
    service: String,
    enabled: bool,
    server: String,
    port: u16,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct SocksState {
    enabled: bool,
    server: String,
    port: u16,
}

/// Keeps the system proxy pointed at our SOCKS listener and restores the
/// previous settings when dropped (stop, window quit, panic, ...).
pub struct Guard {
    saved: Vec<Saved>,
}

impl Guard {
    #[cfg(not(target_os = "macos"))]
    fn noop() -> Self {
        Self { saved: Vec::new() }
    }

    /// Number of network services that were changed.
    pub fn count(&self) -> usize {
        self.saved.len()
    }

    #[cfg(target_os = "macos")]
    fn restore(&self) -> anyhow::Result<()> {
        if self.saved.is_empty() {
            return Ok(());
        }
        let mut cmds = Vec::new();
        for s in &self.saved {
            if s.enabled {
                cmds.push(format!(
                    "{NETWORKSETUP} -setsocksfirewallproxy {} {} {}",
                    shq(&s.service),
                    shq(&s.server),
                    s.port
                ));
                cmds.push(format!(
                    "{NETWORKSETUP} -setsocksfirewallproxystate {} on",
                    shq(&s.service)
                ));
            } else {
                cmds.push(format!(
                    "{NETWORKSETUP} -setsocksfirewallproxystate {} off",
                    shq(&s.service)
                ));
            }
        }
        apply(&cmds)
    }

    #[cfg(not(target_os = "macos"))]
    fn restore(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.saved.is_empty() {
            return;
        }
        match self.restore() {
            Ok(()) => eprintln!("system proxy: restored ({} service(s))", self.saved.len()),
            Err(e) => eprintln!("system proxy: restore failed: {e:#}"),
        }
    }
}

/// Point the system SOCKS proxy at `host:port` on every enabled network
/// service. On non-macOS this is a no-op.
#[cfg(target_os = "macos")]
pub fn enable(host: &str, port: u16) -> anyhow::Result<Guard> {
    use anyhow::{bail, Context};

    let services = services()?;
    if services.is_empty() {
        bail!("no enabled network service found");
    }

    let mut saved = Vec::new();
    let mut cmds = Vec::new();
    for svc in services {
        let prev = get_socks(&svc).unwrap_or_default();
        saved.push(Saved {
            service: svc.clone(),
            enabled: prev.enabled,
            server: prev.server,
            port: prev.port,
        });
        cmds.push(format!(
            "{NETWORKSETUP} -setsocksfirewallproxy {} {} {}",
            shq(&svc),
            shq(host),
            port
        ));
        cmds.push(format!(
            "{NETWORKSETUP} -setsocksfirewallproxystate {} on",
            shq(&svc)
        ));
    }

    apply(&cmds).with_context(|| "setting the system SOCKS proxy")?;
    eprintln!("system proxy: {host}:{port} on {} service(s)", saved.len());
    Ok(Guard { saved })
}

#[cfg(not(target_os = "macos"))]
pub fn enable(_host: &str, _port: u16) -> anyhow::Result<Guard> {
    Ok(Guard::noop())
}

/// Whether system proxy management is available on this platform.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

// ---------------------------------------------------------------------------
// macOS helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn services() -> anyhow::Result<Vec<String>> {
    let out = std::process::Command::new(NETWORKSETUP)
        .arg("-listallnetworkservices")
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "networksetup -listallnetworkservices: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut services = Vec::new();
    for line in text.lines().skip(1) {
        let line = line.trim();
        // Disabled services are prefixed with '*'.
        if line.is_empty() || line.starts_with('*') {
            continue;
        }
        services.push(line.to_string());
    }
    Ok(services)
}

#[cfg(target_os = "macos")]
fn get_socks(service: &str) -> anyhow::Result<SocksState> {
    let out = std::process::Command::new(NETWORKSETUP)
        .args(["-getsocksfirewallproxy", service])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut state = SocksState::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Enabled" => state.enabled = value.eq_ignore_ascii_case("yes"),
            "Server" => state.server = value.to_string(),
            "Port" => state.port = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    Ok(state)
}

/// Run the commands, escalating to a single admin prompt only if needed.
#[cfg(target_os = "macos")]
fn apply(cmds: &[String]) -> anyhow::Result<()> {
    use anyhow::bail;

    for cmd in cmds {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .output()?;
        if out.status.success() {
            continue;
        }
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        if is_permission_error(&err) {
            // Run the whole batch once, privileged: one authorisation prompt.
            return run_admin(&cmds.join(" ; "));
        }
        bail!("networksetup failed: {}", err.trim());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_permission_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("must be root")
        || s.contains("not permitted")
        || s.contains("not authorized")
        || s.contains("authorization")
        || s.contains("operation not permitted")
}

#[cfg(target_os = "macos")]
fn run_admin(script: &str) -> anyhow::Result<()> {
    use anyhow::bail;

    // AppleScript string literal, then it becomes the `do shell script` body.
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(format!(
            "do shell script \"{escaped}\" with administrator privileges"
        ))
        .output()?;
    if !out.status.success() {
        bail!(
            "authorisation refused: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Single-quote a string for `/bin/sh`.
#[cfg(target_os = "macos")]
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
