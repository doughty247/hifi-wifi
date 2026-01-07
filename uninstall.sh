#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== hifi-wifi v3.0 Uninstaller ===${NC}"

# Need root
if [[ $EUID -ne 0 ]]; then
    echo -e "${RED}This script must be run as root (sudo).${NC}"
    exit 1
fi

# 1. Stop and disable service
echo -e "${BLUE}[1/4] Stopping service...${NC}"
if systemctl is-active --quiet hifi-wifi 2>/dev/null; then
    systemctl stop hifi-wifi
    echo "Service stopped."
else
    echo "Service not running."
fi

if systemctl is-enabled --quiet hifi-wifi 2>/dev/null; then
    systemctl disable hifi-wifi
    echo "Service disabled."
fi

# 2. Remove systemd service file
echo -e "${BLUE}[2/4] Removing systemd service...${NC}"
if [[ -f /etc/systemd/system/hifi-wifi.service ]]; then
    rm -f /etc/systemd/system/hifi-wifi.service
    systemctl daemon-reload
    echo "Service file removed."
else
    echo "Service file not found."
fi

# 3. Remove binary and data directory
echo -e "${BLUE}[3/4] Removing binaries and data...${NC}"
if [[ -d /var/lib/hifi-wifi ]]; then
    rm -rf /var/lib/hifi-wifi
    echo "Removed /var/lib/hifi-wifi"
fi

# Remove symlink
if [[ -L /usr/local/bin/hifi-wifi ]]; then
    rm -f /usr/local/bin/hifi-wifi
    echo "Removed /usr/local/bin/hifi-wifi symlink"
fi

# 4. Remove config (optional - ask user)
echo -e "${BLUE}[4/4] Cleaning up configuration...${NC}"
if [[ -d /etc/hifi-wifi ]]; then
    read -p "Remove configuration files in /etc/hifi-wifi? [y/N] " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        rm -rf /etc/hifi-wifi
        echo "Configuration removed."
    else
        echo "Configuration preserved."
    fi
fi

# Remove any driver configs we created
for conf in /etc/modprobe.d/rtl_legacy.conf \
            /etc/modprobe.d/ralink.conf \
            /etc/modprobe.d/mediatek.conf \
            /etc/modprobe.d/intel_wifi.conf \
            /etc/modprobe.d/atheros.conf \
            /etc/modprobe.d/broadcom.conf; do
    if [[ -f "$conf" ]]; then
        rm -f "$conf"
        echo "Removed $conf"
    fi
done

# Remove sysctl config
if [[ -f /etc/sysctl.d/99-hifi-wifi.conf ]]; then
    rm -f /etc/sysctl.d/99-hifi-wifi.conf
    echo "Removed sysctl config"
fi

# Revert any CAKE qdiscs we might have left
echo -e "${BLUE}Reverting network optimizations...${NC}"
for iface in $(ip -o link show | awk -F': ' '{print $2}' | grep -E '^(wl|eth|en)'); do
    if tc qdisc show dev "$iface" 2>/dev/null | grep -q cake; then
        tc qdisc del dev "$iface" root 2>/dev/null || true
        echo "Removed CAKE from $iface"
    fi
done

echo ""
echo -e "${GREEN}hifi-wifi has been completely uninstalled.${NC}"
