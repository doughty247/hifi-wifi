use anyhow::Result;
use log::{debug, info, warn};
use std::net::IpAddr;
use std::process::Command;
use std::str::FromStr;

fn is_valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

pub struct MultipathManager {
    pub is_active: bool,
    primary_interface: Option<String>,
    gateway_ip: Option<String>,
    xt_tee_supported: bool,
    pub mock_mode: bool,
    pub mock_iptables_fail: bool,
}

impl MultipathManager {
    pub fn new() -> Self {
        let xt_tee_supported = Self::check_xt_tee_supported();
        if !xt_tee_supported {
            warn!("xt_TEE netfilter kernel module is not supported (modprobe xt_TEE failed). Single-connection temporal duplication will be disabled.");
        }
        Self {
            is_active: false,
            primary_interface: None,
            gateway_ip: None,
            xt_tee_supported,
            mock_mode: false,
            mock_iptables_fail: false,
        }
    }

    #[allow(dead_code)]
    pub fn enable_mock_mode(&mut self, gateway: &str) {
        self.mock_mode = true;
        self.xt_tee_supported = true;
        self.gateway_ip = Some(gateway.to_string());
    }

    #[allow(dead_code)]
    pub fn set_mock_iptables_fail(&mut self, fail: bool) {
        self.mock_iptables_fail = fail;
    }

    /// Check if xt_TEE netfilter module is loadable/supported
    fn check_xt_tee_supported() -> bool {
        let output = Command::new("modprobe")
            .args(["--dry-run", "xt_TEE"])
            .output();
        match output {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }

    /// Retrieve the default gateway IP for a specific interface
    fn get_default_gateway(interface: &str) -> Option<String> {
        if !is_valid_interface_name(interface) {
            return None;
        }
        let output = Command::new("ip")
            .args(["route", "show", "dev", interface])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for i in 0..parts.len() {
                if parts[i] == "default" && i + 2 < parts.len() && parts[i + 1] == "via" {
                    let gw = parts[i + 2].to_string();
                    if IpAddr::from_str(&gw).is_ok() {
                        return Some(gw);
                    }
                }
            }
        }
        None
    }

    /// Activate single-connection temporal packet duplication for Moonlight and Steam Link UDP traffic.
    /// Clones egress UDP packets using the iptables TEE target and directs them to the default gateway,
    /// while adding a loop blocker mark (0x99) to avoid infinite packet-cloning loops.
    pub fn activate_duplication(&mut self, primary: &str) -> Result<()> {
        if !is_valid_interface_name(primary) {
            anyhow::bail!("Invalid interface name: {}", primary);
        }
        if self.is_active {
            return Ok(());
        }

        if !self.xt_tee_supported {
            warn!("Skipping duplication activation: xt_TEE kernel module is not supported");
            return Ok(());
        }

        info!("Telemetry indicates high jitter/loss. Attempting to activate single-connection temporal packet duplication on {}...", primary);

        let gw_ip_opt = if self.mock_mode {
            self.gateway_ip.clone().or_else(|| Some("192.168.1.1".to_string()))
        } else {
            Self::get_default_gateway(primary)
        };

        if let Some(gw_ip) = gw_ip_opt {
            info!(
                "Activating temporal packet duplication on {} via gateway {}",
                primary, gw_ip
            );

            self.primary_interface = Some(primary.to_string());
            self.gateway_ip = Some(gw_ip.clone());

            let (ml_tee_ok, ml_mark_ok, sl_tee_ok, sl_mark_ok) = if self.mock_mode {
                let success = !self.mock_iptables_fail;
                (success, success, success, success)
            } else {
                let ml_tee = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-A", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "47998:48010",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "TEE", "--gateway", &gw_ip
                    ])
                    .output();
                let ml_mark = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-A", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "47998:48010",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "MARK", "--set-mark", "0x99"
                    ])
                    .output();
                let sl_tee = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-A", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "27031:27036",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "TEE", "--gateway", &gw_ip
                    ])
                    .output();
                let sl_mark = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-A", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "27031:27036",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "MARK", "--set-mark", "0x99"
                    ])
                    .output();

                let ml_tee_status = ml_tee.ok().map(|o| o.status.success()).unwrap_or(false);
                let ml_mark_status = ml_mark.ok().map(|o| o.status.success()).unwrap_or(false);
                let sl_tee_status = sl_tee.ok().map(|o| o.status.success()).unwrap_or(false);
                let sl_mark_status = sl_mark.ok().map(|o| o.status.success()).unwrap_or(false);
                (ml_tee_status, ml_mark_status, sl_tee_status, sl_mark_status)
            };

            let ml_ok = ml_tee_ok && ml_mark_ok;
            let sl_ok = sl_tee_ok && sl_mark_ok;

            if ml_ok && sl_ok {
                self.is_active = true;
                info!("Single-connection temporal duplication active on {}", primary);
            } else {
                warn!("Failed to activate temporal duplication rules, rolling back");
                self.is_active = true; // force deactivation to run deletions
                let _ = self.deactivate_duplication();
                anyhow::bail!("Failed to apply all iptables rules for single-connection temporal duplication");
            }
        } else {
            debug!("No default gateway found on interface {} for packet duplication", primary);
        }

        Ok(())
    }

    /// Deactivate predictive packet duplication and clean up rules
    pub fn deactivate_duplication(&mut self) -> Result<()> {
        if !self.is_active {
            return Ok(());
        }

        info!("Deactivating single-connection temporal packet duplication...");

        if let (Some(primary), Some(gw_ip)) = (&self.primary_interface, &self.gateway_ip) {
            if !self.mock_mode {
                // Delete Moonlight TEE rule
                let _ = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-D", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "47998:48010",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "TEE", "--gateway", gw_ip
                    ])
                    .output();

                // Delete Moonlight MARK rule
                let _ = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-D", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "47998:48010",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "MARK", "--set-mark", "0x99"
                    ])
                    .output();

                // Delete Steam Link TEE rule
                let _ = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-D", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "27031:27036",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "TEE", "--gateway", gw_ip
                    ])
                    .output();

                // Delete Steam Link MARK rule
                let _ = Command::new("iptables")
                    .args([
                        "-t", "mangle", "-D", "POSTROUTING", "-o", primary,
                        "-p", "udp", "--dport", "27031:27036",
                        "-m", "mark", "!", "--mark", "0x99",
                        "-j", "MARK", "--set-mark", "0x99"
                    ])
                    .output();
            }
        }

        self.is_active = false;
        self.primary_interface = None;
        self.gateway_ip = None;
        info!("Single-connection temporal packet duplication deactivated cleanly");
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
        assert!(manager.gateway_ip.is_none());
    }

    #[test]
    fn test_get_default_gateway_invalid() {
        assert!(MultipathManager::get_default_gateway("nonexistent_device_123").is_none());
    }

    #[test]
    fn test_activate_deactivate_resilient() {
        let mut manager = MultipathManager::new();
        let res = manager.activate_duplication("dummy0");
        assert!(res.is_ok());
        assert!(!manager.is_active);

        let res = manager.activate_duplication("invalid_device_name_too_long");
        assert!(res.is_err());

        let res = manager.deactivate_duplication();
        assert!(res.is_ok());
    }
}
