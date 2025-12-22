#!/bin/bash
# hifi-wifi-restore.sh
# Restores hifi-wifi and its dependencies after a SteamOS update

set -e

LOG_FILE="/var/log/hifi-wifi-restore.log"
BACKUP_DIR="/var/lib/hifi-wifi/backup"
CACHE_DIR="/var/lib/hifi-wifi/pkg_cache"
INSTALL_DIR="/usr/local/bin"
SHARE_DIR="/usr/local/share/hifi-wifi"

echo "[$(date)] Starting hifi-wifi restoration..." >> "$LOG_FILE"

# 1. Restore hifi-wifi files
if [[ ! -f "$INSTALL_DIR/hifi-wifi" ]]; then
    echo "Restoring hifi-wifi binaries..." >> "$LOG_FILE"
    
    # Disable read-only
    steamos-readonly disable || true
    
    mkdir -p "$INSTALL_DIR"
    mkdir -p "$SHARE_DIR"
    
    if [[ -d "$BACKUP_DIR/bin" ]]; then
        cp "$BACKUP_DIR/bin/hifi-wifi" "$INSTALL_DIR/"
        chmod +x "$INSTALL_DIR/hifi-wifi"
    fi
    
    if [[ -d "$BACKUP_DIR/src" ]]; then
        mkdir -p "$SHARE_DIR/src"
        cp "$BACKUP_DIR/src/"* "$SHARE_DIR/src/"
        chmod +x "$SHARE_DIR/src/"*.sh
    fi
    
    if [[ -d "$BACKUP_DIR/config" ]]; then
        mkdir -p "$SHARE_DIR/config"
        cp "$BACKUP_DIR/config/"* "$SHARE_DIR/config/"
    fi
    
    echo "Files restored." >> "$LOG_FILE"
else
    echo "hifi-wifi files already present." >> "$LOG_FILE"
fi

# 2. Restore iwd if it was cached
if ! command -v iwd &>/dev/null; then
    if ls "$CACHE_DIR"/iwd*.pkg.tar.zst 1> /dev/null 2>&1; then
        echo "Restoring iwd from cache..." >> "$LOG_FILE"
        steamos-readonly disable || true
        
        # Initialize pacman keyring if needed (often reset on update)
        pacman-key --init || true
        pacman-key --populate archlinux || true
        pacman-key --populate holo || true
        
        pacman -U --noconfirm "$CACHE_DIR"/iwd*.pkg.tar.zst >> "$LOG_FILE" 2>&1 || echo "Failed to restore iwd" >> "$LOG_FILE"
    else
        echo "iwd missing and no cache found." >> "$LOG_FILE"
    fi
fi

# 3. Re-enable read-only
steamos-readonly enable || true

echo "[$(date)] Restoration complete." >> "$LOG_FILE"
