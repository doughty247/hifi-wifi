#!/bin/bash
# Auto-apply Wi-Fi optimizations when connecting to networks
# Triggered by NetworkManager on connection up/down events
#
# v1.3.0: Simplified - always uses fresh Link Statistics (no caching/expiry)
# CAKE always gets accurate bandwidth limits from iw/ethtool
#
# v1.3.0-rc2: Removed is_network_idle() check - apply CAKE immediately
# The idle check caused 4+ second delays and conflicts with Steam auto-downloads

INTERFACE="$1"
ACTION="$2"

[[ "$ACTION" == "up" ]] || exit 0

STATE_DIR="/var/lib/wifi_patch"
LOGFILE="$STATE_DIR/auto-optimize.log"

mkdir -p "$STATE_DIR" 2>/dev/null

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOGFILE"
}

# Get current SSID or Connection Name (for Ethernet)
get_connection_name() {
    local ssid=$(timeout 2 nmcli -t -f active,ssid dev wifi 2>/dev/null | grep '^yes' | cut -d: -f2 | head -1)
    if [[ -n "$ssid" ]]; then
        echo "$ssid"
    else
        timeout 2 nmcli -t -f NAME,DEVICE connection show --active | grep ":$INTERFACE" | cut -d: -f1 | head -1
    fi
}

# Get interface type
get_interface_type() {
    local ifc="$1"
    if [[ "$ifc" =~ ^(wl|wlan|wifi) ]]; then
        echo "wifi"
    elif [[ "$ifc" =~ ^(en|eth) ]]; then
        echo "ethernet"
    else
        local type=$(cat "/sys/class/net/$ifc/type" 2>/dev/null)
        case "$type" in
            1) echo "ethernet" ;;
            *) echo "unknown" ;;
        esac
    fi
}

# Get link speed using iw (Wi-Fi) or ethtool (Ethernet)
# Returns bandwidth limit in Mbit/s
get_link_speed() {
    local ifc="$1"
    local ifc_type=$(get_interface_type "$ifc")
    local speed=""
    local overhead=85
    
    if [[ "$ifc_type" == "ethernet" ]]; then
        speed=$(timeout 2 ethtool "$ifc" 2>/dev/null | grep -oP 'Speed: \K[0-9]+' | head -1)
        overhead=95
    else
        speed=$(timeout 2 iw dev "$ifc" link 2>/dev/null | grep -oP 'tx bitrate: \K[0-9]+' | head -1)
        overhead=85
    fi
    
    if [[ -n "$speed" && "$speed" -gt 0 ]]; then
        local limit=$((speed * overhead / 100))
        [[ $limit -lt 1 ]] && limit=1
        echo "$limit"
        return 0
    fi
    
    echo "200"  # Default fallback
    return 1
}

# Determine power mode based on device type
get_power_mode() {
    # Check for forced performance mode
    if [[ -f "$STATE_DIR/force_performance" ]]; then
        echo "off"
        return
    fi
    
    # Check for system battery
    local has_battery=0
    
    if [[ -d /sys/class/power_supply/BAT0 ]] || [[ -d /sys/class/power_supply/BAT1 ]] || \
       [[ -d /sys/class/power_supply/battery ]]; then
        has_battery=1
    elif [[ -f /sys/class/dmi/id/chassis_type ]]; then
        local chassis=$(cat /sys/class/dmi/id/chassis_type 2>/dev/null)
        [[ "$chassis" =~ ^(8|9|10|11|14|30|31)$ ]] && has_battery=1
    fi
    
    if [[ $has_battery -eq 0 ]]; then
        # Desktop - always performance
        echo "off"
        return
    fi
    
    # Battery device - check AC status
    local ac_online=0
    
    for ac in /sys/class/power_supply/AC*/online /sys/class/power_supply/ADP*/online; do
        if [[ -f "$ac" ]] && [[ $(cat "$ac" 2>/dev/null) == "1" ]]; then
            ac_online=1
            break
        fi
    done
    
    if [[ $ac_online -eq 0 ]]; then
        for bat in /sys/class/power_supply/BAT*/status /sys/class/power_supply/battery/status; do
            if [[ -f "$bat" ]]; then
                local status=$(cat "$bat" 2>/dev/null)
                if [[ "$status" =~ ^(Charging|Full|Not\ charging)$ ]]; then
                    ac_online=1
                    break
                fi
            fi
        done
    fi
    
    if [[ $ac_online -eq 1 ]]; then
        echo "off"  # AC = performance
    else
        echo "on"   # Battery = power save
    fi
}

# --- Main ---

CONNECTION_NAME=$(get_connection_name)
[[ -z "$CONNECTION_NAME" ]] && exit 0

log "Connection UP: $CONNECTION_NAME on $INTERFACE"

# Brief delay to ensure link is established and speed is accurate
# Reduced from 2s to 1s for faster optimization
sleep 1

# Get fresh link speed (instant with iw/ethtool)
LINK_SPEED=$(get_link_speed "$INTERFACE")
BANDWIDTH="${LINK_SPEED}mbit"

log "Link speed: ${LINK_SPEED}Mbit/s -> CAKE bandwidth: $BANDWIDTH"

# Apply CAKE immediately - no idle check needed
# CAKE handles active traffic gracefully; waiting only delays protection
tc qdisc del dev "$INTERFACE" root 2>/dev/null || true
if tc qdisc add dev "$INTERFACE" root cake bandwidth "$BANDWIDTH" diffserv4 dual-dsthost nat wash ack-filter 2>/dev/null; then
    log "Applied CAKE on $INTERFACE: $BANDWIDTH"
else
    log "CAKE unavailable, using fq_codel"
    tc qdisc add dev "$INTERFACE" root handle 1: fq_codel limit 300 target 2ms interval 50ms quantum 300 ecn 2>/dev/null || true
fi

# Apply power mode
POWER_MODE=$(get_power_mode)
if [[ "$POWER_MODE" == "off" ]]; then
    iw dev "$INTERFACE" set power_save off 2>/dev/null
    log "Power save: OFF (performance mode)"
else
    iw dev "$INTERFACE" set power_save on 2>/dev/null
    log "Power save: ON (battery saving)"
fi

exit 0
