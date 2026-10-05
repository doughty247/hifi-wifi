//! The Governor - The "Brain" of hifi-wifi
//!
//! Per rewrite.md: Runs the async loop (Tick Rate: 2 seconds) and implements:
//! - Breathing CAKE (Dynamic QoS with asymmetric response)
//! - CPU Governor (Smart Coalescing)
//! - Smart Band Steering (with Hysteresis)
//! - Game Mode Detection (PPS) with CAKE freezing
//! - Connection Event Handling (inotify-based, per roadmap-beta2.md)

use anyhow::Result;
use log::{debug, info, warn};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command as TokioCommand;
use tokio::time;

use crate::config::structs::{
    AutorateConfig, AutorateMode, GovernorConfig, PowerConfig, ScanSuppressMode, WifiConfig,
};
use crate::network::autorate::{self, AutorateHandle};
use crate::network::nm::NmClient;
use crate::network::stats::PpsMonitor;
use crate::network::tc::{EthtoolManager, TcManager};
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
    aspm_performance: Option<bool>,
    aspm_stable_ticks: u32,
    pending_aspm: Option<bool>,
    /// Last known bytes for throughput calculation
    last_rx_bytes: u64,
    last_tx_bytes: u64,
    last_stats_time: Option<Instant>,
    /// Whether we have valid bandwidth data (false = CAKE disabled)
    bandwidth_valid: bool,
    /// Last known good bitrate (Kbit/s) - used when current reading is garbage (MCS0 probes)
    last_good_bitrate: Option<u32>,
    /// SSID of the current network (key for autorate's remembered rates)
    ssid: Option<String>,
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
                config.qos_use_ifb,
                config.internet_download_mbit,
                config.internet_upload_mbit,
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
            aspm_performance: None,
            aspm_stable_ticks: 0,
            pending_aspm: None,
            last_rx_bytes: 0,
            last_tx_bytes: 0,
            last_stats_time: None,
            bandwidth_valid: false,
            last_good_bitrate: None,
            ssid: None,
        }
    }
}

/// The Network Governor - orchestrates all optimization logic
pub struct Governor {
    config: GovernorConfig,
    wifi_config: WifiConfig,
    power_config: PowerConfig,
    system_config: crate::config::structs::SystemConfig,
    nm_client: NmClient,
    cpu_monitor: CpuMonitor,
    power_manager: PowerManager,
    wifi_manager: WifiManager,
    interface_states: std::collections::HashMap<String, InterfaceState>,
    /// Shared flag: when true, the scan abort task actively suppresses background scans
    scan_suppress_active: Arc<AtomicBool>,
    last_tick_at: Instant,
    last_resume_at: Option<Instant>,
    current_applied_cc: Option<String>,
    initialized_interfaces: std::collections::HashSet<String>,
    autorate_config: AutorateConfig,
    /// Running autorate controller (owns shaping when present)
    autorate: Option<AutorateHandle>,
    game_priority: crate::network::priority::GamePriority,
    /// Last known carrier state per ifindex (netlink), to act only on down -> up
    link_carrier: std::collections::HashMap<u32, bool>,
    /// Ticks since start, for periodic checks
    ticks: u64,
}

impl Governor {
    /// Create a new Governor with the given configuration
    pub async fn new(
        config: GovernorConfig,
        wifi_config: WifiConfig,
        power_config: PowerConfig,
        system_config: crate::config::structs::SystemConfig,
        autorate_config: AutorateConfig,
    ) -> Result<Self> {
        let nm_client = NmClient::new().await?;
        let cpu_monitor = CpuMonitor::new(config.cpu_avg_window_size);
        let power_manager = PowerManager::new();
        let wifi_manager = WifiManager::new()?;
        let now = Instant::now();
        let game_priority = crate::network::priority::GamePriority::new(
            config.game_priority_enabled,
            config.game_priority_udp_ports.clone(),
        );

        let mut initialized_interfaces = std::collections::HashSet::new();
        for ifc in wifi_manager.interfaces() {
            if wifi_manager.is_interface_connected(ifc) {
                initialized_interfaces.insert(ifc.name.clone());
            }
        }

        Ok(Self {
            config,
            wifi_config,
            power_config,
            system_config,
            nm_client,
            cpu_monitor,
            power_manager,
            wifi_manager,
            interface_states: std::collections::HashMap::new(),
            scan_suppress_active: Arc::new(AtomicBool::new(false)),
            last_tick_at: now,
            last_resume_at: Some(now), // Treat startup as initial grace period
            current_applied_cc: None,
            initialized_interfaces,
            autorate_config,
            autorate: None,
            game_priority,
            link_carrier: std::collections::HashMap::new(),
            ticks: 0,
        })
    }

    /// Run the main governor loop
    /// Per rewrite.md: Tick Rate 2 seconds, non-blocking
    /// Per roadmap-beta2.md: Watch for connection events via inotify
    pub async fn run(&mut self, tick_rate_secs: u64) -> Result<()> {
        info!("Governor starting (tick rate: {}s)", tick_rate_secs);

        // Spawn scan suppression task if enabled
        if self.config.scan_suppress != ScanSuppressMode::Off {
            let flag = self.scan_suppress_active.clone();
            tokio::spawn(async move {
                scan_abort_task(flag).await;
            });
            info!(
                "Scan suppression task started (500ms interval, mode: {:?})",
                self.config.scan_suppress
            );
        } else {
            info!("Scan suppression disabled by config");
        }

        // Autorate owns bufferbloat shaping when enabled and tc is present
        // Detect mode only pings; shaping modes also need tc
        let shaping_mode = !matches!(self.autorate_config.mode, AutorateMode::Off | AutorateMode::Detect);
        if self.autorate_config.mode == AutorateMode::Detect
            || (shaping_mode && crate::network::tc::is_tc_available())
        {
            info!(
                "Autorate enabled (mode: {:?}, reflectors: {:?})",
                self.autorate_config.mode, self.autorate_config.reflectors
            );
            self.autorate = Some(autorate::spawn(autorate::Settings {
                cfg: self.autorate_config.clone(),
                internet_down_mbit: self.config.internet_download_mbit,
                internet_up_mbit: self.config.internet_upload_mbit,
                shape_download: self.config.qos_use_ifb,
            }));
        }

        // Setup inotify watcher for connection events
        let (event_tx, event_rx) = channel();
        let watcher_result = self.setup_connection_watcher(event_tx);
        let _watcher = match watcher_result {
            Ok(w) => {
                info!(
                    "Connection event watcher active (watching {})",
                    CONNECTION_EVENT_PATH
                );
                Some(w)
            }
            Err(e) => {
                warn!(
                    "Connection event watcher failed (will use polling only): {}",
                    e
                );
                None
            }
        };

        // Initialize generic Netlink event listener
        let netlink_listener = match crate::network::netlink::NetlinkListener::new() {
            Ok(nl) => Some(nl),
            Err(e) => {
                warn!(
                    "Failed to initialize Netlink listener (falling back to timer-only): {}",
                    e
                );
                None
            }
        };

        let mut interval = time::interval(Duration::from_secs(tick_rate_secs));

        loop {
            // Check for connection events (non-blocking)
            while let Ok(event) = event_rx.try_recv() {
                if let Ok(Event {
                    kind: EventKind::Create(_) | EventKind::Modify(_),
                    ..
                }) = event
                {
                    info!("Connection event detected - clearing bitrate cache and re-optimizing");
                    self.handle_connection_event().await;
                }
            }

            // Wait for either the tick timer OR a Netlink event
            if let Some(ref nl) = netlink_listener {
                tokio::select! {
                    _ = interval.tick() => {
                        debug!("Periodic tick timer expired");
                    }
                    event_res = nl.next_events() => {
                        match event_res {
                            Ok(events) => {
                                let came_up = self.carrier_came_up(&events);
                                if !came_up.is_empty() {
                                    info!("Carrier up on {:?} - re-applying hardware baseline", came_up);
                                    self.reapply_hardware_baseline(Some(&came_up));
                                }
                            }
                            Err(e) => {
                                warn!("Netlink listener error: {}", e);
                            }
                        }
                    }
                }
            } else {
                interval.tick().await;
            }

            // Check if we resumed from suspend (large time gap between ticks)
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_tick_at);

            // Rate-limit netlink-triggered ticks to once per 1 second to prevent CPU event storm DoS
            if elapsed.as_secs() < 1 {
                debug!("Ignoring rapid Netlink event to prevent event storm DoS");
                continue;
            }

            if elapsed.as_secs() > tick_rate_secs * 3 {
                info!("System resume from suspend detected (elapsed: {}s)! Re-applying settings and temporarily allowing background scans.", elapsed.as_secs());
                self.last_resume_at = Some(now);
                // Drivers reset power save (and PCIe link state) on resume; our cache says
                // "already applied", so forget it and re-apply (issue #23)
                self.forget_applied_state();
                self.reapply_hardware_baseline(None);
            }
            self.last_tick_at = now;

            if let Err(e) = self.tick().await {
                warn!("Governor tick error: {}", e);
            }
        }
    }

    /// Setup inotify watcher for connection events
    /// The NetworkManager dispatcher touches /run/hifi-wifi/connection-changed on connect
    fn setup_connection_watcher(
        &self,
        tx: std::sync::mpsc::Sender<notify::Result<Event>>,
    ) -> Result<RecommendedWatcher> {
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
        let config = NotifyConfig::default().with_poll_interval(Duration::from_millis(200));

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
                info!(
                    "Clearing cached bitrate for {} (was {:?} Kbit/s)",
                    interface, state.last_good_bitrate
                );
            }
            state.last_good_bitrate = None;
            state.bandwidth_valid = false;
            state.power_save_enabled = None; // Force re-apply on next tick
        }

        // Wait 1 second for link to stabilize (per legacy dispatcher behavior)
        info!("Waiting 1s for link to stabilize...");
        tokio::time::sleep(Duration::from_secs(1)).await;

        self.reapply_hardware_baseline(None);

        // FIX for Issue #15: Force immediate CAKE application after reconnection
        // Don't wait for warmup samples - apply with conservative 100Mbit default.
        // Autorate re-detects the link on its own, so this only applies to breathing CAKE.
        let breathing = self.config.breathing_cake_enabled && self.autorate.is_none();
        for (interface, state) in self.interface_states.iter_mut().filter(|_| breathing) {
            // Apply CAKE with conservative 100Mbit (will be adjusted by tick() once samples arrive)
            let default_mbit = 100;
            let scaled_mbit = (default_mbit as f64 * 0.85) as u32; // 85Mbit
            if let Err(e) = state.tc_manager.apply_cake(interface) {
                warn!("Failed to force-apply CAKE on {}: {}", interface, e);
            } else {
                // Inject the default into tc_manager so it has a baseline
                state.tc_manager.update_bandwidth(scaled_mbit);
                state.bandwidth_valid = true;
                info!(
                    "Force-applied CAKE on {} at {}Mbit (will adjust dynamically)",
                    interface, scaled_mbit
                );
            }
        }

        // Force immediate tick to apply fresh optimizations
        if let Err(e) = self.tick().await {
            warn!("Post-reconnect tick error: {}", e);
        }

        info!("Post-reconnect optimization complete");
    }

    /// Managed physical interfaces whose carrier went from down (or never seen) to up.
    /// Our own IFB devices and other virtual links are ignored, so shaping changes never
    /// touch the Wi-Fi card.
    fn carrier_came_up(&mut self, events: &[crate::network::netlink::LinkEvent]) -> Vec<String> {
        let mut up = Vec::new();
        for ev in events {
            if ev.removed {
                self.link_carrier.remove(&ev.ifindex);
                continue;
            }
            let was = self.link_carrier.insert(ev.ifindex, ev.carrier);
            if !ev.carrier || was == Some(true) {
                continue;
            }
            let Some(name) = crate::network::netlink::ifindex_name(ev.ifindex) else {
                continue;
            };
            let managed = self.wifi_manager.interfaces().iter().any(|i| i.name == name);
            // A first sighting with carrier up is a real transition only if not yet initialized
            if managed && (was == Some(false) || !self.initialized_interfaces.contains(&name)) {
                up.push(name);
            }
        }
        up
    }

    /// Clear the cached power save state where the driver disagrees with it
    fn verify_power_save(&mut self) {
        for ifc in self.wifi_manager.interfaces() {
            let Some(state) = self.interface_states.get_mut(&ifc.name) else {
                continue;
            };
            let Some(expected) = state.power_save_enabled else {
                continue;
            };
            if let Some(actual) = self.wifi_manager.get_power_save(ifc) {
                if actual != expected {
                    info!(
                        "Power save on {} was changed outside hifi-wifi (now {}), re-applying",
                        ifc.name,
                        if actual { "on" } else { "off" }
                    );
                    state.power_save_enabled = None;
                    state.pending_power_save = None;
                    state.power_save_stable_ticks = 0;
                }
            }
        }
    }

    /// Forget which settings we believe are applied, so the next tick re-applies them
    fn forget_applied_state(&mut self) {
        for state in self.interface_states.values_mut() {
            state.power_save_enabled = None;
            state.pending_power_save = None;
            state.power_save_stable_ticks = 0;
            state.aspm_performance = None;
            state.pending_aspm = None;
            state.aspm_stable_ticks = 0;
            state.eee_enabled = None;
            state.pending_eee = None;
            state.eee_stable_ticks = 0;
        }
    }

    /// Re-apply PCIe link power and IRQ pinning to the given interfaces, or to all connected
    /// ones. Both are idempotent and never bounce the link.
    fn reapply_hardware_baseline(&self, only: Option<&[String]>) {
        if !(self.system_config.irq_affinity_enabled || self.system_config.driver_tweaks_enabled) {
            return;
        }
        let sys_opt = crate::system::optimizer::SystemOptimizer::new(
            self.system_config.sysctl_enabled,
            self.system_config.irq_affinity_enabled,
            self.system_config.driver_tweaks_enabled,
            self.system_config.tcp_congestion_control.clone(),
        );
        for ifc in self.wifi_manager.interfaces() {
            let selected = match only {
                Some(names) => names.contains(&ifc.name),
                None => self.wifi_manager.is_interface_connected(ifc),
            };
            if selected {
                if let Err(e) = sys_opt.reapply_link_baseline(ifc) {
                    warn!("Failed to re-apply hardware baseline on {}: {}", ifc.name, e);
                }
            }
        }
    }

    /// Dynamically adjust the system TCP congestion control algorithm based on active media.
    /// Default to global BBR baseline, only pivot to Cubic if a hard-clamped CAKE shaper is actively saturated.
    fn optimize_congestion_control_for_media(&mut self) {
        if !self.system_config.sysctl_enabled {
            return;
        }

        let mut has_active_interface = false;
        for ifc in self.wifi_manager.interfaces() {
            if self.wifi_manager.is_interface_connected(ifc) {
                has_active_interface = true;
                break;
            }
        }
        if !has_active_interface {
            return;
        }

        // Default global baseline is BBR
        let mut target_algorithm = "bbr";

        // Check if any active interface has an active, hard-clamped CAKE shaper that is actively saturated
        for ifc in self.wifi_manager.interfaces() {
            if self.wifi_manager.is_interface_connected(ifc) {
                if let Some(state) = self.interface_states.get(&ifc.name) {
                    if Self::has_cake(&ifc.name) {
                        if let Some(last_bw_mbit) = state.tc_manager.get_last_bandwidth() {
                            // Get physical link/wire speed (fallback to 1000 for Ethernet, 300 for Wifi)
                            let link_speed_mbit = match ifc.interface_type {
                                crate::network::wifi::InterfaceType::Ethernet => {
                                    let speed_path = format!("/sys/class/net/{}/speed", ifc.name);
                                    std::fs::read_to_string(&speed_path)
                                        .ok()
                                        .and_then(|s| s.trim().parse::<u32>().ok())
                                        .unwrap_or(1000)
                                }
                                crate::network::wifi::InterfaceType::Wifi => {
                                    state.last_good_bitrate.map(|kbit| kbit / 1000).unwrap_or(300)
                                }
                            };

                            // Hard-clamped shaper below physical wire threshold
                            let is_hard_clamped = last_bw_mbit < (link_speed_mbit as f64 * 0.90) as u32;

                            // Check for active saturation (using >= 80% of shaper bandwidth)
                            let current_throughput_mbit = state.tc_manager.get_current_throughput_mbps();
                            let is_saturated = current_throughput_mbit >= (last_bw_mbit as f64 * 0.80) as u32;

                            if is_hard_clamped && is_saturated {
                                info!(
                                    "Interface {} has a hard-clamped shaper ({}Mbit < physical {}Mbit) and is saturated ({}Mbit). Pivoting to Cubic.",
                                    ifc.name, last_bw_mbit, link_speed_mbit, current_throughput_mbit
                                );
                                target_algorithm = "cubic";
                                break;
                            }
                        }
                    }
                }
            }
        }

        let should_apply = match &self.current_applied_cc {
            Some(curr) => curr != target_algorithm,
            None => true,
        };

        if should_apply {
            info!(
                "TCP congestion control shift detected. Dynamically switching to: {}",
                target_algorithm
            );
            
            let status = std::process::Command::new("sysctl")
                .args(["-w", &format!("net.ipv4.tcp_congestion_control={}", target_algorithm)])
                .status();

            match status {
                Ok(stat) if stat.success() => {
                    self.current_applied_cc = Some(target_algorithm.to_string());
                }
                Ok(stat) => {
                    warn!("sysctl dynamic congestion control switch exited with error: {}", stat);
                }
                Err(e) => {
                    warn!("Failed to dynamically switch TCP congestion control: {}", e);
                }
            }
        }
    }

    /// Check for hot-plugged / newly activated interfaces and apply baseline optimizations immediately
    fn check_and_initialize_hotplug_interfaces(&mut self) {
        let sys_opt = crate::system::optimizer::SystemOptimizer::new(
            self.system_config.sysctl_enabled,
            self.system_config.irq_affinity_enabled,
            self.system_config.driver_tweaks_enabled,
            self.system_config.tcp_congestion_control.clone(),
        );

        for ifc in self.wifi_manager.interfaces() {
            if self.wifi_manager.is_interface_connected(ifc) {
                if !self.initialized_interfaces.contains(&ifc.name) {
                    info!(
                        "Hot-plug network transition detected on {}. Applying runtime baseline hardware optimizations immediately.",
                        ifc.name
                    );
                    if let Err(e) = sys_opt.apply_single_interface(ifc) {
                        warn!("Failed to apply hot-plug optimizations to {}: {:?}", ifc.name, e);
                    } else {
                        self.initialized_interfaces.insert(ifc.name.clone());
                    }
                }
            } else {
                // If it is disconnected, remove from initialized so that if it reconnects later we optimize it again!
                if self.initialized_interfaces.remove(&ifc.name) {
                    info!("Interface {} disconnected. Clearing initialization state.", ifc.name);
                }
            }
        }
    }


    /// Single tick of the governor loop
    async fn tick(&mut self) -> Result<()> {
        // Game traffic DSCP marking (re-applied when gamescope's cgroup appears)
        self.game_priority.ensure();

        // Every 30 s, check power save against what we applied. Drivers and the network
        // backend can turn it back on (resume, reconnect, roam) without us noticing (issue #23).
        self.ticks += 1;
        if self.ticks.is_multiple_of(15) {
            self.verify_power_save();
        }

        // Run hot-plug interface check and optimize newly activated interfaces
        self.check_and_initialize_hotplug_interfaces();

        // Optimize TCP congestion control based on currently active connection media
        self.optimize_congestion_control_for_media();

        // 1. Sample CPU load
        let cpu_load = self.cpu_monitor.sample();
        debug!("Tick: CPU load {:.1}%", cpu_load * 100.0);

        // 2. Get wireless devices from NetworkManager
        let devices = self.nm_client.get_wireless_devices().await?;

        // Collect device info we need
        let device_infos: Vec<_> = devices
            .into_iter()
            .filter(|d| d.state == crate::network::nm::DeviceState::Activated)
            .map(|d| {
                (
                    d.interface.clone(),
                    d.path.clone(),
                    d.bitrate,
                    d.active_ap.clone(),
                )
            })
            .collect();

        // Ensure interface states exist
        for (interface, _, _, _) in &device_infos {
            if !self.interface_states.contains_key(interface) {
                let mut state = InterfaceState::new(&self.config);
                let _ = state.tc_manager.remove_cake(interface);
                self.interface_states.insert(interface.clone(), state);
            }
        }

        // Update scan suppression flag based on mode
        let suppress;
        let has_wifi_connection = !device_infos.is_empty();

        match self.config.scan_suppress {
            ScanSuppressMode::Off => {
                suppress = false;
            }
            ScanSuppressMode::On => {
                suppress = has_wifi_connection;
            }
            ScanSuppressMode::Adaptive => {
                if !has_wifi_connection {
                    suppress = false;
                } else {
                    let now = Instant::now();
                    let in_wake_grace_period = self
                        .last_resume_at
                        .map(|resume_time| now.duration_since(resume_time).as_secs() < 30)
                        .unwrap_or(false);

                    if in_wake_grace_period {
                        debug!("Adaptive Scan: Wake/startup grace period active, allowing background scans");
                        suppress = false;
                    } else {
                        let mut in_game_mode = false;
                        let mut weak_signal = false;

                        for (interface, _, _, active_ap) in &device_infos {
                            if let Some(state) = self.interface_states.get(interface) {
                                if state
                                    .game_mode_until
                                    .map(|until| now < until)
                                    .unwrap_or(false)
                                {
                                    in_game_mode = true;
                                }
                            }
                            if let Some(ap) = active_ap {
                                let limit = match ap.band {
                                    crate::network::nm::WifiBand::Band2_4GHz => {
                                        self.wifi_config.min_signal_2g_dbm
                                    }
                                    crate::network::nm::WifiBand::Band5GHz => {
                                        self.wifi_config.min_signal_5g_dbm
                                    }
                                    crate::network::nm::WifiBand::Band6GHz => {
                                        self.wifi_config.min_signal_6g_dbm
                                    }
                                    _ => -75,
                                };
                                if ap.signal_strength <= limit {
                                    weak_signal = true;
                                }
                            }
                        }

                        if in_game_mode {
                            debug!("Adaptive Scan: Game Mode active, suppressing scans");
                            suppress = true;
                        } else if weak_signal {
                            debug!(
                                "Adaptive Scan: Weak signal detected, allowing scans for roaming"
                            );
                            suppress = false;
                        } else {
                            debug!("Adaptive Scan: Strong signal and idle, suppressing scans");
                            suppress = true;
                        }
                    }
                }
            }
        }

        self.scan_suppress_active.store(suppress, Ordering::Relaxed);

        for (interface, path, bitrate, active_ap) in device_infos {
            info!(
                "Processing interface: {}, active_ap: {:?}, band_steering_enabled: {}",
                interface,
                active_ap.as_ref().map(|ap| &ap.bssid),
                self.config.band_steering_enabled
            );

            // Get or create interface state
            let ssid = active_ap.as_ref().map(|ap| ap.ssid.clone());
            self.interface_states
                .entry(interface.clone())
                .or_insert_with(|| InterfaceState::new(&self.config))
                .ssid = ssid;

            // 3. Game Mode Detection (PPS) - with CAKE freezing
            if self.config.game_mode_enabled {
                let pps_threshold = self.config.game_mode_pps_threshold;
                let cooldown_secs = self.config.game_mode_cooldown_secs;
                let freeze_cake = self.config.game_mode_freeze_cake;

                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let pps = state.pps_monitor.sample(&interface);
                    let was_in_game = state
                        .game_mode_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false);

                    if pps > pps_threshold {
                        let cooldown = Duration::from_secs(cooldown_secs);
                        state.game_mode_until = Some(Instant::now() + cooldown);

                        // Freeze CAKE when entering game mode
                        if freeze_cake && !was_in_game {
                            state.tc_manager.enter_game_mode();
                            info!(
                                "Game mode ACTIVATED: {} PPS on {} (CAKE frozen)",
                                pps, interface
                            );
                            if self.config.breathing_cake_enabled && self.autorate.is_none() {
                                let _ = state.tc_manager.apply_cake(&interface);
                            }
                        } else {
                            debug!("Game mode extended: {} PPS on {}", pps, interface);
                        }
                    } else if was_in_game {
                        // Check if cooldown expired
                        let still_in_game = state
                            .game_mode_until
                            .map(|until| Instant::now() < until)
                            .unwrap_or(false);

                        if !still_in_game && freeze_cake {
                            state.tc_manager.exit_game_mode();
                            info!("Game mode ENDED on {} (CAKE unfrozen)", interface);
                            if self.config.breathing_cake_enabled && self.autorate.is_none() {
                                let _ = state.tc_manager.remove_cake(&interface);
                            }
                        }
                    }

                }
            }

            // 4. Breathing CAKE (Dynamic QoS) with throughput monitoring.
            // With autorate this only tracks the link rate (autorate's upper bound); it never shapes.
            if self.config.breathing_cake_enabled || self.autorate.is_some() {
                // Get bitrate from BOTH sources and average for stability
                let nm_bitrate = bitrate; // Already in Kbit/s from NetworkManager
                let iw_bitrate = Self::get_bitrate_from_iw(&interface).unwrap_or(0);

                // Average both sources if both valid, otherwise use whichever is valid
                // Reject readings below 20Mbit (lowered for Steam Deck compatibility)
                // WiFi 4 HT20 MCS7 = 65Mbit, but some devices report lower during idle
                let min_valid_kbit = 20_000; // 20 Mbit minimum

                let nm_valid = nm_bitrate >= min_valid_kbit;
                let iw_valid = iw_bitrate >= min_valid_kbit;

                // Debug logging on first tick or when both invalid
                if !nm_valid && !iw_valid {
                    debug!(
                        "CAKE bitrate check on {}: NM={} Kbit (valid:{}), iw={} Kbit (valid:{})",
                        interface, nm_bitrate, nm_valid, iw_bitrate, iw_valid
                    );
                }

                let effective_bitrate = match (nm_valid, iw_valid) {
                    (true, true) => (nm_bitrate + iw_bitrate) / 2, // Average both
                    (true, false) => nm_bitrate,
                    (false, true) => iw_bitrate,
                    (false, false) => 0, // Both invalid - will use last known good
                };

                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let is_in_game = state
                        .game_mode_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false);
                    let should_apply_cake = (!self.config.game_mode_enabled || is_in_game)
                        && self.autorate.is_none();

                    // Update throughput estimate from actual traffic
                    Self::update_throughput_estimate(state, &interface);

                    let mut bitrate_to_use = None;

                    if effective_bitrate > 0 {
                        state.last_good_bitrate = Some(effective_bitrate);
                        bitrate_to_use = Some(effective_bitrate);
                    } else if let Some(last_good) = state.last_good_bitrate {
                        bitrate_to_use = Some(last_good);
                    }

                    if let Some(kbit) = bitrate_to_use {
                        let bitrate_mbit = kbit / 1000;

                        // Determine dynamic scaling factor based on band and RSSI (Strategy 4)
                        let (base_sf, min_ceil, max_ceil) = if let Some(ref ap) = active_ap {
                            match ap.frequency {
                                0..=2500 => (0.50, 10, 60),      // 2.4 GHz
                                2501..=5924 => (0.70, 50, 500),  // 5 GHz
                                _ => (0.85, 100, 1200),          // 6 GHz
                            }
                        } else {
                            (0.70, 50, 400) // Fallback
                        };

                        let rssi = active_ap.as_ref().map(|ap| ap.signal_strength).unwrap_or(-60);
                        let rssi_multiplier = if rssi >= -60 {
                            1.0
                        } else if rssi >= -71 {
                            0.85
                        } else {
                            0.65
                        };

                        let sf_adjusted = base_sf * rssi_multiplier;
                        
                        // Calculate global ceiling
                        let calculated_mbit = ((bitrate_mbit as f64) * sf_adjusted).round() as u32;
                        let calculated_mbit = calculated_mbit.clamp(min_ceil, max_ceil);

                        debug!(
                            "CAKE: NM={}Kbit, iw={}Kbit, base_kbit={}Kbit, dynamic_scaled={}Mbit (sf_adj={:.3}, rssi={}dBm)",
                            nm_bitrate, iw_bitrate, kbit, calculated_mbit, sf_adjusted, rssi
                        );

                        if state.tc_manager.update_bandwidth(calculated_mbit)
                            && should_apply_cake {
                                let _ = state.tc_manager.apply_cake(&interface);
                            }
                        state.bandwidth_valid = true;
                    } else {
                        // No current OR historical valid bitrate
                        // Use a conservative default of 100Mbit (safe for most WiFi 5/6 networks)
                        let default_mbit = 100;
                        let calculated_mbit = (default_mbit as f64 * self.config.cake_overhead_factor) as u32;

                        if !state.bandwidth_valid {
                            info!("CAKE: No bitrate detected (NM={}, iw={}), using conservative default {}Mbit on {}",
                                  nm_bitrate, iw_bitrate, default_mbit, interface);
                        }

                        if state.tc_manager.update_bandwidth(calculated_mbit)
                            && should_apply_cake {
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
                    let in_game = state
                        .game_mode_until
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
                                debug!(
                                    "Coalescing ENABLED on {} (game:{}, cpu:{:.0}%, battery:{})",
                                    interface,
                                    in_game,
                                    cpu_load * 100.0,
                                    on_battery
                                );
                            } else {
                                let _ = EthtoolManager::disable_coalescing(&interface);
                                debug!(
                                    "Coalescing DISABLED on {} (game:{}, cpu:{:.0}%)",
                                    interface,
                                    in_game,
                                    cpu_load * 100.0
                                );
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
            let battery_pct = self.power_manager.battery_percentage();
            let is_critical_battery = self.power_config.critical_battery_pct > 0
                && battery_pct
                    .map(|pct| pct <= self.power_config.critical_battery_pct)
                    .unwrap_or(false);

            {
                let power_mode = self.power_config.wlan_power_save.as_str();

                match power_mode {
                    "off" => {
                        // User wants power save permanently off — never call enable_power_save
                        if let Some(state) = self.interface_states.get_mut(&interface) {
                            if state.power_save_enabled != Some(false) {
                                let wifi_interfaces = self.wifi_manager.interfaces();
                                if let Some(wifi_ifc) =
                                    wifi_interfaces.iter().find(|i| i.name == interface)
                                {
                                    if self.wifi_manager.disable_power_save(wifi_ifc).is_ok() {
                                        info!(
                                            "Power save forced OFF on {} (config override)",
                                            interface
                                        );
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
                                if let Some(wifi_ifc) =
                                    wifi_interfaces.iter().find(|i| i.name == interface)
                                {
                                    if self.wifi_manager.enable_power_save(wifi_ifc).is_ok() {
                                        info!(
                                            "Power save forced ON on {} (config override)",
                                            interface
                                        );
                                        state.power_save_enabled = Some(true);
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        // "adaptive" — original hysteresis logic, with critical battery override
                        let base_should_enable = self.power_manager.should_enable_power_save();

                        if let Some(state) = self.interface_states.get_mut(&interface) {
                            let pps = state.pps_monitor.sample(&interface);
                            let has_network_activity = pps > 50;

                            let in_game = state
                                .game_mode_until
                                .map(|until| Instant::now() < until)
                                .unwrap_or(false);

                            // Disable power save if:
                            // 1. On AC power, OR
                            // 2. Game mode active, OR
                            // 3. Any significant network activity (>50 PPS)
                            // EXCEPT if we are in a critical battery state (where we force it ON)
                            let should_enable = if is_critical_battery {
                                true
                            } else {
                                base_should_enable && !in_game && !has_network_activity
                            };

                            // Hysteresis: require stable ticks before changing power save
                            // Entering performance mode (power-save OFF) is instant (1 tick) to eliminate latency immediately.
                            // Entering power-save mode (power-save ON) requires 5 stable ticks (10s) to survive game loading screens.
                            if state.power_save_enabled != Some(should_enable) {
                                if state.pending_power_save == Some(should_enable) {
                                    state.power_save_stable_ticks += 1;
                                } else {
                                    state.pending_power_save = Some(should_enable);
                                    state.power_save_stable_ticks = 1;
                                }

                                let target_ticks = if should_enable { 5 } else { 1 };
                                if state.power_save_stable_ticks >= target_ticks {
                                    let wifi_interfaces = self.wifi_manager.interfaces();
                                    if let Some(wifi_ifc) =
                                        wifi_interfaces.iter().find(|i| i.name == interface)
                                    {
                                        if should_enable {
                                            if self.wifi_manager.enable_power_save(wifi_ifc).is_ok() {
                                                let reason = if is_critical_battery {
                                                    format!(
                                                        "critical battery {}%",
                                                        battery_pct.unwrap_or(0)
                                                    )
                                                } else {
                                                    "battery, idle".to_string()
                                                };
                                                info!(
                                                    "Power save ENABLED on {} ({})",
                                                    interface, reason
                                                );
                                                state.power_save_enabled = Some(true);
                                            }
                                        } else {
                                            if self.wifi_manager.disable_power_save(wifi_ifc).is_ok() {
                                                let reason = if !base_should_enable {
                                                    "AC power"
                                                } else if in_game {
                                                    "game mode"
                                                } else {
                                                    "network activity"
                                                };
                                                info!(
                                                    "Power save DISABLED on {} ({})",
                                                    interface, reason
                                                );
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

                            let in_game = state
                                .game_mode_until
                                .map(|until| Instant::now() < until)
                                .unwrap_or(false);

                            // Enable EEE only on battery AND idle (no game, no network activity)
                            // EXCEPT if we are in a critical battery state (where we force it ON)
                            let should_enable = if is_critical_battery {
                                true
                            } else {
                                base_should_enable && !in_game && !has_network_activity
                            };

                            // Hysteresis: require stable ticks before changing EEE
                            // Entering performance mode (EEE OFF) is instant (1 tick) to prevent wakeup latency.
                            // Entering power-saving mode (EEE ON) requires 5 stable ticks (10s) to survive load screens.
                            if state.eee_enabled != Some(should_enable) {
                                if state.pending_eee == Some(should_enable) {
                                    state.eee_stable_ticks += 1;
                                } else {
                                    state.pending_eee = Some(should_enable);
                                    state.eee_stable_ticks = 1;
                                }

                                let target_ticks = if should_enable { 5 } else { 1 };
                                if state.eee_stable_ticks >= target_ticks {
                                    if should_enable {
                                        if EthtoolManager::enable_eee(&interface).is_ok() {
                                            let reason = if is_critical_battery {
                                                format!(
                                                    "critical battery {}%",
                                                    battery_pct.unwrap_or(0)
                                                )
                                            } else {
                                                "battery, idle".to_string()
                                            };
                                            info!("EEE ENABLED on {} ({})", interface, reason);
                                            state.eee_enabled = Some(true);
                                        }
                                    } else {
                                        if EthtoolManager::disable_eee(&interface).is_ok() {
                                            let reason = if !base_should_enable {
                                                "AC power"
                                            } else if in_game {
                                                "game mode"
                                            } else {
                                                "network activity"
                                            };
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

            // 5d. PCIe ASPM & Runtime PM Management - Dynamic based on activity/power source
            if self.power_config.dynamic_aspm {
                let base_should_enable = self.power_manager.should_enable_power_save();

                if let Some(state) = self.interface_states.get_mut(&interface) {
                    let in_game = state
                        .game_mode_until
                        .map(|until| Instant::now() < until)
                        .unwrap_or(false);

                    // ASPM Performance mode is enabled if:
                    // 1. On AC power (base_should_enable is false), OR
                    // 2. In game mode.
                    // EXCEPT if we are in a critical battery state.
                    let should_aspm_performance =
                        !is_critical_battery && (!base_should_enable || in_game);

                    if state.aspm_performance != Some(should_aspm_performance) {
                        if state.pending_aspm == Some(should_aspm_performance) {
                            state.aspm_stable_ticks += 1;
                        } else {
                            state.pending_aspm = Some(should_aspm_performance);
                            state.aspm_stable_ticks = 1;
                        }

                        // Entering performance mode (ASPM OFF) is instant (1 tick) to restore performance.
                        // Restoring ASPM power-save (ASPM ON) requires 5 stable ticks (10s) to survive load screens.
                        let target_ticks = if should_aspm_performance { 1 } else { 5 };
                        if state.aspm_stable_ticks >= target_ticks {
                            let _ =
                                crate::system::optimizer::SystemOptimizer::apply_pcie_aspm_sysfs(
                                    &interface,
                                    should_aspm_performance,
                                );
                            let reason = if is_critical_battery {
                                format!("critical battery {}%", battery_pct.unwrap_or(0))
                            } else if should_aspm_performance {
                                if in_game {
                                    "game mode".to_string()
                                } else {
                                    "AC power".to_string()
                                }
                            } else {
                                "battery, idle".to_string()
                            };
                            info!(
                                "PCIe ASPM state changed on {} to performance={} (reason: {})",
                                interface, should_aspm_performance, reason
                            );
                            state.aspm_performance = Some(should_aspm_performance);
                            state.pending_aspm = None;
                            state.aspm_stable_ticks = 0;
                        }
                    } else {
                        state.pending_aspm = None;
                        state.aspm_stable_ticks = 0;
                    }
                }
            }

            // 6. Smart Band Steering
            // Skip when scan suppress is active — EXCEPT when signal is critically weak (disconnect imminent)
            let is_critical_signal = if let Some(current_ap) = &active_ap {
                current_ap.signal_strength <= -80
            } else {
                false
            };

            if self.config.band_steering_enabled
                && (!self.scan_suppress_active.load(Ordering::Relaxed) || is_critical_signal)
            {
                if let Some(current_ap) = &active_ap {
                    let hysteresis_ticks = self.config.roam_hysteresis_ticks;

                    info!(
                        "Band steering: Checking for better AP (current: {} on {:?}, score: {})",
                        current_ap.bssid,
                        current_ap.band,
                        current_ap.score(
                            self.wifi_config.band_bias_5ghz,
                            self.wifi_config.band_bias_6ghz
                        )
                    );

                    // Get all visible APs
                    match self.nm_client.get_access_points(&path).await {
                        Ok(access_points) => {
                            info!(
                                "Band steering: Found {} visible APs (current SSID: '{}')",
                                access_points.len(),
                                current_ap.ssid
                            );

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
                            info!(
                                "Band steering: About to list {} APs...",
                                access_points.len()
                            );
                            for (i, ap) in access_points.iter().enumerate() {
                                info!(
                                    "  [{}] AP: {} ({}), band={:?}, signal={}dBm, rate={}Mbps",
                                    i,
                                    ap.bssid,
                                    ap.ssid,
                                    ap.band,
                                    ap.signal_strength,
                                    ap.max_bitrate / 1000
                                );
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
                                        let should_trigger =
                                            if let Some(ref mut roam) = state.roam_candidate {
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
                                            info!("Proactive Roaming Governor: {} -> {} (score: {} -> {}, band: {:?} -> {:?})",
                                                  current_ap.bssid, best_candidate.bssid,
                                                  current_score, candidate_score,
                                                  current_ap.band, best_candidate.band);

                                            // Clear cached bitrate - after roaming it will be stale
                                            state.last_good_bitrate = None;
                                            state.bandwidth_valid = false;

                                            // Active connection handover
                                            if let Err(e) = self
                                                .nm_client
                                                .active_roam(&path, &best_candidate.path)
                                                .await
                                            {
                                                warn!("Proactive handover to AP {} failed: {}. Falling back to scan hinting.", best_candidate.bssid, e);
                                                // Fallback: Request scan to hint firmware/driver about better AP
                                                let _ = self.nm_client.request_scan(&path).await;
                                            }
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

        self.update_autorate_target();

        Ok(())
    }

    /// Point autorate at the interface carrying the default route
    fn update_autorate_target(&self) {
        let Some(handle) = &self.autorate else {
            return;
        };
        let managed = default_route_iface().and_then(|name| {
            self.wifi_manager
                .interfaces()
                .iter()
                // A default route through it already means it is up; no `iw` call needed
                .find(|ifc| ifc.name == name)
        });
        let target = match managed {
            None => autorate::Target::default(),
            Some(ifc) if ifc.interface_type == crate::network::wifi::InterfaceType::Wifi => {
                let state = self.interface_states.get(&ifc.name);
                autorate::Target {
                    iface: Some(ifc.name.clone()),
                    network_key: state.and_then(|st| st.ssid.clone()),
                    link_kbit: state.and_then(|st| st.last_good_bitrate),
                }
            }
            Some(ifc) => autorate::Target {
                iface: Some(ifc.name.clone()),
                network_key: Some(format!("wired:{}", ifc.name)),
                link_kbit: std::fs::read_to_string(format!("/sys/class/net/{}/speed", ifc.name))
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .filter(|mbit| *mbit > 0 && *mbit < 1_000_000)
                    .map(|mbit| mbit * 1000),
            },
        };
        handle.set_target(target);
    }

    pub async fn stop(&mut self) {
        info!("Governor stopping, cleaning up...");

        if let Some(handle) = self.autorate.take() {
            handle.shutdown().await;
        }

        for (interface, state) in &mut self.interface_states {
            let _ = state.tc_manager.remove_cake(interface);
        }

        // Clean up DSCP tagging
        self.game_priority.remove();
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
        if !crate::network::tc::is_tc_available() {
            return true; // Pretend it has cake so we don't try to apply it
        }
        let output = Command::new(crate::network::tc::tc_bin())
            .args(["qdisc", "show", "dev", interface])
            .output();

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return stdout.contains("cake");
        }

        false
    }

    /// Calculate CAKE bandwidth from link stats (fallback to 200Mbit)
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
///
/// Uses tokio::process::Command for non-blocking subprocess execution to avoid
/// blocking the async runtime and causing micro-stuttering during streaming.
async fn scan_abort_task(active: Arc<AtomicBool>) {
    // Cache the interface list to avoid reading /sys every tick
    // Refresh every 10 ticks (5 seconds) to pick up hotplug changes
    let mut cached_interfaces: Vec<String> = Vec::new();
    let mut cache_refresh_counter = 0u32;

    let mut interval = time::interval(Duration::from_millis(500));

    loop {
        interval.tick().await;

        if !active.load(Ordering::Relaxed) {
            continue;
        }

        // Refresh interface cache every 10 ticks (5 seconds)
        cache_refresh_counter += 1;
        if cache_refresh_counter >= 10 || cached_interfaces.is_empty() {
            cached_interfaces = find_wifi_interfaces();
            cache_refresh_counter = 0;
        }

        // Abort scans on all connected WiFi interfaces using async subprocess
        for ifc in &cached_interfaces {
            let _ = TokioCommand::new("iw")
                .args(["dev", ifc, "scan", "abort"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await;
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

/// Interface of the IPv4 default route with the lowest metric (from /proc/net/route)
pub fn default_route_iface() -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_default_route(&table)
}

fn parse_default_route(table: &str) -> Option<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask ...
            if f.len() > 7 && f[1] == "00000000" && f[7] == "00000000" {
                Some((f[6].parse::<u32>().unwrap_or(u32::MAX), f[0].to_string()))
            } else {
                None
            }
        })
        .min()
        .map(|(_, iface)| iface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_route_picks_lowest_metric() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            enp3s0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_default_route(table), Some("enp3s0".to_string()));
        assert_eq!(parse_default_route("Iface\tDestination\n"), None);
    }

    #[test]
    fn test_optimize_congestion_control_for_media_logic() {
        // Verify media prioritization: BBR by default, Cubic only when hard-clamped shaper is saturated
        fn determine_target(
            has_cake: bool,
            last_bw_mbit: u32,
            link_speed_mbit: u32,
            current_throughput_mbit: u32,
        ) -> &'static str {
            let is_hard_clamped = last_bw_mbit < (link_speed_mbit as f64 * 0.90) as u32;
            let is_saturated = current_throughput_mbit >= (last_bw_mbit as f64 * 0.80) as u32;

            if has_cake && is_hard_clamped && is_saturated {
                "cubic"
            } else {
                "bbr"
            }
        }

        // Test Case 1: No shaper (or no cake) -> Default to BBR
        assert_eq!(determine_target(false, 0, 1000, 500), "bbr");

        // Test Case 2: Shaper is not hard-clamped (e.g. 1500Mbit shaper on 1000Mbit link) -> BBR
        assert_eq!(determine_target(true, 1500, 1000, 950), "bbr");

        // Test Case 3: Hard-clamped shaper but NOT saturated (e.g. 100Mbit shaper, 10Mbit throughput on 1000Mbit link) -> BBR
        assert_eq!(determine_target(true, 100, 1000, 10), "bbr");

        // Test Case 4: Hard-clamped shaper AND saturated (e.g. 100Mbit shaper, 85Mbit throughput on 1000Mbit link) -> Cubic
        assert_eq!(determine_target(true, 100, 1000, 85), "cubic");
    }
}
