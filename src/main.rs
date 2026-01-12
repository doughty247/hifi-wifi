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

/// Run the Governor in monitor mode (daemon)
async fn run_monitor(config: &config::structs::Config) -> Result<()> {
    info!("=== hifi-wifi v3.0 Monitor Mode ===");

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

    // Reload systemd and enable service
    info!("Enabling service...");
    Command::new("systemctl").args(["daemon-reload"]).output()?;
    Command::new("systemctl").args(["enable", "hifi-wifi.service"]).output()?;
    Command::new("systemctl").args(["start", "hifi-wifi.service"]).output()?;

    info!("\n=== Installation Complete ===");
    info!("Service installed and started.");
    info!("  Status: systemctl status hifi-wifi");
    info!("  Logs:   journalctl -u hifi-wifi -f");
    
    // Setup CLI access via PATH in .bashrc (persists across SteamOS updates!)
    setup_user_path()?;
    
    // Install user-level auto-repair service for SteamOS (survives updates in ~/.config/)
    if is_steamos() {
        install_user_repair_service()?;
    }
    
    Ok(())
}

/// Add /var/lib/hifi-wifi to user's PATH via .bashrc
/// This is the PERSISTENT way to provide CLI access on immutable distros like SteamOS
/// ~/.bashrc lives in /home which is NEVER touched by SteamOS updates
fn setup_user_path() -> Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::process::Command;
    
    let sudo_user = std::env::var("SUDO_USER").unwrap_or_else(|_| "deck".to_string());
    let home = Command::new("getent")
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
    
    let bashrc_path = format!("{}/.bashrc", home);
    let path_line = r#"export PATH="$PATH:/var/lib/hifi-wifi""#;
    
    info!("Setting up CLI access via PATH in {}", bashrc_path);
    
    // Check if already present
    if let Ok(file) = fs::File::open(&bashrc_path) {
        let reader = BufReader::new(file);
        for line in reader.lines() {
            if let Ok(line) = line {
                if line.contains("/var/lib/hifi-wifi") {
                    info!("PATH already configured in .bashrc");
                    return Ok(());
                }
            }
        }
    }
    
    // Append to bashrc
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&bashrc_path)?;
    
    writeln!(file)?;
    writeln!(file, "# hifi-wifi CLI access (survives SteamOS updates)")?;
    writeln!(file, "{}", path_line)?;
    
    // Fix ownership
    let uid_output = Command::new("id").args(["-u", &sudo_user]).output()?;
    let gid_output = Command::new("id").args(["-g", &sudo_user]).output()?;
    let uid: u32 = String::from_utf8_lossy(&uid_output.stdout).trim().parse().unwrap_or(1000);
    let gid: u32 = String::from_utf8_lossy(&gid_output.stdout).trim().parse().unwrap_or(1000);
    
    let _ = std::os::unix::fs::chown(&bashrc_path, Some(uid), Some(gid));
    
    info!("Added /var/lib/hifi-wifi to PATH in .bashrc");
    info!("Run 'source ~/.bashrc' or open a new terminal to use 'hifi-wifi' command");
    
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
    // This script only does work if repair is actually needed (systemd service missing)
    // NOTE: CLI access is now via PATH in .bashrc - no symlinks to repair!
    let repair_script = r#"#!/bin/bash
# hifi-wifi auto-repair script - runs at user login on SteamOS
# Only performs repair if needed (systemd service missing but binary exists)
# CLI access is via PATH in ~/.bashrc (persistent), no symlinks needed!

BINARY="/var/lib/hifi-wifi/hifi-wifi"
SERVICE="/etc/systemd/system/hifi-wifi.service"

# Exit early if no repair needed (service exists)
if [[ -f "$SERVICE" ]]; then
    exit 0
fi

# Exit if binary doesn't exist (not installed)
if [[ ! -x "$BINARY" ]]; then
    exit 0
fi

# Service missing - repair needed
# Use pkexec for GUI-friendly privilege escalation
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
    // Note: With lingering enabled, this runs when user@.service starts at boot,
    // which happens early - even before graphical session in SteamOS Game Mode
    let service_content = format!(r#"[Unit]
Description=hifi-wifi Auto-Repair (restores after SteamOS updates)
After=network-online.target

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
    
    // Enable lingering for the user - ensures user systemd instance starts at boot
    // This is critical for SteamOS Game Mode where gamescope session might not
    // trigger graphical-session.target the same way as KDE Plasma desktop
    let _ = Command::new("loginctl")
        .args(["enable-linger", &sudo_user])
        .output();
    info!("Enabled user lingering for early boot service start");
    
    // Enable the user service (must run as the user)
    let _ = Command::new("sudo")
        .args(["-u", &sudo_user, "systemctl", "--user", "daemon-reload"])
        .output();
    let _ = Command::new("sudo")
        .args(["-u", &sudo_user, "systemctl", "--user", "enable", "hifi-wifi-repair.service"])
        .output();
    
    info!("User repair service installed - will auto-repair at boot after SteamOS updates");
    
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

    // Remove PATH from .bashrc
    remove_user_path();
    
    // Remove user repair service
    remove_user_repair_service();

    // Revert optimizations
    run_revert()?;

    info!("\n=== Uninstallation Complete ===");
    Ok(())
}

/// Remove /var/lib/hifi-wifi from user's PATH in .bashrc
fn remove_user_path() {
    use std::io::{BufRead, BufReader, Write};
    
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
    
    let bashrc_path = format!("{}/.bashrc", home);
    
    if let Ok(file) = std::fs::File::open(&bashrc_path) {
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
        
        // Filter out hifi-wifi PATH lines
        let filtered: Vec<&String> = lines.iter()
            .filter(|line| !line.contains("/var/lib/hifi-wifi") && !line.contains("# hifi-wifi CLI access"))
            .collect();
        
        if filtered.len() != lines.len() {
            // Write back filtered content
            if let Ok(mut file) = std::fs::File::create(&bashrc_path) {
                for line in filtered {
                    let _ = writeln!(file, "{}", line);
                }
                info!("Removed PATH entry from .bashrc");
                
                // Fix ownership
                let uid_output = std::process::Command::new("id").args(["-u", &sudo_user]).output();
                let gid_output = std::process::Command::new("id").args(["-g", &sudo_user]).output();
                if let (Ok(uid_out), Ok(gid_out)) = (uid_output, gid_output) {
                    let uid: u32 = String::from_utf8_lossy(&uid_out.stdout).trim().parse().unwrap_or(1000);
                    let gid: u32 = String::from_utf8_lossy(&gid_out.stdout).trim().parse().unwrap_or(1000);
                    let _ = std::os::unix::fs::chown(&bashrc_path, Some(uid), Some(gid));
                }
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
    
    // Disable lingering (only if no other user services need it)
    // Note: We disable this cautiously - user may have other services that need it
    let _ = Command::new("loginctl")
        .args(["disable-linger", &sudo_user])
        .output();
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
/// This is called by the user repair service on boot to survive SteamOS updates
/// NOTE: CLI access is now via PATH in ~/.bashrc - no symlinks to repair!
fn run_bootstrap() -> Result<()> {
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;
    use std::path::Path;
    
    let service_path = Path::new("/etc/systemd/system/hifi-wifi.service");
    let binary_path = Path::new("/var/lib/hifi-wifi/hifi-wifi");
    
    // Check if binary exists (if not, nothing we can do)
    if !binary_path.exists() {
        warn!("Bootstrap: Binary not found at {}, skipping", binary_path.display());
        return Ok(());
    }
    
    let mut repaired = false;
    
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
    
    // If we repaired anything, reload systemd and apply optimizations
    if repaired {
        info!("Bootstrap: Reloading systemd...");
        let _ = Command::new("systemctl").args(["daemon-reload"]).output();
        let _ = Command::new("systemctl").args(["enable", "hifi-wifi.service"]).output();
        
        // Apply optimizations BEFORE starting the service
        // This restores sysctl, modprobe configs, etc. that were wiped
        info!("Bootstrap: Applying optimizations (configs were wiped by SteamOS update)...");
        let config = load_config();
        if let Err(e) = run_apply(&config) {
            error!("Bootstrap: Failed to apply optimizations: {}", e);
        }
        
        // Now start the service (monitor mode)
        info!("Bootstrap: Starting service...");
        let _ = Command::new("systemctl").args(["start", "hifi-wifi.service"]).output();
        info!("Bootstrap: Repair complete - hifi-wifi fully restored");
    } else {
        // Service file exists - just ensure service is running
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
