#!/bin/bash

function detect_driver_category() {
    if [[ -n "$DETECTED_DRIVER" ]]; then
        case "$DETECTED_DRIVER" in
            rtw89*|*rtw89*)
                DRIVER_CATEGORY="rtw89"
                log_info "Category: Realtek RTW89 (modern)"
                ;;
            rtw88*|*rtw88*)
                DRIVER_CATEGORY="rtw88"
                log_info "Category: Realtek RTW88"
                ;;
            rtl8192ee|rtl8188ee|rtl8723*|rtl8821*|rtl8822*|rtl*)
                DRIVER_CATEGORY="rtl_legacy"
                log_info "Category: Realtek Legacy"
                ;;
            mt7921*|mt76*|mt7*)
                DRIVER_CATEGORY="mediatek"
                log_info "Category: MediaTek"
                ;;
            iwlwifi|iwl*)
                DRIVER_CATEGORY="intel"
                log_info "Category: Intel Wi-Fi"
                ;;
            ath10k*|ath11k*|ath9k*|ath*)
                DRIVER_CATEGORY="atheros"
                log_info "Category: Qualcomm Atheros"
                ;;
            brcm*|wl)
                DRIVER_CATEGORY="broadcom"
                log_info "Category: Broadcom"
                ;;
            carl9170|ar9170usb)
                DRIVER_CATEGORY="carl9170"
                log_info "Category: Atheros AR9170 USB"
                ;;
            rt2*|rt5*|rt73*)
                DRIVER_CATEGORY="ralink"
                log_info "Category: Ralink/MediaTek Legacy"
                ;;
            zd*)
                DRIVER_CATEGORY="zydas"
                log_info "Category: ZyDAS"
                ;;
            mwifiex*|mwl*)
                DRIVER_CATEGORY="marvell"
                log_info "Category: Marvell"
                ;;
            *)
                DRIVER_CATEGORY="generic"
                log_info "Category: Generic (universal optimizations only)"
                ;;
        esac
    else
        DRIVER_CATEGORY="unknown"
        log_warning "Driver category unknown - will apply generic optimizations"
    fi
}

function apply_patches() {
  log_info "Applying enhanced Wi-Fi optimizations..."
  
  local ifc
  if ! ifc=$(detect_interface); then
    log_error "Could not auto-detect Wi-Fi interface"
    return 1
  fi
  
  log_info "Using interface: $ifc"

  if [[ ${DRY_RUN:-0} -eq 1 ]]; then
    log_info "[DRY-RUN] Would apply the following changes:"
    log_info "  - Disable power saving on $ifc"
    log_info "  - Create /etc/modprobe.d/rtw89.conf"
    log_info "  - Create /etc/modprobe.d/rtw89_advanced.conf"
    log_info "  - Create /etc/udev/rules.d/70-wifi-powersave.rules"
    log_info "  - Create /etc/sysctl.d/99-wifi-upload-opt.conf"
    log_info "  - Configure queue discipline with CAKE or fq_codel"
    log_info "  - Optimize IRQ affinity"
    log_info "  - Adjust ethtool settings"
    return 0
  fi

  # 1. Configure Wi-Fi power saving based on device type
  local is_battery_device=0
  local battery_reason=""
  
  if [[ "${DEVICE_TYPE:-}" == "steamdeck" ]]; then
    is_battery_device=1
    battery_reason="Steam Deck detected"
  elif [[ -d /sys/class/power_supply/BAT0 ]] || [[ -d /sys/class/power_supply/BAT1 ]]; then
    is_battery_device=1
    battery_reason="Laptop battery found (BAT0/BAT1)"
  elif [[ -d /sys/class/power_supply/battery ]]; then
    is_battery_device=1
    battery_reason="Laptop battery found (battery)"
  elif [[ -f /sys/class/dmi/id/chassis_type ]]; then
    local chassis_type=$(cat /sys/class/dmi/id/chassis_type 2>/dev/null)
    if [[ "$chassis_type" =~ ^(8|9|10|11|14|30|31)$ ]]; then
      is_battery_device=1
      battery_reason="Laptop chassis type detected (type $chassis_type)"
    elif [[ "$chassis_type" =~ ^(3|4|5|6|7|13|15|16)$ ]]; then
      is_battery_device=0
      battery_reason="Desktop chassis detected (type $chassis_type) - ignoring peripheral batteries"
    else
      for bat in /sys/class/power_supply/*/type; do
        if grep -q "Battery" "$bat" 2>/dev/null; then
          local bat_dir=$(dirname "$bat")
          local bat_name=$(basename "$bat_dir")
          if [[ ! "$bat_name" =~ (hidpp|hid|mouse|keyboard|wacom|peripheral) ]]; then
            if [[ -f "$bat_dir/capacity" ]]; then
              is_battery_device=1
              battery_reason="System battery detected: $bat_name"
              break
            fi
          fi
        fi
      done
    fi
  elif command -v laptop-detect &>/dev/null && laptop-detect; then
    is_battery_device=1
    battery_reason="Detected as laptop via laptop-detect"
  fi
  
  if [[ $is_battery_device -eq 1 ]]; then
    log_info "Battery-powered device detected: $battery_reason"
  else
    log_info "Desktop/server detected - no battery found"
  fi
  
  if [[ $is_battery_device -eq 1 ]]; then
    log_info "Configuring adaptive power management for battery device..."
    
    cp "$PROJECT_ROOT/src/power-manager.sh" /usr/local/bin/wifi-power-manager.sh
    chmod +x /usr/local/bin/wifi-power-manager.sh
    
    cat > /etc/udev/rules.d/70-wifi-power-ac.rules << 'EOF'
# Adjust Wi-Fi power saving based on AC power status
SUBSYSTEM=="power_supply", ENV{POWER_SUPPLY_ONLINE}=="0", RUN+="/usr/local/bin/wifi-power-manager.sh"
SUBSYSTEM=="power_supply", ENV{POWER_SUPPLY_ONLINE}=="1", RUN+="/usr/local/bin/wifi-power-manager.sh"
EOF
    
    /usr/local/bin/wifi-power-manager.sh
    log_success "Adaptive power management configured (AC=performance, Battery=power-saving)"
  else
    log_info "Applying maximum performance settings for desktop..."
    
    if iw dev "$ifc" set power_save off 2>/dev/null; then
      log_success "Power saving DISABLED on $ifc (desktop performance mode)"
    else
      log_warning "Could not disable power saving (may not be supported by driver)"
    fi
    
    ethtool -s "$ifc" speed 1000 duplex full 2>/dev/null && \
      log_success "Set link speed to maximum" || true
    
    cp "$PROJECT_ROOT/src/desktop-performance.sh" /usr/local/bin/wifi-desktop-performance.sh
    chmod +x /usr/local/bin/wifi-desktop-performance.sh
    
    cat > /etc/udev/rules.d/70-wifi-powersave.rules << 'EOF'
# Desktop Wi-Fi - ALWAYS maximum performance mode
ACTION=="add", SUBSYSTEM=="net", KERNEL=="wl*", RUN+="/usr/local/bin/wifi-desktop-performance.sh %k"
EOF
    
    /usr/local/bin/wifi-desktop-performance.sh "$ifc"
    log_success "Desktop performance mode configured and active"
  fi
  
  # Apply driver-specific module parameters
  case "$DRIVER_CATEGORY" in
    rtw89)
      log_info "Applying Realtek RTW89 driver optimizations..."
      cat > /etc/modprobe.d/rtw89.conf << 'EOF'
# Realtek RTW89 optimizations (RTL8852/RTL8852BE/etc)
options rtw89_pci disable_aspm=1 disable_clkreq=1
options rtw89_core tx_ampdu_subframes=32
EOF
      ;;
    rtw88)
      log_info "Applying Realtek RTW88 driver optimizations..."
      cat > /etc/modprobe.d/rtw88.conf << 'EOF'
# Realtek RTW88 optimizations (RTL8822CE/etc)
options rtw88_pci disable_aspm=1
options rtw88_core disable_lps_deep=Y
EOF
      ;;
    rtl_legacy)
      log_info "Applying Legacy Realtek driver optimizations..."
      cat > /etc/modprobe.d/rtl_legacy.conf << 'EOF'
# Legacy Realtek optimizations (RTL8192EE/RTL8188EE/etc)
options rtl8192ee swenc=1 ips=0 fwlps=0 2>/dev/null || true
options rtl8188ee swenc=1 ips=0 fwlps=0 2>/dev/null || true
options rtl_pci disable_aspm=1
options rtl_usb disable_aspm=1
EOF
      ;;
    mediatek)
      log_info "Applying MediaTek driver optimizations..."
      cat > /etc/modprobe.d/mediatek.conf << 'EOF'
# MediaTek optimizations (MT7921/MT76/etc)
options mt7921e disable_aspm=1 2>/dev/null || true
options mt76_usb disable_usb_sg=1 2>/dev/null || true
EOF
      ;;
    intel)
      log_info "Applying Intel Wi-Fi driver optimizations..."
      cat > /etc/modprobe.d/iwlwifi.conf << 'EOF'
# Intel Wi-Fi optimizations
options iwlwifi power_save=0 uapsd_disable=1 11n_disable=0
options iwlmvm power_scheme=1
EOF
      ;;
    atheros)
      log_info "Applying Qualcomm Atheros driver optimizations..."
      cat > /etc/modprobe.d/ath_wifi.conf << 'EOF'
# Qualcomm Atheros Wi-Fi optimizations
options ath10k_core skip_otp=y 2>/dev/null || true
options ath11k_pci disable_aspm=1 2>/dev/null || true
options ath9k nohwcrypt=0 ps_enable=0 2>/dev/null || true
EOF
      ;;
    broadcom)
      log_info "Applying Broadcom driver optimizations..."
      cat > /etc/modprobe.d/broadcom.conf << 'EOF'
# Broadcom Wi-Fi optimizations
options brcmfmac roamoff=1 2>/dev/null || true
options wl interference=0 2>/dev/null || true
EOF
      ;;
    ralink)
      log_info "Applying Ralink/MediaTek Legacy optimizations..."
      cat > /etc/modprobe.d/ralink.conf << 'EOF'
# Ralink/MediaTek Legacy optimizations
options rt2800usb nohwcrypt=0 2>/dev/null || true
options rt2800pci nohwcrypt=0 2>/dev/null || true
EOF
      ;;
    marvell)
      log_info "Applying Marvell driver optimizations..."
      cat > /etc/modprobe.d/marvell.conf << 'EOF'
# Marvell Wi-Fi optimizations
options mwifiex disable_auto_ds=1 2>/dev/null || true
EOF
      ;;
    *)
      log_info "Applying universal Wi-Fi optimizations (driver-agnostic)..."
      cat > /etc/modprobe.d/wifi_generic.conf << 'EOF'
# Universal Wi-Fi optimizations
# These settings work across most Wi-Fi drivers
EOF
      ;;
  esac
  
  if ls /etc/modprobe.d/*.conf 2>/dev/null | grep -qE "(rt|mt|iwl|ath|wifi)"; then
    log_success "Created driver-specific configuration"
  fi

  local UDEV_FILE="/etc/udev/rules.d/70-wifi-powersave.rules"
  echo 'ACTION=="add", SUBSYSTEM=="net", KERNEL=="wl*", RUN+="/usr/sbin/iw dev %k set power_save off"' > "$UDEV_FILE"

  local UUID=$(current_ssid_uuid)
  if [[ -n "$UUID" ]]; then
    backup_connection "$UUID"
    echo "[PATCH] Adjusting NetworkManager connection UUID=$UUID"
    nmcli connection modify "$UUID" ipv6.method ignore || true
    nmcli connection modify "$UUID" ipv4.method auto || true
    nmcli connection modify "$UUID" wifi.cloned-mac-address permanent || true
  else
    echo "[PATCH] No active Wi-Fi connection to tune"
  fi

  echo "[PATCH] Applying upload speed optimizations..."
  
  cat > /etc/modprobe.d/rtw89_advanced.conf << 'EOF'
# RTL8852BE upload speed optimizations and bufferbloat mitigation
options rtw89_8852be disable_clkreq=1
options rtw89_pci disable_aspm=1 disable_clkreq=1
options rtw89_core tx_ampdu_subframes=32
options rtw89_core rx_ampdu_factor=2
options rtw89_8852be thermal_th=85
options rtw89_pci tx_queue_len=1000
options rtw89_8852be tx_power_reduction=2
EOF

  log_info "Configuring network queue discipline for bufferbloat control..."
  tc qdisc del dev "$ifc" root 2>/dev/null || true
  
  local current_ssid
  current_ssid=$(get_current_ssid)
  local bandwidth="200mbit"
  
  if [[ -n "$current_ssid" ]]; then
    log_info "Current network: $current_ssid"
    
    if load_network_profile "$current_ssid"; then
      log_info "Loaded saved profile for $current_ssid: $BANDWIDTH"
      bandwidth="$BANDWIDTH"
    else
      log_info "No profile found for $current_ssid, creating new profile..."
      
      log_info "Checking network activity before bandwidth detection..."
      local rx1 tx1 rx2 tx2 rx_rate tx_rate total_rate
      rx1=$(cat "/sys/class/net/$ifc/statistics/rx_bytes" 2>/dev/null || echo 0)
      tx1=$(cat "/sys/class/net/$ifc/statistics/tx_bytes" 2>/dev/null || echo 0)
      sleep 2
      rx2=$(cat "/sys/class/net/$ifc/statistics/rx_bytes" 2>/dev/null || echo 0)
      tx2=$(cat "/sys/class/net/$ifc/statistics/tx_bytes" 2>/dev/null || echo 0)
      rx_rate=$(( (rx2 - rx1) / 2048 ))
      tx_rate=$(( (tx2 - tx1) / 2048 ))
      total_rate=$((rx_rate + tx_rate))
      
      if [[ $total_rate -gt 500 ]]; then
        log_warning "Network is BUSY (${total_rate} KB/s) - skipping bandwidth detection"
        log_warning "Using safe default: ${bandwidth}"
        log_warning "Profile will be created on next idle connection"
      else
        log_info "Network is IDLE (${total_rate} KB/s) - safe to detect bandwidth"
        
        local link_speed
        local overhead_percent=85
        
        link_speed=$(iw dev "$ifc" link 2>/dev/null | grep -oP 'tx bitrate: \K[0-9]+' | head -1 || true)
        
        if [[ -z "$link_speed" ]]; then
            link_speed=$(ethtool "$ifc" 2>/dev/null | grep -oP 'Speed: \K[0-9]+' | head -1 || true)
            if [[ -n "$link_speed" ]]; then
                overhead_percent=95
                log_info "Ethernet detected, using aggressive ${overhead_percent}% limit"
            fi
        fi
        
        if [[ -n "$link_speed" && $link_speed -gt 0 ]]; then
          local cake_limit=$((link_speed * overhead_percent / 100))
          bandwidth="${cake_limit}mbit"
          log_info "Detected link speed: ${link_speed}Mbit/s, setting CAKE to ${cake_limit}Mbit/s (${overhead_percent}%)"
        else
          log_warning "Could not detect link speed, using default ${bandwidth}"
        fi
        
        save_network_profile "$current_ssid" "$bandwidth" "auto"
      fi
    fi
  else
    log_warning "No active network detected, using default bandwidth"
  fi
  
  if tc qdisc add dev "$ifc" root cake bandwidth "$bandwidth" diffserv4 dual-dsthost nat wash ack-filter 2>/dev/null; then
    log_success "Applied CAKE qdisc with bandwidth ${bandwidth}"
    if tc qdisc show dev "$ifc" | grep -q "cake"; then
      log_info "CAKE qdisc verified active on $ifc"
    fi
  else
    log_warning "CAKE unavailable, falling back to fq_codel"
    if tc qdisc add dev "$ifc" root handle 1: fq_codel limit 300 target 2ms interval 50ms quantum 300 ecn 2>/dev/null; then
      log_info "Applied aggressive fq_codel for bufferbloat control"
    else
      log_warning "Could not apply any queue discipline"
    fi
  fi
  
  echo "$bandwidth" > "$STATE_DIR/cake_bandwidth.txt"
  
  local saved_bandwidth
  saved_bandwidth=$(cat "$STATE_DIR/cake_bandwidth.txt" 2>/dev/null || echo "200mbit")
  
  cat > /etc/systemd/system/wifi-cake-qdisc@.service <<EOF
[Unit]
Description=Apply CAKE qdisc to %I for bufferbloat control
After=network-online.target sys-subsystem-net-devices-%i.device
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'tc qdisc del dev %I root 2>/dev/null || true; tc qdisc add dev %I root cake bandwidth ${saved_bandwidth} diffserv4 dual-dsthost nat wash ack-filter'
ExecStop=/bin/sh -c 'test -d /sys/class/net/%I && tc qdisc del dev %I root 2>/dev/null || true'

[Install]
WantedBy=multi-user.target
EOF

  systemctl enable "wifi-cake-qdisc@${ifc}.service" 2>/dev/null || log_warning "Could not enable CAKE systemd service"
  systemctl daemon-reload 2>/dev/null || true
  log_success "CAKE qdisc will persist across reboots"
  
  cat > /etc/systemd/system/wifi-optimizations-verify.service << 'EOF'
[Unit]
Description=Verify Wi-Fi optimizations after system updates
After=network-online.target multi-user.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/bin/bash -c 'if [[ -f /var/lib/wifi_patch/applied.flag ]]; then sysctl -p /etc/sysctl.d/99-wifi-upload-opt.conf 2>/dev/null || true; udevadm trigger --subsystem-match=net 2>/dev/null || true; fi'
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

  systemctl enable wifi-optimizations-verify.service 2>/dev/null || true
  log_success "Update protection enabled for immutable system"
  
  log_info "Setting up automatic network detection..."
  cp "$PROJECT_ROOT/src/dispatcher.sh" /etc/NetworkManager/dispatcher.d/99-wifi-auto-optimize
  chmod +x /etc/NetworkManager/dispatcher.d/99-wifi-auto-optimize
  log_success "Automatic network optimization enabled - new networks will be configured automatically"
  
  cp "$PROJECT_ROOT/config/99-wifi-upload-opt.conf" /etc/sysctl.d/99-wifi-upload-opt.conf
  sysctl -p /etc/sysctl.d/99-wifi-upload-opt.conf 2>/dev/null || true

  echo "[PATCH] Optimizing IRQ affinity..."
  local wifi_irq=$(grep -E "rtw89_8852be|rtw89" /proc/interrupts | awk -F: '{print $1}' | head -n1 | tr -d ' ')
  if [[ -n "$wifi_irq" && -f "/proc/irq/$wifi_irq/smp_affinity" ]]; then
    echo "2" > "/proc/irq/$wifi_irq/smp_affinity" 2>/dev/null || true
    echo "[INFO] Wi-Fi IRQ $wifi_irq bound to CPU 1"
  fi

  ethtool -K "$ifc" tso off gso off gro on 2>/dev/null || true

  if [[ "${NO_IWD:-0}" -eq 0 ]]; then
      if enable_iwd; then
          # Restart NetworkManager to apply the backend change immediately
          log_info "Restarting NetworkManager to apply iwd backend..."
          systemctl restart NetworkManager
          
          # Wait for NetworkManager to come back up and Wi-Fi to reconnect
          log_info "Waiting for Wi-Fi to reconnect..."
          local reconnect_timeout=30
          local elapsed=0
          local connected=false
          
          while [[ $elapsed -lt $reconnect_timeout ]]; do
              sleep 2
              elapsed=$((elapsed + 2))
              
              # Check if NetworkManager is running and if we have a Wi-Fi connection
              if systemctl is-active --quiet NetworkManager; then
                  if nmcli -t -f DEVICE,STATE device status 2>/dev/null | grep -q "^wlan.*:connected\|^wlp.*:connected"; then
                      connected=true
                      log_success "Wi-Fi reconnected successfully"
                      # Give the network extra time to fully stabilize (DHCP, routing, DNS)
                      log_info "Allowing network to stabilize..."
                      sleep 5
                      break
                  fi
              fi
              
              if [[ $((elapsed % 10)) -eq 0 ]]; then
                  log_info "Still waiting for Wi-Fi connection... (${elapsed}s elapsed)"
              fi
          done
          
          if [[ "$connected" = false ]]; then
              log_warning "Wi-Fi did not reconnect within ${reconnect_timeout}s. Continuing anyway..."
              log_warning "You may need to manually reconnect to your network."
          fi
      fi
  else
      log_info "Skipping iwd enablement (--no-iwd flag used)"
  fi

  if pidof iwd &>/dev/null || grep -q "wifi.backend=iwd" /etc/NetworkManager/conf.d/*.conf 2>/dev/null; then
    echo "[INFO] iNet Wireless Daemon (iwd) detected. Applying specific optimizations..."
    
    local iwd_conf="/etc/iwd/main.conf"
    mkdir -p /etc/iwd
    
    if [[ ! -f "$iwd_conf" ]]; then
      cat > "$iwd_conf" <<EOF
[General]
ControlPortOverNL80211=true
RoamThreshold=-75
RoamThreshold5G=-80
AddressRandomization=network
ManagementFrameProtection=1

[Scan]
DisablePeriodicScan=true

[Rank]
BandModifier2_4GHz=1.0
BandModifier5GHz=2.0
BandModifier6GHz=3.0
EOF
      echo "[PATCH] Created optimized /etc/iwd/main.conf"
    else
      echo "[INFO] Existing /etc/iwd/main.conf found. Skipping overwrite to preserve user settings."
    fi
  fi

  rfkill unblock wifi || true

  touch "$STATE_FLAG"
  echo "[PATCH] Applied with upload optimizations. Reconnect or reboot for full effect. Use --revert to undo."
}

function revert_patches() {
  log_info "Reverting Wi-Fi tweaks..."
  # Remove all Wi-Fi driver configuration files
  rm -f /etc/modprobe.d/rtw89.conf /etc/modprobe.d/rtw89_advanced.conf || true
  rm -f /etc/modprobe.d/rtw88.conf /etc/modprobe.d/rtl_legacy.conf || true
  rm -f /etc/modprobe.d/rtl8192ee.conf /etc/modprobe.d/rtl_wifi.conf || true
  rm -f /etc/modprobe.d/rtl8822ce.conf /etc/modprobe.d/mt7921e.conf || true
  rm -f /etc/modprobe.d/mediatek.conf /etc/modprobe.d/iwlwifi.conf || true
  rm -f /etc/modprobe.d/ath_wifi.conf /etc/modprobe.d/broadcom.conf || true
  rm -f /etc/modprobe.d/ralink.conf /etc/modprobe.d/marvell.conf || true
  rm -f /etc/modprobe.d/wifi_generic.conf || true
  rm -f /etc/udev/rules.d/70-wifi-powersave.rules || true
  rm -f /etc/udev/rules.d/70-wifi-power-ac.rules || true
  rm -f /usr/local/bin/wifi-power-manager.sh || true
  rm -f /usr/local/bin/wifi-desktop-performance.sh || true
  rm -f /etc/NetworkManager/dispatcher.d/99-wifi-auto-optimize || true
  rm -f /etc/sysctl.d/99-wifi-upload-opt.conf || true
  
  # Remove iwd optimization config if we created it (check if it matches our signature)
  if [[ -f "/etc/iwd/main.conf" ]]; then
    if grep -q "ControlPortOverNL80211=true" "/etc/iwd/main.conf" && grep -q "BandModifier6GHz=3.0" "/etc/iwd/main.conf"; then
       rm -f "/etc/iwd/main.conf"
       log_info "Removed optimized /etc/iwd/main.conf"
    fi
  fi

  # Revert backend to default (wpa_supplicant) if currently on iwd
  if NetworkManager --print-config 2>/dev/null | grep -q "wifi.backend=iwd"; then
      log_info "Reverting Wi-Fi backend to default (wpa_supplicant)..."
      
      # Manual revert to avoid TUI and ensure consistency
      rm -f /etc/NetworkManager/conf.d/iwd.conf
      rm -f /etc/NetworkManager/conf.d/wifi_backend.conf
      
      # Unmask wpa_supplicant
      systemctl unmask wpa_supplicant.service 2>/dev/null || true
      
      # Only remove Wi-Fi connections to ensure clean state for wpa_supplicant
      log_info "Cleaning up iwd connection profiles..."
      timeout 10 nmcli -t -f UUID,TYPE connection show | { grep ":802-11-wireless" || true; } | cut -d: -f1 | while read -r uuid; do
          timeout 5 nmcli connection delete "$uuid" 2>/dev/null || true
      done
      
      log_info "Restarting NetworkManager..."
      systemctl restart NetworkManager
      sleep 5
  fi

  # Restore default queue discipline
  local ifc
  ifc=$(detect_interface) || true
  if [[ -n "$ifc" ]]; then
    tc qdisc del dev "$ifc" root 2>/dev/null || true
    log_info "Removed CAKE/fq_codel from $ifc"
    # Reset ethtool settings to defaults
    ethtool -K "$ifc" tso on gso on gro on 2>/dev/null || true
    
    # Disable and remove systemd service for CAKE
    if systemctl is-enabled "wifi-cake-qdisc@${ifc}.service" &>/dev/null; then
      systemctl disable "wifi-cake-qdisc@${ifc}.service" 2>/dev/null || true
      log_info "Disabled CAKE systemd service"
    fi
  fi
  
  # Remove systemd service files
  rm -f /etc/systemd/system/wifi-cake-qdisc@.service || true
  
  # Disable and remove verification service
  if systemctl is-enabled wifi-optimizations-verify.service &>/dev/null; then
    systemctl disable wifi-optimizations-verify.service 2>/dev/null || true
  fi
  rm -f /etc/systemd/system/wifi-optimizations-verify.service || true
  
  systemctl daemon-reload 2>/dev/null || true

  # Restore IRQ affinity to default (all CPUs)
  local wifi_irq=$( { grep -E "rtw89_8852be|rtw89" /proc/interrupts 2>/dev/null || true; } | awk -F: '{print $1}' | head -n1 | tr -d ' ')
  if [[ -n "$wifi_irq" && -f "/proc/irq/$wifi_irq/smp_affinity" ]]; then
    echo "f" > "/proc/irq/$wifi_irq/smp_affinity" 2>/dev/null || true
  fi

  # Restore active & known backed up connections
  for f in "$BACKUP_PREFIX"-*.txt; do
    [[ -e "$f" ]] || continue
    uuid=$(basename "$f" | sed -E 's/backup-([0-9a-fA-F-]+)\.txt/\1/')
    restore_connection "$uuid"
  done

  rm -f "$STATE_FLAG" || true
  
  # Automatically reload the module if we're reverting driver changes
  if lsmod | grep -q rtw89_8852be; then
    echo "[REVERT] Reloading rtw89_8852be module to apply changes..."
    modprobe -r rtw89_8852be 2>/dev/null || true
    sleep 2
    modprobe rtw89_8852be 2>/dev/null || true
    sleep 3
    echo "[REVERT] Module reloaded. Wi-Fi should reconnect automatically."
  fi
  
  echo "[REVERT] Complete. All patches reverted and driver reloaded."
}
