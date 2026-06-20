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

# Detect user
SUDO_USER="${SUDO_USER:-deck}"
USER_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
USER_HOME="${USER_HOME:-/home/$SUDO_USER}"

# 1. Stop and disable service
echo -e "${BLUE}[1/5] Stopping service...${NC}"
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

# 2. Remove systemd service file and NetworkManager configurations
echo -e "${BLUE}[2/5] Removing systemd service and NetworkManager configs...${NC}"
if [[ -f /etc/systemd/system/hifi-wifi.service ]]; then
    rm -f /etc/systemd/system/hifi-wifi.service
    systemctl daemon-reload
    echo "Service file removed."
fi

# Clean up NetworkManager customizations
for nm_file in /etc/NetworkManager/dispatcher.d/99-hifi-wifi-connect \
              /etc/NetworkManager/conf.d/99-hifi-wifi-powersave.conf \
              /etc/NetworkManager/conf.d/99-hifi-wifi-mac.conf; do
    if [[ -f "$nm_file" ]]; then
        rm -f "$nm_file"
        echo "Removed NetworkManager config: $nm_file"
    fi
done
systemctl restart NetworkManager 2>/dev/null || true

# 3. Remove user repair service (SteamOS auto-repair)
echo -e "${BLUE}[3/5] Removing user repair service...${NC}"

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

# Remove PATH from .bashrc
BASHRC="$USER_HOME/.bashrc"
if grep -qF '/var/lib/hifi-wifi' "$BASHRC" 2>/dev/null; then
    # Create backup
    cp "$BASHRC" "$BASHRC.bak"
    # Remove the hifi-wifi lines
    grep -vF '/var/lib/hifi-wifi' "$BASHRC.bak" | grep -v '# hifi-wifi CLI access' > "$BASHRC"
    # Fix ownership
    chown "$SUDO_USER:$SUDO_USER" "$BASHRC"
    rm -f "$BASHRC.bak"
    echo "Removed PATH entry from .bashrc"
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
echo -e "${YELLOW}Note: Open a new terminal for PATH changes to take effect.${NC}"
