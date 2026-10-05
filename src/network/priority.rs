//! Game traffic prioritization (DSCP EF via nftables)
//!
//! Marks outgoing game and game-streaming UDP as DSCP EF. This matters in two places:
//! - mac80211 maps DSCP to the Wi-Fi access category before queueing, so these packets
//!   use the Voice/Video WMM queues and win airtime contention on our own transmissions.
//! - CAKE (diffserv4) puts EF in its latency-sensitive tin, ahead of bulk uploads.
//!
//! It does not change how the AP or the internet treat our traffic.

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const TABLE: &str = "hifi_wifi";

/// cgroup v2 paths whose UDP sockets are game traffic. Only gamescope (SteamOS / Bazzite game
/// mode) is specific enough: user app slices also hold browsers, whose QUIC uploads must not
/// be marked EF.
const GAME_CGROUPS: &[&str] = &["gamescope.slice"];

/// Port ranges ("27000-27100" or "3478") are passed into the nft script, so only digits and one dash.
pub fn is_valid_port_range(s: &str) -> bool {
    let mut parts = s.split('-');
    let valid = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u32>().is_ok_and(|n| n <= 65535));
    match (parts.next(), parts.next(), parts.next()) {
        (a, None, None) => valid(a),
        (a, b, None) => valid(a) && valid(b),
        _ => false,
    }
}

/// Build the complete ruleset. Starts by deleting our table, so loading is atomic and idempotent.
pub fn ruleset(ports: &[String], cgroups: &[String]) -> String {
    let ports: Vec<&str> = ports
        .iter()
        .map(String::as_str)
        .filter(|p| is_valid_port_range(p))
        .collect();
    let mut rules = String::new();
    if !ports.is_empty() {
        let set = ports.join(", ");
        rules.push_str(&format!("        udp dport {{ {} }} jump game\n", set));
        rules.push_str(&format!("        udp sport {{ {} }} jump game\n", set));
    }
    for cg in cgroups {
        let level = cg.split('/').count();
        rules.push_str(&format!(
            "        meta l4proto udp socket cgroupv2 level {} \"{}\" jump game\n",
            level, cg
        ));
    }
    format!(
        "table inet {t}\ndelete table inet {t}\ntable inet {t} {{\n    chain game {{\n        meta nfproto ipv4 ip dscp set ef\n        meta nfproto ipv6 ip6 dscp set ef\n    }}\n    chain postrouting {{\n        type filter hook postrouting priority mangle; policy accept;\n{rules}    }}\n}}\n",
        t = TABLE,
        rules = rules
    )
}

fn nft_load(script: &str) -> Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run nft")?;
    child
        .stdin
        .take()
        .context("nft stdin")?
        .write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("nft: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Keeps the ruleset in place; re-applies when the set of game cgroups changes
/// (gamescope.slice appears only once the game-mode session starts).
#[derive(Default)]
pub struct GamePriority {
    ports: Vec<String>,
    applied: Option<Vec<String>>,
    failed: bool,
}

impl GamePriority {
    pub fn new(enabled: bool, ports: Vec<String>) -> Self {
        for p in ports.iter().filter(|p| !is_valid_port_range(p)) {
            warn!("Ignoring invalid priority port range {:?}", p);
        }
        Self {
            ports,
            applied: None,
            // Disabled behaves like "could not apply": ensure() becomes a no-op
            failed: !enabled,
        }
    }

    pub fn ensure(&mut self) {
        if self.failed {
            return;
        }
        let cgroups: Vec<String> = GAME_CGROUPS
            .iter()
            .filter(|c| Path::new("/sys/fs/cgroup").join(c).is_dir())
            .map(|c| c.to_string())
            .collect();
        if self.applied.as_ref() == Some(&cgroups) {
            return;
        }
        // Kernels without nft socket/cgroupv2 support reject the cgroup rule (and with it
        // the whole atomic load), so fall back to port rules alone.
        let result = nft_load(&ruleset(&self.ports, &cgroups)).or_else(|e| {
            if cgroups.is_empty() {
                return Err(e);
            }
            debug!("cgroup matching unsupported ({}), using port rules only", e);
            nft_load(&ruleset(&self.ports, &[]))
        });
        match result {
            Ok(()) => {
                info!(
                    "Game traffic priority active (DSCP EF): UDP ports {:?}, cgroups {:?}",
                    self.ports, cgroups
                );
                self.applied = Some(cgroups);
            }
            Err(e) => {
                warn!("Game traffic priority unavailable: {}", e);
                self.failed = true;
            }
        }
    }

    pub fn remove(&mut self) {
        remove();
        self.applied = None;
    }
}

/// Remove our table (also cleans up tables left by older versions). Idempotent.
pub fn remove() {
    match nft_load(&format!("table inet {t}\ndelete table inet {t}\n", t = TABLE)) {
        Ok(()) => debug!("Removed nft table {}", TABLE),
        Err(e) => debug!("nft cleanup skipped: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_ranges_are_validated() {
        assert!(is_valid_port_range("27000-27100"));
        assert!(is_valid_port_range("3478"));
        assert!(!is_valid_port_range("70000"));
        assert!(!is_valid_port_range("1-2-3"));
        assert!(!is_valid_port_range("80; flush ruleset"));
        assert!(!is_valid_port_range(""));
        assert!(!is_valid_port_range("-5"));
    }

    #[test]
    fn ruleset_marks_both_directions_and_both_families() {
        let r = ruleset(
            &["27000-27100".into(), "47998-48010".into(), "bad;".into()],
            &["gamescope.slice".into()],
        );
        assert!(r.starts_with("table inet hifi_wifi\ndelete table inet hifi_wifi\n"));
        assert!(r.contains("udp dport { 27000-27100, 47998-48010 } jump game"));
        assert!(r.contains("udp sport { 27000-27100, 47998-48010 } jump game"));
        assert!(r.contains("socket cgroupv2 level 1 \"gamescope.slice\" jump game"));
        assert!(r.contains("ip dscp set ef") && r.contains("ip6 dscp set ef"));
        assert!(!r.contains("bad"));
    }
}
