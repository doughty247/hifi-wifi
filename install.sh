#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

# Detect real user if running as sudo to build as user (not root)
if [ -n "$SUDO_USER" ]; then
    REAL_USER=$SUDO_USER
    REAL_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
else
    REAL_USER=$(whoami)
    REAL_HOME=$HOME
fi

# Helper to run commands as the non-root user WITH environment variables preserved
as_user() {
    if [ "$USER" != "$REAL_USER" ]; then
        sudo -u "$REAL_USER" HOME="$REAL_HOME" PATH="$REAL_HOME/.cargo/bin:$PATH" "$@"
    else
        "$@"
    fi
}

# Source os-release to detect distro
if [ -f /etc/os-release ]; then
    source /etc/os-release
fi

if [[ "$ID" == "steamos" || "$ID_LIKE" == *"arch"* ]]; then
    # Check if 'cc' is missing
    if ! command -v cc &> /dev/null; then
        echo -e "${RED}Linker (cc) not found.${NC}"
        echo -e "${BLUE}On SteamOS, you need to manually prepare the system first:${NC}"
        echo ""
        echo -e "  1. Disable readonly: ${BLUE}sudo steamos-readonly disable${NC}"
        echo -e "  2. Check for sysexts: ${BLUE}systemd-sysext status${NC}"
        echo -e "  3. If sysexts loaded:  ${BLUE}sudo systemd-sysext unmerge${NC}"
        echo -e "  4. Initialize pacman:  ${BLUE}sudo pacman-key --init${NC}"
        echo -e "  5. Populate keys:      ${BLUE}sudo pacman-key --populate archlinux${NC}"
        echo -e "  6. Install tools:      ${BLUE}sudo pacman -S base-devel${NC}"
        echo -e "  7. Re-run this script: ${BLUE}sudo ./install.sh${NC}"
        echo ""
        echo -e "${RED}Automated installation cannot proceed due to system extensions.${NC}"
        exit 1
    fi
fi

if [[ "$ID" == "bazzite" ]] && ! command -v cc &> /dev/null; then
     echo -e "${RED}Warning: 'cc' (gcc) linker not found.${NC}"
     echo -e "On Bazzite, please run: ${BLUE}ujust install-rust${NC} or install development tools manually."
fi

echo -e "${BLUE}=== hifi-wifi v3.0 Installer ===${NC}"

# 1. Rust Detection & Installation
echo -e "${BLUE}[1/3] Checking Rust toolchain...${NC}"

# Look for cargo directly in the user's home, not via PATH
CARGO_BIN="$REAL_HOME/.cargo/bin/cargo"
RUSTUP_BIN="$REAL_HOME/.cargo/bin/rustup"

if [[ ! -x "$CARGO_BIN" ]]; then
    echo -e "${BLUE}Rust not found. Auto-installing for user $REAL_USER...${NC}"
    
    # Check for curl
    if ! command -v curl &> /dev/null; then
        echo -e "${RED}Error: curl is required to install Rust.${NC}"
        exit 1
    fi

    # Install Rust (non-interactive) as the real user
    as_user curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | as_user sh -s -- -y
    
    # Verify installation
    if [[ ! -x "$CARGO_BIN" ]]; then
        echo -e "${RED}Rust installation failed. Please install manually.${NC}"
        exit 1
    fi
else
    echo -e "${GREEN}Rust detected at $CARGO_BIN${NC}"
    # Attempt to fix broken installs
    if ! as_user "$CARGO_BIN" --version &> /dev/null; then
        echo -e "${BLUE}Cargo seems broken. Attempting repair...${NC}"
        as_user "$RUSTUP_BIN" self update 2>/dev/null || true
        as_user "$RUSTUP_BIN" default stable 2>/dev/null || true
    fi
fi

# Final verification
if ! as_user "$CARGO_BIN" --version &> /dev/null; then
    echo -e "${RED}Cargo is not working. Please install Rust manually.${NC}"
    exit 1
fi

# 2. Build Phase
echo -e "${BLUE}[2/3] Building release binary...${NC}"
echo "Building as user: $REAL_USER with cargo: $CARGO_BIN"

# Build as the real user
as_user "$CARGO_BIN" build --release

if [[ ! -f "target/release/hifi-wifi" ]]; then
    echo -e "${RED}Build failed! Binary not found in target/release/.${NC}"
    exit 1
fi

# 3. Install Phase (Needs root)
echo -e "${BLUE}[3/3] Installing system service...${NC}"

RUN_AS_ROOT=""
if [[ $EUID -ne 0 ]]; then
    RUN_AS_ROOT="sudo"
fi

# Stop existing service before upgrading (prevents "Text file busy" error)
if systemctl is-active --quiet hifi-wifi 2>/dev/null; then
    echo -e "${BLUE}Stopping existing hifi-wifi service for upgrade...${NC}"
    $RUN_AS_ROOT systemctl stop hifi-wifi
fi

$RUN_AS_ROOT ./target/release/hifi-wifi install

# Verify symlink exists and use absolute path if needed
if [[ -L /usr/local/bin/hifi-wifi ]]; then
    HIFI_CMD="hifi-wifi"
else
    echo -e "${BLUE}Using direct binary path (symlink not yet in PATH)${NC}"
    H-e "${BLUE}Note:${NC} You may need to start a new shell or run: ${BLUE}hash -r${NC}"
echo IFI_CMD="/var/lib/hifi-wifi/hifi-wifi"
fi

$RUN_AS_ROOT $HIFI_CMD apply

echo -e "${GREEN}Success! hifi-wifi v3.0 is installed and active.${NC}"
echo ""
echo -e "The ${BLUE}hifi-wifi${NC} command is now available system-wide."
echo ""
echo -e "  Check status:    ${BLUE}hifi-wifi status${NC}"
echo -e "  Live monitoring: ${BLUE}sudo hifi-wifi monitor${NC}"
echo -e "  Service logs:    ${BLUE}journalctl -u hifi-wifi -f${NC}"
echo ""
echo -e "${BLUE}Note:${NC} Most optimizations are active immediately."
echo -e "However, driver-level tweaks (modprobe) require a reboot for full effect."
echo ""
read -p "Reboot now for full optimization? [Y/n] " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]] || [[ -z $REPLY ]]; then
    echo -e "${BLUE}Rebooting...${NC}"
    $RUN_AS_ROOT reboot
fi
