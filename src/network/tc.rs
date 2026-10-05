//! Traffic Control (tc) wrapper for CAKE QoS
//!
//! Per rewrite.md: Wrapper around tc binary (Netlink-TC is too unstable).
//! Implements "Breathing CAKE" with asymmetric response (fast down, slow up).

use anyhow::Result;
use log::{debug, info, warn};
use std::collections::VecDeque;
use std::process::Command;
use std::sync::OnceLock;

use crate::network::shaper::{self, Shaper};

static TC_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Check if the `tc` command is available on the system
pub fn is_tc_available() -> bool {
    *TC_AVAILABLE.get_or_init(|| {
        match Command::new("tc").arg("-Version").output() {
            Ok(output) => {
                if output.status.success() {
                    true
                } else {
                    warn!(
                        "Traffic Control (tc) check exited with code {:?}. Stderr: {}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    false
                }
            }
            Err(e) => {
                warn!(
                    "Traffic Control (tc) binary check failed to execute: {}. PATH is {:?}",
                    e,
                    std::env::var("PATH").unwrap_or_default()
                );
                false
            }
        }
    })
}

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
    /// Whether to use IFB for ingress shaping
    qos_use_ifb: bool,
    /// Configured internet download limit (Mbit)
    internet_download_mbit: Option<u32>,
    /// Configured internet upload limit (Mbit)
    internet_upload_mbit: Option<u32>,
    /// Exponential moving average of physical bandwidth (Mbit)
    ema_bandwidth: Option<f64>,
    /// Installed shaper, if any
    shaper: Option<Shaper>,
}

impl TcManager {
    pub fn new(
        window_size: usize,
        threshold_mbit: u32,
        threshold_pct: f64,
        hysteresis_up: u32,
        hysteresis_down: u32,
        qos_use_ifb: bool,
        internet_download_mbit: Option<u32>,
        internet_upload_mbit: Option<u32>,
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
            qos_use_ifb,
            internet_download_mbit,
            internet_upload_mbit,
            ema_bandwidth: None,
            shaper: None,
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

    /// Get the last applied bandwidth (Mbit)
    pub fn get_last_bandwidth(&self) -> Option<u32> {
        self.last_bandwidth
    }

    /// Get the current measured throughput in Mbps
    pub fn get_current_throughput_mbps(&self) -> u32 {
        self.throughput_bandwidth.unwrap_or(0)
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

        // Use PHY rate as the primary signal, smoothed via an EMA (alpha=0.3)
        // to filter out transient microsecond-level hardware tracking dips.
        let rate_f = phy_rate_mbit as f64;
        let smoothed_f = if let Some(prev) = self.ema_bandwidth {
            let alpha = 0.3;
            let current = (alpha * rate_f) + ((1.0 - alpha) * prev);
            self.ema_bandwidth = Some(current);
            current
        } else {
            self.ema_bandwidth = Some(rate_f);
            rate_f
        };
        let effective_mbit = smoothed_f.round() as u32;

        // Stage 1: Add to rolling window
        self.sample_window.push_back(effective_mbit);
        if self.sample_window.len() > self.window_size {
            self.sample_window.pop_front();
        }

        // Need minimum samples before making decisions
        let min_samples = (self.window_size / 2).max(2);
        if self.sample_window.len() < min_samples {
            debug!(
                "CAKE: Warming up ({}/{} samples)",
                self.sample_window.len(),
                min_samples
            );
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

            let significant =
                abs_diff >= self.change_threshold_mbit || pct_diff >= self.change_threshold_pct;
            (significant, diff < 0)
        } else {
            (true, false) // First application
        };

        if !should_consider {
            // Reset hysteresis if not considering a change
            self.stable_ticks = 0;
            self.pending_bandwidth = None;
            debug!(
                "CAKE: No significant change ({}Mbit, last={:?})",
                target_mbit, self.last_bandwidth
            );
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
        let direction_changed =
            self.pending_bandwidth.is_some() && self.pending_direction_up != !is_decrease;

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
            info!(
                "CAKE: Bandwidth {} approved ({} ticks): {:?} -> {}Mbit",
                direction, self.stable_ticks, self.last_bandwidth, target_mbit
            );
            self.stable_ticks = 0;
            self.pending_bandwidth = None;
            true
        } else {
            let direction = if is_decrease { "down" } else { "up" };
            debug!(
                "CAKE: Waiting for {} stability ({}/{} ticks at {}Mbit)",
                direction, self.stable_ticks, required_ticks, target_mbit
            );
            false
        }
    }

    /// Get the target bandwidth to apply
    pub fn get_target_bandwidth(&self) -> u32 {
        self.median().unwrap_or(200).max(10)
    }

    /// Install or update the shaper with the current target bandwidth
    pub fn apply_cake(&mut self, interface: &str) -> Result<()> {
        if !is_tc_available() {
            debug!("Skipping shaper on {} (tc not available)", interface);
            return Ok(());
        }

        // Configured internet limits win; otherwise fall back to the Wi-Fi link estimate.
        let dynamic_bandwidth = self.get_target_bandwidth();
        let up_kbit = self.internet_upload_mbit.unwrap_or(dynamic_bandwidth) * 1000;
        let down_kbit = self
            .qos_use_ifb
            .then(|| self.internet_download_mbit.unwrap_or(dynamic_bandwidth) * 1000);

        let reuse = self
            .shaper
            .as_ref()
            .is_some_and(|s| s.iface == interface && s.is_present());
        if reuse {
            if let Some(s) = self.shaper.as_mut() {
                s.set_rates(up_kbit, down_kbit)?;
            }
        } else {
            self.shaper = Some(Shaper::install(interface, up_kbit, down_kbit)?);
        }

        self.last_bandwidth = Some(dynamic_bandwidth);
        Ok(())
    }

    /// Remove the shaper from the interface
    pub fn remove_cake(&mut self, interface: &str) -> Result<()> {
        shaper::remove(interface);
        self.shaper = None;
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
    /// Uses moderate coalescing to reduce CPU load while maintaining acceptable latency
    pub fn enable_coalescing(interface: &str) -> Result<()> {
        debug!("Enabling interrupt coalescing on {}", interface);

        // Set moderate coalescing: wait up to 50us or 8 frames before interrupt
        // This reduces CPU load significantly while keeping latency under 1ms
        let _ = Command::new("ethtool")
            .args([
                "-C",
                interface,
                "rx-usecs",
                "50",
                "rx-frames",
                "8",
                "tx-usecs",
                "50",
                "tx-frames",
                "8",
            ])
            .output();

        // Also enable adaptive on supported cards as a fallback
        let _ = Command::new("ethtool")
            .args(["-C", interface, "adaptive-rx", "on"])
            .output();

        Ok(())
    }

    /// Disable interrupt coalescing (for low latency gaming/streaming)
    /// Interrupts fire immediately on every packet for minimum latency
    pub fn disable_coalescing(interface: &str) -> Result<()> {
        debug!("Disabling interrupt coalescing on {}", interface);

        // Zero coalescing: interrupt on every packet (lowest latency)
        let _ = Command::new("ethtool")
            .args([
                "-C",
                interface,
                "rx-usecs",
                "0",
                "rx-frames",
                "1",
                "tx-usecs",
                "0",
                "tx-frames",
                "1",
            ])
            .output();

        // Disable adaptive coalescing
        let _ = Command::new("ethtool")
            .args(["-C", interface, "adaptive-rx", "off", "adaptive-tx", "off"])
            .output();

        Ok(())
    }

    /// Enable Energy Efficient Ethernet (for battery/power saving)
    pub fn enable_eee(interface: &str) -> Result<()> {
        debug!("Enabling EEE on {}", interface);
        let _ = Command::new("ethtool")
            .args(["--set-eee", interface, "eee", "on"])
            .output();
        Ok(())
    }

    /// Disable Energy Efficient Ethernet (for streaming/gaming)
    pub fn disable_eee(interface: &str) -> Result<()> {
        debug!("Disabling EEE on {}", interface);
        let _ = Command::new("ethtool")
            .args(["--set-eee", interface, "eee", "off"])
            .output();
        Ok(())
    }
}

impl Default for TcManager {
    fn default() -> Self {
        // Defaults: 3 sample window, 15Mbit/15% threshold, 3 ticks up, 1 tick down
        Self::new(3, 15, 0.15, 3, 1, false, None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_median_filtering() {
        // 3 window, 15mbit/15% threshold, 3 up / 1 down hysteresis
        let mut tc = TcManager::new(3, 15, 0.15, 3, 1, false, None, None);

        // First sample - warming up (need 2 min)
        assert!(!tc.update_bandwidth(100)); // Sample 1 - warming

        // Second sample - still warming but now have enough for first check
        // First real check after warmup should trigger (no previous bandwidth)
        // But we still need to pass hysteresis for first application (3 ticks up)
        assert!(!tc.update_bandwidth(100)); // Sample 2 - tick 1
        assert!(!tc.update_bandwidth(100)); // Sample 3 - tick 2
        assert!(tc.update_bandwidth(100)); // Sample 4 - tick 3 - triggers first application

        tc.set_last_applied(100);

        // One outlier spike should be filtered by median
        // Median of [100, 100, 500] = 100, so no change
        assert!(!tc.update_bandwidth(500)); // Outlier - median still ~100

        // Target should still be close to 100
        assert!(tc.get_target_mbit() <= 200);
    }

    #[test]
    fn test_asymmetric_hysteresis() {
        let mut tc = TcManager::new(3, 15, 0.15, 3, 1, false, None, None); // 3 up, 1 down

        // Warm up and apply initial
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.update_bandwidth(100);
        tc.update_bandwidth(100); // This triggers first application
        tc.set_last_applied(100);

        // Big DROP should trigger fast (1 tick of hysteresis, but needs 2 samples to shift median)
        assert!(!tc.update_bandwidth(50)); // Tick 1 (median remains 100, no change)
        assert!(tc.update_bandwidth(50));  // Tick 2 (median becomes 85, decrease of 15, approved immediately!)
        
        // Manually reset tc to a stable 50 to test the sustained increase
        tc.sample_window.clear();
        tc.sample_window.push_back(50);
        tc.sample_window.push_back(50);
        tc.sample_window.push_back(50);
        tc.ema_bandwidth = Some(50.0);
        tc.set_last_applied(50);

        // Big INCREASE should require 3 ticks of hysteresis
        assert!(!tc.update_bandwidth(100)); // Tick 1 (median remains 50, no change)
        assert!(!tc.update_bandwidth(100)); // Tick 2 (median becomes 65, hysteresis tick 1)
        assert!(!tc.update_bandwidth(100)); // Tick 3 (median becomes 76, hysteresis tick 2)
        let triggered = tc.update_bandwidth(100); // Tick 4 (median becomes 83, hysteresis tick 3 - approved!)
        assert!(triggered, "Increase should trigger after 3 ticks of sustained increase");
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
        assert!(!tc.update_bandwidth(50)); // Would normally trigger
        assert!(!tc.update_bandwidth(200)); // Would normally trigger

        // Exit game mode
        tc.exit_game_mode();
        assert!(!tc.is_game_mode());

        // Now updates work again (after warmup)
        tc.update_bandwidth(50);
        tc.update_bandwidth(50);
        // Would need full hysteresis cycle to trigger
    }
}
