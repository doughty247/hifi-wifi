//! System-level optimizations for Wi-Fi performance
//!
//! Handles sysctl tuning, driver module parameters, IRQ affinity, and ethtool settings.

use anyhow::{Context, Result};
use log::{info, warn, debug};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::network::wifi::{DriverCategory, WifiInterface};

/// System optimizer for kernel and driver tuning
pub struct SystemOptimizer {
    sysctl_enabled: bool,
    irq_affinity_enabled: bool,
    driver_tweaks_enabled: bool,
}

impl SystemOptimizer {
    pub fn new(sysctl: bool, irq: bool, driver: bool) -> Self {
        Self {
            sysctl_enabled: sysctl,
            irq_affinity_enabled: irq,
            driver_tweaks_enabled: driver,
        }
    }

    /// Apply all system optimizations
    pub fn apply(&self, interfaces: &[WifiInterface]) -> Result<()> {
        if self.sysctl_enabled {
            self.apply_sysctl_tuning()?;
        }

        if self.driver_tweaks_enabled {
            for ifc in interfaces {
                self.apply_driver_config(&ifc.category)?;
            }
        }

        if self.irq_affinity_enabled {
            for ifc in interfaces {
                self.optimize_irq_affinity(ifc)?;
            }
        }

        // Apply ethtool optimizations
        for ifc in interfaces {
            self.apply_ethtool_settings(ifc)?;
        }

        Ok(())
    }

    /// Apply sysctl tuning for network performance
    fn apply_sysctl_tuning(&self) -> Result<()> {
        info!("Applying sysctl network optimizations...");

        let settings = [
            ("net.ipv4.tcp_congestion_control", "bbr"),
            ("net.core.rmem_default", "262144"),
            ("net.core.wmem_default", "262144"),
            ("net.core.rmem_max", "4194304"),
            ("net.core.wmem_max", "4194304"),
            ("net.ipv4.tcp_rmem", "4096 131072 4194304"),
            ("net.ipv4.tcp_wmem", "4096 65536 4194304"),
            ("net.ipv4.tcp_fastopen", "3"),
            ("net.core.netdev_max_backlog", "2000"),
            ("net.ipv4.tcp_ecn", "1"),
            ("net.ipv4.tcp_keepalive_time", "60"),
            ("net.ipv4.tcp_keepalive_intvl", "10"),
            ("net.ipv4.tcp_keepalive_probes", "6"),
            ("net.ipv4.tcp_tw_reuse", "1"),
        ];

        let sysctl_path = Path::new("/etc/sysctl.d/99-hifi-wifi.conf");
        let mut config_content = String::from("# hifi-wifi Network Optimizations\n");
        for (key, val) in settings.iter() {
            config_content.push_str(&format!("{} = {}\n", key, val));
        }
        
        // Try to persist to file (best effort)
        let persistence_success = if let Some(parent) = sysctl_path.parent() {
            fs::create_dir_all(parent).ok();
            match File::create(sysctl_path) {
                Ok(mut file) => {
                    if let Err(e) = file.write_all(config_content.as_bytes()) {
                         warn!("Failed to write sysctl config: {}", e);
                         false
                    } else {
                         true
                    }
                },
                Err(e) => {
                    warn!("Could not create sysctl config file (Read-only filesystem?): {}", e);
                    false
                }
            }
        } else {
            false
        };

        // If persistence worked, use 'sysctl -p'. Otherwise, apply manually.
        if persistence_success {
             let output = Command::new("sysctl")
                .args(["-p", sysctl_path.to_str().unwrap()])
                .output();
             if let Ok(o) = output {
                 if !o.status.success() {
                     warn!("sysctl -p failed: {}", String::from_utf8_lossy(&o.stderr));
                 } else {
                     info!("Sysctl optimizations applied via config file");
                     return Ok(());
                 }
             }
        }

        // Fallback: Apply manually
        info!("Applying sysctl settings transiently (runtime only)...");
        for (key, val) in settings.iter() {
             let _ = Command::new("sysctl")
                .arg("-w")
                .arg(format!("{}={}", key, val))
                .status();
        }

        Ok(())
    }

    /// Apply driver-specific module parameters
    fn apply_driver_config(&self, category: &DriverCategory) -> Result<()> {
        let (filename, config) = match category {
            DriverCategory::Rtw89 => ("rtw89.conf", r#"# Realtek RTW89 optimizations (RTL8852/RTL8852BE)
options rtw89_pci disable_aspm=1 disable_clkreq=1
options rtw89_core tx_ampdu_subframes=32
options rtw89_8852be disable_ps_mode=1
"#),
            DriverCategory::Rtw88 => ("rtw88.conf", r#"# Realtek RTW88 optimizations (RTL8822CE)
options rtw88_pci disable_aspm=1
options rtw88_core disable_lps_deep=Y
"#),
            DriverCategory::RtlLegacy => ("rtl_legacy.conf", r#"# Legacy Realtek optimizations
options rtl8192ee swenc=1 ips=0 fwlps=0
options rtl8188ee swenc=1 ips=0 fwlps=0
options rtl_pci disable_aspm=1
"#),
            DriverCategory::MediaTek => ("mediatek.conf", r#"# MediaTek optimizations (MT7921/MT76)
options mt7921e disable_aspm=1
options mt76_usb disable_usb_sg=1
"#),
            DriverCategory::Intel => ("iwlwifi.conf", r#"# Intel Wi-Fi optimizations
options iwlwifi power_save=0 uapsd_disable=1 11n_disable=0
options iwlmvm power_scheme=1
"#),
            DriverCategory::Atheros => ("ath_wifi.conf", r#"# Qualcomm Atheros optimizations
options ath10k_core skip_otp=y
options ath11k_pci disable_aspm=1
options ath9k nohwcrypt=0 ps_enable=0
"#),
            DriverCategory::Broadcom => ("broadcom.conf", r#"# Broadcom optimizations
options brcmfmac roamoff=1
options wl interference=0
"#),
            DriverCategory::Ralink => ("ralink.conf", r#"# Ralink/MediaTek Legacy optimizations
options rt2800usb nohwcrypt=0
options rt2800pci nohwcrypt=0
"#),
            DriverCategory::Marvell => ("marvell.conf", r#"# Marvell optimizations
options mwifiex disable_auto_ds=1
"#),
            DriverCategory::Generic => ("wifi_generic.conf", r#"# Universal Wi-Fi optimizations
# Applied for unknown drivers
"#),
        };

        info!("Applying {:?} driver configuration...", category);

        let modprobe_path = Path::new("/etc/modprobe.d").join(filename);
        
        if let Some(parent) = modprobe_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        match File::create(&modprobe_path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(config.as_bytes()) {
                    warn!("Failed to write driver config to {}: {}", modprobe_path.display(), e);
                } else {
                    info!("Created driver config: {}", modprobe_path.display());
                }
            },
            Err(e) => {
                warn!("Could not create driver config at {} (Read-only filesystem?): {}", modprobe_path.display(), e);
                warn!("Driver optimizations requiring persistence will NOT be applied.");
            }
        }
        
        Ok(())
    }

    /// Optimize IRQ affinity for Wi-Fi adapter
    fn optimize_irq_affinity(&self, ifc: &WifiInterface) -> Result<()> {
        info!("Optimizing IRQ affinity for {}", ifc.name);

        // Check for irqbalance
        if Command::new("pgrep").arg("irqbalance").output().map(|o| o.status.success()).unwrap_or(false) {
            warn!("'irqbalance' daemon detected! It may undo Wi-Fi IRQ pinning.");
            // We proceed anyway, but the warning is crucial for debugging
        }

        // Read /proc/interrupts to find the Wi-Fi IRQ(s)
        let interrupts = fs::read_to_string("/proc/interrupts")
            .context("Failed to read /proc/interrupts")?;

        // Special mappings for drivers that report different names in /proc/interrupts
        // - rtl8192ee reports as "rtl_pci" 
        // - ath11k uses MSI-X with multiple IRQ vectors (ath11k_pci:base, DP, CE0-CE11)
        // - Steam Deck OLED (WCN6855) may show as wcn, ath11k, or other variants
        let search_terms: Vec<&str> = match ifc.driver.as_str() {
            "rtl8192ee" => vec!["rtl_pci"],
            "ath11k_pci" | "ath11k" => vec!["ath11k", "wcn", "wlan0", "MHI"],  // WCN6855 variants
            _ => vec![ifc.driver.as_str()],
        };

        // Find ALL matching IRQs (important for MSI-X drivers like ath11k)
        let irqs: Vec<String> = interrupts.lines()
            .filter(|line| {
                search_terms.iter().any(|term| line.contains(term)) || line.contains(&ifc.name)
            })
            .filter_map(|line| line.trim().split(':').next())
            .map(|s| s.trim().to_string())
            .collect();

        if irqs.is_empty() {
            debug!("Could not find IRQ for {} (driver: {})", ifc.name, ifc.driver);
        } else {
            // Pin ALL matching IRQs to CPU 1
            let mut pinned = 0;
            for irq_num in &irqs {
                let affinity_path = format!("/proc/irq/{}/smp_affinity", irq_num);
                
                // Bind to CPU 1 (affinity mask 0x2)
                if let Err(e) = fs::write(&affinity_path, "2") {
                    warn!("Failed to set IRQ affinity for {}: {}", irq_num, e);
                } else {
                    pinned += 1;
                }
            }
            
            if irqs.len() > 1 {
                info!("Wi-Fi {} IRQs bound to CPU 1 ({} vectors)", pinned, irqs.len());
            } else {
                info!("Wi-Fi IRQ {} bound to CPU 1", irqs[0]);
            }
        }

        Ok(())
    }

    /// Apply ethtool optimizations
    fn apply_ethtool_settings(&self, ifc: &WifiInterface) -> Result<()> {
        debug!("Applying ethtool settings for {}", ifc.name);

        // Disable TSO/GSO, keep GRO enabled
        let _ = Command::new("ethtool")
            .args(["-K", &ifc.name, "tso", "off", "gso", "off", "gro", "on"])
            .output();

        Ok(())
    }

    /// Revert all system optimizations
    pub fn revert(&self) -> Result<()> {
        info!("Reverting system optimizations...");

        // Remove sysctl config
        let _ = fs::remove_file("/etc/sysctl.d/99-hifi-wifi.conf");

        // Remove modprobe configs (list all possible files)
        let modprobe_files = [
            "rtw89.conf", "rtw88.conf", "rtl_legacy.conf", "mediatek.conf",
            "iwlwifi.conf", "ath_wifi.conf", "broadcom.conf", "ralink.conf",
            "marvell.conf", "wifi_generic.conf",
        ];

        for file in modprobe_files {
            let path = Path::new("/etc/modprobe.d").join(file);
            let _ = fs::remove_file(path);
        }

        info!("System optimizations reverted");
        Ok(())
    }
}

impl Default for SystemOptimizer {
    fn default() -> Self {
        Self::new(true, true, true)
    }
}
