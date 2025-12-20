#!/bin/bash
# Desktop Wi-Fi performance mode - ALWAYS max performance

INTERFACE="$1"
[[ -z "$INTERFACE" ]] && INTERFACE=$(ip -o link show | awk -F': ' '{print $2}' | grep -E '^wl' | head -n1)
[[ -z "$INTERFACE" ]] && exit 0

# Force power saving OFF for maximum performance
iw dev "$INTERFACE" set power_save off 2>/dev/null
ethtool -s "$INTERFACE" speed 1000 duplex full 2>/dev/null || true
