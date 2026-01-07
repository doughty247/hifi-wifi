//! Wi-Fi interface management and optimization
//!
//! Handles detection, monitoring, and configuration of Wi-Fi interfaces.

use anyhow::{Context, Result};
use log::{info, warn, debug};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Detected driver category for applying specific optimizations
#[derive(Debug, Clone, PartialEq)]
pub enum DriverCategory {
    Rtw89,      // Realtek RTW89 (modern)
    Rtw88,      // Realtek RTW88
    RtlLegacy,  // Legacy Realtek
    MediaTek,   // MediaTek MT7921/MT76
    Intel,      // Intel iwlwifi
    Atheros,    // Qualcomm Atheros
    Broadcom,   // Broadcom
    Ralink,     // Ralink/MediaTek Legacy
    Marvell,    // Marvell
    Generic,    // Unknown - apply universal optimizations
}

/// Represents a detected Wi-Fi interface
#[derive(Debug, Clone)]
pub struct WifiInterface {
    pub name: String,
    pub driver: String,
    pub category: DriverCategory,
    #[allow(dead_code)]
    pub is_active: bool,
}

/// Manages Wi-Fi interfaces and applies optimizations
pub struct WifiManager {
    interfaces: Vec<WifiInterface>,
}

impl WifiManager {
    pub fn new() -> Result<Self> {
        let interfaces = Self::detect_interfaces()?;
        Ok(Self { interfaces })
    }

    /// Detect all Wi-Fi interfaces on the system
    fn detect_interfaces() -> Result<Vec<WifiInterface>> {
        let mut interfaces = Vec::new();
        
        // Read from /sys/class/net
        let net_path = Path::new("/sys/class/net");
        if !net_path.exists() {
            return Ok(interfaces);
        }

        for entry in fs::read_dir(net_path)? {
            let entry = entry?;
            let ifc_name = entry.file_name().to_string_lossy().to_string();
            
            // Check if it's a wireless interface
            if !ifc_name.starts_with("wl") {
                continue;
            }

            let driver = Self::detect_driver(&ifc_name);
            let category = Self::categorize_driver(&driver);
            let is_active = Self::is_interface_active(&ifc_name);

            info!("Detected interface: {} (driver: {}, category: {:?})", 
                  ifc_name, driver, category);

            interfaces.push(WifiInterface {
                name: ifc_name,
                driver,
                category,
                is_active,
            });
        }

        Ok(interfaces)
    }

    /// Detect the driver for a given interface
    fn detect_driver(ifc_name: &str) -> String {
        let driver_path = format!("/sys/class/net/{}/device/driver", ifc_name);
        
        if let Ok(link) = fs::read_link(&driver_path) {
            if let Some(driver_name) = link.file_name() {
                return driver_name.to_string_lossy().to_string();
            }
        }
        
        "unknown".to_string()
    }

    /// Categorize driver for optimization selection
    fn categorize_driver(driver: &str) -> DriverCategory {
        match driver {
            d if d.contains("rtw89") => DriverCategory::Rtw89,
            d if d.contains("rtw88") => DriverCategory::Rtw88,
            d if d.starts_with("rtl") => DriverCategory::RtlLegacy,
            d if d.starts_with("mt7") || d.contains("mt76") => DriverCategory::MediaTek,
            d if d.starts_with("iwl") => DriverCategory::Intel,
            d if d.starts_with("ath") => DriverCategory::Atheros,
            d if d.starts_with("brcm") || d == "wl" => DriverCategory::Broadcom,
            d if d.starts_with("rt2") || d.starts_with("rt5") => DriverCategory::Ralink,
            d if d.starts_with("mwifiex") || d.starts_with("mwl") => DriverCategory::Marvell,
            _ => DriverCategory::Generic,
        }
    }

    /// Check if interface is currently active (has carrier)
    fn is_interface_active(ifc_name: &str) -> bool {
        let carrier_path = format!("/sys/class/net/{}/carrier", ifc_name);
        fs::read_to_string(&carrier_path)
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    /// Get all detected interfaces
    pub fn interfaces(&self) -> &[WifiInterface] {
        &self.interfaces
    }

    /// Disable power saving on an interface using `iw`
    pub fn disable_power_save(&self, ifc: &WifiInterface) -> Result<()> {
        info!("Disabling power save on {}", ifc.name);
        
        let output = Command::new("iw")
            .args(["dev", &ifc.name, "set", "power_save", "off"])
            .output()
            .context("Failed to execute iw command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Failed to disable power save on {}: {}", ifc.name, stderr);
        } else {
            info!("Power save disabled on {}", ifc.name);
        }

        Ok(())
    }

    /// Enable power saving on an interface
    pub fn enable_power_save(&self, ifc: &WifiInterface) -> Result<()> {
        info!("Enabling power save on {}", ifc.name);
        
        let output = Command::new("iw")
            .args(["dev", &ifc.name, "set", "power_save", "on"])
            .output()
            .context("Failed to execute iw command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Failed to enable power save on {}: {}", ifc.name, stderr);
        }

        Ok(())
    }

    /// Get link statistics for an interface
    pub fn get_link_stats(&self, ifc: &WifiInterface) -> Result<LinkStats> {
        let output = Command::new("iw")
            .args(["dev", &ifc.name, "link"])
            .output()
            .context("Failed to get link stats")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        
        let mut stats = LinkStats::default();
        
        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("signal:") {
                if let Some(val) = line.split_whitespace().nth(1) {
                    stats.signal_dbm = val.parse().unwrap_or(-100);
                }
            } else if line.starts_with("tx bitrate:") {
                if let Some(val) = line.split_whitespace().nth(2) {
                    stats.tx_bitrate_mbps = val.parse().unwrap_or(0.0);
                }
            } else if line.starts_with("rx bitrate:") {
                if let Some(val) = line.split_whitespace().nth(2) {
                    stats.rx_bitrate_mbps = val.parse().unwrap_or(0.0);
                }
            }
        }

        debug!("Link stats for {}: {:?}", ifc.name, stats);
        Ok(stats)
    }

    /// Apply CAKE qdisc for bufferbloat mitigation
    pub fn apply_cake(&self, ifc: &WifiInterface, bandwidth_mbps: u32) -> Result<()> {
        info!("Applying CAKE qdisc on {} with {}mbit bandwidth", ifc.name, bandwidth_mbps);
        
        let bandwidth = format!("{}mbit", bandwidth_mbps);
        
        let output = Command::new("tc")
            .args([
                "qdisc", "replace", "dev", &ifc.name, "root", "cake",
                "bandwidth", &bandwidth,
                "diffserv4", "dual-dsthost", "nat", "wash", "ack-filter"
            ])
            .output()
            .context("Failed to apply CAKE qdisc")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Failed to apply CAKE on {}: {}", ifc.name, stderr);
        } else {
            info!("CAKE applied successfully on {}", ifc.name);
        }

        Ok(())
    }

    /// Remove CAKE qdisc
    pub fn remove_cake(&self, ifc: &WifiInterface) -> Result<()> {
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", &ifc.name, "root"])
            .output();
        Ok(())
    }
}

/// Link statistics for an interface
#[derive(Debug, Default)]
pub struct LinkStats {
    pub signal_dbm: i32,
    pub tx_bitrate_mbps: f64,
    pub rx_bitrate_mbps: f64,
}

impl Default for WifiManager {
    fn default() -> Self {
        Self::new().unwrap_or(Self { interfaces: Vec::new() })
    }
}
