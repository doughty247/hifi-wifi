#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== hifi-wifi v3.0 Installer ===${NC}"

# 1. Build Phase
echo -e "${BLUE}[1/2] Building release binary...${NC}"

# Helper to find cargo
find_cargo() {
    if command -v cargo &> /dev/null; then
        echo "cargo"
        return 0
    fi
    
    # Check if running as sudo and try to find user's cargo
    if [ -n "$SUDO_USER" ]; then
        local user_cargo="/home/$SUDO_USER/.cargo/bin/cargo"
        if [ -f "$user_cargo" ]; then
            echo "$user_cargo"
            return 0
        fi
        
        # Also check common path /var/home for Bazzite/Silverblue
        local user_cargo_var="/var/home/$SUDO_USER/.cargo/bin/cargo"
        if [ -f "$user_cargo_var" ]; then
            echo "$user_cargo_var"
            return 0
        fi
    fi
    
    return 1
}

CARGO_CMD=$(find_cargo) || {
    echo -e "${RED}Error: cargo not found.${NC}"
    echo "If you are running with sudo, try running without sudo first."
    echo "Or install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
}

$CARGO_CMD build --release

if [[ ! -f "target/release/hifi-wifi" ]]; then
    echo -e "${RED}Build failed! Binary not found.${NC}"
    exit 1
fi

# 2. Install Phase (Needs root)
echo -e "${BLUE}[2/2] Installing system service...${NC}"

if [[ $EUID -ne 0 ]]; then
    # Not root, invoke sudo for the install command
    sudo ./target/release/hifi-wifi install
else
    # Already root
    ./target/release/hifi-wifi install
fi

echo -e "${GREEN}Installation Complete!${NC}"
