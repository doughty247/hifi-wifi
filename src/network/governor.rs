//! The Governor - The "Brain" of hifi-wifi
//!
//! Per rewrite.md: Runs the async loop (Tick Rate: 2 seconds) and implements:
//! - Breathing CAKE (Dynamic QoS with EMA)
//! - CPU Governor (Smart Coalescing)
//! - Smart Band Steering (with Hysteresis)
//! - Game Mode Detection (PPS)

use anyhow::Result;
use log::{info, debug, warn};
use std::time::{Duration, Instant};
use tokio::time;

use crate::config::structs::{GovernorConfig, WifiConfig};
use crate::network::nm::NmClient;
use crate::network::tc::{TcManager, EthtoolManager};
use crate::network::stats::PpsMonitor;
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
}

impl InterfaceState {
    fn new(config: &GovernorConfig) -> Self {
        Self {
            pps_monitor: PpsMonitor::new(),
            tc_manager: TcManager::new(
                config.cake_ema_alpha,
                config.cake_change_threshold_mbit,
                config.cake_change_threshold_pct,
            ),
            roam_candidate: None,
            game_mode_until: None,
            coalescing_enabled: false,
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
    interface_states: std::collections::HashMap<String, InterfaceState>,
}

impl Governor {
    /// Create a new Governor with the given configuration
    pub async fn new(config: GovernorConfig, wifi_config: WifiConfig) -> Result<Self> {
        let nm_client = NmClient::new().await?;
        let cpu_monitor = CpuMonitor::new(config.cpu_avg_window_size);
        let power_manager = PowerManager::new();
        
        Ok(Self {
            config,
            wifi_config,
            nm_client,
            cpu_monitor,
            power_manager,
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

            // 3. Game Mode Detection (PPS)
            if self.config.game_mode_enabled {
                let pps_threshold = self.config.game_mode_pps_threshold;
                let cooldown_secs = self.config.game_mode_cooldown_secs;
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let pps = state.pps_monitor.sample(&interface);
                    if pps > pps_threshold {
                        let cooldown = Duration::from_secs(cooldown_secs);
                        state.game_mode_until = Some(Instant::now() + cooldown);
                        debug!("Game mode activated: {} PPS on {} (cooldown: {}s)", 
                               pps, interface, cooldown_secs);
                    }
                }
            }

            // 4. Breathing CAKE (Dynamic QoS)
            if self.config.breathing_cake_enabled && bitrate > 0 {
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    // Scale PHY bitrate using overhead factor (default 0.70)
                    let scaled_bitrate = (bitrate as f64 * self.config.cake_overhead_factor) as u32;
                    
                    if state.tc_manager.update_bandwidth(scaled_bitrate) {
                        let _ = state.tc_manager.apply_cake(&interface, scaled_bitrate);
                    }
                }
            }

            // 5. CPU Governor (Smart Coalescing)
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

                    if should_coalesce != state.coalescing_enabled {
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
}
