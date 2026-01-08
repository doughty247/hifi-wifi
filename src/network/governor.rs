//! The Governor - The "Brain" of hifi-wifi
//!
//! Per rewrite.md: Runs the async loop (Tick Rate: 2 seconds) and implements:
//! - Breathing CAKE (Dynamic QoS with asymmetric response)
//! - CPU Governor (Smart Coalescing)
//! - Smart Band Steering (with Hysteresis)
//! - Game Mode Detection (PPS) with CAKE freezing

use anyhow::Result;
use log::{info, debug, warn};
use std::time::{Duration, Instant};
use std::process::Command;
use tokio::time;

use crate::config::structs::{GovernorConfig, WifiConfig};
use crate::network::nm::NmClient;
use crate::network::tc::{TcManager, EthtoolManager};
use crate::network::stats::PpsMonitor;
use crate::network::wifi::WifiManager;
use crate::system::cpu::CpuMonitor;
use crate::system::power::PowerManager;

/// Band steering candidate tracking for hysteresis
#[derive(Debug, Default)]
struct RoamCandidate {
    bssid: String,
    score: i32,
    consecutive_ticks: u32,
}

/// Per-interface state
struct InterfaceState {
    pps_monitor: PpsMonitor,
    tc_manager: TcManager,
    roam_candidate: Option<RoamCandidate>,
    game_mode_until: Option<Instant>,
    coalescing_enabled: bool,
    coalescing_stable_ticks: u32,
    pending_coalescing: Option<bool>,
    power_save_enabled: Option<bool>,
    power_save_stable_ticks: u32,
    pending_power_save: Option<bool>,
    /// Last known bytes for throughput calculation
    last_rx_bytes: u64,
    last_tx_bytes: u64,
    last_stats_time: Option<Instant>,
    /// Whether we have valid bandwidth data (false = CAKE disabled)
    bandwidth_valid: bool,
}

impl InterfaceState {
    fn new(config: &GovernorConfig) -> Self {
        Self {
            pps_monitor: PpsMonitor::new(),
            tc_manager: TcManager::new(
                config.cake_median_window,
                config.cake_change_threshold_mbit,
                config.cake_change_threshold_pct,
                config.cake_hysteresis_up,
                config.cake_hysteresis_down,
            ),
            roam_candidate: None,
            game_mode_until: None,
            coalescing_enabled: false,
            coalescing_stable_ticks: 0,
            pending_coalescing: None,
            power_save_enabled: None,
            power_save_stable_ticks: 0,
            pending_power_save: None,
            last_rx_bytes: 0,
            last_tx_bytes: 0,
            last_stats_time: None,
            bandwidth_valid: false,
        }
    }
}

/// The Network Governor - orchestrates all optimization logic
pub struct Governor {
    config: GovernorConfig,
    wifi_config: WifiConfig,
    nm_client: NmClient,
    cpu_monitor: CpuMonitor,
    power_manager: PowerManager,
    wifi_manager: WifiManager,
    interface_states: std::collections::HashMap<String, InterfaceState>,
}

impl Governor {
    /// Create a new Governor with the given configuration
    pub async fn new(config: GovernorConfig, wifi_config: WifiConfig) -> Result<Self> {
        let nm_client = NmClient::new().await?;
        let cpu_monitor = CpuMonitor::new(config.cpu_avg_window_size);
        let power_manager = PowerManager::new();
        let wifi_manager = WifiManager::new()?;
        
        Ok(Self {
            config,
            wifi_config,
            nm_client,
            cpu_monitor,
            power_manager,
            wifi_manager,
            interface_states: std::collections::HashMap::new(),
        })
    }

    /// Run the main governor loop
    /// Per rewrite.md: Tick Rate 2 seconds, non-blocking
    pub async fn run(&mut self, tick_rate_secs: u64) -> Result<()> {
        info!("Governor starting (tick rate: {}s)", tick_rate_secs);
        
        let mut interval = time::interval(Duration::from_secs(tick_rate_secs));
        
        loop {
            interval.tick().await;
            
            if let Err(e) = self.tick().await {
                warn!("Governor tick error: {}", e);
            }
        }
    }

    /// Single tick of the governor loop
    async fn tick(&mut self) -> Result<()> {
        // 1. Sample CPU load
        let cpu_load = self.cpu_monitor.sample();
        debug!("Tick: CPU load {:.1}%", cpu_load * 100.0);

        // 2. Get wireless devices from NetworkManager
        let devices = self.nm_client.get_wireless_devices().await?;
        
        // Collect device info we need
        let device_infos: Vec<_> = devices.into_iter()
            .filter(|d| d.state == crate::network::nm::DeviceState::Activated)
            .map(|d| (d.interface.clone(), d.path.clone(), d.bitrate, d.active_ap.clone()))
            .collect();

        for (interface, path, bitrate, active_ap) in device_infos {
            // Get or create interface state
            if !self.interface_states.contains_key(&interface) {
                self.interface_states.insert(
                    interface.clone(), 
                    InterfaceState::new(&self.config)
                );
            }

            // 3. Game Mode Detection (PPS) - with CAKE freezing
            if self.config.game_mode_enabled {
                let pps_threshold = self.config.game_mode_pps_threshold;
                let cooldown_secs = self.config.game_mode_cooldown_secs;
                let freeze_cake = self.config.game_mode_freeze_cake;
                
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let pps = state.pps_monitor.sample(&interface);
                    let was_in_game = state.game_mode_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false);
                    
                    if pps > pps_threshold {
                        let cooldown = Duration::from_secs(cooldown_secs);
                        state.game_mode_until = Some(Instant::now() + cooldown);
                        
                        // Freeze CAKE when entering game mode
                        if freeze_cake && !was_in_game {
                            state.tc_manager.enter_game_mode();
                            info!("Game mode ACTIVATED: {} PPS on {} (CAKE frozen)", pps, interface);
                        } else {
                            debug!("Game mode extended: {} PPS on {}", pps, interface);
                        }
                    } else if was_in_game {
                        // Check if cooldown expired
                        let still_in_game = state.game_mode_until
                            .map(|until| Instant::now() < until)
                            .unwrap_or(false);
                        
                        if !still_in_game && freeze_cake {
                            state.tc_manager.exit_game_mode();
                            info!("Game mode ENDED on {} (CAKE unfrozen)", interface);
                        }
                    }
                }
            }

            // 4. Breathing CAKE (Dynamic QoS) with throughput monitoring
            if self.config.breathing_cake_enabled {
                // Get bitrate from BOTH sources and average for stability
                let nm_bitrate = bitrate;  // Already in Kbit/s from NetworkManager
                let iw_bitrate = Self::get_bitrate_from_iw(&interface).unwrap_or(0);
                
                // Average both sources if both valid, otherwise use whichever is valid
                // Reject readings below 54Mbit (MCS0 probe frames are garbage)
                let min_valid_kbit = 54_000;  // 54 Mbit minimum (802.11g)
                
                let nm_valid = nm_bitrate >= min_valid_kbit;
                let iw_valid = iw_bitrate >= min_valid_kbit;
                
                let effective_bitrate = match (nm_valid, iw_valid) {
                    (true, true) => (nm_bitrate + iw_bitrate) / 2,  // Average both
                    (true, false) => nm_bitrate,
                    (false, true) => iw_bitrate,
                    (false, false) => 0,  // Both invalid - will disable CAKE
                };
                
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    // Update throughput estimate from actual traffic
                    Self::update_throughput_estimate(state, &interface);
                    
                    if effective_bitrate > 0 {
                        // Convert Kbit to Mbit and scale using overhead factor (default 0.85)
                        let bitrate_mbit = effective_bitrate / 1000;
                        let scaled_mbit = (bitrate_mbit as f64 * self.config.cake_overhead_factor) as u32;
                        
                        debug!("CAKE: NM={}Kbit, iw={}Kbit, effective={}Kbit, scaled={}Mbit",
                               nm_bitrate, iw_bitrate, effective_bitrate, scaled_mbit);
                        
                        if state.tc_manager.update_bandwidth(scaled_mbit) {
                            let _ = state.tc_manager.apply_cake(&interface);
                        }
                        state.bandwidth_valid = true;
                    } else {
                        // Both sources invalid - disable CAKE rather than guess
                        if state.bandwidth_valid {
                            warn!("CAKE: No valid bitrate (NM={}, iw={}), disabling CAKE on {}",
                                  nm_bitrate, iw_bitrate, interface);
                            let _ = state.tc_manager.remove_cake(&interface);
                            state.bandwidth_valid = false;
                        }
                    }
                }
            }

            // 5. CPU Governor (Smart Coalescing) - with hysteresis to prevent jitter
            if self.config.cpu_coalescing_enabled {
                let threshold = self.config.cpu_coalescing_threshold;
                let on_battery = self.power_manager.should_enable_power_save();
                
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let in_game = state.game_mode_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false);
                    
                    let high_cpu = cpu_load > threshold;
                    let should_coalesce = if in_game && high_cpu {
                        true
                    } else if in_game {
                        false
                    } else {
                        true // Idle or battery
                    };

                    // Hysteresis: require 2 stable ticks before changing coalescing state
                    if should_coalesce != state.coalescing_enabled {
                        if state.pending_coalescing == Some(should_coalesce) {
                            state.coalescing_stable_ticks += 1;
                        } else {
                            state.pending_coalescing = Some(should_coalesce);
                            state.coalescing_stable_ticks = 1;
                        }
                        
                        // Apply after 2 stable ticks (4 seconds)
                        if state.coalescing_stable_ticks >= 2 {
                            if should_coalesce {
                                let _ = EthtoolManager::enable_coalescing(&interface);
                                debug!("Coalescing ENABLED on {} (game:{}, cpu:{:.0}%, battery:{})",
                                       interface, in_game, cpu_load * 100.0, on_battery);
                            } else {
                                let _ = EthtoolManager::disable_coalescing(&interface);
                                debug!("Coalescing DISABLED on {} (game:{}, cpu:{:.0}%)",
                                       interface, in_game, cpu_load * 100.0);
                            }
                            state.coalescing_enabled = should_coalesce;
                            state.pending_coalescing = None;
                            state.coalescing_stable_ticks = 0;
                        }
                    } else {
                        // State matches, reset pending
                        state.pending_coalescing = None;
                        state.coalescing_stable_ticks = 0;
                    }
                }
            }

            // 5b. Power Save Management (Adaptive) - with hysteresis to prevent flapping
            // FIXED: Also disable power save during ANY network activity, not just game mode
            {
                let base_should_enable = self.power_manager.should_enable_power_save();
                
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    // Check for active network usage (PPS > 50 = meaningful traffic)
                    let pps = state.pps_monitor.sample(&interface);
                    let has_network_activity = pps > 50;
                    
                    let in_game = state.game_mode_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false);
                    
                    // Disable power save if:
                    // 1. On AC power, OR
                    // 2. Game mode active, OR  
                    // 3. Any significant network activity (>50 PPS)
                    let should_enable = base_should_enable && !in_game && !has_network_activity;
                    
                    // Hysteresis: require 3 stable ticks before changing power save
                    // This prevents AC/battery flapping from causing jitter
                    if state.power_save_enabled != Some(should_enable) {
                        if state.pending_power_save == Some(should_enable) {
                            state.power_save_stable_ticks += 1;
                        } else {
                            state.pending_power_save = Some(should_enable);
                            state.power_save_stable_ticks = 1;
                        }
                        
                        // Apply after 3 stable ticks (6 seconds) to avoid brief AC disconnects
                        if state.power_save_stable_ticks >= 3 {
                            let wifi_interfaces = self.wifi_manager.interfaces();
                            if let Some(wifi_ifc) = wifi_interfaces.iter().find(|i| i.name == interface) {
                                if should_enable {
                                    if let Ok(_) = self.wifi_manager.enable_power_save(wifi_ifc) {
                                        info!("Power save ENABLED on {} (battery, idle)", interface);
                                        state.power_save_enabled = Some(true);
                                    }
                                } else {
                                    if let Ok(_) = self.wifi_manager.disable_power_save(wifi_ifc) {
                                        let reason = if !base_should_enable { "AC power" }
                                            else if in_game { "game mode" }
                                            else { "network activity" };
                                        info!("Power save DISABLED on {} ({})", interface, reason);
                                        state.power_save_enabled = Some(false);
                                    }
                                }
                            }
                            state.pending_power_save = None;
                            state.power_save_stable_ticks = 0;
                        }
                    } else {
                        // State matches, reset pending
                        state.pending_power_save = None;
                        state.power_save_stable_ticks = 0;
                    }
                }
            }

            // 6. Smart Band Steering
            if self.config.band_steering_enabled {
                if let Some(current_ap) = &active_ap {
                    let hysteresis_ticks = self.config.roam_hysteresis_ticks;
                    
                    // Get all visible APs
                    if let Ok(access_points) = self.nm_client.get_access_points(&path).await {
                        let bias_5 = self.wifi_config.band_bias_5ghz;
                        let bias_6 = self.wifi_config.band_bias_6ghz;
                        let min_signal = self.wifi_config.min_signal_dbm;

                        let current_score = current_ap.score(bias_5, bias_6);
                        
                        // Find best AP with same SSID and good signal
                        let best = access_points.iter()
                            .filter(|ap| {
                                ap.ssid == current_ap.ssid && 
                                ap.bssid != current_ap.bssid &&
                                ap.signal_strength >= min_signal
                            })
                            .max_by_key(|ap| ap.score(bias_5, bias_6));

                        if let Some(state) = self.interface_states.get_mut(&interface) {
                            if let Some(best_candidate) = best {
                                let candidate_score = best_candidate.score(bias_5, bias_6);
                                
                                if candidate_score > current_score {
                                    // Update hysteresis
                                    let should_trigger = if let Some(ref mut roam) = state.roam_candidate {
                                        if roam.bssid == best_candidate.bssid {
                                            roam.consecutive_ticks += 1;
                                            roam.score = candidate_score;
                                        } else {
                                            *roam = RoamCandidate {
                                                bssid: best_candidate.bssid.clone(),
                                                score: candidate_score,
                                                consecutive_ticks: 1,
                                            };
                                        }
                                        roam.consecutive_ticks >= hysteresis_ticks
                                    } else {
                                        state.roam_candidate = Some(RoamCandidate {
                                            bssid: best_candidate.bssid.clone(),
                                            score: candidate_score,
                                            consecutive_ticks: 1,
                                        });
                                        false
                                    };

                                    if should_trigger {
                                        info!("Band steering: {} -> {} (score: {} -> {})",
                                              current_ap.bssid, best_candidate.bssid, 
                                              current_score, candidate_score);
                                        let _ = self.nm_client.request_scan(&path).await;
                                        state.roam_candidate = None;
                                    }
                                } else {
                                    state.roam_candidate = None;
                                }
                            } else {
                                state.roam_candidate = None;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Stop the governor and clean up
    pub fn stop(&mut self) {
        info!("Governor stopping, cleaning up...");
        
        for (interface, state) in &self.interface_states {
            let _ = state.tc_manager.remove_cake(interface);
        }
    }

    /// Fallback: Get bitrate from `iw` when NetworkManager reports 0
    fn get_bitrate_from_iw(interface: &str) -> Option<u32> {
        let output = Command::new("iw")
            .args(["dev", interface, "link"])
            .output()
            .ok()?;
        
        if !output.status.success() {
            return None;
        }
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        
        // Parse "tx bitrate: 866.7 MBit/s" or similar
        for line in stdout.lines() {
            if line.contains("tx bitrate:") {
                // Extract the number
                let parts: Vec<&str> = line.split_whitespace().collect();
                for (i, part) in parts.iter().enumerate() {
                    if *part == "bitrate:" && i + 1 < parts.len() {
                        if let Ok(mbit) = parts[i + 1].parse::<f64>() {
                            // Convert to Kbit for consistency with NM
                            debug!("iw fallback: {}Mbit on {}", mbit, interface);
                            return Some((mbit * 1000.0) as u32);
                        }
                    }
                }
            }
        }
        
        None
    }

    /// Update throughput estimate from /sys/class/net statistics
    fn update_throughput_estimate(state: &mut InterfaceState, interface: &str) {
        let rx_path = format!("/sys/class/net/{}/statistics/rx_bytes", interface);
        let tx_path = format!("/sys/class/net/{}/statistics/tx_bytes", interface);
        
        let rx_bytes = std::fs::read_to_string(&rx_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let tx_bytes = std::fs::read_to_string(&tx_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        
        let now = Instant::now();
        
        if let Some(last_time) = state.last_stats_time {
            let elapsed = now.duration_since(last_time).as_secs_f64();
            if elapsed > 0.5 {
                let rx_delta = rx_bytes.saturating_sub(state.last_rx_bytes);
                let tx_delta = tx_bytes.saturating_sub(state.last_tx_bytes);
                let total_bytes = rx_delta + tx_delta;
                let bytes_per_sec = (total_bytes as f64 / elapsed) as u64;
                
                // Only update if there's meaningful traffic (>100KB/s)
                if bytes_per_sec > 100_000 {
                    state.tc_manager.update_throughput(bytes_per_sec);
                }
            }
        }
        
        state.last_rx_bytes = rx_bytes;
        state.last_tx_bytes = tx_bytes;
        state.last_stats_time = Some(now);
    }
}
