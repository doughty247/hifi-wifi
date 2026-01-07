#!/bin/bash
# uninstall.sh - Complete removal of hifi-wifi (all versions: v1.0.0 - v1.3.x)
#
# This script thoroughly removes ALL hifi-wifi artifacts from your system,
# including files from older versions that may be causing conflicts.
#
# Usage: sudo ./uninstall.sh
#
# After running this script, you can safely reinstall hifi-wifi.

set -e

echo "=========================================="
echo "  hifi-wifi Uninstaller"
echo "=========================================="
echo ""

# Check for root
if [[ $EUID -ne 0 ]]; then
    echo "This script must be run as root (sudo ./uninstall.sh)"
    exit 1
fi

# Detect SteamOS for read-only filesystem handling
IS_STEAMOS=false
if grep -q "SteamOS" /etc/os-release 2>/dev/null || \
   grep -q "steamdeck" /etc/hostname 2>/dev/null; then
    IS_STEAMOS=true
    echo "[INFO] SteamOS detected"
    
    # Disable read-only filesystem
    if command -v steamos-readonly &>/dev/null; then
        echo "[INFO] Disabling read-only filesystem..."
        steamos-readonly disable 2>/dev/null || true
    fi
fi

echo ""
echo "[1/13] Running hifi-wifi --revert (if installed)..."
if command -v hifi-wifi &>/dev/null; then
    # On SteamOS, skip --revert since it re-enables read-only filesystem
    # We'll do manual cleanup instead
    if [[ "$IS_STEAMOS" != "true" ]]; then
        hifi-wifi --revert --quiet 2>/dev/null || echo "  [WARN] Revert had issues, continuing cleanup..."
    else
        echo "  [SKIP] SteamOS detected - will do manual cleanup to keep filesystem writable"
    fi
else
    echo "  [SKIP] hifi-wifi not in PATH"
fi

# Manual revert operations (especially important on SteamOS)
echo ""
echo "[1b/13] Manual cleanup of network optimizations..."
# Remove CAKE from all interfaces
for ifc in $(ip -o link show | awk -F': ' '{print $2}' | grep -E '^(wl|en|eth)'); do
    tc qdisc del dev "$ifc" root 2>/dev/null || true
    tc qdisc del dev "$ifc" ingress 2>/dev/null || true
done
# Clean up IFB devices (legacy)
for ifb in ifb0 ifb1 ifb2 ifb3; do
    tc qdisc del dev "$ifb" root 2>/dev/null || true
done
echo "  [OK] Network optimizations cleared"

echo ""
echo "[2/13] Stopping systemd services..."
# CAKE services (v1.3.0)
systemctl stop 'wifi-cake-qdisc@*.service' 2>/dev/null || true
systemctl disable 'wifi-cake-qdisc@*.service' 2>/dev/null || true
# Verification service (v1.3.0)
systemctl stop wifi-optimizations-verify.service 2>/dev/null || true
systemctl disable wifi-optimizations-verify.service 2>/dev/null || true
# SteamOS restore service (v1.1.0+)
systemctl stop hifi-wifi-restore.service 2>/dev/null || true
systemctl disable hifi-wifi-restore.service 2>/dev/null || true
# v3.0 Rust service
systemctl stop hifi-wifi.service 2>/dev/null || true
systemctl disable hifi-wifi.service 2>/dev/null || true
echo "  [OK] Services stopped"

echo ""
echo "[3/13] Removing systemd service files..."
rm -f /etc/systemd/system/wifi-cake-qdisc@.service
rm -f /etc/systemd/system/wifi-optimizations-verify.service
rm -f /etc/systemd/system/hifi-wifi-restore.service
rm -f /etc/systemd/system/hifi-wifi.service
systemctl daemon-reload
echo "  [OK] Service files removed"

echo ""
echo "[4/13] Removing binaries..."
rm -f /usr/local/bin/hifi-wifi
rm -f /usr/local/bin/wifi-power-manager.sh
rm -f /usr/local/bin/wifi-desktop-performance.sh
echo "  [OK] Binaries removed"

echo ""
echo "[5/13] Removing shared files..."
rm -rf /usr/local/share/hifi-wifi
echo "  [OK] Shared files removed"

echo ""
echo "[6/13] Removing modprobe driver configs..."
# All possible driver config files from all versions
rm -f /etc/modprobe.d/rtw89.conf
rm -f /etc/modprobe.d/rtw89_advanced.conf
rm -f /etc/modprobe.d/rtw88.conf
rm -f /etc/modprobe.d/rtl_legacy.conf
rm -f /etc/modprobe.d/rtl8192ee.conf
rm -f /etc/modprobe.d/rtl_wifi.conf
rm -f /etc/modprobe.d/rtl8822ce.conf
rm -f /etc/modprobe.d/mt7921e.conf
rm -f /etc/modprobe.d/mediatek.conf
rm -f /etc/modprobe.d/iwlwifi.conf
rm -f /etc/modprobe.d/ath_wifi.conf
rm -f /etc/modprobe.d/broadcom.conf
rm -f /etc/modprobe.d/ralink.conf
rm -f /etc/modprobe.d/marvell.conf
rm -f /etc/modprobe.d/wifi_generic.conf
echo "  [OK] Driver configs removed"

echo ""
echo "[7/13] Removing udev rules..."
rm -f /etc/udev/rules.d/70-wifi-powersave.rules
rm -f /etc/udev/rules.d/70-wifi-powersave-*.rules
rm -f /etc/udev/rules.d/70-wifi-power-ac.rules
udevadm control --reload-rules 2>/dev/null || true
echo "  [OK] Udev rules removed"

echo ""
echo "[8/13] Removing NetworkManager configs..."
# Dispatcher script (v1.2.0+)
rm -f /etc/NetworkManager/dispatcher.d/99-wifi-auto-optimize
# Power save config (v1.3.0)
rm -f /etc/NetworkManager/conf.d/99-hifi-wifi-powersave.conf
# Legacy backend configs (v1.1.0 - v1.2.0, caused issue #5)
rm -f /etc/NetworkManager/conf.d/*hifi*.conf 2>/dev/null || true
rm -f /etc/NetworkManager/conf.d/wifi_backend.conf
rm -f /etc/NetworkManager/conf.d/iwd.conf
echo "  [OK] NetworkManager configs removed"

echo ""
echo "[9/13] Removing sysctl configs..."
rm -f /etc/sysctl.d/99-wifi-upload-opt.conf
echo "  [OK] Sysctl configs removed"

echo ""
echo "[10/13] Removing state directories..."
# Main state directory (all versions)
rm -rf /var/lib/wifi_patch
# SteamOS persistence directory (v1.1.0+ and v3.0)
rm -rf /var/lib/hifi-wifi
# v3.0 Config directory
rm -rf /etc/hifi-wifi
echo "  [OK] State directories removed"

echo ""
echo "[11/13] Checking iwd config..."
# Only remove if we created it (check for our signature)
if [[ -f "/etc/iwd/main.conf" ]]; then
    if grep -q "BandModifier6GHz=3.0" "/etc/iwd/main.conf" 2>/dev/null; then
        rm -f /etc/iwd/main.conf
        echo "  [OK] Removed hifi-wifi iwd config"
    else
        echo "  [SKIP] iwd config not created by hifi-wifi"
    fi
else
    echo "  [SKIP] No iwd config found"
fi

echo ""
echo "[12/13] Unmasking wpa_supplicant..."
systemctl unmask wpa_supplicant.service 2>/dev/null || true
echo "  [OK] wpa_supplicant unmasked"

echo ""
echo "[13/13] Removing tc qdisc from interfaces..."
for ifc in $(ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | grep -E '^(wl|en|eth)'); do
    tc qdisc del dev "$ifc" root 2>/dev/null || true
    tc qdisc del dev "$ifc" ingress 2>/dev/null || true
    echo "  [OK] Cleared qdisc on $ifc"
done

echo ""
echo "[FINAL] Restarting NetworkManager..."
systemctl restart NetworkManager 2>/dev/null || true

# Re-enable read-only filesystem on SteamOS
if [[ "$IS_STEAMOS" == "true" ]]; then
    if command -v steamos-readonly &>/dev/null; then
        echo "[INFO] Re-enabling read-only filesystem..."
        steamos-readonly enable 2>/dev/null || true
    fi
fi

echo ""
echo "=========================================="
echo "  hifi-wifi has been completely removed!"
echo "=========================================="
echo ""
echo "Your system has been restored to default network settings."
echo "You can now safely reinstall hifi-wifi if desired:"
echo ""
echo "  git clone https://github.com/doughty247/hifi-wifi.git"
echo "  cd hifi-wifi"
echo "  sudo ./install.sh"
echo ""
