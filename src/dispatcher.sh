#!/bin/bash
# Auto-apply Wi-Fi optimizations when connecting to new networks
# Triggered by NetworkManager on connection up/down events

INTERFACE="$1"
ACTION="$2"

# Only process Wi-Fi interfaces on connection-up
# [[ "$INTERFACE" =~ ^wl ]] || exit 0
[[ "$ACTION" == "up" ]] || exit 0

STATE_DIR="/var/lib/wifi_patch"
PROFILES_DIR="$STATE_DIR/networks"
LOGFILE="$STATE_DIR/auto-optimize.log"

mkdir -p "$PROFILES_DIR" 2>/dev/null

# Get current SSID or Connection Name (for Ethernet)
get_ssid() {
    # Try Wi-Fi SSID first
    local ssid=$(nmcli -t -f active,ssid dev wifi 2>/dev/null | grep '^yes' | cut -d: -f2 | head -1)
    if [[ -n "$ssid" ]]; then
        echo "$ssid"
    else
        # Fallback to connection name (works for Ethernet)
        nmcli -t -f NAME,DEVICE connection show --active | grep ":$INTERFACE" | cut -d: -f1 | head -1
    fi
}

# Sanitize SSID for filename
sanitize_ssid() {
    echo "$1" | tr -cd 'a-zA-Z0-9._-' | head -c 100
}

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOGFILE"
}

SSID=$(get_ssid)
[[ -z "$SSID" ]] && exit 0

SAFE_SSID=$(sanitize_ssid "$SSID")
PROFILE_FILE="$PROFILES_DIR/${SAFE_SSID}.conf"

# Tiered expiry configuration
EXPIRY_DAILY=180      # 6 months for networks used 5-7x per week
EXPIRY_REGULAR=90     # 3 months for networks used 2-4x per week
EXPIRY_OCCASIONAL=30  # 1 month for networks used <2x per week
EXPIRY_NEW=90         # Default for new networks

# Function to calculate connection frequency
get_frequency() {
    local profile="$1"
    [[ ! -f "$profile" ]] && echo "0" && return
    
    local count=$(grep '^CONNECTION_COUNT=' "$profile" 2>/dev/null | cut -d= -f2)
    local created=$(grep '^CREATED_DATE=' "$profile" 2>/dev/null | cut -d= -f2)
    
    count=${count:-1}
    created=${created:-$(date +%s)}
    
    local now=$(date +%s)
    local age_weeks=$(( (now - created) / 604800 ))
    [[ $age_weeks -lt 1 ]] && age_weeks=1
    
    local freq=$((count / age_weeks))
    echo "$freq"
}

# Function to calculate expiry days based on usage
calc_expiry_days() {
    local profile="$1"
    [[ ! -f "$profile" ]] && echo "$EXPIRY_NEW" && return
    
    local freq=$(get_frequency "$profile")
    
    if [[ $freq -ge 5 ]]; then
        echo "$EXPIRY_DAILY"
    elif [[ $freq -ge 2 ]]; then
        echo "$EXPIRY_REGULAR"
    else
        echo "$EXPIRY_OCCASIONAL"
    fi
}

# Function to check if profile is expired
is_expired() {
    local profile="$1"
    [[ ! -f "$profile" ]] && return 0
    
    if ! grep -q '^EXPIRY_DATE=' "$profile"; then
        # Old profile - add expiry and connection tracking
        local now=$(date +%s)
        local expiry=$((now + EXPIRY_NEW * 86400))
        echo "CREATED_DATE=$now" >> "$profile"
        echo "EXPIRY_DATE=$expiry" >> "$profile"
        echo "CONNECTION_COUNT=1" >> "$profile"
        return 1  # Not expired, just migrated
    fi
    
    local expiry=$(grep '^EXPIRY_DATE=' "$profile" | cut -d= -f2)
    local now=$(date +%s)
    [[ $now -gt $expiry ]]
}

# Function to renew profile expiry with tiered system
renew_expiry() {
    local profile="$1"
    
    # Increment connection count
    local count=$(grep '^CONNECTION_COUNT=' "$profile" 2>/dev/null | cut -d= -f2)
    count=${count:-0}
    count=$((count + 1))
    
    if grep -q '^CONNECTION_COUNT=' "$profile"; then
        sed -i "s/^CONNECTION_COUNT=.*/CONNECTION_COUNT=$count/" "$profile"
    else
        echo "CONNECTION_COUNT=$count" >> "$profile"
    fi
    
    # Calculate new expiry based on frequency
    local expiry_days=$(calc_expiry_days "$profile")
    local now=$(date +%s)
    local expiry=$((now + expiry_days * 86400))
    
    sed -i "s/^EXPIRY_DATE=.*/EXPIRY_DATE=$expiry/" "$profile"
    
    local freq=$(get_frequency "$profile")
    log "Renewed: ${count} connections, ${freq}/week, ${expiry_days}d expiry"
}

# Function to check if network is idle (not busy with downloads)
is_network_idle() {
    local interface="$1"
    local threshold_kbps=500  # Consider busy if >500 KB/s traffic
    
    # Get initial byte counters
    local rx1=$(cat "/sys/class/net/$interface/statistics/rx_bytes" 2>/dev/null || echo 0)
    local tx1=$(cat "/sys/class/net/$interface/statistics/tx_bytes" 2>/dev/null || echo 0)
    
    sleep 2  # Sample period
    
    # Get byte counters after delay
    local rx2=$(cat "/sys/class/net/$interface/statistics/rx_bytes" 2>/dev/null || echo 0)
    local tx2=$(cat "/sys/class/net/$interface/statistics/tx_bytes" 2>/dev/null || echo 0)
    
    # Calculate rate in KB/s
    local rx_rate=$(( (rx2 - rx1) / 2048 ))  # Bytes to KB/s
    local tx_rate=$(( (tx2 - tx1) / 2048 ))
    local total_rate=$((rx_rate + tx_rate))
    
    log "Network activity: ${total_rate} KB/s (threshold: ${threshold_kbps} KB/s)"
    
    [[ $total_rate -lt $threshold_kbps ]]
}

# Check if profile already exists and is not expired
if [[ -f "$PROFILE_FILE" ]]; then
    if is_expired "$PROFILE_FILE"; then
        log "Profile for $SSID expired (>7 days old), will recreate when network is idle..."
        
        # Check if network is busy before recreating profile
        if ! is_network_idle "$INTERFACE"; then
            log "Network is busy, skipping bandwidth detection to avoid interference"
            log "Profile will be recreated on next idle connection"
            rm -f "$PROFILE_FILE"
            exit 0
        fi
        rm -f "$PROFILE_FILE"
    else
        log "Network $SSID already has a profile, applying saved settings..."
        source "$PROFILE_FILE"
        
        # Renew expiry since we're using this profile
        renew_expiry "$PROFILE_FILE"
        log "Renewed profile expiry for $SSID (+7 days)"
        
        # Apply saved bandwidth with CAKE
        tc qdisc del dev "$INTERFACE" root 2>/dev/null || true
        if tc qdisc add dev "$INTERFACE" root cake bandwidth "$BANDWIDTH" diffserv4 dual-dsthost nat wash ack-filter 2>/dev/null; then
            log "Applied CAKE qdisc with saved bandwidth $BANDWIDTH on $INTERFACE"
        fi
        
        # Apply saved power mode
        if [[ "$POWER_MODE" == "on" ]]; then
            iw dev "$INTERFACE" set power_save on 2>/dev/null
            log "Applied power save: on"
        elif [[ "$POWER_MODE" == "off" ]]; then
            iw dev "$INTERFACE" set power_save off 2>/dev/null
            log "Applied power save: off"
        fi
        exit 0
    fi
fi

if true; then  # New profile creation block
    log "New network detected: $SSID - checking network activity before profiling..."
    
    # Wait for connection to stabilize before checking activity
    sleep 3
    
    # Check if network is busy before bandwidth detection
    if ! is_network_idle "$INTERFACE"; then
        log "Network is BUSY (active downloads/uploads detected)"
        log "Skipping bandwidth detection to avoid interference with ongoing transfers"
        log "Will create profile on next idle connection to this network"
        log "Using safe defaults for now: 200mbit, standard power mode"
        
        # Apply safe defaults without creating profile
        tc qdisc del dev "$INTERFACE" root 2>/dev/null || true
        tc qdisc add dev "$INTERFACE" root cake bandwidth 200mbit diffserv4 dual-dsthost nat wash ack-filter 2>/dev/null || true
        iw dev "$INTERFACE" set power_save off 2>/dev/null
        exit 0
    fi
    
    log "Network is IDLE - safe to perform bandwidth detection"
    
    # Auto-detect bandwidth
    # Try iw for Wi-Fi first
    LINK_SPEED=$(iw dev "$INTERFACE" link 2>/dev/null | grep -oP 'tx bitrate: \K[0-9]+' | head -1 || true)
    OVERHEAD_PERCENT=85
    
    # If iw failed (likely Ethernet), try ethtool
    if [[ -z "$LINK_SPEED" ]]; then
        LINK_SPEED=$(ethtool "$INTERFACE" 2>/dev/null | grep -oP 'Speed: \K[0-9]+' | head -1 || true)
        if [[ -n "$LINK_SPEED" ]]; then
            OVERHEAD_PERCENT=95
            log "Ethernet detected, using aggressive ${OVERHEAD_PERCENT}% limit"
        fi
    fi
    
    if [[ -n "$LINK_SPEED" && $LINK_SPEED -gt 0 ]]; then
        CAKE_LIMIT=$((LINK_SPEED * OVERHEAD_PERCENT / 100))
        BANDWIDTH="${CAKE_LIMIT}mbit"
        log "Detected link speed: ${LINK_SPEED}Mbit/s, setting CAKE to ${CAKE_LIMIT}Mbit/s (${OVERHEAD_PERCENT}%)"
    else
        BANDWIDTH="200mbit"
        log "Could not detect link speed, using default $BANDWIDTH"
    fi
    
    # Apply CAKE
    tc qdisc del dev "$INTERFACE" root 2>/dev/null || true
    if tc qdisc add dev "$INTERFACE" root cake bandwidth "$BANDWIDTH" diffserv4 dual-dsthost nat wash ack-filter 2>/dev/null; then
        log "Applied CAKE qdisc with bandwidth $BANDWIDTH on $INTERFACE"
    else
        log "CAKE unavailable, using fq_codel fallback"
        tc qdisc add dev "$INTERFACE" root handle 1: fq_codel limit 300 target 2ms interval 50ms quantum 300 ecn 2>/dev/null || true
    fi
    
    # Determine power mode based on device type with robust detection
    POWER_MODE="off"  # Default: performance mode
    HAS_BATTERY=0
    
    # Check for real system battery (not peripheral devices like mice)
    if [[ -d /sys/class/power_supply/BAT0 ]] || [[ -d /sys/class/power_supply/BAT1 ]] || \
       [[ -d /sys/class/power_supply/battery ]]; then
        HAS_BATTERY=1
    else
        # Check chassis type to distinguish desktops from laptops
        if [[ -f /sys/class/dmi/id/chassis_type ]]; then
            CHASSIS_TYPE=$(cat /sys/class/dmi/id/chassis_type 2>/dev/null)
            # 8,9,10,11,14,30,31 = Portable/Laptop/Notebook/Tablet
            [[ "$CHASSIS_TYPE" =~ ^(8|9|10|11|14|30|31)$ ]] && HAS_BATTERY=1
        fi
        
        # If still uncertain, check for real batteries (exclude peripherals)
        if [[ $HAS_BATTERY -eq 0 ]]; then
            for bat in /sys/class/power_supply/*/type; do
                if grep -q "Battery" "$bat" 2>/dev/null; then
                    BAT_DIR=$(dirname "$bat")
                    BAT_NAME=$(basename "$BAT_DIR")
                    # Exclude peripheral batteries
                    if [[ ! "$BAT_NAME" =~ (hidpp|hid|mouse|keyboard|wacom|peripheral) ]]; then
                        # Real batteries have capacity
                        if [[ -f "$BAT_DIR/capacity" ]]; then
                            HAS_BATTERY=1
                            break
                        fi
                    fi
                fi
            done
        fi
    fi
    
    if [[ $HAS_BATTERY -eq 1 ]]; then
        # Battery device - check AC status with multiple methods
        AC_ONLINE=0
        
        # Method 1: Check AC adapter
        for ac in /sys/class/power_supply/AC*/online /sys/class/power_supply/ADP*/online; do
            if [[ -f "$ac" ]] && [[ $(cat "$ac" 2>/dev/null) == "1" ]]; then
                AC_ONLINE=1
                break
            fi
        done
        
        # Method 2: Check battery status
        if [[ $AC_ONLINE -eq 0 ]]; then
            for bat in /sys/class/power_supply/BAT*/status /sys/class/power_supply/battery/status; do
                if [[ -f "$bat" ]]; then
                    BAT_STATUS=$(cat "$bat" 2>/dev/null)
                    if [[ "$BAT_STATUS" =~ ^(Charging|Full|Not\ charging)$ ]]; then
                        AC_ONLINE=1
                        break
                    fi
                fi
            done
        fi
        
        if [[ $AC_ONLINE -eq 1 ]]; then
            POWER_MODE="off"
            iw dev "$INTERFACE" set power_save off 2>/dev/null
            log "Battery device on AC power - PERFORMANCE mode (power_save=off)"
        else
            POWER_MODE="on"
            iw dev "$INTERFACE" set power_save on 2>/dev/null
            log "Battery device on battery - POWER SAVING mode (power_save=on)"
        fi
    else
        # Desktop - ALWAYS maximum performance
        POWER_MODE="off"
        iw dev "$INTERFACE" set power_save off 2>/dev/null
        ethtool -s "$INTERFACE" speed 1000 duplex full 2>/dev/null || true
        log "Desktop device - PERFORMANCE mode enforced (power_save=off)"
    fi
    
    # Save profile with tiered expiry
    NOW=$(date +%s)
    EXPIRY_DAYS=$EXPIRY_NEW
    EXPIRY=$((NOW + EXPIRY_DAYS * 86400))
    cat > "$PROFILE_FILE" <<PROFILE
# Wi-Fi optimization profile for: $SSID
# Auto-created: $(date '+%Y-%m-%d %H:%M:%S')
# Expires: $(date -d "@$EXPIRY" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || date -r $EXPIRY '+%Y-%m-%d %H:%M:%S')
# Expiry adjusts based on usage: Daily=180d, Regular=90d, Occasional=30d
SSID="$SSID"
BANDWIDTH="$BANDWIDTH"
POWER_MODE="$POWER_MODE"
CREATED_DATE=$NOW
EXPIRY_DATE=$EXPIRY
CONNECTION_COUNT=1
PROFILE
    log "Created new profile for $SSID (bandwidth=$BANDWIDTH, power_mode=$POWER_MODE, ${EXPIRY_DAYS}d expiry)"
fi

exit 0
