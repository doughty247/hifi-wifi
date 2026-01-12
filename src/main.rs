mod network;
mod system;
mod config;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use log::{info, error, warn};

use crate::config::loader::load_config;
use crate::network::wifi::{WifiManager, WifiInterface};
use crate::network::backend_tuner::BackendTuner;
use crate::network::governor::Governor;
use crate::system::power::PowerManager;
use crate::system::optimizer::SystemOptimizer;

#[derive(Parser)]
#[command(name = "hifi-wifi")]
#[command(version = "3.0.0-beta.2")]
#[command(about = "High Fidelity WiFi optimizer for Linux Streaming Handhelds", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    
    /// Run without making changes (show what would be done)
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Apply Wi-Fi optimizations once (default)
    Apply,
    /// Run as daemon with continuous monitoring
    Monitor,
    /// Revert all optimizations to defaults
    Revert,
    /// Show current Wi-Fi status and detected hardware
    Status,
    /// Install system service for automatic optimization
    Install,
    /// Uninstall system service
    Uninstall,
    /// Stop service and revert optimizations (for A/B testing)
    Off,
    /// Start service and apply optimizations (for A/B testing)
    On,
    /// Bootstrap: Check and repair system service (runs on boot via user timer)
    Bootstrap,
}

#[tokio::main]
async fn main() -> Result<()> {
    utils::logger::init();
    
    let cli = Cli::parse();

    // Suppress INFO logs for status command (clean output)
    if matches!(cli.command, Some(Commands::Status)) {
        log::set_max_level(log::LevelFilter::Warn);
    }

    // Self-repair: If CLI symlink is missing (SteamOS update wiped it), restore it
    // This runs on EVERY invocation - makes hifi-wifi self-healing
    // Must run before root check since status doesn't need root but repair does
    if utils::privilege::is_root() {
        quick_self_repair();
    }

    // Root check (except for status command)
    if !matches!(cli.command, Some(Commands::Status)) && !utils::privilege::is_root() {
        error!("This application must be run as root.");
        error!("Try: sudo hifi-wifi");
        std::process::exit(1);
    }

    let config = load_config();

    match cli.command.unwrap_or(Commands::Apply) {
        Commands::Apply => {
            if cli.dry_run {
                info!("[DRY-RUN] Would apply the following optimizations:");
                run_dry_run()?;
            } else {
                run_apply(&config)?;
            }
        }
        Commands::Monitor => {
            run_monitor(&config).await?;
        }
        Commands::Revert => {
            run_revert()?;
        }
        Commands::Status => {
            run_status_async().await?;
        }
        Commands::Install => {
            run_install()?;
        }
        Commands::Uninstall => {
            run_uninstall()?;
        }
        Commands::Off => {
            run_off()?;
        }
        Commands::On => {
            run_on()?;
        }
        Commands::Bootstrap => {
            run_bootstrap()?;
        }
    }

    Ok(())
}

fn run_apply(config: &config::structs::Config) -> Result<()> {
    info!("=== hifi-wifi v3.0 ===");
    info!("Applying Wi-Fi optimizations...\n");

    // 1. Detect Wi-Fi interfaces
    let wifi_mgr = WifiManager::new()?;
    let interfaces = wifi_mgr.interfaces();
    
    if interfaces.is_empty() {
        error!("No Wi-Fi interfaces detected!");
        return Ok(());
    }

    for ifc in interfaces {
        info!("Found: {} (driver: {}, category: {:?})", 
              ifc.name, ifc.driver, ifc.category);
    }

    // 2. Detect power state
    let power_mgr = PowerManager::new();
    info!("Device type: {:?}", power_mgr.device_type());
    info!("Power source: {:?}", power_mgr.power_source());

    // 3. Apply system optimizations
    if config.system.sysctl_enabled || config.system.driver_tweaks_enabled || config.system.irq_affinity_enabled {
        let sys_opt = SystemOptimizer::new(
            config.system.sysctl_enabled,
            config.system.irq_affinity_enabled,
            config.system.driver_tweaks_enabled,
        );
        
        // Only optimize connected/active interfaces
        let active_interfaces: Vec<WifiInterface> = interfaces
            .iter()
            .filter(|ifc| wifi_mgr.is_interface_connected(ifc))
            .cloned()
            .collect();
        
        if active_interfaces.is_empty() {
            warn!("No active network connections - skipping IRQ optimizations");
        } else {
            info!("Optimizing {} active interface(s)", active_interfaces.len());
            sys_opt.apply(&active_interfaces)?;
        }
    }

    // 4. Apply power-aware settings
    for ifc in interfaces {
        // Skip disconnected interfaces
        if !wifi_mgr.is_interface_connected(ifc) {
            info!("Skipping {} (not connected)", ifc.name);
            continue;
        }
        
        info!("Optimizing connected interface: {}", ifc.name);
        let should_save = match config.power.wlan_power_save.as_str() {
            "on" => {
                info!("Power save forced ON by config on {}", ifc.name);
                true
            },
            "off" => {
                info!("Power save forced OFF by config on {}", ifc.name);
                false
            },
            _ => { // adaptive
                let adaptive = power_mgr.should_enable_power_save();
                if adaptive {
                    info!("On battery - enabling power save on {}", ifc.name);
                } else {
                    info!("On AC/Desktop - disabling power save on {}", ifc.name);
                }
                adaptive
            }
        };

        if should_save {
            wifi_mgr.enable_power_save(ifc)?;
        } else {
            wifi_mgr.disable_power_save(ifc)?;
        }

        // 5. Get link stats and apply CAKE
        // Always apply CAKE, even if we can't get link stats
        let bandwidth = match wifi_mgr.get_link_stats(ifc) {
            Ok(stats) if stats.tx_bitrate_mbps > 0.0 => {
                info!("Link: {}Mbps TX, {}dBm signal", stats.tx_bitrate_mbps, stats.signal_dbm);
                // Use 60% of link rate for realistic Wi-Fi throughput
                (stats.tx_bitrate_mbps * 0.60) as u32
            }
            Ok(stats) => {
                warn!("Link stats returned 0 bitrate (signal: {}dBm), using 200Mbit default", stats.signal_dbm);
                200
            }
            Err(e) => {
                warn!("Failed to get link stats: {}, using 200Mbit default", e);
                200
            }
        };
        
        if let Err(e) = wifi_mgr.apply_cake(ifc, bandwidth.max(1)) {
            error!("Failed to apply CAKE on {}: {}", ifc.name, e);
        }
    }

    // 6. Apply backend tuning
    if config.backend.iwd_periodic_scan_disable {
        let backend_tuner = BackendTuner::new(true);
        backend_tuner.apply()?;
    }

    info!("\n=== Optimization Complete ===");
    Ok(())
}

fn run_dry_run() -> Result<()> {
    let wifi_mgr = WifiManager::new()?;
    let power_mgr = PowerManager::new();
    
    info!("  - Detected {} Wi-Fi interface(s)", wifi_mgr.interfaces().len());
    for ifc in wifi_mgr.interfaces() {
        info!("    * {} ({:?})", ifc.name, ifc.category);
    }
    
    info!("  - Device type: {:?}", power_mgr.device_type());
    info!("  - Power source: {:?}", power_mgr.power_source());
    
    if power_mgr.should_enable_power_save() {
        info!("  - Would ENABLE power save (on battery)");
    } else {
        info!("  - Would DISABLE power save (performance mode)");
    }
    
    info!("  - Would create /etc/sysctl.d/99-hifi-wifi.conf");
    info!("  - Would create driver-specific modprobe config");
    info!("  - Would apply CAKE qdisc for bufferbloat mitigation");
    info!("  - Would optimize IRQ affinity");
    
    Ok(())
}

fn run_revert() -> Result<()> {
    info!("=== Reverting hifi-wifi Optimizations ===\n");

    let wifi_mgr = WifiManager::new()?;
    
    // Remove CAKE qdiscs and restore defaults
    for ifc in wifi_mgr.interfaces() {
        // Only operate on connected interfaces
        if !wifi_mgr.is_interface_connected(ifc) {
            info!("Skipping {} (not connected)", ifc.name);
            continue;
        }
        
        info!("Reverting optimizations on {}", ifc.name);
        wifi_mgr.remove_cake(ifc)?;
        
        // Restore power-related defaults based on interface type
        match ifc.interface_type {
            crate::network::wifi::InterfaceType::Wifi => {
                // Re-enable WiFi power save (safe default)
                let _ = wifi_mgr.enable_power_save(ifc);
            },
            crate::network::wifi::InterfaceType::Ethernet => {
                // Re-enable EEE on ethernet (power saving default)
                let _ = crate::network::tc::EthtoolManager::enable_eee(&ifc.name);
                info!("Re-enabled EEE on {}", ifc.name);
            }
        }
    }

    // Revert system optimizations
    let sys_opt = SystemOptimizer::default();
    sys_opt.revert()?;

    // Revert backend tuning
    let backend_tuner = BackendTuner::default();
    backend_tuner.revert()?;

    info!("\n=== Revert Complete ===");
    Ok(())
}

/// Check if we're running on SteamOS
fn is_steamos() -> bool {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        content.contains("ID=steamos")
    } else {
        false
    }
}

/// Quick self-repair: Restore CLI symlink and systemd service if missing
/// This runs on EVERY root invocation to make hifi-wifi self-healing after SteamOS updates
/// SteamOS wipes /etc and /usr on updates, but /var persists - so we repair from there
fn quick_self_repair() {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::process::Command;
    
    let binary_path = Path::new("/var/lib/hifi-wifi/hifi-wifi");
    let symlink_path = Path::new("/usr/local/bin/hifi-wifi");
    let service_path = Path::new("/etc/systemd/system/hifi-wifi.service");
    
    // Only repair if our binary exists in persistent storage
    if !binary_path.exists() {
        return;
    }
    
    let mut needs_repair = false;
    
    // Check if CLI symlink is missing
    if !symlink_path.exists() {
        needs_repair = true;
    }
    
    // Check if service file is missing
    if !service_path.exists() {
        needs_repair = true;
    }
    
    if !needs_repair {
        return;
    }
    
    // On SteamOS, disable read-only filesystem for repairs
    let steamos = is_steamos();
    if steamos {
        let _ = Command::new("systemd-sysext").arg("unmerge").output();
        let _ = Command::new("steamos-readonly").arg("disable").output();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    
    // Repair CLI symlink
    if !symlink_path.exists() {
        eprintln!("[hifi-wifi] Self-repair: Restoring CLI symlink...");
        let _ = std::fs::remove_file(symlink_path); // Remove broken symlink if any
        if symlink(binary_path, symlink_path).is_ok() {
            eprintln!("[hifi-wifi] CLI symlink restored: {} -> {}", symlink_path.display(), binary_path.display());
        }
    }
    
    // Repair systemd service file
    if !service_path.exists() {
        eprintln!("[hifi-wifi] Self-repair: Restoring systemd service...");
        let service_content = r#"[Unit]
Description=hifi-wifi Network Optimizer
Documentation=https://github.com/doughty247/hifi-wifi
After=network-online.target NetworkManager.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/var/lib/hifi-wifi/hifi-wifi monitor
Restart=on-failure
RestartSec=5

# Security hardening
ProtectSystem=full
ProtectHome=true
NoNewPrivileges=false
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN

# Resource limits
MemoryMax=64M
CPUQuota=10%

[Install]
WantedBy=multi-user.target
"#;
        if let Ok(mut file) = std::fs::File::create(service_path) {
            use std::io::Write;
            if file.write_all(service_content.as_bytes()).is_ok() {
                eprintln!("[hifi-wifi] Systemd service restored");
                // Reload and enable
                let _ = Command::new("systemctl").args(["daemon-reload"]).output();
                let _ = Command::new("systemctl").args(["enable", "hifi-wifi.service"]).output();
                let _ = Command::new("systemctl").args(["start", "hifi-wifi.service"]).output();
            }
        }
    }
    
    // Re-enable read-only on SteamOS
    if steamos {
        let _ = Command::new("steamos-readonly").arg("enable").output();
    }
}

/// Self-healing routine to ensure the binary is accessible via CLI
/// On SteamOS, this handles read-only filesystem after system updates
fn ensure_symlinks() {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::process::Command;
    use log::{info, warn, debug};

    let target = "/var/lib/hifi-wifi/hifi-wifi";
    // Prefer /usr/local/bin as it's often writable/overlayed on Bazzite/Silverblue
    let link_path = "/usr/local/bin/hifi-wifi";

    if !Path::new(target).exists() {
        warn!("Source binary not found at {}, skipping symlink creation", target);
        return;
    }

    // Check if symlink already exists and is correct
    if Path::new(link_path).exists() {
        if let Ok(existing_target) = std::fs::read_link(link_path) {
            if existing_target.to_string_lossy() == target {
                debug!("Symlink already exists and is correct");
                return;
            }
        }
        // Symlink exists but points somewhere else - remove it
        let _ = std::fs::remove_file(link_path);
    }

    info!("Self-Healing: Creating symlink {} -> {}", link_path, target);
    
    // On SteamOS, we need to disable read-only filesystem first
    let steamos = is_steamos();
    if steamos {
        debug!("SteamOS detected, disabling read-only filesystem for symlink");
        // Unmerge system extensions first
        let _ = Command::new("systemd-sysext")
            .arg("unmerge")
            .output();
        
        // Disable read-only
        let _ = Command::new("steamos-readonly")
            .arg("disable")
            .output();
        
        // Small delay for filesystem to settle
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Try to create symlink
    if let Err(e) = symlink(target, link_path) {
        warn!("Failed to create symlink at {}: {}", link_path, e);
    } else {
        info!("Symlink created successfully.");
    }

    // Re-enable read-only on SteamOS
    if steamos {
        debug!("Re-enabling SteamOS read-only filesystem");
        let _ = Command::new("steamos-readonly")
            .arg("enable")
            .output();
    }
}

/// Run the Governor in monitor mode (daemon)
async fn run_monitor(config: &config::structs::Config) -> Result<()> {
    info!("=== hifi-wifi v3.0 Monitor Mode ===");
    // Crucial for immutable distros (Bazzite/Silverblue) where /usr/bin is read-only
    ensure_symlinks();

    info!("Starting continuous optimization daemon...\n");

    // Apply initial optimizations
    run_apply(config)?;

    // Start the Governor
    let mut governor = Governor::new(config.governor.clone(), config.wifi.clone()).await?;
    
    info!("Governor initialized, entering main loop (tick: {}s)", 
          config.global.tick_rate_secs);
    
    // Handle graceful shutdown
    let ctrl_c = tokio::signal::ctrl_c();
    
    tokio::select! {
        result = governor.run(config.global.tick_rate_secs) => {
            if let Err(e) = result {
                error!("Governor error: {}", e);
            }
        }
        _ = ctrl_c => {
            info!("\nReceived shutdown signal");
            governor.stop();
        }
    }

    info!("Monitor mode stopped");
    Ok(())
}

/// Convert WiFi frequency (MHz) to channel number
fn freq_to_channel(freq: u32) -> u32 {
    match freq {
        // 2.4 GHz band
        2412 => 1, 2417 => 2, 2422 => 3, 2427 => 4, 2432 => 5,
        2437 => 6, 2442 => 7, 2447 => 8, 2452 => 9, 2457 => 10,
        2462 => 11, 2467 => 12, 2472 => 13, 2484 => 14,
        // 5 GHz band (common channels)
        5180 => 36, 5200 => 40, 5220 => 44, 5240 => 48,
        5260 => 52, 5280 => 56, 5300 => 60, 5320 => 64,
        5500 => 100, 5520 => 104, 5540 => 108, 5560 => 112,
        5580 => 116, 5600 => 120, 5620 => 124, 5640 => 128,
        5660 => 132, 5680 => 136, 5700 => 140, 5720 => 144,
        5745 => 149, 5765 => 153, 5785 => 157, 5805 => 161, 5825 => 165,
        // 6 GHz band (common channels)
        5955 => 1, 5975 => 5, 5995 => 9, 6015 => 13,
        6035 => 17, 6055 => 21, 6075 => 25, 6095 => 29,
        6115 => 33, 6135 => 37, 6155 => 41, 6175 => 45,
        6195 => 49, 6215 => 53, 6235 => 57, 6255 => 61,
        6275 => 65, 6295 => 69, 6315 => 73, 6335 => 77,
        // Fallback: calculate from frequency
        f if f >= 2400 && f <= 2500 => (f - 2407) / 5,
        f if f >= 5150 && f <= 5900 => (f - 5000) / 5,
        f if f >= 5925 && f <= 7125 => (f - 5950) / 5,
        _ => 0,
    }
}

/// Run status with async NetworkManager info
async fn run_status_async() -> Result<()> {
    use crate::network::nm::NmClient;
    use std::process::Command;

    // ANSI Colors
    const RED: &str = "\x1b[0;31m";
    const GREEN: &str = "\x1b[0;32m";
    const YELLOW: &str = "\x1b[0;33m";
    const BLUE: &str = "\x1b[0;34m";
    const CYAN: &str = "\x1b[0;36m";
    const BOLD: &str = "\x1b[1m";
    const DIM: &str = "\x1b[2m";
    const NC: &str = "\x1b[0m";

    println!();
    println!("{}{}{}", BOLD, CYAN, "══════════════════════════════════════");
    println!("       hifi-wifi v3.0 Status");
    println!("{}{}{}", BOLD, CYAN, "══════════════════════════════════════");
    println!();

    // 1. Service Status
    let service_active = Command::new("systemctl")
        .args(["is-active", "--quiet", "hifi-wifi.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if service_active {
        println!("{}Status:{}      {}[ACTIVE]{}", BOLD, NC, GREEN, NC);
    } else {
        println!("{}Status:{}      {}[INACTIVE]{}", BOLD, NC, RED, NC);
    }
    println!();

    // 2. System and Power
    let power_mgr = PowerManager::new();
    println!("{}{}{}┌─ System Info{}", BOLD, BLUE, NC, NC);
    println!("{}│{}  Device: {:?}", BLUE, NC, power_mgr.device_type());
    let bat_pct = power_mgr.battery_percentage().map(|p| format!("{}%", p)).unwrap_or("N/A".to_string());
    println!("{}│{}  Power:  {:?} (Battery: {})", BLUE, NC, power_mgr.power_source(), bat_pct);
    println!("{}└{}", BLUE, NC);
    println!();

    // 3. Interfaces & Tweaks (CAKE, Power Save)
    let wifi_mgr = WifiManager::new_quiet()?;
    println!("{}{}{}┌─ Interfaces & Tweaks{}", BOLD, BLUE, NC, NC);
    
    if wifi_mgr.interfaces().is_empty() {
         println!("{}│{}  {}No network interfaces detected{}", BLUE, NC, DIM, NC);
    }

    for ifc in wifi_mgr.interfaces() {
        let ifc_type = match ifc.interface_type {
            crate::network::wifi::InterfaceType::Wifi => "WiFi",
            crate::network::wifi::InterfaceType::Ethernet => "Ethernet",
        };
        println!("{}│{}  {}{}{} (Type: {}, Driver: {}, {:?})", BLUE, NC, BOLD, ifc.name, NC, ifc_type, ifc.driver, ifc.category);

        // CAKE Status (tc)
        let qdisc_out = Command::new("tc")
            .args(["qdisc", "show", "dev", &ifc.name])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        
        if qdisc_out.contains("cake") {
             // Extract bandwidth if possible
             let bw = qdisc_out.split("bandwidth ").nth(1)
                .and_then(|s| s.split_whitespace().next())
                .unwrap_or("unknown");
             println!("{}│{}    ├─ CAKE:       {}[ACTIVE]{} Bandwidth: {}", BLUE, NC, GREEN, NC, bw);
        } else {
             println!("{}│{}    ├─ CAKE:       {}[INACTIVE]{}", BLUE, NC, RED, NC);
        }

        // Power Save (iw) - WiFi only
        if ifc.interface_type == crate::network::wifi::InterfaceType::Wifi {
            let ps_out = Command::new("iw")
                .args(["dev", &ifc.name, "get", "power_save"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            
            let ps_status = if ps_out.contains("on") {
                 format!("{}[ON]{} (Power Saving)", YELLOW, NC)
            } else {
                 format!("{}[OFF]{} (Performance)", GREEN, NC)
            };
            println!("{}│{}    ├─ Power Save: {}", BLUE, NC, ps_status);
        } else {
            // For ethernet, show EEE status instead
            let eee_out = Command::new("ethtool")
                .args(["--show-eee", &ifc.name])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            
            let eee_status = if eee_out.contains("EEE status: disabled") {
                format!("{}[DISABLED]{} (Low Latency)", GREEN, NC)
            } else if eee_out.contains("EEE status: enabled") {
                format!("{}[ENABLED]{} (Power Saving)", YELLOW, NC)
            } else if eee_out.contains("not supported") || eee_out.contains("Operation not supported") {
                format!("{}[N/A]{} (Not Supported)", DIM, NC)
            } else {
                format!("{}[UNKNOWN]{}", DIM, NC)
            };
            println!("{}│{}    ├─ EEE:        {}", BLUE, NC, eee_status);
        }

        // IRQ Affinity
        let irq_out = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
        
        // USB devices don't have dedicated IRQs we can pin easily
        let is_usb = ifc.driver.contains("usb") || ifc.name.contains("usb") || ifc.driver.starts_with("rt2800usb");

        let irq_status = if is_usb {
             format!("{}[N/A]{} (USB Device)", DIM, NC)
        } else {
            // Special mappings for drivers that report different names in /proc/interrupts
            // - rtl8192ee reports as "rtl_pci"
            // - rtw88_8822ce (Steam Deck LCD) may show as rtw88, rtw_pci, or interface name
            // - ath11k uses MSI-X with multiple IRQ vectors (ath11k_pci:base, DP, CE0-CE11, MHI)
            // - Steam Deck OLED (WCN6855) may show as wcn, ath11k, MHI, or other variants
            let search_terms: Vec<&str> = match ifc.driver.as_str() {
                "rtl8192ee" => vec!["rtl_pci"],
                "rtw88_8822ce" | "rtw88_pci" | "rtw_pci" => vec!["rtw88", "rtw_pci", &ifc.name],
                "ath11k_pci" | "ath11k" => vec!["ath11k", "wcn", "MHI", &ifc.name],
                _ => vec![ifc.driver.as_str(), &ifc.name],
            };

            // Find ALL matching IRQs
            let irq_lines: Vec<&str> = irq_out.lines()
                .filter(|l| search_terms.iter().any(|t| l.contains(t)) || l.contains(&ifc.name))
                .collect();
            
            if !irq_lines.is_empty() {
                 // Check if ALL IRQs are pinned to CPU1
                 let mut all_optimized = true;
                 let mut all_found = true;
                 let mut total = 0;
                 let mut optimized = 0;
                 
                 for line in &irq_lines {
                     let irq_num = line.trim().split(':').next().unwrap_or("?");
                     if let Ok(affinity) = std::fs::read_to_string(format!("/proc/irq/{}/smp_affinity", irq_num)) {
                         total += 1;
                         let aff = affinity.trim();
                         // Check if pinned to CPU1 (mask 0x2 in various formats)
                         let is_cpu1 = aff == "2" || aff == "02" || aff == "00000002" || aff == "000002";
                         if is_cpu1 {
                             optimized += 1;
                         } else {
                             all_optimized = false;
                         }
                     } else {
                         all_found = false;
                     }
                 }
                 
                 if total == 0 || !all_found {
                     format!("{}[UNKNOWN]{}", DIM, NC)
                 } else if all_optimized {
                     if total > 1 {
                         format!("{}[OPTIMIZED]{} (CPU 1, {} vectors)", GREEN, NC, total)
                     } else {
                         format!("{}[OPTIMIZED]{} (CPU 1)", GREEN, NC)
                     }
                 } else if optimized == 0 {
                     // No IRQs pinned = default system distribution
                     format!("{}[DEFAULT]{} (System Managed)", DIM, NC)
                 } else {
                     format!("{}[PARTIAL]{} ({}/{} pinned)", YELLOW, NC, optimized, total)
                 }
            } else {
                 format!("{}[NOT FOUND]{}", DIM, NC)
            }
        };
        println!("{}│{}    └─ IRQ Pin:    {}", BLUE, NC, irq_status);
        println!("{}│{}", BLUE, NC);
    }
    println!("{}└{}", BLUE, NC);
    println!();

    // 4. Backend & Governor
    let backend = BackendTuner::default();
    println!("{}{}{}┌─ Network Governor & Backend{}", BOLD, BLUE, NC, NC);
    println!("{}│{}  Backend: {:?}", BLUE, NC, backend.backend());
    
    let config = load_config();
    let gov_status = if service_active { "Running" } else { "Stopped" };
    println!("{}│{}  Governor: {}", BLUE, NC, gov_status);
    println!("{}│{}    ├─ QoS Mode:   {}", BLUE, NC, if config.governor.breathing_cake_enabled { "Breathing CAKE (Dynamic)" } else { "Static CAKE" });
    println!("{}│{}    ├─ Game Mode:  {}", BLUE, NC, if config.governor.game_mode_enabled { "Available (PPS > 200)" } else { "Disabled" });
    println!("{}│{}    └─ Band Steer: {}", BLUE, NC, if config.governor.band_steering_enabled { "Available" } else { "Disabled" });

    println!("{}└{}", BLUE, NC);
    println!();

    // 5. Connection Details (NM)
    if let Ok(nm) = NmClient::new().await {
        println!("{}{}{}┌─ Active Connection (NetworkManager){}", BOLD, BLUE, NC, NC);
        match nm.get_wireless_devices().await {
            Ok(devices) => {
                 let mut found_conn = false;
                 for device in devices {
                     if let Some(ap) = device.active_ap {
                         found_conn = true;
                         
                         // Calculate band steering score
                         let score = ap.score(10, 15); // Default biases: +10 for 5GHz, +15 for 6GHz
                         
                         // Determine channel from frequency
                         let channel = freq_to_channel(ap.frequency);
                         
                         // Signal quality description
                         let signal_quality = match ap.signal_strength {
                             s if s >= -50 => format!("{}Excellent{}", GREEN, NC),
                             s if s >= -60 => format!("{}Good{}", GREEN, NC),
                             s if s >= -70 => format!("{}Fair{}", YELLOW, NC),
                             _ => format!("{}Poor{}", RED, NC),
                         };
                         
                         println!("{}│{}  {}{}{}: {}", BLUE, NC, BOLD, device.interface, NC, ap.ssid);
                         println!("{}│{}    ├─ BSSID:    {}", BLUE, NC, ap.bssid);
                         println!("{}│{}    ├─ Band:     {:?} (Ch {} @ {} MHz)", BLUE, NC, ap.band, channel, ap.frequency);
                         println!("{}│{}    ├─ Signal:   {} dBm ({})", BLUE, NC, ap.signal_strength, signal_quality);
                         println!("{}│{}    ├─ Link:     {} Mbit/s", BLUE, NC, device.bitrate / 1000);
                         println!("{}│{}    └─ Score:    {} (for band steering)", BLUE, NC, score);
                     }
                 }
                 if !found_conn {
                     // Check for ethernet connection instead
                     let eth_conn = Command::new("nmcli")
                         .args(["-t", "-f", "NAME,DEVICE,TYPE,STATE", "connection", "show", "--active"])
                         .output()
                         .ok()
                         .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                         .unwrap_or_default();
                     
                     let mut eth_found = false;
                     for line in eth_conn.lines() {
                         let parts: Vec<&str> = line.split(':').collect();
                         if parts.len() >= 4 && parts[2] == "802-3-ethernet" && parts[3] == "activated" {
                             eth_found = true;
                             let conn_name = parts[0];
                             let iface = parts[1];
                             
                             // Get ethernet speed
                             let speed = Command::new("ethtool")
                                 .arg(iface)
                                 .output()
                                 .ok()
                                 .and_then(|o| {
                                     let stdout = String::from_utf8_lossy(&o.stdout);
                                     stdout.lines()
                                         .find(|l| l.contains("Speed:"))
                                         .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
                                 })
                                 .unwrap_or_else(|| "Unknown".to_string());
                             
                             println!("{}│{}  {}{}{}: {} (Ethernet)", BLUE, NC, BOLD, iface, NC, conn_name);
                             println!("{}│{}    ├─ Type:     Wired Ethernet", BLUE, NC);
                             println!("{}│{}    ├─ Speed:    {}", BLUE, NC, speed);
                             println!("{}│{}    └─ Latency:  {}Ultra-low{} (wired)", BLUE, NC, GREEN, NC);
                         }
                     }
                     
                     if !eth_found {
                         println!("{}│{}  No active connection found", BLUE, NC);
                     }
                 }
            }
            Err(_) => println!("{}│{}  Error querying NetworkManager", BLUE, NC),
        }
        println!("{}└{}", BLUE, NC);
    }
    
    Ok(())
}

/// Install the systemd service
/// Per rewrite.md: Binary in /var/lib/hifi-wifi (survives SteamOS updates)
fn run_install() -> Result<()> {
    use std::fs::{self, File};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    
    info!("=== Installing hifi-wifi Service ===\n");

    // Create persistent directory (survives SteamOS A/B updates)
    let var_lib = std::path::Path::new("/var/lib/hifi-wifi");
    fs::create_dir_all(var_lib)?;
    
    // Copy binary to persistent location
    let current_exe = std::env::current_exe()?;
    let target_bin = var_lib.join("hifi-wifi");
    
    info!("Copying binary to {}", target_bin.display());
    fs::copy(&current_exe, &target_bin)?;
    
    // Make executable
    let mut perms = fs::metadata(&target_bin)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&target_bin, perms)?;

    // Fix SELinux context on Fedora-based systems (Bazzite, etc.)
    // Without this, systemd cannot execute the binary due to var_lib_t context
    if std::path::Path::new("/usr/sbin/restorecon").exists() {
        info!("Setting SELinux context for binary...");
        // First try restorecon (uses default policy)
        let restorecon = Command::new("restorecon")
            .arg("-v")
            .arg(&target_bin)
            .output();
        
        // If restorecon doesn't set bin_t (var_lib default is var_lib_t), use chcon
        if restorecon.is_ok() {
            // Verify context - if still var_lib_t, force bin_t
            let context_check = Command::new("ls")
                .args(["-Z", target_bin.to_str().unwrap()])
                .output();
            
            if let Ok(output) = context_check {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("var_lib_t") {
                    // Force bin_t context so systemd can execute it
                    let _ = Command::new("chcon")
                        .args(["-t", "bin_t", target_bin.to_str().unwrap()])
                        .output();
                    info!("Applied bin_t SELinux context");
                }
            }
        }
    } else if std::path::Path::new("/usr/bin/chcon").exists() {
        // Fallback: direct chcon if restorecon not available
        info!("Setting SELinux context (chcon fallback)...");
        let _ = Command::new("chcon")
            .args(["-t", "bin_t", target_bin.to_str().unwrap()])
            .output();
    }

    // Create systemd service
    // Per rewrite.md: Service config with capabilities
    let service_content = r#"[Unit]
Description=hifi-wifi Network Optimizer
Documentation=https://github.com/your-repo/hifi-wifi
After=network-online.target NetworkManager.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/var/lib/hifi-wifi/hifi-wifi monitor
Restart=on-failure
RestartSec=5

# Security hardening per rewrite.md
ProtectSystem=full
ProtectHome=true
NoNewPrivileges=false
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN

# Resource limits
MemoryMax=64M
CPUQuota=10%

[Install]
WantedBy=multi-user.target
"#;

    let service_path = std::path::Path::new("/etc/systemd/system/hifi-wifi.service");
    info!("Creating systemd service: {}", service_path.display());
    
    let mut file = File::create(service_path)?;
    file.write_all(service_content.as_bytes())?;

    // Create systemd preset to survive SteamOS updates
    let preset_dir = std::path::Path::new("/etc/systemd/system-preset");
    if let Err(e) = fs::create_dir_all(preset_dir) {
        warn!("Could not create preset directory: {}", e);
    } else {
        let preset_path = preset_dir.join("99-hifi-wifi.preset");
        if let Ok(mut preset_file) = File::create(&preset_path) {
            let _ = preset_file.write_all(b"enable hifi-wifi.service\n");
            info!("Created systemd preset for automatic enable");
        }
    }

    // Reload systemd and enable service
    info!("Enabling service...");
    Command::new("systemctl").args(["daemon-reload"]).output()?;
    Command::new("systemctl").args(["enable", "hifi-wifi.service"]).output()?;
    Command::new("systemctl").args(["start", "hifi-wifi.service"]).output()?;

    info!("\n=== Installation Complete ===");
    info!("Service installed and started.");
    info!("  Status: systemctl status hifi-wifi");
    info!("  Logs:   journalctl -u hifi-wifi -f");
    
    // Create CLI symlink for user convenience
    ensure_symlinks();
    
    // Install tmpfiles.d config for SteamOS persistence (recreates symlinks on every boot)
    install_tmpfiles_config()?;
    
    // Install bootstrap timer as backup
    install_bootstrap_timer()?;
    
    // Install user-level auto-repair service for SteamOS (survives updates in ~/.config/)
    if is_steamos() {
        install_user_repair_service()?;
    }
    
    Ok(())
}

/// Install tmpfiles.d config that recreates symlinks on every boot
/// This is the PRIMARY persistence mechanism for SteamOS - it survives updates!
fn install_tmpfiles_config() -> Result<()> {
    use std::fs::{self, File};
    use std::io::Write;
    use std::process::Command;
    
    // tmpfiles.d in /etc is persistent on SteamOS!
    let tmpfiles_dir = std::path::Path::new("/etc/tmpfiles.d");
    let tmpfiles_path = tmpfiles_dir.join("hifi-wifi.conf");
    
    info!("Installing tmpfiles.d config for boot-time symlink restoration...");
    
    // Create directory if needed
    fs::create_dir_all(tmpfiles_dir)?;
    
    // tmpfiles.d format: Type Path Mode User Group Age Argument
    // L+ = create symlink, replace if exists
    let tmpfiles_content = r#"# hifi-wifi - recreate symlinks on every boot (survives SteamOS updates)
# CLI symlink
L+ /usr/local/bin/hifi-wifi - - - - /var/lib/hifi-wifi/hifi-wifi
# Systemd service symlinks (bootstrap timer restores the actual service)
L+ /etc/systemd/system/hifi-wifi-bootstrap.service - - - - /var/lib/hifi-wifi/hifi-wifi-bootstrap.service
L+ /etc/systemd/system/hifi-wifi-bootstrap.timer - - - - /var/lib/hifi-wifi/hifi-wifi-bootstrap.timer
"#;

    let mut file = File::create(&tmpfiles_path)?;
    file.write_all(tmpfiles_content.as_bytes())?;
    
    info!("Created {}", tmpfiles_path.display());
    info!("Symlinks will be automatically restored on every boot!");
    
    // Run tmpfiles now to apply immediately
    let _ = Command::new("systemd-tmpfiles")
        .args(["--create", tmpfiles_path.to_str().unwrap_or("hifi-wifi.conf")])
        .output();
    
    Ok(())
}

/// Install user-level systemd timer that persists across SteamOS updates
/// This timer runs bootstrap on every login to repair the system service if wiped
fn install_bootstrap_timer() -> Result<()> {
    use std::fs::{self, File};
    use std::io::Write;
    use std::process::Command;
    
    // Store actual service files in persistent location
    let persistent_dir = std::path::Path::new("/var/lib/hifi-wifi");
    let system_dir = std::path::Path::new("/etc/systemd/system");
    
    info!("Installing persistent bootstrap service in {}", persistent_dir.display());
    
    // Create bootstrap service that repairs the main service
    let bootstrap_service = r#"[Unit]
Description=hifi-wifi Bootstrap (repairs main service after SteamOS updates)
After=local-fs.target
Before=network-online.target
ConditionPathExists=/var/lib/hifi-wifi/hifi-wifi
ConditionPathExists=!/etc/systemd/system/hifi-wifi.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/var/lib/hifi-wifi/hifi-wifi bootstrap

[Install]
WantedBy=multi-user.target
"#;

    let bootstrap_timer = r#"[Unit]
Description=hifi-wifi Bootstrap Timer (auto-repairs after SteamOS updates)
ConditionPathExists=/var/lib/hifi-wifi/hifi-wifi

[Timer]
# Run 30 seconds after boot to allow filesystem to settle
OnBootSec=30s
# Also check periodically
OnUnitActiveSec=6h

[Install]
WantedBy=timers.target
"#;

    // Save to persistent location
    let persistent_service = persistent_dir.join("hifi-wifi-bootstrap.service");
    let persistent_timer = persistent_dir.join("hifi-wifi-bootstrap.timer");
    
    let mut f = File::create(&persistent_service)?;
    f.write_all(bootstrap_service.as_bytes())?;
    
    let mut f = File::create(&persistent_timer)?;
    f.write_all(bootstrap_timer.as_bytes())?;
    
    // Create symlinks in system directory
    let system_service = system_dir.join("hifi-wifi-bootstrap.service");
    let system_timer = system_dir.join("hifi-wifi-bootstrap.timer");
    
    // Remove old files/links if they exist
    let _ = fs::remove_file(&system_service);
    let _ = fs::remove_file(&system_timer);
    
    // Create symlinks
    std::os::unix::fs::symlink(&persistent_service, &system_service)?;
    std::os::unix::fs::symlink(&persistent_timer, &system_timer)?;
    
    info!("Created symlinks: {} -> {}", system_service.display(), persistent_service.display());
    
    // Enable and start the timer
    Command::new("systemctl").args(["daemon-reload"]).output()?;
    Command::new("systemctl").args(["enable", "hifi-wifi-bootstrap.timer"]).output()?;
    Command::new("systemctl").args(["start", "hifi-wifi-bootstrap.timer"]).output()?;
    
    info!("Bootstrap timer installed - will auto-repair after SteamOS updates");
    
    Ok(())
}

/// Install a user-level systemd service that auto-repairs hifi-wifi after SteamOS updates
/// Lives in ~/.config/systemd/user/ which PERSISTS across SteamOS updates!
fn install_user_repair_service() -> Result<()> {
    use std::fs::{self, File};
    use std::io::Write;
    use std::process::Command;
    use std::os::unix::fs::PermissionsExt;
    
    // Get the real user info
    let sudo_user = std::env::var("SUDO_USER").unwrap_or_else(|_| "deck".to_string());
    let home = std::process::Command::new("getent")
        .args(["passwd", &sudo_user])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split(':')
                .nth(5)
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| format!("/home/{}", sudo_user));
    
    let user_systemd_dir = format!("{}/.config/systemd/user", home);
    let repair_script_path = "/var/lib/hifi-wifi/repair.sh";
    
    info!("Installing user repair service in {}", user_systemd_dir);
    
    // Create user systemd directory
    fs::create_dir_all(&user_systemd_dir)?;
    
    // Create a repair script that handles the sudo/polkit interaction
    // This script only does work if repair is actually needed
    let repair_script = r#"#!/bin/bash
# hifi-wifi auto-repair script - runs at user login on SteamOS
# Only performs repair if needed (symlink missing but binary exists)

BINARY="/var/lib/hifi-wifi/hifi-wifi"
SYMLINK="/usr/local/bin/hifi-wifi"
SERVICE="/etc/systemd/system/hifi-wifi.service"

# Exit early if no repair needed
if [[ -x "$SYMLINK" ]] && [[ -f "$SERVICE" ]]; then
    exit 0
fi

# Exit if binary doesn't exist (not installed)
if [[ ! -x "$BINARY" ]]; then
    exit 0
fi

# Repair needed - use pkexec for GUI-friendly privilege escalation
# pkexec shows a nice authentication dialog if needed
exec pkexec "$BINARY" bootstrap
"#;

    // Write repair script to persistent location
    let mut script_file = File::create(repair_script_path)?;
    script_file.write_all(repair_script.as_bytes())?;
    let mut perms = fs::metadata(repair_script_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(repair_script_path, perms)?;
    
    // Create polkit rule to allow passwordless bootstrap (better UX)
    let polkit_dir = "/etc/polkit-1/rules.d";
    if std::path::Path::new("/etc/polkit-1").exists() {
        let _ = fs::create_dir_all(polkit_dir);
        let polkit_rule = format!(r#"// Allow hifi-wifi bootstrap without password for {}
polkit.addRule(function(action, subject) {{
    if (action.id == "org.freedesktop.policykit.exec" &&
        action.lookup("program") == "/var/lib/hifi-wifi/hifi-wifi" &&
        subject.user == "{}") {{
        return polkit.Result.YES;
    }}
}});
"#, sudo_user, sudo_user);
        
        let polkit_path = format!("{}/49-hifi-wifi.rules", polkit_dir);
        if let Ok(mut f) = File::create(&polkit_path) {
            let _ = f.write_all(polkit_rule.as_bytes());
            info!("Created polkit rule for passwordless repair");
        }
    }
    
    // Create user systemd service
    let service_content = format!(r#"[Unit]
Description=hifi-wifi Auto-Repair (restores after SteamOS updates)
After=graphical-session.target

[Service]
Type=oneshot
ExecStart={}
RemainAfterExit=yes

[Install]
WantedBy=default.target
"#, repair_script_path);

    let service_path = format!("{}/hifi-wifi-repair.service", user_systemd_dir);
    let mut service_file = File::create(&service_path)?;
    service_file.write_all(service_content.as_bytes())?;
    
    // Fix ownership of user config directory
    let uid_output = Command::new("id").args(["-u", &sudo_user]).output()?;
    let gid_output = Command::new("id").args(["-g", &sudo_user]).output()?;
    let uid: u32 = String::from_utf8_lossy(&uid_output.stdout).trim().parse().unwrap_or(1000);
    let gid: u32 = String::from_utf8_lossy(&gid_output.stdout).trim().parse().unwrap_or(1000);
    
    // Recursively chown the .config/systemd directory
    let _ = Command::new("chown")
        .args(["-R", &format!("{}:{}", uid, gid), &format!("{}/.config/systemd", home)])
        .output();
    
    // Enable the user service (must run as the user)
    let _ = Command::new("sudo")
        .args(["-u", &sudo_user, "systemctl", "--user", "daemon-reload"])
        .output();
    let _ = Command::new("sudo")
        .args(["-u", &sudo_user, "systemctl", "--user", "enable", "hifi-wifi-repair.service"])
        .output();
    
    info!("User repair service installed - will auto-repair at login after SteamOS updates");
    
    Ok(())
}

/// Uninstall the systemd service
fn run_uninstall() -> Result<()> {
    use std::fs;
    use std::process::Command;
    
    info!("=== Uninstalling hifi-wifi Service ===\n");

    // Stop and disable services
    info!("Stopping services...");
    let _ = Command::new("systemctl").args(["stop", "hifi-wifi.service"]).output();
    let _ = Command::new("systemctl").args(["stop", "hifi-wifi-bootstrap.timer"]).output();
    let _ = Command::new("systemctl").args(["disable", "hifi-wifi.service"]).output();
    let _ = Command::new("systemctl").args(["disable", "hifi-wifi-bootstrap.timer"]).output();

    // Remove service files and symlinks
    let files_to_remove = [
        "/etc/systemd/system/hifi-wifi.service",
        "/etc/systemd/system/hifi-wifi-bootstrap.service",
        "/etc/systemd/system/hifi-wifi-bootstrap.timer",
        "/var/lib/hifi-wifi/hifi-wifi-bootstrap.service",
        "/var/lib/hifi-wifi/hifi-wifi-bootstrap.timer",
        "/usr/local/bin/hifi-wifi",
        "/etc/tmpfiles.d/hifi-wifi.conf",
    ];
    
    for path in &files_to_remove {
        if std::path::Path::new(path).exists() {
            info!("Removing {}...", path);
            let _ = fs::remove_file(path);
        }
    }

    // Reload systemd
    Command::new("systemctl").args(["daemon-reload"]).output()?;

    // Optionally remove binary (keep /var/lib/hifi-wifi for config)
    let binary_path = "/var/lib/hifi-wifi/hifi-wifi";
    if std::path::Path::new(binary_path).exists() {
        info!("Removing binary...");
        let _ = fs::remove_file(binary_path);
    }

    // Remove bashrc hook if present
    remove_bashrc_hook();
    
    // Remove user repair service
    remove_user_repair_service();

    // Revert optimizations
    run_revert()?;

    info!("\n=== Uninstallation Complete ===");
    Ok(())
}

/// Remove the bashrc auto-repair hook
fn remove_bashrc_hook() {
    // Get the real user's home directory
    let home = std::env::var("SUDO_USER")
        .ok()
        .and_then(|user| {
            std::process::Command::new("getent")
                .args(["passwd", &user])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split(':')
                        .nth(5)
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string()));
    
    let bashrc_path = format!("{}/.bashrc", home);
    
    if let Ok(contents) = std::fs::read_to_string(&bashrc_path) {
        // Remove the hook section
        let hook_start = "# hifi-wifi auto-repair hook";
        if contents.contains(hook_start) {
            let new_contents: String = contents
                .lines()
                .filter(|line| {
                    !line.contains("hifi-wifi") || line.contains("alias")
                })
                .collect::<Vec<_>>()
                .join("\n");
            
            if let Ok(mut file) = std::fs::File::create(&bashrc_path) {
                use std::io::Write;
                let _ = file.write_all(new_contents.as_bytes());
                info!("Removed bashrc hook");
            }
        }
    }
}

/// Remove the user repair service
fn remove_user_repair_service() {
    use std::process::Command;
    
    let sudo_user = std::env::var("SUDO_USER").unwrap_or_else(|_| "deck".to_string());
    let home = std::process::Command::new("getent")
        .args(["passwd", &sudo_user])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split(':')
                .nth(5)
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| format!("/home/{}", sudo_user));
    
    // Disable and remove user service
    let _ = Command::new("sudo")
        .args(["-u", &sudo_user, "systemctl", "--user", "disable", "hifi-wifi-repair.service"])
        .output();
    let _ = Command::new("sudo")
        .args(["-u", &sudo_user, "systemctl", "--user", "stop", "hifi-wifi-repair.service"])
        .output();
    
    let service_path = format!("{}/.config/systemd/user/hifi-wifi-repair.service", home);
    if std::path::Path::new(&service_path).exists() {
        let _ = std::fs::remove_file(&service_path);
        info!("Removed user repair service");
    }
    
    // Remove repair script
    let _ = std::fs::remove_file("/var/lib/hifi-wifi/repair.sh");
    
    // Remove polkit rule
    let _ = std::fs::remove_file("/etc/polkit-1/rules.d/49-hifi-wifi.rules");
}

/// Turn off hifi-wifi (stop service, revert optimizations) for A/B testing
fn run_off() -> Result<()> {
    use std::process::Command;
    
    info!("=== Turning OFF hifi-wifi ===\n");

    // Stop service if running
    if Command::new("systemctl").args(["is-active", "--quiet", "hifi-wifi"]).status()?.success() {
        info!("Stopping hifi-wifi service...");
        Command::new("systemctl").args(["stop", "hifi-wifi.service"]).output()?;
    } else {
        info!("Service not running.");
    }

    // Revert all optimizations
    run_revert()?;

    info!("\n=== hifi-wifi is OFF ===");
    info!("Network is now using default settings.");
    info!("To turn back on: sudo hifi-wifi on");
    Ok(())
}

/// Turn on hifi-wifi (start service, apply optimizations) for A/B testing
fn run_on() -> Result<()> {
    use std::process::Command;
    
    info!("=== Turning ON hifi-wifi ===\n");

    // Check if service exists
    if !std::path::Path::new("/etc/systemd/system/hifi-wifi.service").exists() {
        error!("hifi-wifi service not installed. Run: sudo hifi-wifi install");
        return Ok(());
    }

    // Start service
    info!("Starting hifi-wifi service...");
    Command::new("systemctl").args(["start", "hifi-wifi.service"]).output()?;

    info!("\n=== hifi-wifi is ON ===");
    info!("Network optimizations are active.");
    info!("Check status: hifi-wifi status");
    Ok(())
}

/// Bootstrap: Check if system service exists and repair if missing
/// This is called by the system-level timer on boot to survive SteamOS updates
fn run_bootstrap() -> Result<()> {
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;
    use std::path::Path;
    
    let service_path = Path::new("/etc/systemd/system/hifi-wifi.service");
    let binary_path = Path::new("/var/lib/hifi-wifi/hifi-wifi");
    let symlink_path = Path::new("/usr/local/bin/hifi-wifi");
    let persistent_dir = Path::new("/var/lib/hifi-wifi");
    let system_dir = Path::new("/etc/systemd/system");
    
    // Check if binary exists (if not, nothing we can do)
    if !binary_path.exists() {
        warn!("Bootstrap: Binary not found at {}, skipping", binary_path.display());
        return Ok(());
    }
    
    let mut repaired = false;
    let steamos = is_steamos();
    
    // On SteamOS, need to disable read-only first for any repairs
    if steamos {
        let _ = Command::new("systemd-sysext").arg("unmerge").output();
        let _ = Command::new("steamos-readonly").arg("disable").output();
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    
    // Check/repair bootstrap timer symlinks (these get wiped too!)
    let bootstrap_service_link = system_dir.join("hifi-wifi-bootstrap.service");
    let bootstrap_timer_link = system_dir.join("hifi-wifi-bootstrap.timer");
    let persistent_service = persistent_dir.join("hifi-wifi-bootstrap.service");
    let persistent_timer = persistent_dir.join("hifi-wifi-bootstrap.timer");
    
    if persistent_service.exists() && !bootstrap_service_link.exists() {
        info!("Bootstrap: Restoring bootstrap service symlink...");
        let _ = std::os::unix::fs::symlink(&persistent_service, &bootstrap_service_link);
        repaired = true;
    }
    if persistent_timer.exists() && !bootstrap_timer_link.exists() {
        info!("Bootstrap: Restoring bootstrap timer symlink...");
        let _ = std::os::unix::fs::symlink(&persistent_timer, &bootstrap_timer_link);
        repaired = true;
    }
    
    // Check if main service file exists
    if !service_path.exists() {
        info!("Bootstrap: Service file missing (likely after SteamOS update), recreating...");
        
        // Recreate service file
        let service_content = r#"[Unit]
Description=hifi-wifi Network Optimizer
Documentation=https://github.com/doughty247/hifi-wifi
After=network-online.target NetworkManager.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/var/lib/hifi-wifi/hifi-wifi monitor
Restart=on-failure
RestartSec=5

# Security hardening
ProtectSystem=full
ProtectHome=true
NoNewPrivileges=false
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN

# Resource limits
MemoryMax=64M
CPUQuota=10%

[Install]
WantedBy=multi-user.target
"#;
        
        if let Ok(mut file) = File::create(service_path) {
            let _ = file.write_all(service_content.as_bytes());
            repaired = true;
            info!("Bootstrap: Service file recreated");
        } else {
            error!("Bootstrap: Failed to create service file");
        }
    }
    
    // Check/repair CLI symlink
    if !symlink_path.exists() {
        info!("Bootstrap: CLI symlink missing, recreating...");
        ensure_symlinks();
        repaired = true;
    }
    
    // Re-enable read-only on SteamOS
    if steamos {
        let _ = Command::new("steamos-readonly").arg("enable").output();
    }
    
    // If we repaired anything, reload systemd
    if repaired {
        info!("Bootstrap: Reloading systemd and starting services...");
        let _ = Command::new("systemctl").args(["daemon-reload"]).output();
        let _ = Command::new("systemctl").args(["enable", "hifi-wifi.service"]).output();
        let _ = Command::new("systemctl").args(["enable", "hifi-wifi-bootstrap.timer"]).output();
        let _ = Command::new("systemctl").args(["start", "hifi-wifi.service"]).output();
        info!("Bootstrap: Repair complete - hifi-wifi restored");
    } else {
        // Just ensure service is running
        let status = Command::new("systemctl")
            .args(["is-active", "--quiet", "hifi-wifi.service"])
            .status();
        
        if status.map(|s| !s.success()).unwrap_or(true) {
            info!("Bootstrap: Service not running, starting...");
            let _ = Command::new("systemctl").args(["start", "hifi-wifi.service"]).output();
        }
    }
    
    Ok(())
}
