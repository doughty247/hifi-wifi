#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== hifi-wifi v3.0 Installer ===${NC}"

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

# =============================================================================
# STEP 1: Check for pre-compiled binary FIRST (before any dependency checks)
# =============================================================================
PRECOMPILED_BIN=""
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo -e "${BLUE}Checking for pre-compiled binary...${NC}"
if [[ -f "$SCRIPT_DIR/bin/hifi-wifi-x86_64" ]]; then
    PRECOMPILED_BIN="$SCRIPT_DIR/bin/hifi-wifi-x86_64"
    echo -e "${GREEN}Found bundled binary at bin/hifi-wifi-x86_64${NC}"
elif [[ -f "$SCRIPT_DIR/hifi-wifi-x86_64" ]]; then
    PRECOMPILED_BIN="$SCRIPT_DIR/hifi-wifi-x86_64"
    echo -e "${GREEN}Found bundled binary in release root${NC}"
fi

# =============================================================================
# STEP 2: If we have a pre-compiled binary, use it (skip ALL build logic)
# =============================================================================
if [[ -n "$PRECOMPILED_BIN" ]]; then
    echo -e "${GREEN}Pre-compiled binary found: $PRECOMPILED_BIN${NC}"
    echo -e "${BLUE}Skipping Rust toolchain and build phase entirely.${NC}"
    
    # Verify binary is executable and correct architecture
    if ! file "$PRECOMPILED_BIN" | grep -q "x86-64"; then
        echo -e "${RED}Error: Pre-compiled binary is not x86_64 architecture.${NC}"
        exit 1
    fi
    
    # Copy to expected location for install phase
    mkdir -p target/release
    cp "$PRECOMPILED_BIN" target/release/hifi-wifi
    chmod +x target/release/hifi-wifi
    
    echo -e "${GREEN}[1/2] Using pre-compiled binary${NC}"

# =============================================================================
# STEP 3: No binary found - need to build from source (may fail on SteamOS)
# =============================================================================
else
    echo -e "${YELLOW}No pre-compiled binary found. Will build from source.${NC}"
    
    # SteamOS warning
    if [[ "$ID" == "steamos" ]]; then
        echo -e "${YELLOW}WARNING: Building from source on SteamOS is unreliable.${NC}"
        echo -e "${YELLOW}It is STRONGLY recommended to download the official release:${NC}"
        echo -e "${BLUE}https://github.com/doughty247/hifi-wifi/releases${NC}"
        echo ""
        read -p "Continue with build from source anyway? [y/N] " -n 1 -r
        echo
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then
            exit 1
        fi
    fi

    # SteamOS/Arch: Check if 'cc' is missing and try to install build tools
    if [[ "$ID" == "steamos" || "$ID_LIKE" == *"arch"* ]]; then
        if ! command -v cc &> /dev/null; then
            echo -e "${BLUE}Linker (cc) not found. Preparing SteamOS for build...${NC}"
            
            # We need root for this
            if [[ $EUID -ne 0 ]]; then
                echo -e "${RED}This script must be run as root on SteamOS.${NC}"
                echo -e "Try: ${BLUE}sudo ./install.sh${NC}"
                exit 1
            fi
            
            echo -e "${BLUE}[SteamOS] Unmerging system extensions (if any)...${NC}"
            systemd-sysext unmerge 2>/dev/null || true
            sleep 1
            
            echo -e "${BLUE}[SteamOS] Disabling readonly filesystem...${NC}"
            steamos-readonly disable 2>&1 | grep -v "Warning:" || true
            sleep 2
            
            # Double-check: try to write to a test location
            if ! touch /usr/test-write 2>/dev/null; then
                echo -e "${RED}Filesystem is still not writable!${NC}"
                echo -e "${YELLOW}Attempting aggressive unmerge...${NC}"
                
                systemd-sysext unmerge 2>/dev/null || true
                sleep 2
                steamos-readonly disable 2>&1 | grep -v "Warning:" || true
                sleep 2
                
                if ! touch /usr/test-write 2>/dev/null; then
                    echo -e "${RED}Still cannot write to filesystem after multiple attempts.${NC}"
                    echo -e "${BLUE}Please reboot and try again, or use the pre-compiled release.${NC}"
                    exit 1
                fi
            fi
            rm -f /usr/test-write 2>/dev/null
            
            echo -e "${BLUE}[SteamOS] Initializing pacman...${NC}"
            if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
                pacman-key --init || {
                    echo -e "${RED}pacman-key --init failed${NC}"
                    exit 1
                }
            fi
            
            echo -e "${BLUE}[SteamOS] Populating pacman keys...${NC}"
            # Try archlinux + holo, fallback to just archlinux if holo not available
            if ! pacman-key --populate archlinux holo 2>/dev/null; then
                echo -e "${YELLOW}holo keyring not found, trying archlinux only...${NC}"
                if ! pacman-key --populate archlinux; then
                    echo -e "${RED}pacman-key --populate failed${NC}"
                    exit 1
                fi
            fi
            
            echo -e "${BLUE}[SteamOS] Syncing package database...${NC}"
            if ! pacman -Sy; then
                echo -e "${RED}Package database sync failed!${NC}"
                echo -e "${YELLOW}This usually means the filesystem is still read-only somewhere.${NC}"
                echo -e "${BLUE}Try: Reboot your Steam Deck and run the installer again.${NC}"
                exit 1
            fi
            
            echo -e "${BLUE}[SteamOS] Installing build dependencies...${NC}"
            if ! pacman -S --noconfirm --needed base-devel glibc linux-api-headers; then
                echo -e "${RED}Package installation failed!${NC}"
                exit 1
            fi
            
            echo -e "${GREEN}[SteamOS] Build environment ready!${NC}"
        fi
    fi

    # Bazzite warning
    if [[ "$ID" == "bazzite" ]] && ! command -v cc &> /dev/null; then
         echo -e "${RED}Warning: 'cc' (gcc) linker not found.${NC}"
         echo -e "On Bazzite, please run: ${BLUE}ujust install-rust${NC} or install development tools manually."
    fi

    # Rust Detection & Installation
    echo -e "${BLUE}[1/3] Checking Rust toolchain...${NC}"

    CARGO_BIN="$REAL_HOME/.cargo/bin/cargo"
    RUSTUP_BIN="$REAL_HOME/.cargo/bin/rustup"

    if [[ ! -x "$CARGO_BIN" ]]; then
        echo -e "${BLUE}Rust not found. Auto-installing for user $REAL_USER...${NC}"
        
        if ! command -v curl &> /dev/null; then
            echo -e "${RED}Error: curl is required to install Rust.${NC}"
            exit 1
        fi

        as_user curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | as_user sh -s -- -y
        
        if [[ ! -x "$CARGO_BIN" ]]; then
            echo -e "${RED}Rust installation failed. Please install manually.${NC}"
            exit 1
        fi
    else
        echo -e "${GREEN}Rust detected at $CARGO_BIN${NC}"
        if ! as_user "$CARGO_BIN" --version &> /dev/null; then
            echo -e "${BLUE}Cargo seems broken. Attempting repair...${NC}"
            as_user "$RUSTUP_BIN" self update 2>/dev/null || true
            as_user "$RUSTUP_BIN" default stable 2>/dev/null || true
        fi
    fi

    if ! as_user "$CARGO_BIN" --version &> /dev/null; then
        echo -e "${RED}Cargo is not working. Please install Rust manually.${NC}"
        exit 1
    fi

    # Build Phase
    echo -e "${BLUE}[2/3] Building release binary...${NC}"
    echo "Building as user: $REAL_USER with cargo: $CARGO_BIN"

    as_user "$CARGO_BIN" build --release

    if [[ ! -f "target/release/hifi-wifi" ]]; then
        echo -e "${RED}Build failed! Binary not found in target/release/.${NC}"
        exit 1
    fi
fi

# =============================================================================
# STEP 4: Install Phase (common for both pre-compiled and built-from-source)
# =============================================================================
if [[ -n "$PRECOMPILED_BIN" ]]; then
    echo -e "${BLUE}[2/2] Installing system service...${NC}"
else
    echo -e "${BLUE}[3/3] Installing system service...${NC}"
fi

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

# Bazzite/Fedora: Fix SELinux context for the installed binary
if command -v chcon &> /dev/null && [[ -f /var/lib/hifi-wifi/hifi-wifi ]]; then
    echo -e "${BLUE}Setting SELinux context for binary...${NC}"
    $RUN_AS_ROOT chcon -t bin_t /var/lib/hifi-wifi/hifi-wifi 2>/dev/null || true
fi

# SteamOS: Handle read-only filesystem for symlink creation
if [[ "$ID" == "steamos" ]]; then
    if [[ ! -L /usr/local/bin/hifi-wifi ]]; then
        echo -e "${BLUE}[SteamOS] Creating CLI symlink...${NC}"
        systemd-sysext unmerge 2>/dev/null || true
        steamos-readonly disable 2>&1 | grep -v "Warning:" || true
        sleep 1
        
        ln -sf /var/lib/hifi-wifi/hifi-wifi /usr/local/bin/hifi-wifi 2>/dev/null || true
        
        steamos-readonly enable 2>&1 | grep -v "Warning:" || true
    fi
fi

# Verify symlink exists and use absolute path if needed
if [[ -L /usr/local/bin/hifi-wifi ]]; then
    HIFI_CMD="hifi-wifi"
else
    echo -e "${BLUE}Using direct binary path (symlink not yet in PATH)${NC}"
    echo -e "${BLUE}Note:${NC} You may need to start a new shell or run: ${BLUE}hash -r${NC}"
    HIFI_CMD="/var/lib/hifi-wifi/hifi-wifi"
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
    
    # Try systemctl first (works on most distros)
    if ! $RUN_AS_ROOT systemctl reboot 2>/dev/null; then
        # If blocked by desktop session inhibitors, try desktop-specific methods
        # Must run as the logged-in user (not root) to access session DBus
        if command -v gnome-session-quit &> /dev/null; then
            # GNOME: Use session manager (handles inhibitors)
            sudo -u "$REAL_USER" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u "$REAL_USER")/bus" gnome-session-quit --reboot --no-prompt 2>/dev/null || {
                echo -e "${YELLOW}Automatic reboot blocked by active session.${NC}"
                echo -e "${BLUE}Please reboot manually from your desktop.${NC}"
            }
        elif command -v qdbus &> /dev/null; then
            # KDE: Use ksmserver
            sudo -u "$REAL_USER" qdbus org.kde.Shutdown /Shutdown org.kde.Shutdown.logoutAndReboot 2>/dev/null || {
                echo -e "${YELLOW}Automatic reboot blocked by active session.${NC}"
                echo -e "${BLUE}Please reboot manually from your desktop.${NC}"
            }
        else
            echo -e "${YELLOW}Automatic reboot blocked by active session.${NC}"
            echo -e "${BLUE}Please reboot manually from your desktop.${NC}"
        fi
    fi
fi
