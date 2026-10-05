//! System-level optimizations for Wi-Fi performance
//!
//! Handles sysctl tuning, driver module parameters, IRQ affinity, and ethtool settings.

use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::network::wifi::{DriverCategory, InterfaceType, WifiInterface};

/// System optimizer for kernel and driver tuning
pub struct SystemOptimizer {
    sysctl_enabled: bool,
    irq_affinity_enabled: bool,
    driver_tweaks_enabled: bool,
    tcp_congestion_control: String,
}

impl SystemOptimizer {
    pub fn new(
        sysctl: bool,
        irq: bool,
        driver: bool,
        tcp_congestion_control: String,
    ) -> Self {
        Self {
            sysctl_enabled: sysctl,
            irq_affinity_enabled: irq,
            driver_tweaks_enabled: driver,
            tcp_congestion_control,
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
                if ifc.interface_type == InterfaceType::Wifi {
                    if let Err(e) = Self::apply_pcie_aspm_sysfs(&ifc.name, true) {
                        warn!("Failed to disable PCIe ASPM sysfs for {}: {}", ifc.name, e);
                    }
                }
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

    /// Re-apply only what a resume or link re-init can reset: PCIe link power and IRQ pinning.
    /// Deliberately no ethtool calls: offload/EEE settings survive carrier changes, and setting
    /// EEE can renegotiate an Ethernet link, which would bounce the carrier and re-trigger this.
    pub fn reapply_link_baseline(&self, ifc: &WifiInterface) -> Result<()> {
        if self.driver_tweaks_enabled && ifc.interface_type == InterfaceType::Wifi {
            Self::apply_pcie_aspm_sysfs(&ifc.name, true)?;
        }
        if self.irq_affinity_enabled {
            self.optimize_irq_affinity(ifc)?;
        }
        Ok(())
    }

    /// Apply optimizations to a single interface (used for hot-plugged/newly connected interfaces)
    pub fn apply_single_interface(&self, ifc: &WifiInterface) -> Result<()> {
        if self.driver_tweaks_enabled {
            self.apply_driver_config(&ifc.category)?;
            if ifc.interface_type == InterfaceType::Wifi {
                if let Err(e) = Self::apply_pcie_aspm_sysfs(&ifc.name, true) {
                    warn!("Failed to disable PCIe ASPM sysfs for {}: {}", ifc.name, e);
                }
            }
        }

        if self.irq_affinity_enabled {
            self.optimize_irq_affinity(ifc)?;
        }

        self.apply_ethtool_settings(ifc)?;

        Ok(())
    }


    /// Apply sysctl tuning for network performance
    fn apply_sysctl_tuning(&self) -> Result<()> {
        info!("Applying sysctl network optimizations...");

        let settings = vec![
            ("net.ipv4.tcp_congestion_control".to_string(), self.tcp_congestion_control.clone()),
            ("net.core.rmem_default".to_string(), "262144".to_string()),
            ("net.core.wmem_default".to_string(), "262144".to_string()),
            ("net.core.rmem_max".to_string(), "4194304".to_string()),
            ("net.core.wmem_max".to_string(), "4194304".to_string()),
            ("net.ipv4.tcp_rmem".to_string(), "4096 131072 4194304".to_string()),
            ("net.ipv4.tcp_wmem".to_string(), "4096 65536 4194304".to_string()),
            ("net.ipv4.tcp_fastopen".to_string(), "3".to_string()),
            ("net.core.netdev_max_backlog".to_string(), "2000".to_string()),
            ("net.core.netdev_budget".to_string(), "600".to_string()),
            ("net.core.netdev_budget_usecs".to_string(), "8000".to_string()),
            ("net.ipv4.tcp_slow_start_after_idle".to_string(), "0".to_string()),
            ("net.ipv4.tcp_ecn".to_string(), "1".to_string()),
            ("net.ipv4.tcp_keepalive_time".to_string(), "60".to_string()),
            ("net.ipv4.tcp_keepalive_intvl".to_string(), "10".to_string()),
            ("net.ipv4.tcp_keepalive_probes".to_string(), "6".to_string()),
            ("net.ipv4.tcp_tw_reuse".to_string(), "1".to_string()),
        ];

        let sysctl_path = Path::new("/etc/sysctl.d/99-hifi-wifi.conf");
        let mut config_content = String::from("# hifi-wifi Network Optimizations\n");
        for (key, val) in &settings {
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
                }
                Err(e) => {
                    warn!(
                        "Could not create sysctl config file (Read-only filesystem?): {}",
                        e
                    );
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
        for (key, val) in &settings {
            let _ = Command::new("sysctl")
                .arg("-w")
                .arg(format!("{}={}", key, val))
                .status();
        }

        Ok(())
    }

    /// Apply driver-specific module parameters
    ///
    /// References:
    /// - RTW89: https://github.com/lwfinger/rtw89 (disable_aspm_l1, disable_aspm_l1ss for HP/Lenovo)
    /// - MT7921: https://wiki.archlinux.org/title/Network_configuration/Wireless#mt7921_/_mt7922
    /// - iwlwifi: https://wiki.archlinux.org/title/Power_management#Intel_wireless_cards_(iwlwifi)
    /// - ath11k: Steam Deck OLED WCN6855 - limited params, kernel handles most
    fn apply_driver_config(&self, category: &DriverCategory) -> Result<()> {
        let (filename, config) = match category {
            DriverCategory::Rtw89 => (
                "rtw89.conf",
                r#"# Realtek RTW89 optimizations (RTL8851BE/RTL8852AE/RTL8852BE/RTL8852CE)
# Disables PCIe Active State Power Management for stability
# Required for HP/Lenovo laptops with buggy BIOS PCIe implementations
options rtw89_pci disable_aspm_l1=y disable_aspm_l1ss=y
# Disable firmware-level power save for consistent low latency
options rtw89_core disable_ps_mode=y
"#,
            ),
            DriverCategory::Rtw88 => (
                "rtw88.conf",
                r#"# Realtek RTW88 optimizations (RTL8822CE - Steam Deck LCD)
# Disables PCIe ASPM for stability and lower latency
options rtw88_pci disable_aspm=1
# Disables deep low-power states that cause reconnection issues
options rtw88_core disable_lps_deep=Y
"#,
            ),
            DriverCategory::RtlLegacy => (
                "rtl_legacy.conf",
                r#"# Legacy Realtek optimizations (RTL8192EE/RTL8188EE)
# swenc=1: Software encryption (more stable on some chips)
# ips=0: Disable inactive power save
# fwlps=0: Disable firmware low-power state
options rtl8192ee swenc=1 ips=0 fwlps=0
options rtl8188ee swenc=1 ips=0 fwlps=0
options rtl_pci disable_aspm=1
"#,
            ),
            DriverCategory::MediaTek => (
                "mediatek.conf",
                r#"# MediaTek optimizations (MT7921/MT7922/MT76)
# Fixes high latency issues documented in Arch Wiki
options mt7921e disable_aspm=1
# Disable USB scatter-gather for better stability on USB adapters
options mt76_usb disable_usb_sg=1
"#,
            ),
            DriverCategory::Intel => (
                "iwlwifi.conf",
                r#"# Intel Wi-Fi optimizations (AX200/AX201/AX210/AX211/BE200)
# power_save=0: Disable driver-level power saving
# uapsd_disable=1: Disable U-APSD (unscheduled automatic power save delivery)
#   - U-APSD can cause latency spikes during gaming
options iwlwifi power_save=0 uapsd_disable=1
# power_scheme=1: "Always Active" mode (vs 2=Balanced, 3=Low-power)
# Prevents WiFi card disappearing on battery or after suspend
options iwlmvm power_scheme=1
"#,
            ),
            DriverCategory::Atheros => (
                "ath_wifi.conf",
                r#"# Qualcomm Atheros optimizations
# ath11k: Steam Deck OLED (WCN6855) and other WiFi 6E chips
# disable_aspm=1: Prevents latency spikes from PCIe power transitions
options ath11k_pci disable_aspm=1
# ath9k: Legacy 802.11n chips (AR9285/AR9287/etc)
# ps_enable=0: Disable hardware power save
options ath9k ps_enable=0
"#,
            ),
            DriverCategory::Broadcom => (
                "broadcom.conf",
                r#"# Broadcom optimizations
options brcmfmac roamoff=1
options wl interference=0
"#,
            ),
            DriverCategory::Ralink => (
                "ralink.conf",
                r#"# Ralink/MediaTek Legacy optimizations
options rt2800usb nohwcrypt=0
options rt2800pci nohwcrypt=0
"#,
            ),
            DriverCategory::Marvell => (
                "marvell.conf",
                r#"# Marvell optimizations
options mwifiex disable_auto_ds=1
"#,
            ),
            DriverCategory::Generic => (
                "wifi_generic.conf",
                r#"# Universal Wi-Fi optimizations
# Applied for unknown drivers
"#,
            ),
        };

        info!("Applying {:?} driver configuration...", category);

        let modprobe_path = Path::new("/etc/modprobe.d").join(filename);

        if let Some(parent) = modprobe_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        match File::create(&modprobe_path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(config.as_bytes()) {
                    warn!(
                        "Failed to write driver config to {}: {}",
                        modprobe_path.display(),
                        e
                    );
                } else {
                    info!("Created driver config: {}", modprobe_path.display());
                }
            }
            Err(e) => {
                warn!(
                    "Could not create driver config at {} (Read-only filesystem?): {}",
                    modprobe_path.display(),
                    e
                );
                warn!("Driver optimizations requiring persistence will NOT be applied.");
            }
        }

        Ok(())
    }

    /// Pin the Wi-Fi adapter's interrupts to CPU 1 and record the outcome for `status`
    fn optimize_irq_affinity(&self, ifc: &WifiInterface) -> Result<()> {
        let cpus = fs::read_to_string("/sys/devices/system/cpu/online")
            .ok()
            .map(|s| s.trim() != "0")
            .unwrap_or(false);
        let irqs = find_wifi_irqs(ifc)?;
        let mut outcome = IrqOutcome {
            irqs: irqs.clone(),
            ..Default::default()
        };

        if !cpus {
            debug!("Single CPU system, skipping IRQ pinning for {}", ifc.name);
        } else if irqs.is_empty() {
            debug!(
                "No PCI interrupts for {} (driver: {}); USB/SDIO devices have none to pin",
                ifc.name, ifc.driver
            );
        } else {
            if Command::new("pgrep")
                .arg("irqbalance")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                warn!("'irqbalance' is running and may undo Wi-Fi IRQ pinning.");
            }
            for irq in &irqs {
                let path = format!("/proc/irq/{}/smp_affinity", irq);
                // Already pinned: skip the write
                if fs::read_to_string(&path).ok().as_deref().is_some_and(mask_is_cpu1) {
                    outcome.pinned.push(irq.clone());
                    continue;
                }
                match fs::write(&path, "2") {
                    Ok(()) => outcome.pinned.push(irq.clone()),
                    // EIO: kernel-managed MSI-X vector, affinity is fixed by the kernel
                    Err(e) if e.raw_os_error() == Some(5) => outcome.managed.push(irq.clone()),
                    Err(e) => {
                        debug!("Cannot set affinity of IRQ {}: {}", irq, e);
                        outcome.failed.push(irq.clone());
                    }
                }
            }
            info!(
                "Wi-Fi IRQs for {}: {} pinned to CPU 1, {} kernel-managed, {} failed ({} total)",
                ifc.name,
                outcome.pinned.len(),
                outcome.managed.len(),
                outcome.failed.len(),
                irqs.len()
            );
        }

        outcome.save(&ifc.name);
        Ok(())
    }

    /// Apply ethtool optimizations
    fn apply_ethtool_settings(&self, ifc: &WifiInterface) -> Result<()> {
        debug!("Applying ethtool settings for {}", ifc.name);

        // GRO is safe and beneficial for both media types as it coalesces incoming packets
        let _ = Command::new("ethtool")
            .args(["-K", &ifc.name, "gro", "on"])
            .output();

        // Restrict TSO/GSO disablement strictly to wireless interfaces
        if ifc.interface_type == InterfaceType::Wifi {
            info!("Disabling TSO/GSO on wireless interface {} to minimize airtime jitter", ifc.name);
            let _ = Command::new("ethtool")
                .args(["-K", &ifc.name, "tso", "off", "gso", "off"])
                .output();
        } else {
            info!("Preserving hardware TSO/GSO offloading on wired interface {} for wire-speed throughput", ifc.name);
            let _ = Command::new("ethtool")
                .args(["-K", &ifc.name, "tso", "on", "gso", "on"])
                .output();
        }

        // Ethernet-specific optimizations for streaming/gaming
        if ifc.interface_type == InterfaceType::Ethernet {
            info!("Applying ethernet streaming optimizations for {}", ifc.name);

            // Disable Energy Efficient Ethernet (EEE) - causes micro-stutters in streaming.
            // Only when it is on: some drivers renegotiate the link on every --set-eee.
            match crate::network::tc::EthtoolManager::set_eee(&ifc.name, false) {
                Ok(true) => info!(
                    "Disabled EEE (Energy Efficient Ethernet) on {} for low latency",
                    ifc.name
                ),
                Ok(false) => debug!("EEE already off or unsupported on {}", ifc.name),
                Err(e) => debug!("EEE command failed: {}", e),
            }
        }

        // Dynamically check if the interface driver supports ethtool coalescing queries
        let coalescing_supported = Command::new("ethtool")
            .args(["-c", &ifc.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if coalescing_supported {
            // Set initial low-latency coalescing defaults
            // rx-usecs=0, rx-frames=1 means "interrupt immediately on every packet"
            let coal_result = Command::new("ethtool")
                .args([
                    "-C",
                    &ifc.name,
                    "rx-usecs",
                    "0",
                    "rx-frames",
                    "1",
                    "tx-usecs",
                    "0",
                    "tx-frames",
                    "1",
                ])
                .output();

            match coal_result {
                Ok(output) if output.status.success() => {
                    info!("Set low-latency interrupt coalescing on {}", ifc.name);
                }
                Ok(_) => debug!(
                    "Coalescing settings may not be fully supported on {}",
                    ifc.name
                ),
                Err(e) => debug!("Coalescing command failed: {}", e),
            }

            // Disable adaptive coalescing
            let _ = Command::new("ethtool")
                .args(["-C", &ifc.name, "adaptive-rx", "off", "adaptive-tx", "off"])
                .output();
        } else {
            debug!(
                "Interrupt coalescing query not supported on {}. Skipping coalescing configuration.",
                ifc.name
            );
        }

        Ok(())
    }

    /// Enable (disable ASPM / enforce power on) or disable (restore ASPM / auto power) PCIe ASPM for the interface
    pub fn apply_pcie_aspm_sysfs(iface_name: &str, enable: bool) -> Result<()> {
        let device_path = format!("/sys/class/net/{}/device", iface_name);
        let device_path = match fs::canonicalize(&device_path) {
            Ok(p) => p,
            Err(_) => {
                debug!(
                    "Interface {} does not have a physical sysfs device path",
                    iface_name
                );
                return Ok(());
            }
        };

        let link_dir = device_path.join("link");
        if link_dir.is_dir() {
            let val = if enable { "0" } else { "1" };
            let aspm_files = [
                "l0s_aspm",
                "l1_aspm",
                "l1_1_aspm",
                "l1_2_aspm",
                "l1_1_pcipm",
                "l1_2_pcipm",
            ];
            for filename in &aspm_files {
                let filepath = link_dir.join(filename);
                // Writing a link state, even an unchanged one, can retrain the PCIe link and
                // briefly stall the Wi-Fi card. Only write when it actually differs.
                let current = fs::read_to_string(&filepath).ok();
                if current.as_deref().map(str::trim) == Some(val) {
                    continue;
                }
                if filepath.exists() {
                    match fs::write(&filepath, val) {
                        Ok(_) => debug!("Set ASPM state in {} to {}", filepath.display(), val),
                        Err(e) => debug!(
                            "Failed to write to {} (unsupported or permission denied): {}",
                            filepath.display(),
                            e
                        ),
                    }
                }
            }
        }

        let power_control = device_path.join("power").join("control");
        let val = if enable { "on" } else { "auto" };
        let current = fs::read_to_string(&power_control).ok();
        if power_control.exists() && current.as_deref().map(str::trim) != Some(val) {
            match fs::write(&power_control, val) {
                Ok(_) => info!(
                    "Set runtime PCI power control to '{}' for {}",
                    val, iface_name
                ),
                Err(e) => warn!(
                    "Failed to write to {} (runtime power control): {}",
                    power_control.display(),
                    e
                ),
            }
        }

        Ok(())
    }

    /// Revert all system optimizations
    pub fn revert(&self) -> Result<()> {
        info!("Reverting system optimizations...");

        // Remove sysctl config
        let _ = fs::remove_file("/etc/sysctl.d/99-hifi-wifi.conf");

        // Remove modprobe configs (list all possible files)
        let modprobe_files = [
            "rtw89.conf",
            "rtw88.conf",
            "rtl_legacy.conf",
            "mediatek.conf",
            "iwlwifi.conf",
            "ath_wifi.conf",
            "broadcom.conf",
            "ralink.conf",
            "marvell.conf",
            "wifi_generic.conf",
        ];

        for file in modprobe_files {
            let path = Path::new("/etc/modprobe.d").join(file);
            let _ = fs::remove_file(path);
        }

        // Revert PCIe ASPM and TSO/GSO/gro offloads for any physical interfaces
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.filter_map(|e| e.ok()) {
                let iface_name = entry.file_name().to_string_lossy().into_owned();
                let device_path = format!("/sys/class/net/{}/device", iface_name);
                if Path::new(&device_path).exists() {
                    // Revert TSO/GSO/gro to default enabled state
                    let _ = Command::new("ethtool")
                        .args(["-K", &iface_name, "tso", "on", "gso", "on", "gro", "on"])
                        .output();

                    let phy_path = format!("/sys/class/net/{}/phy80211", iface_name);
                    if Path::new(&phy_path).exists() || iface_name.starts_with('w') {
                        let _ = Self::apply_pcie_aspm_sysfs(&iface_name, false);
                    }
                }
            }
        }

        // Reload system default sysctl parameters to clear our runtime updates
        let _ = Command::new("sysctl").arg("--system").output();

        info!("System optimizations reverted");
        Ok(())
    }
}

impl Default for SystemOptimizer {
    fn default() -> Self {
        Self::new(true, true, true, "bbr".to_string())
    }
}

/// Helper to find all IRQs associated with a Wi-Fi interface in /proc/interrupts.
/// Returns a list of IRQ numbers.
/// Result of the last IRQ pinning attempt, saved for `hifi-wifi status`
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct IrqOutcome {
    pub irqs: Vec<String>,
    pub pinned: Vec<String>,
    pub managed: Vec<String>,
    pub failed: Vec<String>,
}

impl IrqOutcome {
    fn path(iface: &str) -> String {
        format!("/run/hifi-wifi/irq-{}.json", iface)
    }

    fn save(&self, iface: &str) {
        let _ = fs::create_dir_all("/run/hifi-wifi");
        if let Ok(json) = serde_json::to_string(self) {
            let _ = fs::write(Self::path(iface), json);
        }
    }

    pub fn load(iface: &str) -> Option<Self> {
        serde_json::from_str(&fs::read_to_string(Self::path(iface)).ok()?).ok()
    }
}

/// smp_affinity is a hex mask, possibly comma-grouped ("00000000,00000002")
pub fn mask_is_cpu1(mask: &str) -> bool {
    let hex: String = mask.trim().chars().filter(|c| *c != ',').collect();
    !hex.is_empty() && hex.trim_start_matches('0') == "2"
}

/// Interrupt numbers of the device behind `ifc`.
/// sysfs is authoritative (MSI/MSI-X vectors, else the legacy line); matching driver names in
/// /proc/interrupts is a fallback, since many drivers label their vectors differently.
pub fn find_wifi_irqs(ifc: &WifiInterface) -> Result<Vec<String>> {
    // The net device's "device" is usually the PCI function itself; for some buses (virtio,
    // SDIO bridges) the PCI function is its parent
    let dev = format!("/sys/class/net/{}/device", ifc.name);
    for d in [dev.clone(), format!("{}/..", dev)] {
        if let Ok(entries) = fs::read_dir(format!("{}/msi_irqs", d)) {
            let mut irqs: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.parse::<u32>().is_ok())
                .collect();
            if !irqs.is_empty() {
                irqs.sort_by_key(|n| n.parse::<u32>().unwrap_or(0));
                return Ok(irqs);
            }
        }
        if let Some(irq) = fs::read_to_string(format!("{}/irq", d))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| s.parse::<u32>().is_ok_and(|n| n > 0))
        {
            if Path::new(&format!("/proc/irq/{}", irq)).exists() {
                return Ok(vec![irq]);
            }
        }
    }

    let interrupts =
        fs::read_to_string("/proc/interrupts").context("Failed to read /proc/interrupts")?;
    let search_terms: Vec<&str> = match ifc.driver.as_str() {
        "rtl8192ee" => vec!["rtl_pci"],
        "rtw88_8822ce" | "rtw88_pci" | "rtw_pci" => vec!["rtw88", "rtw_pci", &ifc.name],
        "ath11k_pci" | "ath11k" => vec!["ath11k", "wcn", "mhi", "bhi", &ifc.name],
        _ => vec![ifc.driver.as_str(), &ifc.name],
    };
    Ok(parse_proc_interrupts(&interrupts, &search_terms))
}

fn parse_proc_interrupts(interrupts: &str, terms: &[&str]) -> Vec<String> {
    interrupts
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            terms
                .iter()
                .filter(|t| !t.is_empty())
                .any(|t| lower.contains(&t.to_lowercase()))
        })
        .filter_map(|line| line.trim().split(':').next())
        .map(|s| s.trim().to_string())
        .filter(|s| s.parse::<u32>().is_ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_masks() {
        assert!(mask_is_cpu1("2\n"));
        assert!(mask_is_cpu1("02"));
        assert!(mask_is_cpu1("00000000,00000002"));
        assert!(!mask_is_cpu1("ff"));
        assert!(!mask_is_cpu1("20"));
        assert!(!mask_is_cpu1(""));
    }

    #[test]
    fn proc_interrupts_fallback_matches_only_numbered_lines() {
        let table = "           CPU0       CPU1\n  58:          0       1234  PCI-MSI 1048576-edge      rtw88_pci\n  61:         10          0  PCI-MSI 524288-edge      nvme0q0\nNMI:          0          0   Non-maskable interrupts\n";
        assert_eq!(parse_proc_interrupts(table, &["rtw88", ""]), vec!["58".to_string()]);
        assert!(parse_proc_interrupts(table, &["iwlwifi"]).is_empty());
    }

    /// Needs root and a PCI NIC: `cargo test -- --ignored irq_real_device`
    #[test]
    #[ignore]
    fn irq_real_device() {
        let name = fs::read_dir("/sys/class/net")
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| Path::new(&format!("/sys/class/net/{}/device", n)).exists())
            .expect("no device-backed interface");
        let ifc = WifiInterface {
            name: name.clone(),
            driver: "unknown".into(),
            category: DriverCategory::Generic,
            interface_type: InterfaceType::Ethernet,
            is_active: true,
        };
        let irqs = find_wifi_irqs(&ifc).unwrap();
        assert!(!irqs.is_empty(), "no IRQs found for {}", name);
        SystemOptimizer::new(false, true, false, "bbr".into()).optimize_irq_affinity(&ifc).unwrap();
        let o = IrqOutcome::load(&name).unwrap();
        assert_eq!(o.irqs, irqs);
        assert_eq!(o.pinned.len() + o.managed.len() + o.failed.len(), irqs.len());
        println!("{}: {:?}", name, o);
    }
}
