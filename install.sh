#!/bin/bash
# Installer for hifi-wifi

set -e

INSTALL_DIR="/usr/local/bin"
SHARE_DIR="/usr/local/share/hifi-wifi"

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

echo "Installation complete!"
echo "Run 'sudo hifi-wifi --help' to get started."
