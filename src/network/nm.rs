//! NetworkManager D-Bus Client
//!
//! Pure Rust D-Bus implementation using zbus to communicate with NetworkManager.
//! Per rewrite.md: No text parsing - use structured DBus APIs.

use anyhow::{Context, Result};
use log::{info, debug};
use std::collections::HashMap;
use zbus::{Connection, proxy};

/// WiFi frequency band
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WifiBand {
    Band2_4GHz,
    Band5GHz,
    Band6GHz,
    Unknown,
}

impl WifiBand {
    /// Determine band from frequency in MHz
    pub fn from_frequency(freq: u32) -> Self {
        match freq {
            2400..=2500 => WifiBand::Band2_4GHz,
            5150..=5924 => WifiBand::Band5GHz,
            5925..=7125 => WifiBand::Band6GHz,
            _ => WifiBand::Unknown,
        }
    }
}

/// Access Point information from NetworkManager
#[derive(Debug, Clone)]
pub struct AccessPoint {
    #[allow(dead_code)]
    pub path: String,
    pub ssid: String,
    pub bssid: String,
    pub frequency: u32,
    pub band: WifiBand,
    pub signal_strength: i32, // dBm (typically -30 to -90)
    #[allow(dead_code)]
    pub max_bitrate: u32,     // Kbit/s
}

impl AccessPoint {
    /// Calculate roaming score (RSSI + band bias)
    pub fn score(&self, bias_5ghz: i32, bias_6ghz: i32) -> i32 {
        let bias = match self.band {
            WifiBand::Band2_4GHz => 0,
            WifiBand::Band5GHz => bias_5ghz,
            WifiBand::Band6GHz => bias_6ghz,
            WifiBand::Unknown => 0,
        };
        self.signal_strength + bias
    }
}

/// NetworkManager device state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceState {
    Unknown,
    Unmanaged,
    Unavailable,
    Disconnected,
    Preparing,
    ConfigIp,
    NeedAuth,
    Activated,
    Deactivating,
    Failed,
}

impl From<u32> for DeviceState {
    fn from(state: u32) -> Self {
        match state {
            0 => DeviceState::Unknown,
            10 => DeviceState::Unmanaged,
            20 => DeviceState::Unavailable,
            30 => DeviceState::Disconnected,
            40 => DeviceState::Preparing,
            70 => DeviceState::ConfigIp,
            60 => DeviceState::NeedAuth,
            100 => DeviceState::Activated,
            110 => DeviceState::Deactivating,
            120 => DeviceState::Failed,
            _ => DeviceState::Unknown,
        }
    }
}

/// Wireless device info from NetworkManager
#[derive(Debug, Clone)]
pub struct WirelessDevice {
    pub path: String,
    pub interface: String,
    pub state: DeviceState,
    pub bitrate: u32,         // Current bitrate in Kbit/s
    pub active_ap: Option<AccessPoint>,
}

// NetworkManager D-Bus proxy for the main interface
#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    #[zbus(property)]
    fn devices(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;
    
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
}

// Device proxy
#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmDevice {
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

// Wireless device proxy
#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmWireless {
    #[zbus(property)]
    fn bitrate(&self) -> zbus::Result<u32>;
    
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    
    #[zbus(property)]
    fn access_points(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;
    
    fn request_scan(&self, options: HashMap<String, zbus::zvariant::Value<'_>>) -> zbus::Result<()>;
}

// Access Point proxy
#[proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmAccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
    
    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<u32>;
    
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    
    #[zbus(property)]
    fn max_bitrate(&self) -> zbus::Result<u32>;
}

/// NetworkManager D-Bus Client
pub struct NmClient {
    connection: Connection,
}

impl NmClient {
    /// Create a new NetworkManager client
    pub async fn new() -> Result<Self> {
        let connection = Connection::system()
            .await
            .context("Failed to connect to system D-Bus")?;
        
        // Verify NetworkManager is available
        let nm = NetworkManagerProxy::new(&connection).await?;
        let version = nm.version().await.unwrap_or_else(|_| "unknown".to_string());
        info!("Connected to NetworkManager v{}", version);
        
        Ok(Self { connection })
    }

    /// Get all wireless devices
    pub async fn get_wireless_devices(&self) -> Result<Vec<WirelessDevice>> {
        let nm = NetworkManagerProxy::new(&self.connection).await?;
        let device_paths = nm.devices().await?;
        
        let mut wireless_devices = Vec::new();
        
        for path in device_paths {
            let device = NmDeviceProxy::builder(&self.connection)
                .path(path.as_ref())?
                .build()
                .await?;
            
            // Check if it's a WiFi device (type 2)
            let device_type = device.device_type().await.unwrap_or(0);
            if device_type != 2 {
                continue;
            }
            
            let interface = device.interface().await.unwrap_or_default();
            let state = DeviceState::from(device.state().await.unwrap_or(0));
            
            // Skip virtual interfaces per rewrite.md
            if Self::is_virtual_interface(&interface) {
                debug!("Skipping virtual interface: {}", interface);
                continue;
            }
            
            // Get wireless-specific properties
            let wireless = NmWirelessProxy::builder(&self.connection)
                .path(path.as_ref())?
                .build()
                .await?;
            
            let bitrate = wireless.bitrate().await.unwrap_or(0);
            
            // Get active AP info
            let active_ap = match wireless.active_access_point().await {
                Ok(ap_path) if !ap_path.as_str().is_empty() && ap_path.as_str() != "/" => {
                    self.get_access_point_info(ap_path.as_str()).await.ok()
                }
                _ => None,
            };
            
            wireless_devices.push(WirelessDevice {
                path: path.to_string(),
                interface,
                state,
                bitrate,
                active_ap,
            });
        }
        
        Ok(wireless_devices)
    }

    /// Get access point information
    async fn get_access_point_info(&self, path: &str) -> Result<AccessPoint> {
        let ap = NmAccessPointProxy::builder(&self.connection)
            .path(path)?
            .build()
            .await?;
        
        let ssid_bytes = ap.ssid().await.unwrap_or_default();
        let ssid = String::from_utf8_lossy(&ssid_bytes).to_string();
        let bssid = ap.hw_address().await.unwrap_or_default();
        let frequency = ap.frequency().await.unwrap_or(0);
        let strength = ap.strength().await.unwrap_or(0);
        let max_bitrate = ap.max_bitrate().await.unwrap_or(0);
        
        // Convert strength (0-100) to approximate dBm
        let signal_dbm = Self::strength_to_dbm(strength);
        
        Ok(AccessPoint {
            path: path.to_string(),
            ssid,
            bssid,
            frequency,
            band: WifiBand::from_frequency(frequency),
            signal_strength: signal_dbm,
            max_bitrate,
        })
    }

    /// Get all visible access points for a device
    pub async fn get_access_points(&self, device_path: &str) -> Result<Vec<AccessPoint>> {
        let wireless = NmWirelessProxy::builder(&self.connection)
            .path(device_path)?
            .build()
            .await?;
        
        let ap_paths = wireless.access_points().await?;
        let mut access_points = Vec::new();
        
        for ap_path in ap_paths {
            if let Ok(ap) = self.get_access_point_info(ap_path.as_str()).await {
                access_points.push(ap);
            }
        }
        
        Ok(access_points)
    }

    /// Request a WiFi scan
    pub async fn request_scan(&self, device_path: &str) -> Result<()> {
        let wireless = NmWirelessProxy::builder(&self.connection)
            .path(device_path)?
            .build()
            .await?;
        
        let options: HashMap<String, zbus::zvariant::Value> = HashMap::new();
        wireless.request_scan(options).await?;
        debug!("Scan requested for device: {}", device_path);
        
        Ok(())
    }

    /// Check if interface is virtual (per rewrite.md: ignore docker, veth, virbr, tun, tap)
    fn is_virtual_interface(name: &str) -> bool {
        name.starts_with("docker") ||
        name.starts_with("veth") ||
        name.starts_with("virbr") ||
        name.starts_with("tun") ||
        name.starts_with("tap") ||
        name.starts_with("br-") ||
        name.starts_with("lo")
    }

    /// Convert NM strength (0-100) to approximate dBm
    fn strength_to_dbm(strength: u8) -> i32 {
        // Approximate conversion: strength 0 = -100dBm, strength 100 = -30dBm
        -100 + (strength as i32 * 70 / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wifi_band_detection() {
        assert_eq!(WifiBand::from_frequency(2412), WifiBand::Band2_4GHz);
        assert_eq!(WifiBand::from_frequency(5180), WifiBand::Band5GHz);
        assert_eq!(WifiBand::from_frequency(5925), WifiBand::Band6GHz); // 6GHz start
        assert_eq!(WifiBand::from_frequency(6000), WifiBand::Band6GHz);
        assert_eq!(WifiBand::from_frequency(900), WifiBand::Unknown);
    }
    
    #[test]
    fn test_access_point_scoring() {
        let ap = AccessPoint {
            path: "/".to_string(),
            ssid: "Test".to_string(),
            bssid: "00:11:22:33:44:55".to_string(),
            frequency: 5180,
            band: WifiBand::Band5GHz,
            signal_strength: -60,
            max_bitrate: 1000,
        };
        
        // Base score -60 + bias 15 = -45
        assert_eq!(ap.score(15, 20), -45);
        
        // With higher bias
        assert_eq!(ap.score(30, 20), -30);
    }
}
