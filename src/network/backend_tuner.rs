//! Backend tuner for iwd and wpa_supplicant
//!
//! Applies optimizations specific to the active Wi-Fi backend.

use anyhow::{Context, Result};
use log::{info, debug, warn};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Detected Wi-Fi backend
#[derive(Debug, Clone, PartialEq)]
pub enum WifiBackend {
    Iwd,
    WpaSupplicant,
    Unknown,
}

/// Tunes the active Wi-Fi backend for optimal performance
pub struct BackendTuner {
    backend: WifiBackend,
    disable_periodic_scan: bool,
}

impl BackendTuner {
    pub fn new(disable_periodic_scan: bool) -> Self {
        let backend = Self::detect_backend();
        info!("Detected Wi-Fi backend: {:?}", backend);
        
        Self {
            backend,
            disable_periodic_scan,
        }
    }

    /// Detect the active Wi-Fi backend
    fn detect_backend() -> WifiBackend {
        // Check if iwd is running
        if Self::is_process_running("iwd") {
            return WifiBackend::Iwd;
        }

        // Check NetworkManager config for iwd backend
        if let Ok(entries) = fs::read_dir("/etc/NetworkManager/conf.d") {
            for entry in entries.flatten() {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if content.contains("wifi.backend=iwd") {
                        return WifiBackend::Iwd;
                    }
                }
            }
        }

        // Check main NM config
        if let Ok(content) = fs::read_to_string("/etc/NetworkManager/NetworkManager.conf") {
            if content.contains("wifi.backend=iwd") {
                return WifiBackend::Iwd;
            }
        }

        // Check if wpa_supplicant is running
        if Self::is_process_running("wpa_supplicant") {
            return WifiBackend::WpaSupplicant;
        }

        WifiBackend::Unknown
    }

    /// Check if a process is running
    fn is_process_running(name: &str) -> bool {
        Command::new("pgrep")
            .arg("-x")
            .arg(name)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Get the detected backend
    pub fn backend(&self) -> &WifiBackend {
        &self.backend
    }

    /// Apply backend-specific optimizations
    pub fn apply(&self) -> Result<()> {
        match self.backend {
            WifiBackend::Iwd => self.tune_iwd(),
            WifiBackend::WpaSupplicant => self.tune_wpa_supplicant(),
            WifiBackend::Unknown => {
                debug!("Unknown backend, skipping tuning");
                Ok(())
            }
        }
    }

    /// Apply iwd-specific optimizations
    fn tune_iwd(&self) -> Result<()> {
        info!("Applying iwd optimizations...");

        let iwd_conf_dir = Path::new("/etc/iwd");
        let iwd_conf_path = iwd_conf_dir.join("main.conf");

        // Don't overwrite existing config
        if iwd_conf_path.exists() {
            info!("Existing /etc/iwd/main.conf found, checking for updates...");
            return self.update_iwd_config(&iwd_conf_path);
        }

        // Create directory
        fs::create_dir_all(iwd_conf_dir)
            .context("Failed to create /etc/iwd directory")?;

        let config = format!(r#"[General]
# Use control port over nl80211 for better performance
ControlPortOverNL80211=true

# Roaming thresholds (dBm)
RoamThreshold=-75
RoamThreshold5G=-80

# Randomize MAC per network for privacy
AddressRandomization=network

# Enable Management Frame Protection
ManagementFrameProtection=1

[Scan]
# Disable periodic scanning when connected (reduces latency spikes)
DisablePeriodicScan={}

[Rank]
# Prefer 5GHz and 6GHz bands
BandModifier2_4GHz=1.0
BandModifier5GHz=2.0
BandModifier6GHz=3.0
"#, self.disable_periodic_scan);

        let mut file = File::create(&iwd_conf_path)
            .context("Failed to create iwd config")?;
        file.write_all(config.as_bytes())?;

        info!("Created optimized /etc/iwd/main.conf");

        // Restart iwd to apply changes
        let _ = Command::new("systemctl")
            .args(["restart", "iwd.service"])
            .output();

        Ok(())
    }

    /// Update existing iwd config without full overwrite
    fn update_iwd_config(&self, path: &Path) -> Result<()> {
        let content = fs::read_to_string(path)?;
        
        // Check if DisablePeriodicScan is already set
        if content.contains("DisablePeriodicScan") {
            debug!("DisablePeriodicScan already configured in iwd");
            return Ok(());
        }

        // Append Scan section if missing
        if !content.contains("[Scan]") && self.disable_periodic_scan {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(path)?;
            
            writeln!(file, "\n[Scan]")?;
            writeln!(file, "DisablePeriodicScan=true")?;
            
            info!("Added DisablePeriodicScan to existing iwd config");
        }

        Ok(())
    }

    /// Apply wpa_supplicant optimizations (minimal, as NM handles most)
    fn tune_wpa_supplicant(&self) -> Result<()> {
        info!("wpa_supplicant backend detected - using NetworkManager defaults");
        // wpa_supplicant is typically managed by NetworkManager
        // Most optimizations are handled via nmcli connection settings
        Ok(())
    }

    /// Revert backend tuning
    pub fn revert(&self) -> Result<()> {
        info!("Reverting backend tuning...");

        // Only remove config files we created (check for our marker comment)
        let iwd_conf = Path::new("/etc/iwd/main.conf");
        if iwd_conf.exists() {
            if let Ok(content) = fs::read_to_string(iwd_conf) {
                if content.contains("ControlPortOverNL80211") {
                    warn!("Not removing /etc/iwd/main.conf - may contain user customizations");
                }
            }
        }

        Ok(())
    }
}

impl Default for BackendTuner {
    fn default() -> Self {
        Self::new(true)
    }
}
