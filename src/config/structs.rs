use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub global: GlobalConfig,
    #[serde(default)]
    pub wifi: WifiConfig,
    #[serde(default)]
    pub power: PowerConfig,
    #[serde(default)]
    pub system: SystemConfig,
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub governor: GovernorConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            global: GlobalConfig::default(),
            wifi: WifiConfig::default(),
            power: PowerConfig::default(),
            system: SystemConfig::default(),
            backend: BackendConfig::default(),
            governor: GovernorConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct GlobalConfig {
    /// Tick rate for the governor loop in seconds
    pub tick_rate_secs: u64,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            tick_rate_secs: 2, // Per rewrite.md: 2 second tick rate
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WifiConfig {
    #[allow(dead_code)]
    pub enabled: bool,
    /// Minimum signal for 2.4GHz (more tolerant)
    pub min_signal_2g_dbm: i32,
    /// Minimum signal for 5GHz (needs stronger signal)
    pub min_signal_5g_dbm: i32,
    /// Minimum signal for 6GHz (needs even stronger due to higher path loss)
    pub min_signal_6g_dbm: i32,
    /// Band bias for scoring (5GHz gets +15 - prefers 5GHz even with 15dB weaker signal)
    pub band_bias_5ghz: i32,
    /// Band bias for 6GHz (gets +25 - less interference, 160MHz channels, ideal for gaming)
    pub band_bias_6ghz: i32,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Per-band thresholds: 5GHz/6GHz need stronger signals due to path loss
            min_signal_2g_dbm: -75,
            min_signal_5g_dbm: -72,  // 5GHz: slightly stricter
            min_signal_6g_dbm: -70,  // 6GHz: even stricter (higher path loss)
            band_bias_5ghz: 15,  // Per rewrite.md
            band_bias_6ghz: 25,  // Higher than 5GHz - 6GHz has less interference, better for gaming
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PowerConfig {
    #[allow(dead_code)]
    pub enabled: bool,
    pub wlan_power_save: String, // "on", "off", "adaptive"
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wlan_power_save: "adaptive".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SystemConfig {
    pub sysctl_enabled: bool,
    pub irq_affinity_enabled: bool,
    pub driver_tweaks_enabled: bool,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            sysctl_enabled: true,
            irq_affinity_enabled: true,
            driver_tweaks_enabled: true,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BackendConfig {
    pub iwd_periodic_scan_disable: bool,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            iwd_periodic_scan_disable: true,
        }
    }
}

/// Governor-specific settings (the "brain" of hifi-wifi)
#[derive(Debug, Clone, Deserialize)]
pub struct GovernorConfig {
    /// Enable dynamic CAKE bandwidth adjustment
    pub breathing_cake_enabled: bool,
    /// Median filter window size (samples)
    pub cake_median_window: usize,
    /// Minimum bandwidth change to trigger CAKE update (Mbit)
    pub cake_change_threshold_mbit: u32,
    /// Minimum percentage change to trigger CAKE update
    pub cake_change_threshold_pct: f64,
    /// Overhead factor for CAKE bandwidth (0.0-1.0, default 0.85)
    pub cake_overhead_factor: f64,
    /// Hysteresis ticks for bandwidth INCREASES (slow, prevents oscillation)
    pub cake_hysteresis_up: u32,
    /// Hysteresis ticks for bandwidth DECREASES (fast, prevents bufferbloat)
    pub cake_hysteresis_down: u32,
    
    /// Enable game mode detection via PPS
    pub game_mode_enabled: bool,
    /// PPS threshold to trigger game mode
    pub game_mode_pps_threshold: u64,
    /// Game mode cooldown in seconds
    pub game_mode_cooldown_secs: u64,
    /// Freeze CAKE during game mode (prevents mid-game jitter)
    pub game_mode_freeze_cake: bool,
    
    /// Enable smart band steering
    pub band_steering_enabled: bool,
    /// Hysteresis ticks before roaming (consecutive ticks required)
    pub roam_hysteresis_ticks: u32,
    
    /// Enable CPU-based interrupt coalescing
    pub cpu_coalescing_enabled: bool,
    /// CPU load threshold for coalescing (0.0-1.0)
    pub cpu_coalescing_threshold: f64,
    
    /// Rolling average window size for CPU monitoring
    pub cpu_avg_window_size: usize,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            breathing_cake_enabled: true,
            cake_median_window: 3,             // 3 samples = 6 seconds (reduced from 5)
            cake_change_threshold_mbit: 15,    // Reduced from 25 for better responsiveness
            cake_change_threshold_pct: 0.15,   // Reduced from 20% to 15%
            cake_overhead_factor: 0.85,        // 85% of link bandwidth
            cake_hysteresis_up: 3,             // 3 ticks (6 sec) for increases
            cake_hysteresis_down: 1,           // 1 tick (2 sec) for decreases - FAST
            
            game_mode_enabled: true,
            game_mode_pps_threshold: 200,
            game_mode_cooldown_secs: 30,
            game_mode_freeze_cake: true,       // NEW: Freeze CAKE during gaming
            
            band_steering_enabled: true,
            roam_hysteresis_ticks: 3,
            
            cpu_coalescing_enabled: true,
            cpu_coalescing_threshold: 0.90,
            
            cpu_avg_window_size: 3,
        }
    }
}

