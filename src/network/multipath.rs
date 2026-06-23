use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::fs;
use std::process::Command;

pub struct MultipathManager {
    is_active: bool,
    primary_interface: Option<String>,
    secondary_interface: Option<String>,
    secondary_gateway: Option<String>,
}

impl MultipathManager {
    pub fn new() -> Self {
        Self {
            is_active: false,
            primary_interface: None,
            secondary_interface: None,
            secondary_gateway: None,
        }
    }

    /// Retrieve the default gateway IP for a specific interface
    fn get_default_gateway(interface: &str) -> Option<String> {
        let output = Command::new("ip")
            .args(["route", "show", "dev", interface])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for i in 0..parts.len() {
                if parts[i] == "default" && i + 2 < parts.len() && parts[i + 1] == "via" {
                    return Some(parts[i + 2].to_string());
                }
            }
        }
        None
    }

    /// Detect if the interface link carrier is up
    fn is_interface_up(interface: &str) -> bool {
        let path = format!("/sys/class/net/{}/carrier", interface);
        if let Ok(content) = fs::read_to_string(path) {
            content.trim() == "1"
        } else {
            false
        }
    }

    /// Identify a suitable secondary interface that has carrier up and a default gateway
    fn find_secondary_interface(&self, primary: &str) -> Option<(String, String)> {
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != primary
                    && name != "lo"
                    && !name.starts_with("ifb")
                    && !name.starts_with("docker")
                    && !name.starts_with("br-")
                    && !name.starts_with("veth")
                    && Self::is_interface_up(&name)
                {
                    if let Some(gw) = Self::get_default_gateway(&name) {
                        debug!("Found candidate secondary interface: {} via {}", name, gw);
                        return Some((name, gw));
                    }
                }
            }
        }
        None
    }

    /// Activate predictive packet duplication for Moonlight and Steam Link UDP traffic
    pub fn activate_duplication(&mut self, primary: &str) -> Result<()> {
        if self.is_active {
            return Ok(());
        }

        info!("Telemetry indicates high jitter/loss. Attempting to activate predictive packet duplication on {}...", primary);

        if let Some((sec_iface, sec_gw)) = self.find_secondary_interface(primary) {
            info!(
                "Activating packet duplication from {} to {} via gateway {}",
                primary, sec_iface, sec_gw
            );

            self.primary_interface = Some(primary.to_string());
            self.secondary_interface = Some(sec_iface);
            self.secondary_gateway = Some(sec_gw.clone());

            // 1. Create duplication rules using iptables TEE target
            // Moonlight UDP ports
            let ml_rule = Command::new("iptables")
                .args([
                    "-t",
                    "mangle",
                    "-A",
                    "POSTROUTING",
                    "-o",
                    primary,
                    "-p",
                    "udp",
                    "--dport",
                    "47998:48010",
                    "-j",
                    "TEE",
                    "--gateway",
                    &sec_gw,
                ])
                .output()
                .context("Failed to run iptables Moonlight duplication command")?;

            let ml_ok = if ml_rule.status.success() {
                true
            } else {
                let stderr = String::from_utf8_lossy(&ml_rule.stderr);
                warn!("iptables Moonlight TEE failed: {}", stderr.trim());
                false
            };

            // Steam Link UDP ports
            let sl_rule = Command::new("iptables")
                .args([
                    "-t",
                    "mangle",
                    "-A",
                    "POSTROUTING",
                    "-o",
                    primary,
                    "-p",
                    "udp",
                    "--dport",
                    "27031:27036",
                    "-j",
                    "TEE",
                    "--gateway",
                    &sec_gw,
                ])
                .output()
                .context("Failed to run iptables Steam Link duplication command")?;

            let sl_ok = if sl_rule.status.success() {
                true
            } else {
                let stderr = String::from_utf8_lossy(&sl_rule.stderr);
                warn!("iptables Steam Link TEE failed: {}", stderr.trim());
                false
            };

            if ml_ok && sl_ok {
                self.is_active = true;
                info!("Predictive packet duplication active on {}", primary);
            } else {
                if ml_ok {
                    let _ = Command::new("iptables")
                        .args([
                            "-t",
                            "mangle",
                            "-D",
                            "POSTROUTING",
                            "-o",
                            primary,
                            "-p",
                            "udp",
                            "--dport",
                            "47998:48010",
                            "-j",
                            "TEE",
                            "--gateway",
                            &sec_gw,
                        ])
                        .output();
                }
                warn!("Failed to activate predictive packet duplication rules");
            }
        } else {
            debug!("No suitable secondary interface found for packet duplication");
        }

        Ok(())
    }

    /// Deactivate predictive packet duplication and clean up rules
    pub fn deactivate_duplication(&mut self) -> Result<()> {
        if !self.is_active {
            return Ok(());
        }

        info!("Deactivating predictive packet duplication...");

        if let (Some(primary), Some(sec_gw)) = (&self.primary_interface, &self.secondary_gateway) {
            // Delete Moonlight TEE rule
            let _ = Command::new("iptables")
                .args([
                    "-t",
                    "mangle",
                    "-D",
                    "POSTROUTING",
                    "-o",
                    primary,
                    "-p",
                    "udp",
                    "--dport",
                    "47998:48010",
                    "-j",
                    "TEE",
                    "--gateway",
                    sec_gw,
                ])
                .output();

            // Delete Steam Link TEE rule
            let _ = Command::new("iptables")
                .args([
                    "-t",
                    "mangle",
                    "-D",
                    "POSTROUTING",
                    "-o",
                    primary,
                    "-p",
                    "udp",
                    "--dport",
                    "27031:27036",
                    "-j",
                    "TEE",
                    "--gateway",
                    sec_gw,
                ])
                .output();
        }

        self.is_active = false;
        self.primary_interface = None;
        self.secondary_interface = None;
        self.secondary_gateway = None;
        info!("Predictive packet duplication deactivated cleanly");
        Ok(())
    }

    /// Clean up any lingering rules (failsafe for daemon shutdown)
    pub fn cleanup(&mut self) -> Result<()> {
        self.deactivate_duplication()
    }
}

impl Drop for MultipathManager {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multipath_manager_init() {
        let manager = MultipathManager::new();
        assert!(!manager.is_active);
        assert!(manager.primary_interface.is_none());
        assert!(manager.secondary_interface.is_none());
        assert!(manager.secondary_gateway.is_none());
    }

    #[test]
    fn test_is_interface_up_invalid() {
        assert!(!MultipathManager::is_interface_up("nonexistent_device_123"));
    }

    #[test]
    fn test_get_default_gateway_invalid() {
        assert!(MultipathManager::get_default_gateway("nonexistent_device_123").is_none());
    }

    #[test]
    fn test_activate_deactivate_resilient() {
        let mut manager = MultipathManager::new();
        let res = manager.activate_duplication("nonexistent_device");
        assert!(res.is_ok());
        assert!(!manager.is_active);

        let res = manager.deactivate_duplication();
        assert!(res.is_ok());
    }
}
