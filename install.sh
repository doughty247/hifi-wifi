#!/bin/bash
# Installer for hifi-wifi

set -e

INSTALL_DIR="/usr/local/bin"
SHARE_DIR="/usr/local/share/hifi-wifi"

IS_STEAMOS=false
if grep -q "SteamOS" /etc/os-release 2>/dev/null; then
    IS_STEAMOS=true
    echo "SteamOS detected. Disabling read-only filesystem..."
    sudo steamos-readonly disable
fi

# Check for existing installation and cleanup
if command -v hifi-wifi &>/dev/null; then
    echo "Detected existing installation. Cleaning up old patches..."
    # Try to revert patches using the OLD version to ensure clean state
    sudo hifi-wifi --revert --quiet || echo "Warning: Failed to revert old patches. Proceeding anyway."
fi

echo "Installing hifi-wifi..."

# Create directories
sudo mkdir -p "$SHARE_DIR/src"
sudo mkdir -p "$SHARE_DIR/config"
sudo mkdir -p "$INSTALL_DIR"

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

echo "Installation complete!"
echo "Run 'sudo hifi-wifi --apply' to apply optimizations."
