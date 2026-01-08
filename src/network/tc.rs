//! Traffic Control (tc) wrapper for CAKE QoS
//!
//! Per rewrite.md: Wrapper around tc binary (Netlink-TC is too unstable).
//! Implements "Breathing CAKE" with asymmetric response (fast down, slow up).

use anyhow::{Context, Result};
use log::{info, debug, warn};
use std::process::Command;
use std::collections::VecDeque;

/// Traffic Control manager with asymmetric response
/// 
/// Design philosophy: Bandwidth DROPS are dangerous (bufferbloat), INCREASES are safe.
/// - Drops: Apply immediately after 1 tick confirmation
/// - Increases: Require full hysteresis (3 ticks) to prevent oscillation
/// 
/// Uses single-stage median filter (no EMA) for faster response.
pub struct TcManager {
    /// Last applied bandwidth (Mbit)
    last_bandwidth: Option<u32>,
    /// Rolling window for median calculation
    sample_window: VecDeque<u32>,
    /// Window size for median (default: 3 samples = 6 seconds)
    window_size: usize,
    /// Minimum change threshold (Mbit) to trigger update
    change_threshold_mbit: u32,
    /// Minimum percentage change to trigger update
    change_threshold_pct: f64,
    /// Consecutive ticks the target has been stable (for increases)
    stable_ticks: u32,
    /// Ticks required before applying INCREASE (drops are faster)
    hysteresis_ticks_up: u32,
    /// Ticks required before applying DECREASE
    hysteresis_ticks_down: u32,
    /// Target bandwidth (proposed but not yet applied)
    pending_bandwidth: Option<u32>,
    /// Direction of pending change (true = up, false = down)
    pending_direction_up: bool,
    /// Whether game mode is active (freezes CAKE)
    game_mode_frozen: bool,
    /// Bandwidth frozen at when game mode started
    frozen_bandwidth: Option<u32>,
    /// Throughput-based bandwidth estimate (bytes/sec monitoring)
    throughput_bandwidth: Option<u32>,
}

impl TcManager {
    pub fn new(
        window_size: usize,
        threshold_mbit: u32, 
        threshold_pct: f64,
        hysteresis_up: u32,
        hysteresis_down: u32,
    ) -> Self {
        Self {
            last_bandwidth: None,
            sample_window: VecDeque::with_capacity(window_size + 2),
            window_size,
            change_threshold_mbit: threshold_mbit,
            change_threshold_pct: threshold_pct,
            stable_ticks: 0,
            hysteresis_ticks_up: hysteresis_up,
            hysteresis_ticks_down: hysteresis_down,
            pending_bandwidth: None,
            pending_direction_up: false,
            game_mode_frozen: false,
            frozen_bandwidth: None,
            throughput_bandwidth: None,
        }
    }

    /// Calculate median of samples
    fn median(&self) -> Option<u32> {
        if self.sample_window.is_empty() {
            return None;
        }
        let mut sorted: Vec<u32> = self.sample_window.iter().copied().collect();
        sorted.sort();
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 0 && sorted.len() > 1 {
            Some((sorted[mid - 1] + sorted[mid]) / 2)
        } else {
            Some(sorted[mid])
        }
    }

    /// Update throughput-based bandwidth estimate from actual bytes transferred
    /// This provides a reality check against PHY rate
    pub fn update_throughput(&mut self, bytes_per_sec: u64) {
        // Convert to Mbit/s - NO headroom, use actual measured value
        // The 85% scaling in governor provides the margin for CAKE
        let mbit = ((bytes_per_sec * 8) as f64 / 1_000_000.0) as u32;
        if mbit > 0 {
            self.throughput_bandwidth = Some(mbit);
            debug!("CAKE: Measured throughput {} Mbit/s", mbit);
        }
    }

    /// Enter game mode - freeze CAKE at current value
    pub fn enter_game_mode(&mut self) {
        if !self.game_mode_frozen {
            self.frozen_bandwidth = self.last_bandwidth;
            self.game_mode_frozen = true;
            debug!("CAKE: Game mode FROZEN at {:?}Mbit", self.frozen_bandwidth);
        }
    }

    /// Exit game mode - resume dynamic adjustments
    pub fn exit_game_mode(&mut self) {
        if self.game_mode_frozen {
            self.game_mode_frozen = false;
            self.frozen_bandwidth = None;
            // Reset state for clean restart
            self.stable_ticks = 0;
            self.pending_bandwidth = None;
            debug!("CAKE: Game mode UNFROZEN, resuming dynamic");
        }
    }

    /// Update the bandwidth with a new PHY rate sample
    /// Returns true if CAKE should be updated
    pub fn update_bandwidth(&mut self, phy_rate_mbit: u32) -> bool {
        // Don't adjust during game mode
        if self.game_mode_frozen {
            debug!("CAKE: Skipping update (game mode frozen)");
            return false;
        }

        if phy_rate_mbit == 0 {
            debug!("CAKE: Skipping update (0 Mbit PHY rate)");
            return false;
        }

        // Option C: Use minimum of PHY rate and measured throughput
        let effective_mbit = if let Some(throughput) = self.throughput_bandwidth {
            // Use the lower of PHY rate or throughput-based estimate
            // This catches cases where PHY shows 866Mbps but real throughput is 400Mbps
            let min_val = phy_rate_mbit.min(throughput.max(50)); // Floor at 50Mbit
            debug!("CAKE: PHY={}Mbit, Throughput={}Mbit, Using={}Mbit", 
                   phy_rate_mbit, throughput, min_val);
            min_val
        } else {
            phy_rate_mbit
        };

        // Stage 1: Add to rolling window
        self.sample_window.push_back(effective_mbit);
        if self.sample_window.len() > self.window_size {
            self.sample_window.pop_front();
        }

        // Need minimum samples before making decisions
        let min_samples = (self.window_size / 2).max(2);
        if self.sample_window.len() < min_samples {
            debug!("CAKE: Warming up ({}/{} samples)", self.sample_window.len(), min_samples);
            return false;
        }

        // Stage 2: Get median (removes outliers) - NO EMA, direct response
        let target_mbit = match self.median() {
            Some(m) => m,
            None => return false,
        };
        
        // Stage 3: Check if significant change
        let (should_consider, is_decrease) = if let Some(last) = self.last_bandwidth {
            let diff = target_mbit as i32 - last as i32;
            let abs_diff = diff.unsigned_abs();
            let pct_diff = abs_diff as f64 / last as f64;
            
            let significant = abs_diff >= self.change_threshold_mbit || 
                              pct_diff >= self.change_threshold_pct;
            (significant, diff < 0)
        } else {
            (true, false) // First application
        };

        if !should_consider {
            // Reset hysteresis if not considering a change
            self.stable_ticks = 0;
            self.pending_bandwidth = None;
            debug!("CAKE: No significant change ({}Mbit, last={:?})", target_mbit, self.last_bandwidth);
            return false;
        }

        // Stage 4: Asymmetric hysteresis
        // - Decreases: Fast response (1 tick) to prevent bufferbloat
        // - Increases: Slow response (3 ticks) to prevent oscillation
        let required_ticks = if is_decrease { 
            self.hysteresis_ticks_down 
        } else { 
            self.hysteresis_ticks_up 
        };

        // Check direction consistency
        let direction_changed = self.pending_bandwidth.is_some() && 
                                self.pending_direction_up != !is_decrease;

        if direction_changed {
            // Direction reversed, reset
            debug!("CAKE: Direction changed, resetting hysteresis");
            self.pending_bandwidth = Some(target_mbit);
            self.pending_direction_up = !is_decrease;
            self.stable_ticks = 1;
        } else if self.pending_bandwidth.is_some() {
            self.stable_ticks += 1;
            self.pending_bandwidth = Some(target_mbit);
        } else {
            self.pending_bandwidth = Some(target_mbit);
            self.pending_direction_up = !is_decrease;
            self.stable_ticks = 1;
        }

        if self.stable_ticks >= required_ticks {
            let direction = if is_decrease { "DOWN" } else { "UP" };
            info!("CAKE: Bandwidth {} approved ({} ticks): {:?} -> {}Mbit",
                  direction, self.stable_ticks, self.last_bandwidth, target_mbit);
            self.stable_ticks = 0;
            self.pending_bandwidth = None;
            true
        } else {
            let direction = if is_decrease { "down" } else { "up" };
            debug!("CAKE: Waiting for {} stability ({}/{} ticks at {}Mbit)",
                   direction, self.stable_ticks, required_ticks, target_mbit);
            false
        }
    }

    /// Get the target bandwidth to apply
    pub fn get_target_bandwidth(&self) -> u32 {
        self.median().unwrap_or(200).max(10)
    }

    /// Apply CAKE qdisc to interface
    pub fn apply_cake(&mut self, interface: &str) -> Result<()> {
        let bandwidth_mbit = self.get_target_bandwidth();
        
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
    pub fn is_game_mode(&self) -> bool {
        self.game_mode_frozen
    }

    #[cfg(test)]
    pub fn get_target_mbit(&self) -> u32 {
        self.get_target_bandwidth()
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
        // Defaults: 3 sample window, 15Mbit/15% threshold, 3 ticks up, 1 tick down
        Self::new(3, 15, 0.15, 3, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_median_filtering() {
        // 3 window, 15mbit/15% threshold, 3 up / 1 down hysteresis
        let mut tc = TcManager::new(3, 15, 0.15, 3, 1);
        
        // First sample - warming up (need 2 min)
        assert!(!tc.update_bandwidth(100)); // Sample 1 - warming
        
        // Second sample - still warming but now have enough for first check
        // First real check after warmup should trigger (no previous bandwidth)
        // But we still need to pass hysteresis for first application (3 ticks up)
        assert!(!tc.update_bandwidth(100)); // Sample 2 - tick 1
        assert!(!tc.update_bandwidth(100)); // Sample 3 - tick 2
        assert!(tc.update_bandwidth(100));  // Sample 4 - tick 3 - triggers first application
        
        tc.set_last_applied(100);
        
        // One outlier spike should be filtered by median
        // Median of [100, 100, 500] = 100, so no change
        assert!(!tc.update_bandwidth(500)); // Outlier - median still ~100
        
        // Target should still be close to 100
        assert!(tc.get_target_mbit() <= 200);
    }

    #[test]
    fn test_asymmetric_hysteresis() {
        let mut tc = TcManager::new(3, 15, 0.15, 3, 1); // 3 up, 1 down
        
        // Warm up and apply initial
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.update_bandwidth(100); // This triggers first application
        tc.set_last_applied(100);
        
        // Big DROP should trigger fast (1 tick) after meeting threshold
        // Need to fill window with 50s first
        tc.update_bandwidth(50);
        tc.update_bandwidth(50);
        assert!(tc.update_bandwidth(50)); // Now median is 50, triggers drop!
        tc.set_last_applied(50);
        
        // Big INCREASE should require 3 ticks
        // Fill window with 100s - need several to shift median and pass hysteresis
        tc.update_bandwidth(100); // Tick 1 - shifts window
        tc.update_bandwidth(100); // Tick 2 - median now ~100
        tc.update_bandwidth(100); // Tick 3
        // May need one more tick since the window needs to stabilize
        let triggered = tc.update_bandwidth(100);
        assert!(triggered, "Increase should trigger after 3+ ticks");
    }

    #[test]
    fn test_game_mode_freezes_cake() {
        let mut tc = TcManager::default();
        
        // Set up initial state
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.set_last_applied(100);
        
        // Enter game mode
        tc.enter_game_mode();
        assert!(tc.is_game_mode());
        
        // Updates should be ignored during game mode
        assert!(!tc.update_bandwidth(50));  // Would normally trigger
        assert!(!tc.update_bandwidth(200)); // Would normally trigger
        
        // Exit game mode
        tc.exit_game_mode();
        assert!(!tc.is_game_mode());
        
        // Now updates work again (after warmup)
        tc.update_bandwidth(50);
        tc.update_bandwidth(50);
        // Would need full hysteresis cycle to trigger
    }

    #[test]
    fn test_throughput_based_limit() {
        let mut tc = TcManager::default();
        
        // PHY says 866 Mbps but throughput is only 400
        // 400 Mbps = 50MB/s = 50_000_000 bytes/sec
        tc.update_throughput(50_000_000); // ~400 Mbps
        
        // With 1.2x headroom, throughput-based = ~480Mbit
        // min(866, 480) = 480
        tc.update_bandwidth(866);
        tc.update_bandwidth(866);
        
        // Should use the lower value (throughput-based ~480 with 1.2x headroom)
        let target = tc.get_target_bandwidth();
        assert!(target < 600, "Should limit based on throughput, got {}", target);
    }
}
