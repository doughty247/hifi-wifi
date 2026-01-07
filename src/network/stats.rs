//! Network Statistics Monitor
//!
//! Reads /sys/class/net/<iface>/statistics for PPS (packets per second) calculation.
//! Per rewrite.md: Game Mode detection via PPS threshold > 200.

use log::debug;
use std::fs;
use std::time::Instant;

/// Network statistics from sysfs
#[derive(Debug, Clone, Default)]
pub struct NetStats {
    pub rx_packets: u64,
    pub tx_packets: u64,
}

impl NetStats {
    /// Read stats from /sys/class/net/<iface>/statistics
    pub fn read(interface: &str) -> Option<Self> {
        let base = format!("/sys/class/net/{}/statistics", interface);
        
        Some(NetStats {
            rx_packets: Self::read_stat(&base, "rx_packets")?,
            tx_packets: Self::read_stat(&base, "tx_packets")?,
        })
    }

    fn read_stat(base: &str, name: &str) -> Option<u64> {
        fs::read_to_string(format!("{}/{}", base, name))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Total packets (rx + tx)
    pub fn total_packets(&self) -> u64 {
        self.rx_packets + self.tx_packets
    }
}

/// Packets Per Second (PPS) monitor for game mode detection
pub struct PpsMonitor {
    last_stats: Option<NetStats>,
    last_sample_time: Option<Instant>,
    current_pps: u64,
}

impl PpsMonitor {
    pub fn new() -> Self {
        Self {
            last_stats: None,
            last_sample_time: None,
            current_pps: 0,
        }
    }

    /// Sample current PPS for an interface
    /// Per rewrite.md: (Current - Last) / TimeDelta
    pub fn sample(&mut self, interface: &str) -> u64 {
        let now = Instant::now();
        let stats = match NetStats::read(interface) {
            Some(s) => s,
            None => return self.current_pps,
        };

        if let (Some(last_stats), Some(last_time)) = (&self.last_stats, self.last_sample_time) {
            let time_delta = now.duration_since(last_time).as_secs_f64();
            
            if time_delta > 0.0 {
                let packet_delta = stats.total_packets().saturating_sub(last_stats.total_packets());
                self.current_pps = (packet_delta as f64 / time_delta).round() as u64;
            }
        }

        self.last_stats = Some(stats);
        self.last_sample_time = Some(now);

        debug!("PPS for {}: {}", interface, self.current_pps);
        self.current_pps
    }
}

impl Default for PpsMonitor {
    fn default() -> Self {
        Self::new()
    }
}
