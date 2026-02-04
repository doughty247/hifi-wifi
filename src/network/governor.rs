//! The Governor - The "Brain" of hifi-wifi
//!
//! Per rewrite.md: Runs the async loop (Tick Rate: 2 seconds) and implements:
//! - Breathing CAKE (Dynamic QoS with asymmetric response)
//! - CPU Governor (Smart Coalescing)
//! - Smart Band Steering (with Hysteresis)
//! - Game Mode Detection (PPS) with CAKE freezing
//! - Connection Event Handling (inotify-based, per roadmap-beta2.md)

use anyhow::Result;
use log::{info, debug, warn};
use std::time::{Duration, Instant};
use std::process::{Command, Stdio};
use std::path::Path;
use std::sync::mpsc::channel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time;
use notify::{Watcher, RecursiveMode, Config as NotifyConfig, RecommendedWatcher, Event, EventKind};

use crate::config::structs::{GovernorConfig, PowerConfig, WifiConfig};
use crate::network::nm::NmClient;
use crate::network::tc::{TcManager, EthtoolManager};
use crate::network::stats::PpsMonitor;
use crate::network::wifi::WifiManager;
use crate::system::cpu::CpuMonitor;
use crate::system::power::PowerManager;

/// Path for connection event signaling (touched by NetworkManager dispatcher)
const CONNECTION_EVENT_PATH: &str = "/run/hifi-wifi/connection-changed";

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
    eee_enabled: Option<bool>,
    eee_stable_ticks: u32,
    pending_eee: Option<bool>,
    /// Last known bytes for throughput calculation
    last_rx_bytes: u64,
    last_tx_bytes: u64,
    last_stats_time: Option<Instant>,
    /// Whether we have valid bandwidth data (false = CAKE disabled)
    bandwidth_valid: bool,
    /// Last known good bitrate (Kbit/s) - used when current reading is garbage (MCS0 probes)
    last_good_bitrate: Option<u32>,
    /// Bypass hysteresis for next power save application (for reconnection fix)
    bypass_power_save_hysteresis: bool,
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
            eee_enabled: None,
            eee_stable_ticks: 0,
            pending_eee: None,
            last_rx_bytes: 0,
            last_tx_bytes: 0,
            last_stats_time: None,
            bandwidth_valid: false,
            last_good_bitrate: None,
            bypass_power_save_hysteresis: false,
        }
    }
}

/// The Network Governor - orchestrates all optimization logic
pub struct Governor {
    config: GovernorConfig,
    wifi_config: WifiConfig,
    power_config: PowerConfig,
    nm_client: NmClient,
    cpu_monitor: CpuMonitor,
    power_manager: PowerManager,
    wifi_manager: WifiManager,
    interface_states: std::collections::HashMap<String, InterfaceState>,
    /// Shared flag: when true, the scan abort task actively suppresses background scans
    scan_suppress_active: Arc<AtomicBool>,
}

impl Governor {
    /// Create a new Governor with the given configuration
    pub async fn new(config: GovernorConfig, wifi_config: WifiConfig, power_config: PowerConfig) -> Result<Self> {
        let nm_client = NmClient::new().await?;
        let cpu_monitor = CpuMonitor::new(config.cpu_avg_window_size);
        let power_manager = PowerManager::new();
        let wifi_manager = WifiManager::new()?;

        Ok(Self {
            config,
            wifi_config,
            power_config,
            nm_client,
            cpu_monitor,
            power_manager,
            wifi_manager,
            interface_states: std::collections::HashMap::new(),
            scan_suppress_active: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Run the main governor loop
    /// Per rewrite.md: Tick Rate 2 seconds, non-blocking
    /// Per roadmap-beta2.md: Watch for connection events via inotify
    pub async fn run(&mut self, tick_rate_secs: u64) -> Result<()> {
        info!("Governor starting (tick rate: {}s)", tick_rate_secs);

        // Spawn scan suppression task if enabled
        if self.config.scan_suppress {
            let flag = self.scan_suppress_active.clone();
            tokio::spawn(async move {
                scan_abort_task(flag).await;
            });
            info!("Scan suppression task started (500ms interval)");
        } else {
            info!("Scan suppression disabled by config");
        }

        // Setup inotify watcher for connection events
        let (event_tx, event_rx) = channel();
        let watcher_result = self.setup_connection_watcher(event_tx);
        let _watcher = match watcher_result {
            Ok(w) => {
                info!("Connection event watcher active (watching {})", CONNECTION_EVENT_PATH);
                Some(w)
            }
            Err(e) => {
                warn!("Connection event watcher failed (will use polling only): {}", e);
                None
            }
        };

        let mut interval = time::interval(Duration::from_secs(tick_rate_secs));

        loop {
            // Check for connection events (non-blocking)
            while let Ok(event) = event_rx.try_recv() {
                if let Ok(Event { kind: EventKind::Create(_) | EventKind::Modify(_), .. }) = event {
                    info!("Connection event detected - clearing bitrate cache and re-optimizing");
                    self.handle_connection_event().await;
                }
            }
            
            interval.tick().await;
            
            if let Err(e) = self.tick().await {
                warn!("Governor tick error: {}", e);
            }
        }
    }

    /// Setup inotify watcher for connection events
    /// The NetworkManager dispatcher touches /run/hifi-wifi/connection-changed on connect
    fn setup_connection_watcher(&self, tx: std::sync::mpsc::Sender<notify::Result<Event>>) -> Result<RecommendedWatcher> {
        use std::fs;
        
        // Ensure /run/hifi-wifi directory exists
        let run_dir = Path::new("/run/hifi-wifi");
        if !run_dir.exists() {
            fs::create_dir_all(run_dir)?;
        }
        
        // Create the file if it doesn't exist (so we can watch it)
        let event_file = Path::new(CONNECTION_EVENT_PATH);
        if !event_file.exists() {
            fs::write(event_file, "")?;
        }
        
        // Create watcher with reasonable poll interval
        let config = NotifyConfig::default()
            .with_poll_interval(Duration::from_millis(200));
        
        let mut watcher = RecommendedWatcher::new(tx, config)?;
        watcher.watch(event_file, RecursiveMode::NonRecursive)?;
        
        Ok(watcher)
    }

    /// Handle a connection event (WiFi reconnect)
    /// Per roadmap-beta2.md: Clear cache, wait for link stability, re-optimize
    async fn handle_connection_event(&mut self) {
        // Clear all cached state - stale after reconnection
        for (interface, state) in &mut self.interface_states {
            if state.last_good_bitrate.is_some() {
                info!("Clearing cached bitrate for {} (was {:?} Kbit/s)",
                      interface, state.last_good_bitrate);
            }
            state.last_good_bitrate = None;
            state.bandwidth_valid = false;
            state.power_save_enabled = None; // Force re-apply on next tick
        }
        
        // Wait 1 second for link to stabilize (per legacy dispatcher behavior)
        info!("Waiting 1s for link to stabilize...");
        tokio::time::sleep(Duration::from_secs(1)).await;
        
        // FIX for Issue #15: Force immediate CAKE application after reconnection
        // Don't wait for warmup samples - apply with conservative 100Mbit default
        info!("Forcing immediate CAKE application on all interfaces (warmup bypass)");
        for (interface, state) in &mut self.interface_states {
            // Apply CAKE with conservative 100Mbit (will be adjusted by tick() once samples arrive)
            let default_mbit = 100;
            let scaled_mbit = (default_mbit as f64 * 0.85) as u32; // 85Mbit
            if let Err(e) = state.tc_manager.apply_cake(interface) {
                warn!("Failed to force-apply CAKE on {}: {}", interface, e);
            } else {
                // Inject the default into tc_manager so it has a baseline
                state.tc_manager.update_bandwidth(scaled_mbit);
                state.bandwidth_valid = true;
                info!("Force-applied CAKE on {} at {}Mbit (will adjust dynamically)", interface, scaled_mbit);
            }
        }
        
        // Force immediate tick to apply fresh optimizations
        if let Err(e) = self.tick().await {
            warn!("Post-reconnect tick error: {}", e);
        }
        
        info!("Post-reconnect optimization complete");
    }

    /// Single tick of the governor loop
    async fn tick(&mut self) -> Result<()> {
        // 0. Ensure CAKE is applied on active Ethernet interfaces
        for ifc in self.wifi_manager.interfaces() {
            if ifc.interface_type == crate::network::wifi::InterfaceType::Ethernet
                && self.wifi_manager.is_interface_connected(ifc)
                && !Self::has_cake(&ifc.name)
            {
                let bandwidth = self.calculate_cake_bandwidth(ifc);
                if let Err(e) = self.wifi_manager.apply_cake(ifc, bandwidth.max(1)) {
                    warn!("Failed to apply CAKE on {}: {}", ifc.name, e);
                }
            }
        }

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

        // Update scan suppression flag: suppress when connected, allow when disconnected
        if self.config.scan_suppress {
            let has_wifi_connection = !device_infos.is_empty();
            self.scan_suppress_active.store(has_wifi_connection, Ordering::Relaxed);
        }

        for (interface, path, bitrate, active_ap) in device_infos {
            info!("Processing interface: {}, active_ap: {:?}, band_steering_enabled: {}", 
                  interface, active_ap.as_ref().map(|ap| &ap.bssid), self.config.band_steering_enabled);
            
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
                // Reject readings below 20Mbit (lowered for Steam Deck compatibility)
                // WiFi 4 HT20 MCS7 = 65Mbit, but some devices report lower during idle
                let min_valid_kbit = 20_000;  // 20 Mbit minimum
                
                let nm_valid = nm_bitrate >= min_valid_kbit;
                let iw_valid = iw_bitrate >= min_valid_kbit;
                
                // Debug logging on first tick or when both invalid
                if !nm_valid && !iw_valid {
                    debug!("CAKE bitrate check on {}: NM={} Kbit (valid:{}), iw={} Kbit (valid:{})",
                           interface, nm_bitrate, nm_valid, iw_bitrate, iw_valid);
                }
                
                let effective_bitrate = match (nm_valid, iw_valid) {
                    (true, true) => (nm_bitrate + iw_bitrate) / 2,  // Average both
                    (true, false) => nm_bitrate,
                    (false, true) => iw_bitrate,
                    (false, false) => 0,  // Both invalid - will use last known good
                };
                
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    // Update throughput estimate from actual traffic
                    Self::update_throughput_estimate(state, &interface);
                    
                    if effective_bitrate > 0 {
                        // Store as last known good bitrate
                        state.last_good_bitrate = Some(effective_bitrate);
                        
                        // Convert Kbit to Mbit and scale using overhead factor (default 0.85)
                        let bitrate_mbit = effective_bitrate / 1000;
                        let scaled_mbit = (bitrate_mbit as f64 * self.config.cake_overhead_factor) as u32;
                        
                        debug!("CAKE: NM={}Kbit, iw={}Kbit, effective={}Kbit, scaled={}Mbit",
                               nm_bitrate, iw_bitrate, effective_bitrate, scaled_mbit);
                        
                        if state.tc_manager.update_bandwidth(scaled_mbit) {
                            let _ = state.tc_manager.apply_cake(&interface);
                        }
                        state.bandwidth_valid = true;
                    } else if let Some(last_good) = state.last_good_bitrate {
                        // Both sources invalid BUT we have a last known good value - use it
                        // This handles MCS0 probe frames during idle periods
                        let bitrate_mbit = last_good / 1000;
                        let scaled_mbit = (bitrate_mbit as f64 * self.config.cake_overhead_factor) as u32;
                        
                        debug!("CAKE: Invalid readings (NM={}, iw={}), using last known good {}Kbit -> {}Mbit",
                               nm_bitrate, iw_bitrate, last_good, scaled_mbit);
                        
                        if state.tc_manager.update_bandwidth(scaled_mbit) {
                            let _ = state.tc_manager.apply_cake(&interface);
                        }
                        state.bandwidth_valid = true;
                    } else {
                        // No current OR historical valid bitrate
                        // Use a conservative default of 100Mbit (safe for most WiFi 5/6 networks)
                        // This ensures CAKE is enabled even when bitrate detection fails
                        let default_mbit = 100;
                        let scaled_mbit = (default_mbit as f64 * self.config.cake_overhead_factor) as u32;
                        
                        if !state.bandwidth_valid {
                            info!("CAKE: No bitrate detected (NM={}, iw={}), using conservative default {}Mbit on {}",
                                  nm_bitrate, iw_bitrate, default_mbit, interface);
                        }
                        
                        if state.tc_manager.update_bandwidth(scaled_mbit) {
                            let _ = state.tc_manager.apply_cake(&interface);
                        }
                        state.bandwidth_valid = true;
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

            // 5b. Power Save Management - respects config mode
            // "off"/"on" = user override (skip adaptive logic entirely)
            // "adaptive" = original hysteresis logic based on AC/battery/activity
            {
                let power_mode = self.power_config.wlan_power_save.as_str();

                match power_mode {
                    "off" => {
                        // User wants power save permanently off — never call enable_power_save
                        if let Some(state) = self.interface_states.get_mut(&interface) {
                            if state.power_save_enabled != Some(false) {
                                let wifi_interfaces = self.wifi_manager.interfaces();
                                if let Some(wifi_ifc) = wifi_interfaces.iter().find(|i| i.name == interface) {
                                    if let Ok(_) = self.wifi_manager.disable_power_save(wifi_ifc) {
                                        info!("Power save forced OFF on {} (config override)", interface);
                                        state.power_save_enabled = Some(false);
                                    }
                                }
                            }
                        }
                    }
                    "on" => {
                        // User wants power save permanently on — never call disable_power_save
                        if let Some(state) = self.interface_states.get_mut(&interface) {
                            if state.power_save_enabled != Some(true) {
                                let wifi_interfaces = self.wifi_manager.interfaces();
                                if let Some(wifi_ifc) = wifi_interfaces.iter().find(|i| i.name == interface) {
                                    if let Ok(_) = self.wifi_manager.enable_power_save(wifi_ifc) {
                                        info!("Power save forced ON on {} (config override)", interface);
                                        state.power_save_enabled = Some(true);
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        // "adaptive" — original hysteresis logic, unchanged
                        let base_should_enable = self.power_manager.should_enable_power_save();

                        if let Some(state) = self.interface_states.get_mut(&interface) {
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
                }
            }

            // 5c. Energy Efficient Ethernet (EEE) Management - Adaptive based on power source
            // EEE causes 50-200us wakeup latency on ethernet, so disable for gaming/streaming
            {
                let base_should_enable = self.power_manager.should_enable_power_save();
                
                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let wifi_interfaces = self.wifi_manager.interfaces();
                    if let Some(ifc) = wifi_interfaces.iter().find(|i| i.name == interface) {
                        // Only manage EEE for ethernet interfaces
                        if ifc.interface_type == crate::network::wifi::InterfaceType::Ethernet {
                            let pps = state.pps_monitor.sample(&interface);
                            let has_network_activity = pps > 50;
                            
                            let in_game = state.game_mode_until
                                .map(|until| Instant::now() < until)
                                .unwrap_or(false);
                            
                            // Enable EEE only on battery AND idle (no game, no network activity)
                            // Otherwise disable for minimum latency
                            let should_enable = base_should_enable && !in_game && !has_network_activity;
                            
                            // Hysteresis: require 3 stable ticks before changing EEE
                            if state.eee_enabled != Some(should_enable) {
                                if state.pending_eee == Some(should_enable) {
                                    state.eee_stable_ticks += 1;
                                } else {
                                    state.pending_eee = Some(should_enable);
                                    state.eee_stable_ticks = 1;
                                }
                                
                                // Apply after 3 stable ticks (6 seconds)
                                if state.eee_stable_ticks >= 3 {
                                    if should_enable {
                                        if let Ok(_) = EthtoolManager::enable_eee(&interface) {
                                            info!("EEE ENABLED on {} (battery, idle)", interface);
                                            state.eee_enabled = Some(true);
                                        }
                                    } else {
                                        if let Ok(_) = EthtoolManager::disable_eee(&interface) {
                                            let reason = if !base_should_enable { "AC power" }
                                                else if in_game { "game mode" }
                                                else { "network activity" };
                                            info!("EEE DISABLED on {} ({})", interface, reason);
                                            state.eee_enabled = Some(false);
                                        }
                                    }
                                    state.pending_eee = None;
                                    state.eee_stable_ticks = 0;
                                }
                            } else {
                                // State matches, reset pending
                                state.pending_eee = None;
                                state.eee_stable_ticks = 0;
                            }
                        }
                    }
                }
            }

            // 6. Smart Band Steering
            // Skip when scan suppress is active — scan results are stale/empty
            if self.config.band_steering_enabled && !self.scan_suppress_active.load(Ordering::Relaxed) {
                if let Some(current_ap) = &active_ap {
                    let hysteresis_ticks = self.config.roam_hysteresis_ticks;
                    
                    info!("Band steering: Checking for better AP (current: {} on {:?}, score: {})", 
                           current_ap.bssid, current_ap.band, 
                           current_ap.score(self.wifi_config.band_bias_5ghz, self.wifi_config.band_bias_6ghz));
                    
                    // Get all visible APs
                    match self.nm_client.get_access_points(&path).await {
                        Ok(access_points) => {
                            info!("Band steering: Found {} visible APs (current SSID: '{}')", access_points.len(), current_ap.ssid);
                            info!("Band steering: access_points is_empty={}, len={}", access_points.is_empty(), access_points.len());
                            
                            if access_points.is_empty() {
                                info!("Band steering: No APs returned from NetworkManager");
                                continue;
                            }
                            
                            let bias_5 = self.wifi_config.band_bias_5ghz;
                            let bias_6 = self.wifi_config.band_bias_6ghz;
                            let min_2g = self.wifi_config.min_signal_2g_dbm;
                            let min_5g = self.wifi_config.min_signal_5g_dbm;
                            let min_6g = self.wifi_config.min_signal_6g_dbm;

                            let current_score = current_ap.score(bias_5, bias_6);
                            
                            // First, log all APs to see what we have
                            info!("Band steering: About to list {} APs...", access_points.len());
                            for i in 0..access_points.len() {
                                let ap = &access_points[i];
                                info!("  [{}] AP: {} ({}), band={:?}, signal={}dBm, rate={}Mbps", 
                                       i, ap.bssid, ap.ssid, ap.band, ap.signal_strength, ap.max_bitrate / 1000);
                            }
                            info!("Band steering: Done listing APs");
                            
                            // Find best AP with same SSID and usable signal for its band
                            let best = access_points.iter()
                                .filter(|ap| {
                                    let same_ssid = ap.ssid == current_ap.ssid;
                                    let different_bssid = ap.bssid != current_ap.bssid;
                                    let signal_ok = ap.signal_usable(min_2g, min_5g, min_6g);
                                    
                                    info!("  AP {}: ssid={} (same={}), band={:?}, signal={}dBm (ok={}), max_rate={}Mbps, score={}", 
                                           ap.bssid, ap.ssid, same_ssid, ap.band, ap.signal_strength, signal_ok,
                                           ap.max_bitrate / 1000, ap.score(bias_5, bias_6));
                                    
                                    same_ssid && different_bssid && signal_ok
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
                                        info!("Band steering: {} -> {} (score: {} -> {}, band: {:?} -> {:?})",
                                              current_ap.bssid, best_candidate.bssid, 
                                              current_score, candidate_score,
                                              current_ap.band, best_candidate.band);
                                        
                                        // Clear cached bitrate - after roaming it will be stale
                                        state.last_good_bitrate = None;
                                        state.bandwidth_valid = false;
                                        
                                        // Request scan to hint firmware/driver about better AP
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
                        Err(e) => {
                            debug!("Band steering: Failed to get APs: {}", e);
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
        
        // Parse bitrate from iw output - multiple formats supported:
        // "tx bitrate: 866.7 MBit/s ..."
        // "	tx bitrate: 866.7 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 2"
        // Steam Deck ath11k may report: "tx bitrate: 1201.0 MBit/s 80MHz HE-MCS 11 HE-NSS 2 HE-GI 0 HE-DCM 0"
        for line in stdout.lines() {
            let line_lower = line.to_lowercase();
            if line_lower.contains("tx bitrate:") || line_lower.contains("bitrate:") {
                // Extract the number - look for pattern like "866.7 MBit/s" or "1201.0 Mbit/s"
                let parts: Vec<&str> = line.split_whitespace().collect();
                for (i, part) in parts.iter().enumerate() {
                    // Look for "bitrate:" followed by a number
                    if part.to_lowercase().contains("bitrate:") {
                        // Next part should be the number
                        if i + 1 < parts.len() {
                            if let Ok(mbit) = parts[i + 1].parse::<f64>() {
                                // Convert to Kbit for consistency with NM
                                debug!("iw fallback: {}Mbit on {}", mbit, interface);
                                return Some((mbit * 1000.0) as u32);
                            }
                        }
                    }
                    // Also try matching "NNN.N" followed by "MBit" in case format varies
                    if i + 1 < parts.len() && parts[i + 1].to_lowercase().contains("mbit") {
                        if let Ok(mbit) = part.parse::<f64>() {
                            debug!("iw fallback (alt format): {}Mbit on {}", mbit, interface);
                            return Some((mbit * 1000.0) as u32);
                        }
                    }
                }
            }
        }
        
        // Final fallback: try to get signal from iw station dump
        // Some drivers (ath11k) may report better data this way
        let station_output = Command::new("iw")
            .args(["dev", interface, "station", "dump"])
            .output()
            .ok()?;
        
        if station_output.status.success() {
            let station_out = String::from_utf8_lossy(&station_output.stdout);
            for line in station_out.lines() {
                let line_lower = line.to_lowercase();
                if line_lower.contains("tx bitrate:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    for (i, part) in parts.iter().enumerate() {
                        if part.to_lowercase().contains("bitrate:") && i + 1 < parts.len() {
                            if let Ok(mbit) = parts[i + 1].parse::<f64>() {
                                debug!("iw station dump fallback: {}Mbit on {}", mbit, interface);
                                return Some((mbit * 1000.0) as u32);
                            }
                        }
                    }
                }
            }
        }
        
        None
    }

    /// Check if CAKE qdisc is active on an interface
    fn has_cake(interface: &str) -> bool {
        let output = Command::new("tc")
            .args(["qdisc", "show", "dev", interface])
            .output();

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return stdout.contains("cake");
        }

        false
    }

    /// Calculate CAKE bandwidth from link stats (fallback to 200Mbit)
    fn calculate_cake_bandwidth(&self, ifc: &crate::network::wifi::WifiInterface) -> u32 {
        match self.wifi_manager.get_link_stats(ifc) {
            Ok(stats) if stats.tx_bitrate_mbps > 0.0 => (stats.tx_bitrate_mbps * 0.60) as u32,
            Ok(_) => 200,
            Err(_) => 200,
        }
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

/// Background task that aborts iwd's background scans every 500ms.
///
/// iwd initiates a full-channel scan cycle every ~15 seconds (5.8s of off-channel time)
/// that causes 150-175ms latency spikes. By aborting these scans before the radio leaves
/// the home channel for the 5GHz+6GHz sweep, latency drops from ~20ms avg / 170ms max
/// to ~3.5ms avg / 4ms max.
///
/// The abort command is a no-op when no scan is in progress (returns ENOENT, harmless).
/// Only aborts when the flag is set (interface is connected). When disconnected, scans
/// are allowed so reconnection can proceed.
async fn scan_abort_task(active: Arc<AtomicBool>) {
    let mut interval = time::interval(Duration::from_millis(500));

    loop {
        interval.tick().await;

        if !active.load(Ordering::Relaxed) {
            continue;
        }

        // Find connected WiFi interfaces and abort their scans
        let interfaces = find_wifi_interfaces();
        for ifc in &interfaces {
            let _ = Command::new("iw")
                .args(["dev", ifc, "scan", "abort"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output();
        }
    }
}

/// Find WiFi interfaces that are currently connected (operstate "up").
/// Reads from /sys/class/net to avoid any D-Bus overhead.
fn find_wifi_interfaces() -> Vec<String> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Check if it's a wireless interface
            let wireless_path = format!("/sys/class/net/{}/wireless", name);
            if !Path::new(&wireless_path).exists() {
                continue;
            }
            // Check if it's up (connected)
            let operstate_path = format!("/sys/class/net/{}/operstate", name);
            if let Ok(state) = std::fs::read_to_string(&operstate_path) {
                if state.trim() == "up" {
                    result.push(name);
                }
            }
        }
    }
    result
}
