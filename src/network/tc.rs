//! Traffic Control (tc) wrapper for CAKE QoS
//!
//! Per rewrite.md: Wrapper around tc binary (Netlink-TC is too unstable).
//! Implements "Breathing CAKE" with asymmetric response (fast down, slow up).

use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::collections::VecDeque;
use std::process::Command;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassStats {
    pub bytes: u64,
    pub packets: u64,
    pub dropped: u64,
    pub overlimits: u64,
}

static GATEWAY_RTT: RwLock<Option<String>> = RwLock::new(None);
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

/// Reset gateway RTT cache (call on connection events)
pub fn reset_gateway_rtt_cache() {
    if let Ok(mut cache) = GATEWAY_RTT.write() {
        *cache = None;
        debug!("Gateway RTT cache cleared");
    }
}

/// Detect appropriate CAKE RTT by pinging the default gateway.
/// Result is cached after first call per connection.
pub fn detect_gateway_rtt() -> String {
    // Try to read cached value
    if let Ok(cache) = GATEWAY_RTT.read() {
        if let Some(ref cached) = *cache {
            return cached.clone();
        }
    }

    // Measure and cache
    let rtt = measure_gateway_rtt();
    info!("CAKE: Auto-detected gateway RTT -> using {}", rtt);

    if let Ok(mut cache) = GATEWAY_RTT.write() {
        *cache = Some(rtt.clone());
    }

    rtt
}

fn measure_gateway_rtt() -> String {
    // Get default gateway IP from routing table
    let gateway_ip = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .ok()
        .and_then(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            stdout
                .split_whitespace()
                .skip_while(|w| *w != "via")
                .nth(1)
                .map(|s| s.to_string())
        });

    let gateway_ip = match gateway_ip {
        Some(ip) => ip,
        None => {
            debug!("Could not detect default gateway, using 50ms RTT");
            return "50ms".to_string();
        }
    };

    // Ping gateway 3 times with 1s timeout
    let avg_ms = Command::new("ping")
        .args(["-c", "3", "-W", "1", &gateway_ip])
        .output()
        .ok()
        .and_then(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            // Parse "rtt min/avg/max/mdev = 1.234/2.345/3.456/0.567 ms"
            stdout
                .lines()
                .find(|l| l.contains("rtt") || l.contains("round-trip"))
                .and_then(|l| l.split('=').nth(1))
                .and_then(|s| s.split('/').nth(1))
                .and_then(|s| s.trim().parse::<f64>().ok())
        });

    match avg_ms {
        Some(rtt) if rtt < 5.0 => {
            info!("Gateway RTT {:.1}ms (local WiFi)", rtt);
            "20ms".to_string()
        }
        Some(rtt) if rtt < 20.0 => {
            info!("Gateway RTT {:.1}ms (mesh/multi-hop)", rtt);
            "50ms".to_string()
        }
        Some(rtt) => {
            info!("Gateway RTT {:.1}ms (high latency path)", rtt);
            "100ms".to_string()
        }
        None => {
            debug!("Could not measure gateway RTT, using 50ms");
            "50ms".to_string()
        }
    }
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
        let mut resolved_qos_use_ifb = qos_use_ifb;
        if resolved_qos_use_ifb {
            let status = Command::new("modprobe").args(["--dry-run", "ifb"]).status();
            let is_available = match status {
                Ok(s) => s.success(),
                Err(_) => false,
            };
            if !is_available {
                warn!("The 'ifb' kernel module is not available on this system. Ingress (download) shaping fallback disabled.");
                resolved_qos_use_ifb = false;
            } else {
                let load_status = Command::new("modprobe").args(["ifb", "numifbs=1"]).status();
                match load_status {
                    Ok(s) => {
                        if !s.success() {
                            warn!("Failed to load 'ifb' kernel module (exit code: {}). Ingress (download) shaping fallback disabled.", s);
                            resolved_qos_use_ifb = false;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to execute modprobe to load 'ifb': {}. Ingress (download) shaping fallback disabled.", e);
                        resolved_qos_use_ifb = false;
                    }
                }
            }
        }

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
            qos_use_ifb: resolved_qos_use_ifb,
            internet_download_mbit,
            internet_upload_mbit,
            ema_bandwidth: None,
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

    /// Apply CAKE qdisc to interface (with IFB ingress redirection if enabled)
    pub fn apply_cake(&mut self, interface: &str) -> Result<()> {
        if !is_tc_available() {
            debug!(
                "Skipping CAKE application on {} (tc not available)",
                interface
            );
            return Ok(());
        }

        // Determine upload and download bandwidths.
        // If internet upload/download limits are set, use them. Otherwise fall back to PHY rate.
        let dynamic_bandwidth = self.get_target_bandwidth();
        let upload_limit = self.internet_upload_mbit.unwrap_or(dynamic_bandwidth);
        let download_limit = self.internet_download_mbit.unwrap_or(dynamic_bandwidth);

        // 1. Ingress Shaping via IFB (Download)
        if self.qos_use_ifb {
            info!(
                "Applying IFB ingress redirection and CAKE on {} (download limit: {}mbit)",
                interface, download_limit
            );

            // Load ifb module (ignore failure if already loaded)
            let _ = Command::new("modprobe").args(["ifb", "numifbs=1"]).output();

            // Set ifb0 device UP
            let _ = Command::new("ip")
                .args(["link", "set", "dev", "ifb0", "up"])
                .output();

            // Clear any existing ingress qdisc on physical interface to start fresh
            let _ = Command::new("tc")
                .args(["qdisc", "del", "dev", interface, "ingress"])
                .output();

            // Add ingress qdisc to physical interface
            let output = Command::new("tc")
                .args([
                    "qdisc", "add", "dev", interface, "handle", "ffff:", "ingress",
                ])
                .output();

            if let Ok(out) = output {
                if out.status.success() {
                    // Redirect ingress traffic of physical interface to ifb0
                    let output = Command::new("tc")
                        .args([
                            "filter", "add", "dev", interface, "parent", "ffff:", "matchall",
                            "action", "mirred", "egress", "redirect", "dev", "ifb0",
                        ])
                        .output();

                    if let Ok(out_filter) = output {
                        if out_filter.status.success() {
                            // Apply CAKE on ifb0 (for download shaping)
                            let rtt = detect_gateway_rtt();
                            let output = Command::new("tc")
                                .args([
                                    "qdisc",
                                    "replace",
                                    "dev",
                                    "ifb0",
                                    "root",
                                    "cake",
                                    "bandwidth",
                                    &format!("{}mbit", download_limit),
                                    "rtt",
                                    &rtt,
                                    "diffserv4",
                                    "dual-dsthost",
                                    "nat",
                                    "wash",
                                    "ack-filter",
                                ])
                                .output();

                            if let Ok(out_cake) = output {
                                if out_cake.status.success() {
                                    info!("Ingress CAKE applied successfully on ifb0");
                                } else {
                                    let stderr = String::from_utf8_lossy(&out_cake.stderr);
                                    warn!("Failed to apply CAKE on ifb0: {}", stderr);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Egress Shaping (Upload) via classful HTB hierarchy
        // This enforces a global bandwidth ceiling across both primary CAKE and netem delay paths.
        info!(
            "Applying egress HTB classful hierarchy on {} (upload limit: {}mbit)",
            interface, upload_limit
        );

        if self.last_bandwidth.is_some() {
            if let Err(e) = self.change_htb_rates(interface, upload_limit) {
                warn!("Failed to dynamically change HTB rates: {}, re-applying fresh hierarchy", e);
                self.apply_htb_hierarchy(interface, upload_limit)?;
            }
        } else {
            self.apply_htb_hierarchy(interface, upload_limit)?;
        }

        self.last_bandwidth = Some(dynamic_bandwidth);
        Ok(())
    }

    /// Set up a fresh classful HTB shaping hierarchy on the given interface.
    /// Redirects marked packets (fwmark 0x99) into the 3ms netem delay queue (1:12)
    /// while primary traffic runs through the unshaped flow-isolating CAKE queue (1:11).
    pub fn apply_htb_hierarchy(&self, interface: &str, bandwidth_mbit: u32) -> Result<()> {
        if !is_tc_available() {
            return Ok(());
        }

        // 1. Delete any existing root qdisc
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", interface, "root"])
            .output();

        // 2. Add root HTB qdisc with default class 11
        let root_status = Command::new("tc")
            .args([
                "qdisc", "add", "dev", interface, "root", "handle", "1:", "htb", "default", "11"
            ])
            .status()
            .context("Failed to add root htb qdisc")?;

        if !root_status.success() {
            anyhow::bail!("Failed to add root htb qdisc on {}", interface);
        }

        // 3. Add parent class with global ceiling.
        // Allocate custom burst depth based on rate: ~15KB per 100 Mbps (min 15k) to prevent token starvation.
        let burst_kb = ((bandwidth_mbit as f64 / 100.0) * 15.0).max(15.0).round() as u32;
        let burst_str = format!("{}k", burst_kb);

        let parent_status = Command::new("tc")
            .args([
                "class", "add", "dev", interface, "parent", "1:", "classid", "1:1",
                "htb", "rate", &format!("{}mbit", bandwidth_mbit),
                "ceil", &format!("{}mbit", bandwidth_mbit),
                "burst", &burst_str, "cburst", &burst_str
            ])
            .status()
            .context("Failed to add parent htb class")?;

        if !parent_status.success() {
            anyhow::bail!("Failed to add parent htb class on {}", interface);
        }

        // 4. Add primary class (1:11) - 85% rate limit, can borrow up to ceil (100%)
        let primary_rate = (bandwidth_mbit * 85) / 100;
        let primary_rate = primary_rate.max(1);
        let primary_status = Command::new("tc")
            .args([
                "class", "add", "dev", interface, "parent", "1:1", "classid", "1:11",
                "htb", "rate", &format!("{}mbit", primary_rate),
                "ceil", &format!("{}mbit", bandwidth_mbit),
                "burst", &burst_str, "cburst", &burst_str, "prio", "1"
            ])
            .status()
            .context("Failed to add primary htb class")?;

        if !primary_status.success() {
            anyhow::bail!("Failed to add primary htb class on {}", interface);
        }

        // 5. Add delayed class (1:12) - 15% rate limit, can borrow up to ceil
        let delayed_rate = (bandwidth_mbit * 15) / 100;
        let delayed_rate = delayed_rate.max(1);
        let delayed_status = Command::new("tc")
            .args([
                "class", "add", "dev", interface, "parent", "1:1", "classid", "1:12",
                "htb", "rate", &format!("{}mbit", delayed_rate),
                "ceil", &format!("{}mbit", bandwidth_mbit),
                "burst", &burst_str, "cburst", &burst_str, "prio", "2"
            ])
            .status()
            .context("Failed to add delayed htb class")?;

        if !delayed_status.success() {
            anyhow::bail!("Failed to add delayed htb class on {}", interface);
        }

        // 6. Attach CAKE leaf to primary class (1:11) for pure flow isolation
        let rtt = detect_gateway_rtt();
        let cake_status = Command::new("tc")
            .args([
                "qdisc", "add", "dev", interface, "parent", "1:11", "handle", "10:",
                "cake", "rtt", &rtt, "triple-isolate", "wash", "nat", "ack-filter"
            ])
            .status()
            .context("Failed to attach CAKE qdisc to primary class")?;

        if !cake_status.success() {
            anyhow::bail!("Failed to attach CAKE qdisc to primary class on {}", interface);
        }

        // 7. Attach netem delay leaf to delayed class (1:12)
        let netem_status = Command::new("tc")
            .args([
                "qdisc", "add", "dev", interface, "parent", "1:12", "handle", "20:",
                "netem", "delay", "3ms"
            ])
            .status()
            .context("Failed to attach netem qdisc to delayed class")?;

        if !netem_status.success() {
            anyhow::bail!("Failed to attach netem qdisc to delayed class on {}", interface);
        }

        // 8. Add fw filter mapping fwmark 0x99 to class 1:12
        let filter_status = Command::new("tc")
            .args([
                "filter", "add", "dev", interface, "protocol", "ip", "parent", "1:0",
                "prio", "1", "handle", "0x99", "fw", "flowid", "1:12"
            ])
            .status()
            .context("Failed to add fwmark filter to root")?;

        if !filter_status.success() {
            anyhow::bail!("Failed to add fwmark filter on {}", interface);
        }

        info!("HTB+CAKE+netem egress shaping hierarchy applied successfully on {}", interface);
        Ok(())
    }

    /// Dynamically change the rates of the existing HTB classes without deleting/rebuilding the qdisc structure.
    pub fn change_htb_rates(&self, interface: &str, bandwidth_mbit: u32) -> Result<()> {
        if !is_tc_available() {
            return Ok(());
        }

        let burst_kb = ((bandwidth_mbit as f64 / 100.0) * 15.0).max(15.0).round() as u32;
        let burst_str = format!("{}k", burst_kb);

        // 1. Change parent class rate
        let parent_status = Command::new("tc")
            .args([
                "class", "change", "dev", interface, "parent", "1:", "classid", "1:1",
                "htb", "rate", &format!("{}mbit", bandwidth_mbit),
                "ceil", &format!("{}mbit", bandwidth_mbit),
                "burst", &burst_str, "cburst", &burst_str
            ])
            .status()
            .context("Failed to change parent htb class rates")?;

        if !parent_status.success() {
            anyhow::bail!("Failed to change parent htb class rates on {}", interface);
        }

        // 2. Change primary class rate (85%)
        let primary_rate = (bandwidth_mbit * 85) / 100;
        let primary_rate = primary_rate.max(1);
        let primary_status = Command::new("tc")
            .args([
                "class", "change", "dev", interface, "parent", "1:1", "classid", "1:11",
                "htb", "rate", &format!("{}mbit", primary_rate),
                "ceil", &format!("{}mbit", bandwidth_mbit),
                "burst", &burst_str, "cburst", &burst_str, "prio", "1"
            ])
            .status()
            .context("Failed to change primary htb class rates")?;

        if !primary_status.success() {
            anyhow::bail!("Failed to change primary htb class rates on {}", interface);
        }

        // 3. Change delayed class rate (15%)
        let delayed_rate = (bandwidth_mbit * 15) / 100;
        let delayed_rate = delayed_rate.max(1);
        let delayed_status = Command::new("tc")
            .args([
                "class", "change", "dev", interface, "parent", "1:1", "classid", "1:12",
                "htb", "rate", &format!("{}mbit", delayed_rate),
                "ceil", &format!("{}mbit", bandwidth_mbit),
                "burst", &burst_str, "cburst", &burst_str, "prio", "2"
            ])
            .status()
            .context("Failed to change delayed htb class rates")?;

        if !delayed_status.success() {
            anyhow::bail!("Failed to change delayed htb class rates on {}", interface);
        }

        Ok(())
    }

    #[allow(dead_code)]
    fn apply_root_cake_fallback(&self, interface: &str, bandwidth_mbit: u32) -> Result<()> {
        let rtt = detect_gateway_rtt();
        let output = Command::new("tc")
            .args([
                "qdisc",
                "replace",
                "dev",
                interface,
                "root",
                "cake",
                "bandwidth",
                &format!("{}mbit", bandwidth_mbit),
                "rtt",
                &rtt,
                "diffserv4",
                "dual-dsthost",
                "nat",
                "wash",
                "ack-filter",
            ])
            .output()
            .context("Failed to execute fallback tc command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Fallback tc failed: {}", stderr);

            // Simpler CAKE config
            let output = Command::new("tc")
                .args([
                    "qdisc",
                    "replace",
                    "dev",
                    interface,
                    "root",
                    "cake",
                    "bandwidth",
                    &format!("{}mbit", bandwidth_mbit),
                    "rtt",
                    &rtt,
                    "besteffort",
                    "nat",
                ])
                .output()?;

            if !output.status.success() {
                anyhow::bail!("Failed to apply fallback CAKE qdisc");
            }
        }
        info!(
            "Fallback root CAKE applied successfully: {}mbit on {}",
            bandwidth_mbit, interface
        );
        Ok(())
    }

    /// Remove CAKE qdisc from interface
    pub fn remove_cake(&self, interface: &str) -> Result<()> {
        if !is_tc_available() {
            return Ok(());
        }

        // 1. Clean up egress (root) qdisc on physical interface
        let output = Command::new("tc")
            .args(["qdisc", "del", "dev", interface, "root"])
            .output();

        if let Ok(o) = output {
            if o.status.success() {
                info!("Removed root qdisc from {}", interface);
            }
        }

        // Check if this was a multi-queue interface and restore mq
        let mut tx_queues = 1;
        if let Ok(entries) = std::fs::read_dir(format!("/sys/class/net/{}/queues", interface)) {
            let count = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("tx-"))
                .count();
            if count > 0 {
                tx_queues = count;
            }
        }

        if tx_queues > 1 {
            info!("Restoring default mq root qdisc on {}", interface);
            let _ = Command::new("tc")
                .args([
                    "qdisc", "replace", "dev", interface, "root", "handle", "1:", "mq",
                ])
                .output();
        }

        // 2. Clean up ingress redirection (if used)
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", interface, "ingress"])
            .output();

        let _ = Command::new("ip")
            .args(["link", "set", "dev", "ifb0", "down"])
            .output();

        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", "ifb0", "root"])
            .output();

        info!("Cleaned up ingress redirect for {}", interface);
        Ok(())
    }

    /// Query statistics for HTB classes 1:11 (primary) and 1:12 (delayed)
    pub fn query_class_stats(&self, interface: &str) -> Result<(Option<ClassStats>, Option<ClassStats>)> {
        if !is_tc_available() {
            return Ok((None, None));
        }

        let output = Command::new("tc")
            .args(["-s", "class", "show", "dev", interface])
            .output()
            .context("Failed to run tc -s class show")?;

        if !output.status.success() {
            return Ok((None, None));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Self::parse_class_stats(&stdout))
    }

    pub fn parse_class_stats(stdout: &str) -> (Option<ClassStats>, Option<ClassStats>) {
        let mut primary_stats = None;
        let mut delayed_stats = None;
        let mut current_class: Option<&str> = None;

        for line in stdout.lines() {
            let line = line.trim();
            if line.contains("class htb 1:11") {
                current_class = Some("1:11");
            } else if line.contains("class htb 1:12") {
                current_class = Some("1:12");
            } else if line.starts_with("class htb") || line.starts_with("class cake") {
                current_class = None;
            } else if let Some(class_id) = current_class {
                if line.contains("Sent") && line.contains("bytes") {
                    let mut stats = ClassStats::default();
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    
                    if parts.len() > 1 {
                        if let Ok(bytes) = parts[1].parse::<u64>() {
                            stats.bytes = bytes;
                        }
                    }
                    
                    if let Some(pos) = parts.iter().position(|&p| p == "pkt" || p == "packets") {
                        if pos > 0 {
                            if let Ok(pkts) = parts[pos - 1].parse::<u64>() {
                                stats.packets = pkts;
                            }
                        }
                    }

                    if let Some(pos) = parts.iter().position(|&p| p.contains("dropped")) {
                        let val_str = if pos + 1 < parts.len() {
                            parts[pos + 1].trim_matches(|c| c == ',' || c == ')')
                        } else {
                            ""
                        };
                        if let Ok(dropped) = val_str.parse::<u64>() {
                            stats.dropped = dropped;
                        }
                    }

                    if let Some(pos) = parts.iter().position(|&p| p.contains("overlimits")) {
                        let val_str = if pos + 1 < parts.len() {
                            parts[pos + 1].trim_matches(|c| c == ',' || c == ')')
                        } else {
                            ""
                        };
                        if let Ok(overlimits) = val_str.parse::<u64>() {
                            stats.overlimits = overlimits;
                        }
                    }

                    if class_id == "1:11" {
                        primary_stats = Some(stats);
                    } else {
                        delayed_stats = Some(stats);
                    }
                    current_class = None;
                }
            }
        }

        (primary_stats, delayed_stats)
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

    #[test]
    fn test_parse_class_stats() {
        let sample_output = "class htb 1:11 parent 1:1 leaf 10: prio 1 rate 85000Kbit ceil 100Mbit burst 15Kb cburst 15Kb\n\
                             Sent 412598042 bytes 283120 pkt (dropped 12, overlimits 45 requeues 0)\n\
                             lended: 12840 borrowed: 0 giant: 0\n\
                             class htb 1:12 parent 1:1 leaf 20: prio 2 rate 15000Kbit ceil 100Mbit burst 15Kb cburst 15Kb\n\
                             Sent 102934 bytes 431 pkt (dropped 1, overlimits 2 requeues 0)\n\
                             lended: 431 borrowed: 0 giant: 0";
        let (primary, delayed) = TcManager::parse_class_stats(sample_output);
        
        let p = primary.unwrap();
        assert_eq!(p.bytes, 412598042);
        assert_eq!(p.packets, 283120);
        assert_eq!(p.dropped, 12);
        assert_eq!(p.overlimits, 45);

        let d = delayed.unwrap();
        assert_eq!(d.bytes, 102934);
        assert_eq!(d.packets, 431);
        assert_eq!(d.dropped, 1);
        assert_eq!(d.overlimits, 2);
    }
}
