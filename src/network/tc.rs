//! Traffic Control (tc) wrapper for CAKE QoS
//!
//! Per rewrite.md: Wrapper around tc binary (Netlink-TC is too unstable).
//! Implements "Breathing CAKE" with Exponential Moving Average (EMA) smoothing.

use anyhow::{Context, Result};
use log::{info, debug, warn};
use std::process::Command;

/// Traffic Control manager with EMA smoothing
pub struct TcManager {
    /// Last applied bandwidth (Mbit)
    last_bandwidth: Option<u32>,
    /// EMA-smoothed bandwidth
    smoothed_bandwidth: f64,
    /// EMA alpha (weight for current sample)
    ema_alpha: f64,
    /// Minimum change threshold (Mbit) to trigger update
    change_threshold_mbit: u32,
    /// Minimum percentage change to trigger update
    change_threshold_pct: f64,
}

impl TcManager {
    pub fn new(ema_alpha: f64, threshold_mbit: u32, threshold_pct: f64) -> Self {
        Self {
            last_bandwidth: None,
            smoothed_bandwidth: 0.0,
            ema_alpha,
            change_threshold_mbit: threshold_mbit,
            change_threshold_pct: threshold_pct,
        }
    }

    /// Update the smoothed bandwidth with a new sample
    /// Returns true if CAKE should be updated
    pub fn update_bandwidth(&mut self, current_speed_kbit: u32) -> bool {
        let current_mbit = current_speed_kbit / 1000;
        
        if current_mbit == 0 {
            return false;
        }

        // Apply EMA smoothing: smoothed = (current * alpha) + (previous * (1 - alpha))
        // Per rewrite.md: smoothed_bw = (current_speed * 0.3) + (previous_bw * 0.7)
        if self.smoothed_bandwidth == 0.0 {
            self.smoothed_bandwidth = current_mbit as f64;
        } else {
            self.smoothed_bandwidth = (current_mbit as f64 * self.ema_alpha) + 
                                      (self.smoothed_bandwidth * (1.0 - self.ema_alpha));
        }

        let smoothed_mbit = self.smoothed_bandwidth.round() as u32;
        
        // Check if we should update CAKE
        // Per rewrite.md: Only update if shift > 5 Mbit OR > 10%
        if let Some(last) = self.last_bandwidth {
            let abs_diff = (smoothed_mbit as i32 - last as i32).unsigned_abs();
            let pct_diff = abs_diff as f64 / last as f64;
            
            if abs_diff < self.change_threshold_mbit && pct_diff < self.change_threshold_pct {
                debug!("CAKE: No update needed (diff: {}Mbit, {:.1}%)", abs_diff, pct_diff * 100.0);
                return false;
            }
        }

        debug!("CAKE: Bandwidth update triggered: {} -> {}Mbit", 
               self.last_bandwidth.unwrap_or(0), smoothed_mbit);
        true
    }

    /// Apply CAKE qdisc to interface
    /// Per rewrite.md: tc qdisc replace dev <iface> root cake bandwidth <X>mbit besteffort nat
    pub fn apply_cake(&mut self, interface: &str, _bandwidth_kbit: u32) -> Result<()> {
        let bandwidth_mbit = self.smoothed_bandwidth.round().max(1.0) as u32;
        
        info!("Applying CAKE on {} with {}mbit bandwidth", interface, bandwidth_mbit);
        
        let output = Command::new("tc")
            .args([
                "qdisc", "replace", "dev", interface, "root", "cake",
                "bandwidth", &format!("{}mbit", bandwidth_mbit),
                "diffserv4",      // Differentiated services
                "dual-dsthost",   // Fair queuing per destination
                "nat",            // NAT awareness
                "wash",           // Clear DSCP on ingress
                "ack-filter",     // ACK filtering
            ])
            .output()
            .context("Failed to execute tc command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("tc failed: {}", stderr);
            
            // Fallback to simpler CAKE config
            let output = Command::new("tc")
                .args([
                    "qdisc", "replace", "dev", interface, "root", "cake",
                    "bandwidth", &format!("{}mbit", bandwidth_mbit),
                    "besteffort", "nat",
                ])
                .output()?;
            
            if !output.status.success() {
                anyhow::bail!("Failed to apply CAKE qdisc");
            }
        }

        self.last_bandwidth = Some(bandwidth_mbit);
        info!("CAKE applied successfully: {}mbit on {}", bandwidth_mbit, interface);
        
        Ok(())
    }

    /// Remove CAKE qdisc from interface
    pub fn remove_cake(&self, interface: &str) -> Result<()> {
        let output = Command::new("tc")
            .args(["qdisc", "del", "dev", interface, "root"])
            .output();
        
        // Ignore errors (may not have qdisc)
        if let Ok(o) = output {
            if o.status.success() {
                info!("Removed CAKE from {}", interface);
            }
        }
        
        Ok(())
    }

    #[cfg(test)]
    pub fn get_smoothed_mbit(&self) -> u32 {
        self.smoothed_bandwidth.round() as u32
    }

    #[cfg(test)]
    pub fn set_last_applied(&mut self, mbit: u32) {
        self.last_bandwidth = Some(mbit);
    }
}

/// Ethtool wrapper for hardware offload settings
pub struct EthtoolManager;

impl EthtoolManager {
    /// Enable interrupt coalescing (for high CPU scenarios)
    /// Per rewrite.md: ethtool -C adaptive-rx on
    pub fn enable_coalescing(interface: &str) -> Result<()> {
        debug!("Enabling interrupt coalescing on {}", interface);
        
        let _ = Command::new("ethtool")
            .args(["-C", interface, "adaptive-rx", "on"])
            .output();

        Ok(())
    }

    /// Disable interrupt coalescing (for low latency)
    pub fn disable_coalescing(interface: &str) -> Result<()> {
        debug!("Disabling interrupt coalescing on {}", interface);
        
        let _ = Command::new("ethtool")
            .args(["-C", interface, "adaptive-rx", "off"])
            .output();

        Ok(())
    }
}

impl Default for TcManager {
    fn default() -> Self {
        Self::new(0.3, 5, 0.10) // Per rewrite.md defaults
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_smoothing() {
        let mut tc = TcManager::new(0.5, 5, 0.10);
        
        // 1. First update initializes it (100 Mbit)
        assert!(tc.update_bandwidth(100_000));
        assert_eq!(tc.get_smoothed_mbit(), 100);
        
        // Simulate "Applied"
        tc.set_last_applied(100);
        
        // 2. Stable bandwidth should not trigger update
        assert!(!tc.update_bandwidth(100_000));
        
        // 3. Small change (102 Mbit) with alpha 0.5 -> 101. diff 1. < 5.
        assert!(!tc.update_bandwidth(102_000));
        assert_eq!(tc.get_smoothed_mbit(), 101);
        
        // 4. Large drop to 50 Mbit (from 101 smoothed)
        // smoothed = 50 * 0.5 + 101 * 0.5 = 25 + 50.5 = 75.5 -> 76
        // diff |100 (applied) - 76| = 24. > 5. Should trigger.
        assert!(tc.update_bandwidth(50_000));
        assert_eq!(tc.get_smoothed_mbit(), 76);
    }
}
