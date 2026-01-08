//! Power management for Wi-Fi adapters
//!
//! Adaptive power management based on AC/battery status.

use log::info;
use std::fs;
use std::path::Path;

/// Power source state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PowerSource {
    AC,
    Battery,
    Unknown,
}

/// Device type classification
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceType {
    Desktop,
    Laptop,
    SteamDeck,
}

/// Manages power-aware Wi-Fi settings
pub struct PowerManager {
    device_type: DeviceType,
}

impl PowerManager {
    pub fn new() -> Self {
        let device_type = Self::detect_device_type();
        let current_source = Self::detect_power_source();
        
        info!("Device type: {:?}, Power source: {:?}", device_type, current_source);
        
        Self {
            device_type,
        }
    }

    /// Detect if this is a portable/battery-powered device
    fn detect_device_type() -> DeviceType {
        // Check for Steam Deck
        if let Ok(board) = fs::read_to_string("/sys/class/dmi/id/board_name") {
            if board.trim().contains("Jupiter") || board.trim().contains("Galileo") {
                return DeviceType::SteamDeck;
            }
        }

        // Check chassis type
        if let Ok(chassis) = fs::read_to_string("/sys/class/dmi/id/chassis_type") {
            let chassis_type: u32 = chassis.trim().parse().unwrap_or(0);
            
            // Desktop chassis types
            if matches!(chassis_type, 3 | 4 | 5 | 6 | 7 | 13 | 15 | 16) {
                return DeviceType::Desktop;
            }
            
            // Laptop/portable chassis types
            if matches!(chassis_type, 8 | 9 | 10 | 11 | 14 | 30 | 31) {
                return DeviceType::Laptop;
            }
        }

        // Check for battery presence
        if Self::has_system_battery() {
            return DeviceType::Laptop;
        }

        DeviceType::Desktop
    }

    /// Check if system has a real battery (not peripherals)
    fn has_system_battery() -> bool {
        let power_supply = Path::new("/sys/class/power_supply");
        
        if !power_supply.exists() {
            return false;
        }

        if let Ok(entries) = fs::read_dir(power_supply) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                
                // Skip peripheral batteries (mice, keyboards, etc.)
                if name.contains("hidpp") || name.contains("hid") || 
                   name.contains("mouse") || name.contains("keyboard") ||
                   name.contains("wacom") {
                    continue;
                }

                // Check if it's a battery
                let type_path = entry.path().join("type");
                if let Ok(bat_type) = fs::read_to_string(&type_path) {
                    if bat_type.trim() == "Battery" {
                        // Verify it has capacity (real battery)
                        let cap_path = entry.path().join("capacity");
                        if cap_path.exists() {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    /// Detect current power source
    /// FIXED: Collect ALL power supply info first, then decide (prevents race condition)
    pub fn detect_power_source() -> PowerSource {
        let power_supply = Path::new("/sys/class/power_supply");
        
        let mut ac_online = false;
        let mut battery_discharging = false;
        let mut battery_found = false;
        
        if let Ok(entries) = fs::read_dir(power_supply) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                
                // Skip peripheral batteries
                if name.contains("hidpp") || name.contains("hid") || 
                   name.contains("mouse") || name.contains("keyboard") {
                    continue;
                }
                
                // Check AC adapters
                if name.starts_with("AC") || name.starts_with("ADP") || name.contains("ACAD") {
                    let online_path = entry.path().join("online");
                    if let Ok(status) = fs::read_to_string(&online_path) {
                        if status.trim() == "1" {
                            ac_online = true;
                        }
                    }
                }

                // Check battery status
                if name.starts_with("BAT") || name == "battery" {
                    battery_found = true;
                    let status_path = entry.path().join("status");
                    if let Ok(status) = fs::read_to_string(&status_path) {
                        let status = status.trim();
                        match status {
                            "Charging" | "Full" | "Not charging" => {
                                // Battery connected to power = AC
                                ac_online = true;
                            }
                            "Discharging" => {
                                battery_discharging = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // AC takes priority - if adapter is online, we're on AC regardless of battery state
        if ac_online {
            return PowerSource::AC;
        }
        
        // Only report battery if we found one and it's discharging
        if battery_found && battery_discharging {
            return PowerSource::Battery;
        }
        
        // No battery = desktop = treat as AC
        if !battery_found {
            return PowerSource::AC;
        }

        PowerSource::Unknown
    }

    /// Get current power source (refreshed dynamically)
    pub fn power_source(&self) -> PowerSource {
        // Always get fresh reading
        Self::detect_power_source()
    }

    /// Get device type
    pub fn device_type(&self) -> &DeviceType {
        &self.device_type
    }

    /// Should power saving be enabled based on current state?
    /// FIXED: Now refreshes power source dynamically instead of using cached value
    pub fn should_enable_power_save(&self) -> bool {
        let current_source = Self::detect_power_source();
        
        match self.device_type {
            DeviceType::Desktop => false, // Always performance mode
            DeviceType::SteamDeck | DeviceType::Laptop => {
                // Enable power save only when on battery
                current_source == PowerSource::Battery
            }
        }
    }

    /// Get battery percentage (if available)
    pub fn battery_percentage(&self) -> Option<u32> {
        let power_supply = Path::new("/sys/class/power_supply");
        
        if let Ok(entries) = fs::read_dir(power_supply) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                
                if name.starts_with("BAT") || name == "battery" {
                    let capacity_path = entry.path().join("capacity");
                    if let Ok(capacity) = fs::read_to_string(&capacity_path) {
                        return capacity.trim().parse().ok();
                    }
                }
            }
        }

        None
    }
}

impl Default for PowerManager {
    fn default() -> Self {
        Self::new()
    }
}
