//! Continuous ICMP latency probes using the system `ping` (iputils).
//!
//! One long-running `ping` child per reflector; each reply line becomes a `Sample`.
//! Children are killed when the `Pinger` is dropped.

use std::net::IpAddr;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub reflector: usize,
    pub rtt_ms: f64,
}

pub struct Pinger {
    _children: Vec<Child>,
}

/// Parse the RTT from an iputils/busybox reply line ("... time=12.3 ms").
pub fn parse_rtt(line: &str) -> Option<f64> {
    let rest = &line[line.find("time=")? + 5..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

impl Pinger {
    /// Start probing. `iface` binds probes to that interface so they measure its path.
    /// Reflectors that are not literal IP addresses are skipped (they end up in ping's argv).
    pub fn start(
        reflectors: &[String],
        iface: Option<&str>,
        interval_ms: u64,
        tx: mpsc::Sender<Sample>,
    ) -> Self {
        let interval = format!("{:.2}", interval_ms.max(200) as f64 / 1000.0);
        let mut children = Vec::new();

        for (idx, reflector) in reflectors.iter().enumerate() {
            if reflector.parse::<IpAddr>().is_err() {
                log::warn!("Ignoring reflector {:?}: not an IP address", reflector);
                continue;
            }
            let mut cmd = Command::new("ping");
            cmd.args(["-n", "-i", &interval, "-W", "1"]);
            if let Some(i) = iface {
                cmd.args(["-I", i]);
            }
            cmd.arg(reflector)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("Failed to start ping to {}: {}", reflector, e);
                    continue;
                }
            };
            if let Some(stdout) = child.stdout.take() {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(rtt_ms) = parse_rtt(&line) {
                            if tx.send(Sample { reflector: idx, rtt_ms }).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
            children.push(child);
        }

        Self { _children: children }
    }

    pub fn is_empty(&self) -> bool {
        self._children.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iputils_and_busybox_lines() {
        assert_eq!(
            parse_rtt("64 bytes from 1.1.1.1: icmp_seq=3 ttl=57 time=12.3 ms"),
            Some(12.3)
        );
        assert_eq!(
            parse_rtt("64 bytes from 9.9.9.9: seq=0 ttl=60 time=8.012 ms"),
            Some(8.012)
        );
        assert_eq!(parse_rtt("64 bytes from ::1: icmp_seq=1 ttl=64 time=0.040 ms"), Some(0.04));
        assert_eq!(parse_rtt("From 192.168.1.1 icmp_seq=4 Destination Host Unreachable"), None);
        assert_eq!(parse_rtt("PING 1.1.1.1 (1.1.1.1) 56(84) bytes of data."), None);
    }
}
