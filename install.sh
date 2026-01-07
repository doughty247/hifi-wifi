#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

# Detect real user if running as sudo to build as user (not root)
if [ -n "" ]; then
    REAL_USER=
    REAL_HOME=$(getent passwd $SUDO_USER | cut -d: -f6)
else
    REAL_USER=$(whoami)
    REAL_HOME=$HOME
fi

# Helper to run commands as the non-root user
as_user() {
    if [ "$USER" != "$REAL_USER" ]; then
        sudo -u "$REAL_USER" "$@"
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
        echo -e "${BLUE}Linker (cc) not found. Preparing SteamOS for build...${NC}"
        
        # We need root for this
        if [[ $EUID -ne 0 ]]; then
            echo -e "${BLUE}Requesting sudo to install build dependencies (pacman)...${NC}"
            sudo steamos-readonly disable || true
            sudo pacman-key --init
            sudo pacman-key --populate archlinux holo 2>/dev/null || sudo pacman-key --populate archlinux
            sudo pacman -S --noconfirm --needed base-devel glibc linux-api-headers
        else
            steamos-readonly disable || true
            pacman-key --init
            pacman-key --populate archlinux holo 2>/dev/null || pacman-key --populate archlinux
            pacman -S --noconfirm --needed base-devel glibc linux-api-headers
        fi
    fi
fi

if [[ "$ID" == "bazzite" ]] && ! command -v cc &> /dev/null; then
     echo -e "${RED}Warning: 'cc' (gcc) linker not found.${NC}"
     echo -e "On Bazzite, please run: ${BLUE}ujust install-rust${NC} or install development tools manually."
fi

echo -e "${BLUE}=== hifi-wifi v3.0 Installer ===${NC}"

# 1. Rust Detection & Installation
echo -e "${BLUE}[1/3] Checking Rust toolchain...${NC}"

# Try to find cargo in PATH or common user locations
export PATH="$REAL_HOME/.cargo/bin:$PATH"

if ! command -v cargo &> /dev/null; then
    echo -e "${BLUE}Rust not found. Auto-installing for user $REAL_USER...${NC}"
    
    # Check for curl
    if ! command -v curl &> /dev/null; then
        echo -e "${RED}Error: curl is required to install Rust.${NC}"
        exit 1
    fi

    # Install Rust (non-interactive)
    as_user curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | as_user sh -s -- -y
    
    # Source the environment immediately
    if [ -f "$REAL_HOME/.cargo/env" ]; then
        source "$REAL_HOME/.cargo/env"
    fi
else
    echo -e "${GREEN}Rust detected.${NC}"
    # Attempt to fix broken installs
    if ! cargo --version &> /dev/null; then
        echo -e "${BLUE}Cargo detected but seems broken. Attempting repair...${NC}"
        as_user "$REAL_HOME/.cargo/bin/rustup" self update
        as_user "$REAL_HOME/.cargo/bin/rustup" default stable
    fi
fi

# Verify cargo works now
if ! command -v cargo &> /dev/null; then
    # Fallback check for ~/.cargo/bin/cargo explicit existence
    if [[ -x "$REAL_HOME/.cargo/bin/cargo" ]]; then
       export PATH="$REAL_HOME/.cargo/bin:$PATH"
    else
       echo -e "${RED}Failed to configure Rust. Please install manually.${NC}"
       exit 1
    fi
fi

# 2. Build Phase
echo -e "${BLUE}[2/3] Building release binary...${NC}"
echo "Building as user: $REAL_USER"

# Resolve absolute path to cargo to survive sudo PATH reset
if [[ -x "$REAL_HOME/.cargo/bin/cargo" ]]; then
    CARGO_EXEC="$REAL_HOME/.cargo/bin/cargo"
elif command -v cargo &> /dev/null; then
    CARGO_EXEC=$(command -v cargo)
else
    echo -e "${RED}Error: Unexpectedly lost track of cargo binary.${NC}"
    exit 1
fi

# Run build as the real user to avoid root-owned target artifacts
as_user "$CARGO_EXEC" build --release

if [[ ! -f "target/release/hifi-wifi" ]]; then
    echo "Retrying build as current user..."
    "$CARGO_EXEC" build --release
    
    # If still not found, check if maybe "cargo install" path was used or different layout
    if [[ ! -f "target/release/hifi-wifi" ]]; then
        echo -e "${RED}Build failed! Binary not found in target/release/.${NC}"
        exit 1
    fi
fi

# 3. Install Phase (Needs root)
echo -e "${BLUE}[3/3] Installing system service...${NC}"

RUN_AS_ROOT=""
if [[ $EUID -ne 0 ]]; then
    RUN_AS_ROOT="sudo"
fi

$RUN_AS_ROOT ./target/release/hifi-wifi install
$RUN_AS_ROOT hifi-wifi apply

echo -e "${GREEN}Success! hifi-wifi v3.0 is installed and active.${NC}"
echo -e "Monitor with: ${BLUE}hifi-wifi status${NC}"
