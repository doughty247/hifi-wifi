#!/bin/bash

# Track changes made during apply for potential rollback
declare -a CHANGES_MADE=()
APPLY_IN_PROGRESS=0

# Track a change for potential rollback
track_change() {
  local change_type="$1"
  local change_data="$2"
  CHANGES_MADE+=("$change_type:$change_data")
}

# Create a file and automatically track it for rollback
create_tracked_file() {
  local filepath="$1"
  cat > "$filepath"
  track_change "FILE" "$filepath"
}

# Copy a file and automatically track the destination for rollback
cp_tracked() {
  local src="$1"
  local dest="$2"
  cp "$src" "$dest"
  track_change "FILE" "$dest"
}

# Cleanup function for abort/failure scenarios
cleanup_on_abort() {
  local exit_code=$?
  
  # Only cleanup if we're in the middle of apply and it failed
  if [[ $APPLY_IN_PROGRESS -eq 1 && $exit_code -ne 0 ]]; then
    log_warning "Apply failed, reverting changes..."
    
    # Revert changes in reverse order
    for (( idx=${#CHANGES_MADE[@]}-1 ; idx>=0 ; idx-- )) ; do
      local change="${CHANGES_MADE[idx]}"
      local type="${change%%:*}"
      local data="${change#*:}"
      
      case "$type" in
        FILE)
          if [[ -f "$data" ]]; then
            rm -f "$data" 2>/dev/null
            log_info "Removed: $data"
          fi
          ;;
        POWER_SAVE)
          local ifc="$data"
          iw dev "$ifc" set power_save on 2>/dev/null
          log_info "Restored power save on $ifc"
          ;;
        TC_QDISC)
          local ifc="$data"
          tc qdisc del dev "$ifc" root 2>/dev/null || true
          log_info "Removed tc qdisc from $ifc"
          ;;
        SYSTEMD_SERVICE)
          systemctl stop "$data" 2>/dev/null || true
          systemctl disable "$data" 2>/dev/null || true
          rm -f "/etc/systemd/system/$data" 2>/dev/null
          systemctl daemon-reload 2>/dev/null || true
          log_info "Removed service: $data"
          ;;
        BACKEND_CHANGE)
          # AGGRESSIVE cleanup: Remove ALL possible backend config files (fixes GitHub issue #5)
          rm -f /etc/NetworkManager/conf.d/wifi_backend.conf 2>/dev/null || true
          rm -f /etc/NetworkManager/conf.d/iwd.conf 2>/dev/null || true
          rm -f /etc/NetworkManager/conf.d/*hifi*.conf 2>/dev/null || true
          # Unmask wpa_supplicant in case it was masked
          systemctl unmask wpa_supplicant.service 2>/dev/null || true
          systemctl restart NetworkManager 2>/dev/null || true
          log_info "Reverted backend change (cleaned all configs)"
          ;;
      esac
    done
    
    log_success "Cleanup completed, system restored to previous state"
    CHANGES_MADE=()
  fi
  
  APPLY_IN_PROGRESS=0
}

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

# --- Latency Monitoring Functions (Ported from wifi-grader.sh) ---
PING_PIDS=()
TARGETS=("8.8.8.8" "1.1.1.1")

function start_ping_monitor() {
    local label="$1"
    PING_PIDS=()
    
    # log_info "[$label] Starting latency monitor to: ${TARGETS[*]}"
    
    for target in "${TARGETS[@]}"; do
        local safe_target=$(echo "$target" | tr . _)
        # Run ping in background, redirecting output
        # -i 0.2: Fast interval (5 packets/sec) for high resolution
        ping -i 0.2 "$target" > "/tmp/ping_${label}_${safe_target}.txt" 2>&1 &
        PING_PIDS+=($!)
    done
}

function stop_ping_monitor() {
    # Send SIGINT (Ctrl+C) to all pings to force them to print summary
    for pid in "${PING_PIDS[@]}"; do
        kill -2 $pid 2>/dev/null
        wait $pid 2>/dev/null
    done
}

function calc_stats() {
    local label="$1"
    local total_avg=0
    local total_jitter=0
    local count=0
    
    for target in "${TARGETS[@]}"; do
        local safe_target=$(echo "$target" | tr . _)
        local file="/tmp/ping_${label}_${safe_target}.txt"
        
        if [[ -f "$file" ]]; then
            # Parse ping summary: rtt min/avg/max/mdev = 15.1/16.2/18.3/1.1 ms
            local stats=$(grep "rtt" "$file" | tail -1)
            local values=$(echo "$stats" | awk -F'=' '{print $2}' | awk '{print $1}')
            
            # Extract avg and mdev (jitter)
            local avg=$(echo "$values" | cut -d'/' -f2)
            local jitter=$(echo "$values" | cut -d'/' -f4)
            
            if [[ "$avg" =~ ^[0-9.]+$ ]]; then
                total_avg=$(echo "$total_avg + $avg" | bc)
                total_jitter=$(echo "$total_jitter + $jitter" | bc)
                count=$((count + 1))
            fi
            rm -f "$file"
        fi
    done
    
    if [[ $count -gt 0 ]]; then
        FINAL_AVG=$(echo "scale=1; $total_avg / $count" | bc)
        FINAL_JITTER=$(echo "scale=1; $total_jitter / $count" | bc)
    else
        FINAL_AVG="0"
        FINAL_JITTER="0"
    fi
}
# -----------------------------------------------------------------

# Bandwidth measurement functions removed in favor of Link Statistics (v1.3.0)
# User feedback indicates Link Statistics are more reliable and faster than speedtests.

function apply_patches() {
  log_info "Applying enhanced Wi-Fi optimizations..."
  
  # Set up cleanup trap for failures
  trap cleanup_on_abort EXIT ERR
  APPLY_IN_PROGRESS=1
  CHANGES_MADE=()
  
  # Ensure state directories exist (fixes #4: missing directory error)
  mkdir -p "$STATE_DIR" 2>/dev/null || true
  
  local interfaces
  mapfile -t interfaces < <(detect_all_interfaces)
  
  if [[ ${#interfaces[@]} -eq 0 ]]; then
    log_error "Could not auto-detect any active network interface"
    return 1
  fi
  
  log_info "Detected interfaces: ${interfaces[*]}"
  
  # Abort early if network is busy - accurate bandwidth detection requires idle network
  if ! check_network_idle_or_abort "${interfaces[0]}"; then
    return 1
  fi

  if [[ ${DRY_RUN:-0} -eq 1 ]]; then
    log_info "[DRY-RUN] Would apply the following changes:"
    log_info "  - Disable power saving on detected interfaces"
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
    
    create_tracked_file /etc/udev/rules.d/70-wifi-power-ac.rules << 'EOF'
# Adjust Wi-Fi power saving based on AC power status
SUBSYSTEM=="power_supply", ENV{POWER_SUPPLY_ONLINE}=="0", RUN+="/usr/local/bin/wifi-power-manager.sh"
SUBSYSTEM=="power_supply", ENV{POWER_SUPPLY_ONLINE}=="1", RUN+="/usr/local/bin/wifi-power-manager.sh"
EOF
    
    /usr/local/bin/wifi-power-manager.sh
    log_success "Adaptive power management configured (AC=performance, Battery=power-saving)"
  else
    log_info "Applying maximum performance settings for desktop..."
    
    cp "$PROJECT_ROOT/src/desktop-performance.sh" /usr/local/bin/wifi-desktop-performance.sh
    chmod +x /usr/local/bin/wifi-desktop-performance.sh
    
    create_tracked_file /etc/udev/rules.d/70-wifi-powersave.rules << 'EOF'
# Desktop Wi-Fi - ALWAYS maximum performance mode
ACTION=="add", SUBSYSTEM=="net", KERNEL=="wl*", RUN+="/usr/local/bin/wifi-desktop-performance.sh %k"
EOF
    
    log_success "Desktop performance mode configured"
  fi
  
  # Apply driver-specific module parameters
  case "$DRIVER_CATEGORY" in
    rtw89)
      log_info "Applying Realtek RTW89 driver optimizations..."
      create_tracked_file /etc/modprobe.d/rtw89.conf << 'EOF'
# Realtek RTW89 optimizations (RTL8852/RTL8852BE/etc)
# Disable power management for consistent latency
options rtw89_pci disable_aspm=1 disable_clkreq=1
options rtw89_core tx_ampdu_subframes=32
# Disable low power states that cause latency spikes
options rtw89_8852be disable_ps_mode=1
EOF
      ;;
    rtw88)
      log_info "Applying Realtek RTW88 driver optimizations..."
      create_tracked_file /etc/modprobe.d/rtw88.conf << 'EOF'
# Realtek RTW88 optimizations (RTL8822CE/etc)
options rtw88_pci disable_aspm=1
options rtw88_core disable_lps_deep=Y
EOF
      ;;
    rtl_legacy)
      log_info "Applying Legacy Realtek driver optimizations..."
      create_tracked_file /etc/modprobe.d/rtl_legacy.conf << 'EOF'
# Legacy Realtek optimizations (RTL8192EE/RTL8188EE/etc)
options rtl8192ee swenc=1 ips=0 fwlps=0 2>/dev/null || true
options rtl8188ee swenc=1 ips=0 fwlps=0 2>/dev/null || true
options rtl_pci disable_aspm=1
options rtl_usb disable_aspm=1
EOF
      ;;
    mediatek)
      log_info "Applying MediaTek driver optimizations..."
      create_tracked_file /etc/modprobe.d/mediatek.conf << 'EOF'
# MediaTek optimizations (MT7921/MT76/etc)
options mt7921e disable_aspm=1 2>/dev/null || true
options mt76_usb disable_usb_sg=1 2>/dev/null || true
EOF
      ;;
    intel)
      log_info "Applying Intel Wi-Fi driver optimizations..."
      create_tracked_file /etc/modprobe.d/iwlwifi.conf << 'EOF'
# Intel Wi-Fi optimizations
options iwlwifi power_save=0 uapsd_disable=1 11n_disable=0
options iwlmvm power_scheme=1
EOF
      ;;
    atheros)
      log_info "Applying Qualcomm Atheros driver optimizations..."
      create_tracked_file /etc/modprobe.d/ath_wifi.conf << 'EOF'
# Qualcomm Atheros Wi-Fi optimizations
options ath10k_core skip_otp=y 2>/dev/null || true
options ath11k_pci disable_aspm=1 2>/dev/null || true
options ath9k nohwcrypt=0 ps_enable=0 2>/dev/null || true
EOF
      ;;
    broadcom)
      log_info "Applying Broadcom driver optimizations..."
      create_tracked_file /etc/modprobe.d/broadcom.conf << 'EOF'
# Broadcom Wi-Fi optimizations
options brcmfmac roamoff=1 2>/dev/null || true
options wl interference=0 2>/dev/null || true
EOF
      ;;
    ralink)
      log_info "Applying Ralink/MediaTek Legacy optimizations..."
      create_tracked_file /etc/modprobe.d/ralink.conf << 'EOF'
# Ralink/MediaTek Legacy optimizations
options rt2800usb nohwcrypt=0 2>/dev/null || true
options rt2800pci nohwcrypt=0 2>/dev/null || true
EOF
      ;;
    marvell)
      log_info "Applying Marvell driver optimizations..."
      create_tracked_file /etc/modprobe.d/marvell.conf << 'EOF'
# Marvell Wi-Fi optimizations
options mwifiex disable_auto_ds=1 2>/dev/null || true
EOF
      ;;
    *)
      log_info "Applying universal Wi-Fi optimizations (driver-agnostic)..."
      create_tracked_file /etc/modprobe.d/wifi_generic.conf << 'EOF'
# Universal Wi-Fi optimizations
# These settings work across most Wi-Fi drivers
EOF
      ;;
  esac
  
  if ls /etc/modprobe.d/*.conf 2>/dev/null | grep -qE "(rt|mt|iwl|ath|wifi)"; then
    log_success "Created driver-specific configuration"
  fi

  # --- Global Setup ---
  
  # Measure Internet Bandwidth ONCE for all interfaces
  # REPLACED with Link Statistics (v1.3.0)
  log_info "Using Link Statistics for bandwidth estimation (faster/more reliable)..."
  local global_download_speed=0
  local global_upload_speed=0
  
  # Only create rtw89 config if that driver is actually in use
  if [[ "$DRIVER_CATEGORY" == "rtw89" ]]; then
    create_tracked_file /etc/modprobe.d/rtw89_advanced.conf << 'EOF'
# RTL8852BE upload speed optimizations and bufferbloat mitigation
options rtw89_8852be disable_clkreq=1
options rtw89_pci disable_aspm=1 disable_clkreq=1
options rtw89_core tx_ampdu_subframes=32
options rtw89_core rx_ampdu_factor=2
options rtw89_8852be thermal_th=85
options rtw89_pci tx_queue_len=1000
options rtw89_8852be tx_power_reduction=2
EOF
  fi

  # Load IFB module for download shaping (support up to 4 interfaces)
  if ! lsmod | grep -q ifb; then
      modprobe ifb numifbs=4 2>/dev/null || log_warning "Could not load IFB module"
  fi

  # NOTE: wifi-cake-qdisc@.service REMOVED in v1.3.0
  # The systemd service caused race conditions with the dispatcher.
  # The dispatcher (99-wifi-auto-optimize) is now the single source of truth
  # for applying CAKE on connection events.

  # --- Per-Interface Loop ---
  for ifc in "${interfaces[@]}"; do
      log_info "--- Configuring interface: $ifc ---"
      
      # 1. Power Save (Desktop)
      if [[ $is_battery_device -eq 0 ]]; then
          if iw dev "$ifc" set power_save off 2>/dev/null; then
              track_change "POWER_SAVE" "$ifc"
              log_success "Power saving DISABLED on $ifc"
          fi
          ethtool -s "$ifc" speed 1000 duplex full 2>/dev/null || true
          /usr/local/bin/wifi-desktop-performance.sh "$ifc" 2>/dev/null || true
      fi
      
      # 2. UDEV Rules (Power Save)
      local UDEV_FILE="/etc/udev/rules.d/70-wifi-powersave-${ifc}.rules"
      echo "ACTION==\"add\", SUBSYSTEM==\"net\", KERNEL==\"$ifc\", RUN+=\"/usr/sbin/iw dev %k set power_save off\"" | create_tracked_file "$UDEV_FILE"
      
      # 3. NM Connection UUID
      local UUID
      UUID=$(nmcli -t -f UUID,DEVICE connection show --active | grep ":$ifc" | cut -d: -f1 | head -1)
      
      if [[ -n "$UUID" ]]; then
          backup_connection "$UUID"
          echo "[PATCH] Adjusting NetworkManager connection UUID=$UUID ($ifc)"
          nmcli connection modify "$UUID" ipv6.method ignore || true
          nmcli connection modify "$UUID" ipv4.method auto || true
          
          local ifc_type=$(get_interface_type "$ifc")
          if [[ "$ifc_type" == "wifi" ]]; then
              nmcli connection modify "$UUID" wifi.cloned-mac-address permanent || true
          fi
      fi
      
      # 4. Bandwidth & CAKE
      local ifc_type=$(get_interface_type "$ifc")
      local link_speed=0
      local overhead_percent=85
      
      if [[ "$ifc_type" == "ethernet" ]]; then
          link_speed=$(ethtool "$ifc" 2>/dev/null | grep -oP 'Speed: \K[0-9]+' | head -1 || true)
          overhead_percent=95
      else
          # Wi-Fi
          link_speed=$(iw dev "$ifc" link 2>/dev/null | grep -oP 'tx bitrate: \K[0-9]+' | head -1 || true)
          overhead_percent=85
      fi
      
      # Calculate Download Limit
      local bandwidth_limit
      if [[ -n "$link_speed" && "$link_speed" -gt 0 ]]; then
          local link_limit=$((link_speed * overhead_percent / 100))
          
          # Always use Link Speed (v1.3.0 change)
          bandwidth_limit="$link_limit"
          log_info "Using Link Speed limit: ${bandwidth_limit}Mbit/s (Link: ${link_speed}Mbit/s)"
      else
          bandwidth_limit="200" # Default
          log_warning "Could not detect link speed, using default ${bandwidth_limit}Mbit/s"
      fi
      
      [[ $bandwidth_limit -lt 1 ]] && bandwidth_limit=1
      local bandwidth="${bandwidth_limit}mbit"
      
      # Calculate Upload Limit
      # Use the same link-based limit for upload (symmetric assumption or TX rate)
      local upload_limit="$bandwidth_limit"
      [[ $upload_limit -lt 1 ]] && upload_limit=1
      local upload_bandwidth="${upload_limit}mbit"
      
      # Apply CAKE (egress only - ingress shaping via IFB removed in v1.3.0)
      # CAKE on egress handles bufferbloat; download shaping is less critical
      if tc qdisc replace dev "$ifc" root cake bandwidth "$upload_bandwidth" diffserv4 dual-dsthost nat wash ack-filter 2>/dev/null; then
          track_change "TC_QDISC" "$ifc"
          log_success "Applied CAKE on $ifc (bandwidth: $upload_bandwidth)"
      else
          log_warning "Failed to apply CAKE on $ifc"
      fi
      
      ethtool -K "$ifc" tso off gso off gro on 2>/dev/null || true
      
  done
  
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
  track_change "SYSTEMD_SERVICE" "wifi-optimizations-verify.service"
  log_success "Update protection enabled for immutable system"
  
  log_info "Setting up automatic network detection..."
  cp_tracked "$PROJECT_ROOT/src/dispatcher.sh" /etc/NetworkManager/dispatcher.d/99-wifi-auto-optimize
  chmod +x /etc/NetworkManager/dispatcher.d/99-wifi-auto-optimize
  log_success "Automatic network optimization enabled - new networks will be configured automatically"
  
  cp_tracked "$PROJECT_ROOT/config/99-wifi-upload-opt.conf" /etc/sysctl.d/99-wifi-upload-opt.conf
  sysctl -p /etc/sysctl.d/99-wifi-upload-opt.conf 2>/dev/null || true

  echo "[PATCH] Optimizing IRQ affinity..."
  local wifi_irq=$(grep -E "rtw89_8852be|rtw89" /proc/interrupts | awk -F: '{print $1}' | head -n1 | tr -d ' ')
  if [[ -n "$wifi_irq" && -f "/proc/irq/$wifi_irq/smp_affinity" ]]; then
    echo "2" > "/proc/irq/$wifi_irq/smp_affinity" 2>/dev/null || true
    echo "[INFO] Wi-Fi IRQ $wifi_irq bound to CPU 1"
  fi

  ethtool -K "$ifc" tso off gso off gro on 2>/dev/null || true

  # NOTE: hifi-wifi no longer switches WiFi backends (removed in v1.2.1)
  # - SteamOS: Use Developer Options → "Force WPA Supplicant" (iwd is default)
  # - Bazzite: Use `ujust toggle-iwd` to switch backends
  # We only optimize iwd settings if it's ALREADY the active backend

  if pidof iwd &>/dev/null || grep -q "wifi.backend=iwd" /etc/NetworkManager/conf.d/*.conf 2>/dev/null; then
    echo "[INFO] iNet Wireless Daemon (iwd) detected. Applying specific optimizations..."
    
    local iwd_conf="/etc/iwd/main.conf"
    mkdir -p /etc/iwd
    
    if [[ ! -f "$iwd_conf" ]]; then
      create_tracked_file "$iwd_conf" <<EOF
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

  # Force NetworkManager to disable power saving
  log_info "Configuring NetworkManager to disable Wi-Fi power saving..."
  create_tracked_file /etc/NetworkManager/conf.d/99-hifi-wifi-powersave.conf << 'EOF'
[connection]
wifi.powersave=2
EOF

  rfkill unblock wifi || true

  touch "$STATE_FLAG"
  
  # Mark apply as successfully completed - disable cleanup trap
  APPLY_IN_PROGRESS=0
  trap - EXIT ERR
  
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

  # Capture current active connection (Wi-Fi or Ethernet) before backend revert
  local current_connection current_connection_type is_wifi
  current_connection=$(timeout 5 nmcli -t -f NAME connection show --active 2>/dev/null | head -1 || true)
  current_connection_type=$(timeout 5 nmcli -t -f NAME,TYPE connection show --active 2>/dev/null | head -1 | cut -d: -f2 || true)
  
  if [[ "$current_connection_type" == "802-11-wireless" ]]; then
      is_wifi=true
      log_info "Current connection: $current_connection (Wi-Fi)"
  elif [[ "$current_connection_type" == "802-3-ethernet" ]]; then
      is_wifi=false
      log_info "Current connection: $current_connection (Ethernet)"
  else
      is_wifi=false
  fi
  
  # Revert backend to default (wpa_supplicant) - be aggressive about cleanup
  # Check for ANY hifi-wifi related backend configs (including legacy file names)
  local backend_cleanup_needed=false
  if [[ -f /etc/NetworkManager/conf.d/wifi_backend.conf ]] || \
     [[ -f /etc/NetworkManager/conf.d/iwd.conf ]] || \
     ls /etc/NetworkManager/conf.d/*hifi*.conf &>/dev/null 2>&1; then
      backend_cleanup_needed=true
  fi
  
  if [[ "$backend_cleanup_needed" == "true" ]]; then
      log_info "Reverting Wi-Fi backend to default (wpa_supplicant)..."
      
      # Backup network connection profiles before deletion
      local backup_dir
      backup_dir=$(mktemp -d)
      cp -r /etc/NetworkManager/system-connections/. "$backup_dir/" 2>/dev/null || true
      
      # AGGRESSIVE cleanup: Remove ALL hifi-wifi related backend configs
      # This ensures we don't leave any "poisoned" configs behind (fixes GitHub issue #5)
      rm -f /etc/NetworkManager/conf.d/iwd.conf
      rm -f /etc/NetworkManager/conf.d/wifi_backend.conf
      rm -f /etc/NetworkManager/conf.d/*hifi*.conf 2>/dev/null || true
      
      # Unmask wpa_supplicant
      systemctl unmask wpa_supplicant.service 2>/dev/null || true
      
      # Only remove Wi-Fi connections to ensure clean state for wpa_supplicant
      log_info "Cleaning up iwd connection profiles..."
      timeout 10 nmcli -t -f UUID,TYPE connection show | { grep ":802-11-wireless" || true; } | cut -d: -f1 | while read -r uuid; do
          timeout 5 nmcli connection delete "$uuid" 2>/dev/null || true
      done
      
      log_info "Restarting NetworkManager..."
      systemctl restart NetworkManager
      sleep 3
      
      # Restore network connection profiles
      log_info "Restoring network connection profiles..."
      cp -r "$backup_dir/"* /etc/NetworkManager/system-connections/ 2>/dev/null || true
      chmod 600 /etc/NetworkManager/system-connections/* 2>/dev/null || true
      timeout 5 nmcli connection reload || true
      rm -rf "$backup_dir"
      
      # Explicitly reconnect to the original network
      if [[ -n "$current_connection" ]]; then
          log_info "Reconnecting to $current_connection..."
          timeout 10 nmcli connection up "$current_connection" 2>/dev/null || true
          sleep 2
      fi
      
      # Wait for network to reconnect and verify connectivity
      if [[ "$is_wifi" == "true" ]]; then
          log_info "Waiting for Wi-Fi to reconnect..."
      else
          log_info "Waiting for network to reconnect..."
      fi
      local timeout=45
      local elapsed=0
      while [[ $elapsed -lt $timeout ]]; do
          local connected=false
          
          if [[ -n "$current_connection" ]]; then
              # Check if our specific connection is back
              if timeout 5 nmcli -t -f NAME connection show --active 2>/dev/null | grep -q "^${current_connection}$"; then
                  connected=true
              fi
          else
              # Fallback: check for any connected device
              if timeout 5 nmcli -t -f DEVICE,STATE device status 2>/dev/null | grep -q ":connected"; then
                  connected=true
              fi
          fi
          
          if [[ "$connected" == "true" ]]; then
              log_info "Network connection detected."
              # Verify internet connectivity
              if ping -c 1 -W 1 8.8.8.8 &>/dev/null; then
                  log_success "Internet connectivity verified. Successfully reconnected to $current_connection"
                  break
              fi
          fi
          
          sleep 2
          elapsed=$((elapsed + 2))
      done
      
      if [[ $elapsed -ge $timeout ]]; then
          log_warning "Network did not reconnect within timeout. You may need to manually reconnect."
      fi
  elif NetworkManager --print-config 2>/dev/null | grep -q "wifi.backend=iwd"; then
      log_info "System is using iwd, but no hifi-wifi override found. Assuming system default. Skipping backend revert."
  fi

  # Restore default queue discipline
  local ifc
  ifc=$(detect_interface) || true
  if [[ -n "$ifc" ]]; then
    tc qdisc del dev "$ifc" root 2>/dev/null || true
    tc qdisc del dev "$ifc" ingress 2>/dev/null || true
    log_info "Removed CAKE/fq_codel from $ifc"
    # Reset ethtool settings to defaults
    ethtool -K "$ifc" tso on gso on gro on 2>/dev/null || true
  fi
  
  # Clean up IFB interfaces
  for ifb_dev in ifb0 ifb1 ifb2 ifb3; do
    tc qdisc del dev "$ifb_dev" root 2>/dev/null || true
  done
  
  # Disable and remove ALL systemd CAKE services (any interface)
  systemctl stop 'wifi-cake-qdisc@*.service' 2>/dev/null || true
  systemctl disable 'wifi-cake-qdisc@*.service' 2>/dev/null || true
  rm -f /etc/systemd/system/wifi-cake-qdisc@.service || true
  
  # Remove stale bandwidth files (no longer used)
  rm -f "$STATE_DIR"/bandwidth_*.txt 2>/dev/null || true
  rm -f "$STATE_DIR"/upload_bandwidth_*.txt 2>/dev/null || true
  
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
