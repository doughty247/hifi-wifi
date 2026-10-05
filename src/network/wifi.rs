//! Wi-Fi interface management and optimization
//!
//! Handles detection, monitoring, and configuration of Wi-Fi interfaces.

use anyhow::{Context, Result};
use log::{info, warn};
use std::fs;
use std::path::Path;
use std::process::Command;


/// Interface type (WiFi or Ethernet)
#[derive(Debug, Clone, PartialEq)]
pub enum InterfaceType {
    Wifi,
    Ethernet,
}

/// Detected driver category for applying specific optimizations
#[derive(Debug, Clone, PartialEq)]
pub enum DriverCategory {
    Rtw89,     // Realtek RTW89 (modern)
    Rtw88,     // Realtek RTW88
    RtlLegacy, // Legacy Realtek
    MediaTek,  // MediaTek MT7921/MT76
    Intel,     // Intel iwlwifi
    Atheros,   // Qualcomm Atheros
    Broadcom,  // Broadcom
    Ralink,    // Ralink/MediaTek Legacy
    Marvell,   // Marvell
    Generic,   // Unknown - apply universal optimizations
}

/// Represents a detected network interface (WiFi or Ethernet)
#[derive(Debug, Clone)]
pub struct WifiInterface {
    pub name: String,
    pub driver: String,
    pub category: DriverCategory,
    pub interface_type: InterfaceType,
    #[allow(dead_code)]
    pub is_active: bool,
}

/// Manages Wi-Fi interfaces and applies optimizations
pub struct WifiManager {
    interfaces: Vec<WifiInterface>,
}

impl WifiManager {
    pub fn new() -> Result<Self> {
        let interfaces = Self::detect_interfaces(true)?;
        Ok(Self { interfaces })
    }

    /// Create WifiManager without logging (for status display)
    pub fn new_quiet() -> Result<Self> {
        let interfaces = Self::detect_interfaces(false)?;
        Ok(Self { interfaces })
    }

    /// Detect all Wi-Fi interfaces on the system
    fn detect_interfaces(log_output: bool) -> Result<Vec<WifiInterface>> {
        let mut interfaces = Vec::new();

        // Read from /sys/class/net
        let net_path = Path::new("/sys/class/net");
        if !net_path.exists() {
            return Ok(interfaces);
        }

        for entry in fs::read_dir(net_path)? {
            let entry = entry?;
            let ifc_name = entry.file_name().to_string_lossy().to_string();

            // Check if it's a wireless or ethernet interface
            let interface_type = if ifc_name.starts_with("wl") {
                InterfaceType::Wifi
            } else if ifc_name.starts_with("en") || ifc_name.starts_with("eth") {
                InterfaceType::Ethernet
            } else {
                continue;
            };

            let driver = Self::detect_driver(&ifc_name);
            let category = Self::categorize_driver(&driver);
            let is_active = Self::is_interface_active(&ifc_name);

            if log_output {
                let type_str = match interface_type {
                    InterfaceType::Wifi => "WiFi",
                    InterfaceType::Ethernet => "Ethernet",
                };
                info!(
                    "Detected interface: {} (type: {}, driver: {}, category: {:?})",
                    ifc_name, type_str, driver, category
                );
            }

            interfaces.push(WifiInterface {
                name: ifc_name,
                driver,
                category,
                interface_type,
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
    /// Current power save state as the driver reports it (None if unknown / not Wi-Fi)
    pub fn get_power_save(&self, ifc: &WifiInterface) -> Option<bool> {
        if ifc.interface_type != InterfaceType::Wifi {
            return None;
        }
        let out = Command::new("iw")
            .args(["dev", &ifc.name, "get", "power_save"])
            .output()
            .ok()?;
        parse_power_save(&String::from_utf8_lossy(&out.stdout))
    }

    pub fn disable_power_save(&self, ifc: &WifiInterface) -> Result<()> {
        // Power save only applies to WiFi
        if ifc.interface_type != InterfaceType::Wifi {
            return Ok(());
        }

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
        // Power save only applies to WiFi
        if ifc.interface_type != InterfaceType::Wifi {
            return Ok(());
        }

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

    /// Check if interface is connected and active
    pub fn is_interface_connected(&self, ifc: &WifiInterface) -> bool {
        match ifc.interface_type {
            InterfaceType::Wifi => {
                // For WiFi, check if we're connected via iw
                let output = Command::new("iw").args(["dev", &ifc.name, "link"]).output();

                if let Ok(output) = output {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // If connected, output will contain "Connected to" and not "Not connected"
                    stdout.contains("Connected to")
                        || (stdout.contains("SSID:") && !stdout.contains("Not connected"))
                } else {
                    false
                }
            }
            InterfaceType::Ethernet => {
                // For Ethernet, check carrier status
                let carrier_path = format!("/sys/class/net/{}/carrier", ifc.name);
                std::fs::read_to_string(&carrier_path)
                    .map(|s| s.trim() == "1")
                    .unwrap_or(false)
            }
        }
    }
}

impl Default for WifiManager {
    fn default() -> Self {
        Self::new().unwrap_or(Self {
            interfaces: Vec::new(),
        })
    }
}

/// Parse `iw dev X get power_save` ("Power save: on")
pub fn parse_power_save(out: &str) -> Option<bool> {
    let v = out.split(':').nth(1)?.trim();
    match v {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod power_save_tests {
    use super::parse_power_save;

    #[test]
    fn parses_iw_power_save() {
        assert_eq!(parse_power_save("Power save: on\n"), Some(true));
        assert_eq!(parse_power_save("Power save: off\n"), Some(false));
        assert_eq!(parse_power_save("command failed: No such device (-19)\n"), None);
        assert_eq!(parse_power_save(""), None);
    }
}
