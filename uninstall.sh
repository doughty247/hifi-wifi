#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== hifi-wifi v3.0 Uninstaller ===${NC}"

# Need root
if [[ $EUID -ne 0 ]]; then
    echo -e "${RED}This script must be run as root (sudo).${NC}"
    exit 1
fi

# Source os-release to detect distro
IS_STEAMOS=false
if [ -f /etc/os-release ]; then
    source /etc/os-release
    if [[ "$ID" == "steamos" ]]; then
        IS_STEAMOS=true
    fi
fi

# SteamOS: Handle read-only filesystem for /usr/local/bin
if [[ "$IS_STEAMOS" == true ]]; then
    echo -e "${BLUE}[SteamOS] Preparing filesystem for uninstall...${NC}"
    
    # Unmerge system extensions
    systemd-sysext unmerge 2>/dev/null || true
    sleep 1
    
    # Disable readonly
    steamos-readonly disable 2>&1 | grep -v "Warning:" || true
    sleep 1
    
    # Verify we can write
    if ! touch /usr/local/bin/.test-write 2>/dev/null; then
        echo -e "${YELLOW}Filesystem still read-only, attempting aggressive unmerge...${NC}"
        systemd-sysext unmerge 2>/dev/null || true
        sleep 2
        steamos-readonly disable 2>&1 | grep -v "Warning:" || true
        sleep 1
    fi
    rm -f /usr/local/bin/.test-write 2>/dev/null
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
echo -e "${BLUE}[2/5] Removing systemd service...${NC}"
if [[ -f /etc/systemd/system/hifi-wifi.service ]]; then
    rm -f /etc/systemd/system/hifi-wifi.service
    systemctl daemon-reload
    echo "Service file removed."
else
    echo "Service file not found."
fi

# 3. Remove user repair service (SteamOS auto-repair)
echo -e "${BLUE}[3/5] Removing user repair service...${NC}"
SUDO_USER="${SUDO_USER:-deck}"
USER_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
USER_HOME="${USER_HOME:-/home/$SUDO_USER}"

# Disable and stop user service
sudo -u "$SUDO_USER" systemctl --user disable hifi-wifi-repair.service 2>/dev/null || true
sudo -u "$SUDO_USER" systemctl --user stop hifi-wifi-repair.service 2>/dev/null || true

# Remove user service file
if [[ -f "$USER_HOME/.config/systemd/user/hifi-wifi-repair.service" ]]; then
    rm -f "$USER_HOME/.config/systemd/user/hifi-wifi-repair.service"
    echo "Removed user repair service"
fi

# Reload user daemon
sudo -u "$SUDO_USER" systemctl --user daemon-reload 2>/dev/null || true

# Remove polkit rule
if [[ -f /etc/polkit-1/rules.d/49-hifi-wifi.rules ]]; then
    rm -f /etc/polkit-1/rules.d/49-hifi-wifi.rules
    echo "Removed polkit rule"
fi

# Disable lingering (was enabled for Game Mode support)
loginctl disable-linger "$SUDO_USER" 2>/dev/null || true
echo "Disabled user lingering"

# 4. Remove binary and data directory
echo -e "${BLUE}[4/5] Removing binaries and data...${NC}"
if [[ -d /var/lib/hifi-wifi ]]; then
    rm -rf /var/lib/hifi-wifi
    echo "Removed /var/lib/hifi-wifi"
fi

# Remove symlink
if [[ -L /usr/local/bin/hifi-wifi ]]; then
    rm -f /usr/local/bin/hifi-wifi
    echo "Removed /usr/local/bin/hifi-wifi symlink"
fi

# Remove repair script (stored with binary)
if [[ -f /var/lib/hifi-wifi/repair.sh ]]; then
    rm -f /var/lib/hifi-wifi/repair.sh
    echo "Removed repair script"
fi

# 5. Remove config (optional - ask user)
echo -e "${BLUE}[5/5] Cleaning up configuration...${NC}"
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

# SteamOS: Re-enable read-only filesystem
if [[ "$IS_STEAMOS" == true ]]; then
    echo -e "${BLUE}[SteamOS] Re-enabling read-only filesystem...${NC}"
    steamos-readonly enable 2>&1 | grep -v "Warning:" || true
    echo -e "${GREEN}Filesystem protection restored.${NC}"
fi
