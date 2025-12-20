#!/bin/bash
# Adaptive Wi-Fi power management based on AC/battery status

INTERFACE=$(ip -o link show | awk -F': ' '{print $2}' | grep -E '^wl' | head -n1)
[[ -z "$INTERFACE" ]] && exit 0

# Check if on AC power with multiple detection methods
on_ac_power() {
    # Method 1: Check AC adapter online status
    for ac in /sys/class/power_supply/AC*/online /sys/class/power_supply/ADP*/online; do
        if [[ -f "$ac" ]]; then
            local status=$(cat "$ac" 2>/dev/null)
            [[ "$status" == "1" ]] && return 0
        fi
    done
    
    # Method 2: Check battery status (not discharging = on AC)
    for bat_status in /sys/class/power_supply/BAT*/status /sys/class/power_supply/battery/status; do
        if [[ -f "$bat_status" ]]; then
            local status=$(cat "$bat_status" 2>/dev/null)
            # Charging, Full, or Not charging means AC is connected
            [[ "$status" =~ ^(Charging|Full|Not\ charging|Unknown)$ ]] && return 0
            # Discharging means on battery
            [[ "$status" == "Discharging" ]] && return 1
        fi
    done
    
    # Default: assume AC power if battery status unclear
    return 0
}

if on_ac_power; then
    # On AC: Maximum performance mode
    iw dev "$INTERFACE" set power_save off 2>/dev/null
    # Also try to set performance mode via ethtool if available
    ethtool -s "$INTERFACE" speed 1000 duplex full 2>/dev/null || true
else
    # On Battery: Enable power saving for better battery life
    iw dev "$INTERFACE" set power_save on 2>/dev/null
fi
