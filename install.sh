#!/bin/bash
# Installer for hifi-wifi

set -e

INSTALL_DIR="/usr/local/bin"
SHARE_DIR="/usr/local/share/hifi-wifi"
NEW_VERSION="1.3.0"

# Function to compare versions (returns 0 if $1 < $2, i.e. $1 is older)
version_lt() {
    [ "$1" != "$2" ] && [ "$1" = "$(echo -e "$1\n$2" | sort -V | head -n1)" ]
}

# Capture current active connection (Wi-Fi or Ethernet) to ensure we reconnect
CURRENT_CONNECTION=$(nmcli -t -f NAME connection show --active 2>/dev/null | head -1 || true)
CURRENT_CONNECTION_TYPE=$(nmcli -t -f NAME,TYPE connection show --active 2>/dev/null | head -1 | cut -d: -f2 || true)

if [[ "$CURRENT_CONNECTION_TYPE" == "802-11-wireless" ]]; then
    IS_WIFI=true
    echo "Current active connection: $CURRENT_CONNECTION (Wi-Fi)"
elif [[ "$CURRENT_CONNECTION_TYPE" == "802-3-ethernet" ]]; then
    IS_WIFI=false
    echo "Current active connection: $CURRENT_CONNECTION (Ethernet)"
else
    IS_WIFI=false
    [[ -n "$CURRENT_CONNECTION" ]] && echo "Current active connection: $CURRENT_CONNECTION"
fi

IS_STEAMOS=false
if grep -q "SteamOS" /etc/os-release 2>/dev/null; then
    IS_STEAMOS=true
    echo "SteamOS detected. Disabling read-only filesystem..."
    sudo steamos-readonly disable
fi

# Check for existing installation and cleanup
if command -v hifi-wifi &>/dev/null; then
    # Get installed version
    INSTALLED_VERSION=""
    IS_LEGACY_INSTALL=false
    
    if [[ -f "$SHARE_DIR/src/lib.sh" ]]; then
        # New-style install (v1.3.0+) - version in lib.sh
        INSTALLED_VERSION=$(grep -oP 'VERSION="\K[^"]+' "$SHARE_DIR/src/lib.sh" 2>/dev/null || true)
    elif [[ -f "/usr/local/bin/hifi-wifi" ]] && [[ ! -d "$SHARE_DIR" ]]; then
        # Old-style install (pre-1.3.0) - single bundled script, no lib directory
        IS_LEGACY_INSTALL=true
    fi
    
    # If upgrading from before v1.3.0, require uninstall first
    # This ensures legacy configs (systemd CAKE service, bandwidth files, etc.) are cleaned up
    if [[ "$IS_LEGACY_INSTALL" == "true" ]] || { [[ -n "$INSTALLED_VERSION" ]] && version_lt "$INSTALLED_VERSION" "1.3.0"; }; then
        echo ""
        echo "=========================================="
        echo "  IMPORTANT: Clean Upgrade Required"
        echo "=========================================="
        echo ""
        if [[ "$IS_LEGACY_INSTALL" == "true" ]]; then
            echo "You have a legacy version of hifi-wifi installed (pre-1.3.0)."
        else
            echo "You have hifi-wifi v${INSTALLED_VERSION} installed."
        fi
        echo ""
        echo "v1.3.0 includes major architectural changes that require a clean"
        echo "uninstall before upgrading. This ensures all legacy configurations"
        echo "are properly removed to prevent conflicts."
        echo ""
        echo "Please run the uninstall script first:"
        echo ""
        echo "  sudo ./uninstall.sh"
        echo ""
        echo "Then re-run this installer:"
        echo ""
        echo "  sudo ./install.sh"
        echo ""
        echo "This is a one-time requirement for the v1.3.0 upgrade."
        echo "Future updates will not require this step."
        echo ""
        echo "=========================================="
        exit 1
    fi
    
    echo "Detected existing installation. Cleaning up old patches..."
    
    # Backup network connections to prevent data loss during revert
    echo "Backing up network connections..."
    NM_BACKUP_DIR=$(mktemp -d)
    sudo cp -r /etc/NetworkManager/system-connections/. "$NM_BACKUP_DIR/" 2>/dev/null || true
    
    # Try to revert patches using the LOCAL version to ensure we use the latest fixes
    # (The installed version might be broken or hanging)
    if [[ -f "./bin/hifi-wifi" ]]; then
        sudo ./bin/hifi-wifi --revert --quiet || echo "Warning: Failed to revert old patches. Proceeding anyway."
    else
        sudo hifi-wifi --revert --quiet || echo "Warning: Failed to revert old patches. Proceeding anyway."
    fi
    
    # Restore network connections
    echo "Restoring network connections..."
    sudo cp -r "$NM_BACKUP_DIR/"* /etc/NetworkManager/system-connections/ 2>/dev/null || true
    sudo chmod 600 /etc/NetworkManager/system-connections/* 2>/dev/null || true
    sudo nmcli connection reload || true
    
    # Force reconnection to pick up restored profiles
    if [[ "$IS_WIFI" == "true" ]]; then
        echo "Triggering Wi-Fi reconnection..."
        sudo nmcli radio wifi off 2>/dev/null || true
        sleep 2
        sudo nmcli radio wifi on 2>/dev/null || true
        sleep 3
        
        # Explicitly bring up the saved connection (SteamOS doesn't auto-reconnect)
        if [[ -n "$CURRENT_CONNECTION" ]]; then
            echo "Reconnecting to $CURRENT_CONNECTION..."
            sudo nmcli connection up "$CURRENT_CONNECTION" 2>/dev/null || true
        else
            # Try to connect to any available known network
            SAVED_WIFI=$(nmcli -t -f NAME,TYPE connection show 2>/dev/null | grep ":802-11-wireless" | cut -d: -f1 | head -1)
            if [[ -n "$SAVED_WIFI" ]]; then
                echo "Reconnecting to $SAVED_WIFI..."
                sudo nmcli connection up "$SAVED_WIFI" 2>/dev/null || true
            fi
        fi
    else
        echo "Reloading network connections..."
        sudo nmcli connection reload
        [[ -n "$CURRENT_CONNECTION" ]] && sudo nmcli connection up "$CURRENT_CONNECTION" 2>/dev/null || true
    fi
    
    # Wait for connection to stabilize before proceeding
    if [[ "$IS_WIFI" == "true" ]]; then
        echo "Waiting for Wi-Fi to reconnect..."
    else
        echo "Waiting for network to reconnect..."
    fi
    timeout=45
    elapsed=0
    while [ $elapsed -lt $timeout ]; do
        CONNECTED=false
        if [[ -n "$CURRENT_CONNECTION" ]]; then
            if nmcli -t -f NAME connection show --active 2>/dev/null | grep -q "^${CURRENT_CONNECTION}$"; then
                CONNECTED=true
            fi
        else
            if nmcli -t -f DEVICE,STATE device status 2>/dev/null | grep -q ":connected"; then
                CONNECTED=true
            fi
        fi
        if [ "$CONNECTED" = true ]; then
            echo "Network connection detected."
            if ping -c 1 -W 1 8.8.8.8 &>/dev/null; then
                echo "Internet connectivity verified."
                break
            fi
        fi
        sleep 2
        elapsed=$((elapsed + 2))
        if [ $((elapsed % 10)) -eq 0 ]; then
             echo "Still waiting... (${elapsed}s)"
        fi
    done
    
    rm -rf "$NM_BACKUP_DIR"
    
    # Re-disable read-only if the revert script re-enabled it
    if [ "$IS_STEAMOS" = true ]; then
        sudo steamos-readonly disable
    fi
fi

echo "Installing hifi-wifi..."

# =============================================================================
# LEGACY CLEANUP (v1.0.0 - v1.3.0-rc1)
# This section removes artifacts from prior versions that may cause conflicts
# =============================================================================
echo "Cleaning up legacy configurations..."

# v1.1.0 - v1.2.0: Backend configs (caused GitHub issue #5)
if ls /etc/NetworkManager/conf.d/*hifi*.conf &>/dev/null 2>&1; then
    echo "  Removing legacy hifi-wifi backend configs..."
    sudo rm -f /etc/NetworkManager/conf.d/*hifi*.conf
fi
if [[ -f /etc/NetworkManager/conf.d/wifi_backend.conf ]]; then
    echo "  Removing legacy wifi_backend.conf..."
    sudo rm -f /etc/NetworkManager/conf.d/wifi_backend.conf
fi
if [[ -f /etc/NetworkManager/conf.d/iwd.conf ]]; then
    echo "  Removing legacy iwd.conf..."
    sudo rm -f /etc/NetworkManager/conf.d/iwd.conf
fi

# v1.2.0: Network profile cache (no longer used in v1.3.0)
if [[ -d /var/lib/wifi_patch/networks ]]; then
    echo "  Removing legacy network profile cache..."
    sudo rm -rf /var/lib/wifi_patch/networks
fi

# v1.3.0-rc1: Stale bandwidth files (systemd CAKE service removed)
if ls /var/lib/wifi_patch/bandwidth_*.txt &>/dev/null 2>&1; then
    echo "  Removing stale bandwidth files..."
    sudo rm -f /var/lib/wifi_patch/bandwidth_*.txt
    sudo rm -f /var/lib/wifi_patch/upload_bandwidth_*.txt
fi

# v1.3.0-rc1: Disable redundant systemd CAKE service (causes race condition)
if systemctl list-unit-files 2>/dev/null | grep -q "wifi-cake-qdisc@"; then
    echo "  Disabling redundant CAKE systemd services..."
    sudo systemctl disable 'wifi-cake-qdisc@*.service' 2>/dev/null || true
    sudo systemctl stop 'wifi-cake-qdisc@*.service' 2>/dev/null || true
fi
sudo rm -f /etc/systemd/system/wifi-cake-qdisc@.service 2>/dev/null || true

# v1.1.0: Unmask wpa_supplicant if it was masked
if systemctl is-enabled wpa_supplicant.service 2>&1 | grep -q "masked"; then
    echo "  Unmasking wpa_supplicant..."
    sudo systemctl unmask wpa_supplicant.service 2>/dev/null || true
fi

sudo systemctl daemon-reload 2>/dev/null || true
echo "Legacy cleanup complete."
echo ""

# Create directories
sudo mkdir -p "$SHARE_DIR/src"
sudo mkdir -p "$SHARE_DIR/config"
sudo mkdir -p "$INSTALL_DIR"
# Create state directories for runtime data (fixes #4: missing directory error)
sudo mkdir -p "/var/lib/wifi_patch"

# Copy files
sudo cp bin/hifi-wifi "$INSTALL_DIR/hifi-wifi"
sudo cp src/* "$SHARE_DIR/src/"
sudo cp config/* "$SHARE_DIR/config/"

# Set permissions
sudo chmod +x "$INSTALL_DIR/hifi-wifi"
sudo chmod +x "$SHARE_DIR/src/"*.sh

if [ "$IS_STEAMOS" = true ]; then
    echo "Setting up persistence for SteamOS updates..."
    
    # Create persistence directories
    PERSIST_DIR="/var/lib/hifi-wifi"
    sudo mkdir -p "$PERSIST_DIR/backup/bin"
    sudo mkdir -p "$PERSIST_DIR/backup/src"
    sudo mkdir -p "$PERSIST_DIR/backup/config"
    sudo mkdir -p "$PERSIST_DIR/pkg_cache"
    
    # Backup files for restoration
    sudo cp bin/hifi-wifi "$PERSIST_DIR/backup/bin/"
    sudo cp src/* "$PERSIST_DIR/backup/src/"
    sudo cp config/* "$PERSIST_DIR/backup/config/"
    
    # Install restore script
    sudo cp src/steamos-restore.sh "$PERSIST_DIR/restore.sh"
    sudo chmod +x "$PERSIST_DIR/restore.sh"
    
    # Create systemd service for restoration
    cat <<EOF | sudo tee /etc/systemd/system/hifi-wifi-restore.service > /dev/null
[Unit]
Description=Restore hifi-wifi after SteamOS update
ConditionPathExists=!/usr/local/bin/hifi-wifi
After=network.target

[Service]
Type=oneshot
ExecStart=$PERSIST_DIR/restore.sh
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

    sudo systemctl enable hifi-wifi-restore.service
    echo "Persistence service enabled."

    echo "Re-enabling read-only filesystem..."
    sudo steamos-readonly enable
fi

echo ""
echo "Installation complete!"
echo ""

# Prompt to apply optimizations
read -p "Would you like to apply network optimizations now? [Y/n] " -n 1 -r
echo ""
if [[ ! $REPLY =~ ^[Nn]$ ]]; then
    echo ""
    sudo hifi-wifi --apply
else
    echo ""
    echo "Run 'sudo hifi-wifi --apply' when you're ready to apply optimizations."
fi
