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
    #[serde(default)]
    pub autorate: AutorateConfig,
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
            autorate: AutorateConfig::default(),
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
#[serde(default)]
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
    /// Custom MAC address or "permanent" to disable randomization
    #[serde(default)]
    pub wifi_mac_address: Option<String>,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Per-band thresholds: 5GHz/6GHz need stronger signals due to path loss
            min_signal_2g_dbm: -75,
            min_signal_5g_dbm: -72, // 5GHz: slightly stricter
            min_signal_6g_dbm: -70, // 6GHz: even stricter (higher path loss)
            band_bias_5ghz: 15,     // Per rewrite.md
            band_bias_6ghz: 25, // Higher than 5GHz - 6GHz has less interference, better for gaming
            wifi_mac_address: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PowerConfig {
    #[allow(dead_code)]
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_adaptive")]
    pub wlan_power_save: String, // "on", "off", "adaptive"
    #[serde(default = "default_critical_battery")]
    pub critical_battery_pct: u32,
    #[serde(default = "default_dynamic_aspm")]
    pub dynamic_aspm: bool,
}

fn default_true() -> bool {
    true
}
fn default_adaptive() -> String {
    "adaptive".to_string()
}
fn default_critical_battery() -> u32 {
    15
}
fn default_dynamic_aspm() -> bool {
    true
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wlan_power_save: "adaptive".to_string(),
            critical_battery_pct: 15,
            dynamic_aspm: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    pub sysctl_enabled: bool,
    pub irq_affinity_enabled: bool,
    pub driver_tweaks_enabled: bool,
    /// Custom system hostname to set at startup
    #[serde(default)]
    pub hostname: Option<String>,
    /// TCP congestion control algorithm (e.g., "bbr", "cubic")
    #[serde(default = "default_tcp_congestion")]
    pub tcp_congestion_control: String,
}

fn default_tcp_congestion() -> String {
    "bbr".to_string()
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            sysctl_enabled: true,
            irq_affinity_enabled: true,
            driver_tweaks_enabled: true,
            hostname: None,
            tcp_congestion_control: default_tcp_congestion(),
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
#[serde(default)]
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

    /// Mark game / game-streaming UDP as DSCP EF (Wi-Fi voice/video queue, CAKE priority tin)
    pub game_priority_enabled: bool,
    /// UDP port ranges treated as game traffic (source or destination)
    pub game_priority_udp_ports: Vec<String>,

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

    /// Suppress background scans to eliminate latency spikes
    /// - On: Always suppress scans when connected
    /// - Off: Never suppress scans (roaming active)
    /// - Adaptive: Suppress when gaming or signal is good; allow when signal is weak or during wake/boot grace periods.
    #[serde(default = "default_scan_suppress")]
    pub scan_suppress: ScanSuppressMode,

    /// Enable virtual IFB redirection for ingress (download) shaping
    pub qos_use_ifb: bool,
    /// Configured internet download limit in Mbit/s (optional)
    pub internet_download_mbit: Option<u32>,
    /// Configured internet upload limit in Mbit/s (optional)
    pub internet_upload_mbit: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanSuppressMode {
    On,
    Off,
    Adaptive,
}

impl<'de> Deserialize<'de> for ScanSuppressMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ScanSuppressVisitor;

        impl<'de> serde::de::Visitor<'de> for ScanSuppressVisitor {
            type Value = ScanSuppressMode;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a boolean or a string ('on', 'off', 'adaptive')")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v {
                    Ok(ScanSuppressMode::On)
                } else {
                    Ok(ScanSuppressMode::Off)
                }
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match v.to_lowercase().as_str() {
                    "on" | "true" | "yes" => Ok(ScanSuppressMode::On),
                    "off" | "false" | "no" => Ok(ScanSuppressMode::Off),
                    "adaptive" => Ok(ScanSuppressMode::Adaptive),
                    _ => Err(serde::de::Error::custom(format!(
                        "invalid scan suppress mode: {}",
                        v
                    ))),
                }
            }
        }

        deserializer.deserialize_any(ScanSuppressVisitor)
    }
}

fn default_scan_suppress() -> ScanSuppressMode {
    ScanSuppressMode::Adaptive
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            breathing_cake_enabled: false,
            cake_median_window: 3, // 3 samples = 6 seconds (reduced from 5)
            cake_change_threshold_mbit: 15, // Reduced from 25 for better responsiveness
            cake_change_threshold_pct: 0.15, // Reduced from 20% to 15%
            cake_overhead_factor: 0.85, // 85% of link bandwidth
            cake_hysteresis_up: 3, // 3 ticks (6 sec) for increases
            cake_hysteresis_down: 1, // 1 tick (2 sec) for decreases - FAST

            game_mode_enabled: true,
            game_mode_pps_threshold: 200,
            game_mode_cooldown_secs: 30,
            game_mode_freeze_cake: true, // NEW: Freeze CAKE during gaming

            game_priority_enabled: true,
            // Steam (game servers, Remote Play, SDR) and Moonlight/Sunshine
            game_priority_udp_ports: vec!["27000-27100".into(), "47998-48010".into()],

            band_steering_enabled: true,
            roam_hysteresis_ticks: 3,

            cpu_coalescing_enabled: true,
            cpu_coalescing_threshold: 0.90,

            cpu_avg_window_size: 3,

            scan_suppress: ScanSuppressMode::Adaptive,
            qos_use_ifb: true,
            internet_download_mbit: None,
            internet_upload_mbit: None,

        }
    }
}
/// When autorate shapes the connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutorateMode {
    /// Shape only while the link carries real traffic (default; no cost when idle)
    Busy,
    /// Keep the shaper installed whenever connected
    Always,
    /// Disable autorate (falls back to breathing CAKE settings)
    Off,
}

/// Latency-driven bufferbloat control (cake-autorate style)
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AutorateConfig {
    pub mode: AutorateMode,
    /// ICMP reflectors (IP literals). At least half of the responsive ones must agree on bloat.
    pub reflectors: Vec<String>,
    /// Interval between pings to each reflector while shaping
    pub ping_interval_ms: u64,
    /// Interval while connected but idle (keeps baselines clean). 0 disables idle probing.
    pub idle_ping_interval_ms: u64,
    /// Smoothed RTT increase over baseline that counts as bufferbloat
    pub delay_threshold_ms: f64,
    /// Lower bounds for the shaper, so a bad measurement can never starve the link
    pub min_download_mbit: f64,
    pub min_upload_mbit: f64,
    /// Upper bounds. Default: governor.internet_*_mbit if set, else the link rate.
    pub max_download_mbit: Option<f64>,
    pub max_upload_mbit: Option<f64>,
    /// Remember learned rates per Wi-Fi network across restarts
    pub remember_rates: bool,
}

impl Default for AutorateConfig {
    fn default() -> Self {
        Self {
            mode: AutorateMode::Busy,
            reflectors: vec!["1.1.1.1".into(), "8.8.8.8".into(), "9.9.9.9".into()],
            ping_interval_ms: 300,
            idle_ping_interval_ms: 2000,
            delay_threshold_ms: 15.0,
            min_download_mbit: 5.0,
            min_upload_mbit: 1.0,
            max_download_mbit: None,
            max_upload_mbit: None,
            remember_rates: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        scan_suppress: ScanSuppressMode,
    }

    #[test]
    fn test_scan_suppress_deserialization() {
        // Test booleans (backward compatibility)
        let c: TestConfig = toml::from_str("scan_suppress = true").unwrap();
        assert_eq!(c.scan_suppress, ScanSuppressMode::On);

        let c: TestConfig = toml::from_str("scan_suppress = false").unwrap();
        assert_eq!(c.scan_suppress, ScanSuppressMode::Off);

        // Test strings
        let c: TestConfig = toml::from_str("scan_suppress = \"on\"").unwrap();
        assert_eq!(c.scan_suppress, ScanSuppressMode::On);

        let c: TestConfig = toml::from_str("scan_suppress = \"off\"").unwrap();
        assert_eq!(c.scan_suppress, ScanSuppressMode::Off);

        let c: TestConfig = toml::from_str("scan_suppress = \"adaptive\"").unwrap();
        assert_eq!(c.scan_suppress, ScanSuppressMode::Adaptive);
    }

    #[test]
    fn test_backward_compatible_deserialization() {
        let toml_str = r#"
            [governor]
            breathing_cake_enabled = true
        "#;
        let c: Config = toml::from_str(toml_str).unwrap();
        assert!(c.governor.breathing_cake_enabled);
        // Removed v3.1.0-beta.1 keys must not break parsing of old config files
        let old = "[governor]\nebpf_bypass_enabled = true\n[multipath]\nenabled = true\n";
        let c: Config = toml::from_str(old).unwrap();
        assert_eq!(c.autorate.mode, AutorateMode::Busy);
    }

    #[test]
    fn test_autorate_config() {
        let c: Config = toml::from_str("[autorate]\nmode = \"always\"\nreflectors = [\"1.0.0.1\"]\nmax_download_mbit = 300.0\n").unwrap();
        assert_eq!(c.autorate.mode, AutorateMode::Always);
        assert_eq!(c.autorate.reflectors, vec!["1.0.0.1".to_string()]);
        assert_eq!(c.autorate.max_download_mbit, Some(300.0));
        assert_eq!(c.autorate.ping_interval_ms, 300);
    }
}
