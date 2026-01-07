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
    pub min_signal_dbm: i32,
    /// Band bias for scoring (5GHz gets +15, 6GHz gets +20)
    pub band_bias_5ghz: i32,
    pub band_bias_6ghz: i32,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_signal_dbm: -75,
            band_bias_5ghz: 15,  // Per rewrite.md
            band_bias_6ghz: 20,  // Per rewrite.md
        }
    }
}

#[derive(Debug, Deserialize)]
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
    /// EMA alpha for bandwidth smoothing (0.0-1.0)
    pub cake_ema_alpha: f64,
    /// Minimum bandwidth change to trigger CAKE update (Mbit)
    pub cake_change_threshold_mbit: u32,
    /// Minimum percentage change to trigger CAKE update
    pub cake_change_threshold_pct: f64,
    /// Overhead factor for CAKE bandwidth (0.0-1.0, default 0.70)
    pub cake_overhead_factor: f64,
    
    /// Enable game mode detection via PPS
    pub game_mode_enabled: bool,
    /// PPS threshold to trigger game mode
    pub game_mode_pps_threshold: u64,
    /// Game mode cooldown in seconds
    pub game_mode_cooldown_secs: u64,
    
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
            cake_ema_alpha: 0.1,              // Reduced from 0.3 for smoother transitions
            cake_change_threshold_mbit: 25,   // Increased from 5 to prevent jitter
            cake_change_threshold_pct: 0.20,  // Increased from 10% to 20%
            cake_overhead_factor: 0.70,       // Conservative default for real throughput
            
            game_mode_enabled: true,
            game_mode_pps_threshold: 200,     // Per rewrite.md
            game_mode_cooldown_secs: 30,      // Per rewrite.md
            
            band_steering_enabled: true,
            roam_hysteresis_ticks: 3,         // Per rewrite.md: 3 ticks (6 seconds)
            
            cpu_coalescing_enabled: true,
            cpu_coalescing_threshold: 0.90,   // Per rewrite.md: 90%
            
            cpu_avg_window_size: 3,           // Per rewrite.md
        }
    }
}

