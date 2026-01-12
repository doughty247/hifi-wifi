#!/bin/bash
set -e

# ============================================================================
# hifi-wifi v3.0 Installer - Refactored for clarity and maintainability
# ============================================================================

# Colors
readonly GREEN='\033[0;32m'
readonly BLUE='\033[0;34m'
readonly YELLOW='\033[1;33m'
readonly RED='\033[0;31m'
readonly NC='\033[0m'

echo -e "${BLUE}=== hifi-wifi v3.0 Installer ===${NC}\n"

# ============================================================================
# Helper Functions
# ============================================================================

# Detect the real user when running under sudo
detect_user() {
    if [ -n "$SUDO_USER" ]; then
        REAL_USER="$SUDO_USER"
        REAL_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
    else
        REAL_USER=$(whoami)
        REAL_HOME="$HOME"
    fi
}

# Run command as non-root user with preserved environment
as_user() {
    if [ "$USER" != "$REAL_USER" ]; then
        sudo -u "$REAL_USER" HOME="$REAL_HOME" PATH="$REAL_HOME/.cargo/bin:$PATH" "$@"
    else
        "$@"
    fi
}

# Check for pre-compiled binary
find_precompiled_binary() {
    local script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    
    if [[ -f "$script_dir/bin/hifi-wifi-x86_64" ]]; then
        echo "$script_dir/bin/hifi-wifi-x86_64"
    elif [[ -f "$script_dir/hifi-wifi-x86_64" ]]; then
        echo "$script_dir/hifi-wifi-x86_64"
    else
        echo ""
    fi
}

# Setup SteamOS build environment (disable readonly, install build tools)
setup_steamos_build_env() {
    echo -e "${BLUE}[SteamOS] Preparing build environment...${NC}"
    
    # Require root
    if [[ $EUID -ne 0 ]]; then
        echo -e "${RED}This script must be run as root on SteamOS for build setup.${NC}"
        echo -e "Try: ${BLUE}sudo ./install.sh${NC}"
        exit 1
    fi
    
    # Disable read-only filesystem
    echo -e "${BLUE}Disabling read-only filesystem...${NC}"
    systemd-sysext unmerge 2>/dev/null || true
    steamos-readonly disable 2>&1 | grep -v "Warning:" || true
    sleep 2
    
    # Verify writability
    if ! touch /usr/test-write 2>/dev/null; then
        echo -e "${YELLOW}Retrying with aggressive unmerge...${NC}"
        systemd-sysext unmerge 2>/dev/null || true
        steamos-readonly disable 2>&1 | grep -v "Warning:" || true
        sleep 2
        
        if ! touch /usr/test-write 2>/dev/null; then
            echo -e "${RED}Filesystem is still read-only after multiple attempts.${NC}"
            echo -e "${BLUE}Please reboot and try again, or download the pre-compiled release.${NC}"
            exit 1
        fi
    fi
    rm -f /usr/test-write
    
    # Initialize pacman keyring if needed
    if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
        echo -e "${BLUE}Initializing pacman keyring...${NC}"
        pacman-key --init || {
            echo -e "${RED}pacman-key --init failed${NC}"
            exit 1
        }
    fi
    
    # Populate keyrings - run separately, failures OK if already populated
    echo -e "${BLUE}Populating pacman keys...${NC}"
    pacman-key --populate archlinux || true
    pacman-key --populate holo 2>/dev/null || true
    
    # Sync package database
    echo -e "${BLUE}Syncing package database...${NC}"
    pacman -Sy || {
        echo -e "${RED}Package database sync failed (filesystem may still be read-only)${NC}"
        exit 1
    }
    
    # Install build dependencies
    echo -e "${BLUE}Installing build tools...${NC}"
    pacman -S --noconfirm --needed base-devel glibc linux-api-headers || {
        echo -e "${RED}Package installation failed${NC}"
        exit 1
    }
    
    echo -e "${GREEN}Build environment ready!${NC}\n"
}

# Check for Rust toolchain, install if missing
setup_rust() {
    echo -e "${BLUE}Checking Rust toolchain...${NC}"
    
    local cargo_bin="$REAL_HOME/.cargo/bin/cargo"
    local rustup_bin="$REAL_HOME/.cargo/bin/rustup"
    
    if [[ ! -x "$cargo_bin" ]]; then
        echo -e "${BLUE}Rust not found. Installing for user $REAL_USER...${NC}"
        
        if ! command -v curl &>/dev/null; then
            echo -e "${RED}Error: curl is required to install Rust${NC}"
            exit 1
        fi
        
        as_user curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | as_user sh -s -- -y
        
        if [[ ! -x "$cargo_bin" ]]; then
            echo -e "${RED}Rust installation failed${NC}"
            exit 1
        fi
    fi
    
    # Verify cargo works
    if ! as_user "$cargo_bin" --version &>/dev/null; then
        echo -e "${YELLOW}Cargo appears broken, attempting repair...${NC}"
        as_user "$rustup_bin" self update 2>/dev/null || true
        as_user "$rustup_bin" default stable 2>/dev/null || true
        
        if ! as_user "$cargo_bin" --version &>/dev/null; then
            echo -e "${RED}Cargo is not working. Please reinstall Rust manually.${NC}"
            exit 1
        fi
    fi
    
    echo -e "${GREEN}Rust toolchain ready${NC}\n"
}

# Build binary from source
build_from_source() {
    echo -e "${BLUE}Building release binary...${NC}"
    echo "Building as user: $REAL_USER"
    
    local cargo_bin="$REAL_HOME/.cargo/bin/cargo"
    as_user "$cargo_bin" build --release
    
    if [[ ! -f "target/release/hifi-wifi" ]]; then
        echo -e "${RED}Build failed! Binary not found in target/release/${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}Build complete${NC}\n"
}

# Install the hifi-wifi service
install_service() {
    echo -e "${BLUE}Installing hifi-wifi service...${NC}"
    
    local run_as_root=""
    [[ $EUID -ne 0 ]] && run_as_root="sudo"
    
    # Stop existing service to prevent "text file busy" errors
    if systemctl is-active --quiet hifi-wifi 2>/dev/null; then
        echo -e "${BLUE}Stopping existing service...${NC}"
        $run_as_root systemctl stop hifi-wifi
    fi
    
    # Run the binary's install command
    $run_as_root ./target/release/hifi-wifi install
    
    # SELinux: Fix context on Fedora-based systems (Bazzite)
    if command -v chcon &>/dev/null && [[ -f /var/lib/hifi-wifi/hifi-wifi ]]; then
        echo -e "${BLUE}Setting SELinux context...${NC}"
        $run_as_root chcon -t bin_t /var/lib/hifi-wifi/hifi-wifi 2>/dev/null || true
    fi
    
    echo -e "${GREEN}Service installed${NC}\n"
}

# Create CLI symlink (handles SteamOS read-only filesystem)
create_cli_symlink() {
    local distro_id="${1:-}"
    local run_as_root=""
    [[ $EUID -ne 0 ]] && run_as_root="sudo"
    
    if [[ -L /usr/local/bin/hifi-wifi ]]; then
        return 0  # Already exists
    fi
    
    echo -e "${BLUE}Creating CLI symlink...${NC}"
    
    if [[ "$distro_id" == "steamos" ]]; then
        # SteamOS: Need to disable read-only temporarily
        systemd-sysext unmerge 2>/dev/null || true
        steamos-readonly disable 2>&1 | grep -v "Warning:" || true
        sleep 1
        $run_as_root ln -sf /var/lib/hifi-wifi/hifi-wifi /usr/local/bin/hifi-wifi 2>/dev/null || true
        steamos-readonly enable 2>&1 | grep -v "Warning:" || true
    else
        $run_as_root ln -sf /var/lib/hifi-wifi/hifi-wifi /usr/local/bin/hifi-wifi 2>/dev/null || true
    fi
}

# Apply initial optimizations
apply_optimizations() {
    local run_as_root=""
    [[ $EUID -ne 0 ]] && run_as_root="sudo"
    
    local hifi_cmd
    if [[ -L /usr/local/bin/hifi-wifi ]]; then
        hifi_cmd="hifi-wifi"
    else
        echo -e "${YELLOW}CLI symlink not in PATH yet. Using absolute path.${NC}"
        hifi_cmd="/var/lib/hifi-wifi/hifi-wifi"
    fi
    
    echo -e "${BLUE}Applying initial optimizations...${NC}"
    $run_as_root $hifi_cmd apply
    echo ""
}

# Offer reboot
offer_reboot() {
    echo -e "${GREEN}Success! hifi-wifi v3.0 is installed and active.${NC}\n"
    echo -e "  Check status:    ${BLUE}hifi-wifi status${NC}"
    echo -e "  Live monitoring: ${BLUE}sudo hifi-wifi monitor${NC}"
    echo -e "  Service logs:    ${BLUE}journalctl -u hifi-wifi -f${NC}\n"
    echo -e "${BLUE}Note:${NC} Driver-level tweaks require a reboot for full effect.\n"
    
    read -p "Reboot now? [Y/n] " -n 1 -r
    echo
    
    if [[ $REPLY =~ ^[Yy]$ ]] || [[ -z $REPLY ]]; then
        echo -e "${BLUE}Rebooting...${NC}"
        
        local run_as_root=""
        [[ $EUID -ne 0 ]] && run_as_root="sudo"
        
        # Try systemctl first
        if ! $run_as_root systemctl reboot 2>/dev/null; then
            # Fallback for desktop environments with session inhibitors
            if command -v gnome-session-quit &>/dev/null; then
                local user_id=$(id -u "$REAL_USER")
                sudo -u "$REAL_USER" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$user_id/bus" \
                    gnome-session-quit --reboot --no-prompt 2>/dev/null || {
                    echo -e "${YELLOW}Please reboot manually from your desktop${NC}"
                }
            elif command -v qdbus &>/dev/null; then
                sudo -u "$REAL_USER" qdbus org.kde.Shutdown /Shutdown org.kde.Shutdown.logoutAndReboot 2>/dev/null || {
                    echo -e "${YELLOW}Please reboot manually from your desktop${NC}"
                }
            else
                echo -e "${YELLOW}Please reboot manually${NC}"
            fi
        fi
    fi
}

# ============================================================================
# Main Installation Flow
# ============================================================================

main() {
    # Detect user and platform
    detect_user
    
    local distro_id=""
    if [[ -f /etc/os-release ]]; then
        source /etc/os-release
        distro_id="$ID"
    fi
    
    # Step 1: Check for pre-compiled binary
    echo -e "${BLUE}[1/5] Checking for pre-compiled binary...${NC}"
    local precompiled_bin=$(find_precompiled_binary)
    
    if [[ -n "$precompiled_bin" ]]; then
        echo -e "${GREEN}Found: $precompiled_bin${NC}"
        
        # Verify architecture
        if ! file "$precompiled_bin" | grep -q "x86-64"; then
            echo -e "${RED}Error: Binary is not x86_64 architecture${NC}"
            exit 1
        fi
        
        # Copy to target/release
        mkdir -p target/release
        cp "$precompiled_bin" target/release/hifi-wifi
        chmod +x target/release/hifi-wifi
        echo -e "${GREEN}Using pre-compiled binary (skipping build)${NC}\n"
    else
        echo -e "${YELLOW}No pre-compiled binary found. Will build from source.${NC}\n"
        
        # SteamOS warning
        if [[ "$distro_id" == "steamos" ]]; then
            echo -e "${YELLOW}WARNING: Building from source on SteamOS is complex.${NC}"
            echo -e "${YELLOW}It's recommended to download the official release:${NC}"
            echo -e "${BLUE}https://github.com/doughty247/hifi-wifi/releases${NC}\n"
            read -p "Continue anyway? [y/N] " -n 1 -r
            echo
            [[ ! $REPLY =~ ^[Yy]$ ]] && exit 1
        fi
        
        # Step 2: Setup build environment (SteamOS only)
        if [[ "$distro_id" == "steamos" || "$distro_id" == *"arch"* ]]; then
            if ! command -v cc &>/dev/null; then
                echo -e "${BLUE}[2/5] Setting up build environment...${NC}"
                setup_steamos_build_env
            else
                echo -e "${BLUE}[2/5] Build tools already installed${NC}\n"
            fi
        else
            echo -e "${BLUE}[2/5] Build environment check...${NC}"
            if ! command -v cc &>/dev/null && [[ "$distro_id" == "bazzite" ]]; then
                echo -e "${YELLOW}gcc not found. On Bazzite, run: ${BLUE}ujust install-rust${NC}\n"
            else
                echo -e "${GREEN}Build tools available${NC}\n"
            fi
        fi
        
        # Step 3: Setup Rust
        echo -e "${BLUE}[3/5] Setting up Rust toolchain...${NC}"
        setup_rust
        
        # Step 4: Build
        echo -e "${BLUE}[4/5] Building from source...${NC}"
        build_from_source
    fi
    
    # Step 5: Install
    echo -e "${BLUE}[5/5] Installing service...${NC}"
    install_service
    create_cli_symlink "$distro_id"
    apply_optimizations
    
    # Offer reboot
    offer_reboot
}

# Run main
main
