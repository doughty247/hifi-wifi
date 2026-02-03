//! Hardware detection and validation for Steam Deck OLED
//!
//! This module implements the two-layer hardware gate:
//! 1. DMI check: Valve + Galileo (Steam Deck OLED only)
//! 2. PCI ID check: 17cb:1103 with subsystem 17cb:0108 (QCA2066 exact variant)

use std::fs;
use std::path::Path;

/// Expected values for Steam Deck OLED
const EXPECTED_BOARD_VENDOR: &str = "Valve";
const EXPECTED_BOARD_NAME: &str = "Galileo";  // OLED = Galileo, LCD = Jupiter
const EXPECTED_WIFI_VENDOR: &str = "0x17cb";  // Qualcomm
const EXPECTED_WIFI_DEVICE: &str = "0x1103";  // QCNFA765 / QCA2066
const EXPECTED_WIFI_SUBSYS_VENDOR: &str = "0x17cb";
const EXPECTED_WIFI_SUBSYS_DEVICE: &str = "0x0108";

/// Detected device information
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    // DMI info
    pub board_vendor: Option<String>,
    pub board_name: Option<String>,

    // WiFi PCI IDs
    pub wifi_vendor: Option<String>,
    pub wifi_device: Option<String>,
    pub wifi_subsys_vendor: Option<String>,
    pub wifi_subsys_device: Option<String>,

    // Derived flags
    dmi_valid: bool,
    wifi_valid: bool,
}

impl DeviceInfo {
    /// Detect device information from sysfs
    pub fn detect() -> Self {
        let board_vendor = read_dmi("board_vendor");
        let board_name = read_dmi("board_name");

        // Try to find WiFi device - prefer wlan0, fall back to scanning
        let wifi_path = find_wifi_device_path();

        let (wifi_vendor, wifi_device, wifi_subsys_vendor, wifi_subsys_device) =
            if let Some(path) = wifi_path {
                (
                    read_sysfs(&path.join("vendor")),
                    read_sysfs(&path.join("device")),
                    read_sysfs(&path.join("subsystem_vendor")),
                    read_sysfs(&path.join("subsystem_device")),
                )
            } else {
                (None, None, None, None)
            };

        // Validate DMI
        let dmi_valid = board_vendor.as_deref() == Some(EXPECTED_BOARD_VENDOR)
            && board_name.as_deref() == Some(EXPECTED_BOARD_NAME);

        // Validate WiFi PCI IDs
        let wifi_valid = wifi_vendor.as_deref() == Some(EXPECTED_WIFI_VENDOR)
            && wifi_device.as_deref() == Some(EXPECTED_WIFI_DEVICE)
            && wifi_subsys_vendor.as_deref() == Some(EXPECTED_WIFI_SUBSYS_VENDOR)
            && wifi_subsys_device.as_deref() == Some(EXPECTED_WIFI_SUBSYS_DEVICE);

        Self {
            board_vendor,
            board_name,
            wifi_vendor,
            wifi_device,
            wifi_subsys_vendor,
            wifi_subsys_device,
            dmi_valid,
            wifi_valid,
        }
    }

    /// Check if this is a supported device (Steam Deck OLED with QCA2066)
    pub fn is_supported(&self) -> bool {
        self.dmi_valid && self.wifi_valid
    }

    /// Check if WiFi card is the supported QCA2066 variant
    pub fn is_wifi_supported(&self) -> bool {
        self.wifi_valid
    }
}

/// Read a DMI attribute from sysfs
fn read_dmi(attr: &str) -> Option<String> {
    let path = format!("/sys/class/dmi/id/{}", attr);
    read_sysfs(Path::new(&path))
}

/// Read a sysfs file, returning trimmed contents
fn read_sysfs(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Find the WiFi device path in sysfs
///
/// Tries wlan0 first (most common), then scans for ath11k devices
fn find_wifi_device_path() -> Option<std::path::PathBuf> {
    // Try wlan0 first (standard interface name)
    let wlan0_path = Path::new("/sys/class/net/wlan0/device");
    if wlan0_path.exists() {
        return Some(wlan0_path.to_path_buf());
    }

    // Try wlp* interfaces (some distros use predictable names)
    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("wl") {
                let device_path = entry.path().join("device");
                if device_path.exists() {
                    // Verify it's an ath11k device
                    let driver_link = device_path.join("driver");
                    if let Ok(driver_target) = fs::read_link(&driver_link) {
                        let driver_name = driver_target.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if driver_name.contains("ath11k") {
                            return Some(device_path);
                        }
                    }
                }
            }
        }
    }

    // Scan PCI bus for Qualcomm 17cb:1103
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") {
        for entry in entries.flatten() {
            let device_path = entry.path();
            let vendor = read_sysfs(&device_path.join("vendor"));
            let device = read_sysfs(&device_path.join("device"));

            if vendor.as_deref() == Some(EXPECTED_WIFI_VENDOR)
                && device.as_deref() == Some(EXPECTED_WIFI_DEVICE)
            {
                return Some(device_path);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_detection() {
        // This will work on any system, just may not be a Steam Deck
        let device = DeviceInfo::detect();
        println!("Detected device: {:?}", device);
        println!("Is supported: {}", device.is_supported());
    }
}
