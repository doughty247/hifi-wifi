#!/bin/bash
set -e

# ============================================================================
# hifi-wifi v3.0.0-beta2 Installer
# ============================================================================

# Colors
readonly GREEN='\033[0;32m'
readonly BLUE='\033[0;34m'
readonly YELLOW='\033[1;33m'
readonly RED='\033[0;31m'
readonly NC='\033[0m'

echo -e "${BLUE}=== hifi-wifi v3.0.0-beta3 Installer ===${NC}\n"

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
        # Build env string with optional CC/CXX for Homebrew builds
        local env_vars="HOME=$REAL_HOME PATH=$REAL_HOME/.cargo/bin:$PATH"
        [[ -n "$CC" ]] && env_vars="$env_vars CC=$CC"
        [[ -n "$CXX" ]] && env_vars="$env_vars CXX=$CXX"
        sudo -u "$REAL_USER" env $env_vars "$@"
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

# Setup Homebrew (works on SteamOS, persists across updates!)
# Installs to /home/linuxbrew/.linuxbrew - doesn't touch rootfs
setup_homebrew() {
    local HOMEBREW_PREFIX="/home/linuxbrew/.linuxbrew"
    
    # Check if already installed
    if [[ -x "$HOMEBREW_PREFIX/bin/brew" ]]; then
        echo -e "${GREEN}Homebrew already installed${NC}"
        eval "$($HOMEBREW_PREFIX/bin/brew shellenv)"
        return 0
    fi
    
    echo -e "${BLUE}Installing Homebrew (one-time setup, persists across SteamOS updates)...${NC}"
    echo -e "${YELLOW}This may take 5-10 minutes on first run.${NC}"
    
    # Homebrew needs to run as non-root user
    if [[ $EUID -eq 0 ]] && [[ -n "$SUDO_USER" ]]; then
        # Create linuxbrew directory with correct permissions
        mkdir -p /home/linuxbrew
        chown "$SUDO_USER:$SUDO_USER" /home/linuxbrew
        
        # Install as the real user (non-interactive)
        sudo -u "$SUDO_USER" bash -c 'NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"' || {
            echo -e "${RED}Homebrew installation failed${NC}"
            return 1
        }
    else
        NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" || {
            echo -e "${RED}Homebrew installation failed${NC}"
            return 1
        }
    fi
    
    # Set up environment
    eval "$($HOMEBREW_PREFIX/bin/brew shellenv)"
    echo -e "${GREEN}Homebrew installed successfully${NC}"
}

# Find Homebrew's GCC binary (installed as gcc-VERSION, e.g., gcc-15)
find_homebrew_gcc() {
    local HOMEBREW_PREFIX="/home/linuxbrew/.linuxbrew"
    local gcc_cellar="$HOMEBREW_PREFIX/Cellar/gcc"
    
    if [[ ! -d "$gcc_cellar" ]]; then
        return 1
    fi
    
    # Get the installed version directory (e.g., 15.2.0)
    local version_dir
    version_dir=$(ls -1 "$gcc_cellar" 2>/dev/null | head -1)
    if [[ -z "$version_dir" ]]; then
        return 1
    fi
    
    # Extract major version (15.2.0 -> 15)
    local major_version="${version_dir%%.*}"
    
    # The actual binaries are gcc-MAJOR and g++-MAJOR
    local gcc_bin="$HOMEBREW_PREFIX/bin/gcc-$major_version"
    local gxx_bin="$HOMEBREW_PREFIX/bin/g++-$major_version"
    
    if [[ -x "$gcc_bin" ]] && [[ -x "$gxx_bin" ]]; then
        echo "$gcc_bin:$gxx_bin"
        return 0
    fi
    
    return 1
}

# Install build dependencies via Homebrew
setup_homebrew_build_deps() {
    echo -e "${BLUE}Installing build dependencies via Homebrew...${NC}"
    
    local HOMEBREW_PREFIX="/home/linuxbrew/.linuxbrew"
    eval "$($HOMEBREW_PREFIX/bin/brew shellenv)"
    
    # Install gcc (includes everything needed for Rust compilation)
    # Note: brew install may return non-zero for post-install warnings
    if [[ $EUID -eq 0 ]] && [[ -n "$SUDO_USER" ]]; then
        sudo -u "$SUDO_USER" "$HOMEBREW_PREFIX/bin/brew" install gcc || true
    else
        brew install gcc || true
    fi
    
    # Verify GCC actually works by finding the versioned binary
    local gcc_paths
    if gcc_paths=$(find_homebrew_gcc); then
        local gcc_bin="${gcc_paths%%:*}"
        if "$gcc_bin" --version &>/dev/null; then
            echo -e "${GREEN}Build dependencies ready! ($(basename "$gcc_bin"))${NC}"
            return 0
        fi
    fi
    
    echo -e "${RED}Failed to install gcc via Homebrew${NC}"
    return 1
}

# Setup SteamOS build environment using Homebrew (persists across updates!)
setup_steamos_build_env() {
    echo -e "${BLUE}[SteamOS] Preparing build environment via Homebrew...${NC}"
    echo -e "${YELLOW}Homebrew installs to home directory - survives SteamOS updates!${NC}\n"
    
    # Homebrew approach - no root needed, persists across updates
    setup_homebrew || {
        echo -e "${RED}Failed to set up Homebrew${NC}"
        echo -e "${YELLOW}Consider using the pre-compiled release instead:${NC}"
        echo -e "${BLUE}https://github.com/doughty247/hifi-wifi/releases${NC}"
        exit 1
    }
    
    setup_homebrew_build_deps || {
        echo -e "${RED}Failed to install build dependencies${NC}"
        exit 1
    }
    
    # Find and export the versioned GCC binaries
    local gcc_paths
    gcc_paths=$(find_homebrew_gcc)
    local gcc_bin="${gcc_paths%%:*}"
    local gxx_bin="${gcc_paths##*:}"
    local HOMEBREW_PREFIX="/home/linuxbrew/.linuxbrew"
    
    # Create cc/c++ symlinks - Rust's cc crate looks for 'cc' not $CC
    if [[ ! -e "$HOMEBREW_PREFIX/bin/cc" ]]; then
        echo -e "${BLUE}Creating cc symlink...${NC}"
        ln -sf "$gcc_bin" "$HOMEBREW_PREFIX/bin/cc"
    fi
    if [[ ! -e "$HOMEBREW_PREFIX/bin/c++" ]]; then
        echo -e "${BLUE}Creating c++ symlink...${NC}"
        ln -sf "$gxx_bin" "$HOMEBREW_PREFIX/bin/c++"
    fi
    
    export PATH="$HOMEBREW_PREFIX/bin:$PATH"
    export CC="$gcc_bin"
    export CXX="$gxx_bin"
    
    echo -e "${GREEN}Build environment ready! (CC=$CC)${NC}\n"
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

# Add hifi-wifi to user's PATH via .bashrc (survives SteamOS updates!)
setup_user_path() {
    local user_home="$REAL_HOME"
    local bashrc="$user_home/.bashrc"
    local path_line='export PATH="$PATH:/var/lib/hifi-wifi"'
    
    echo -e "${BLUE}Setting up CLI access...${NC}"
    
    # Check if already in bashrc
    if grep -qF '/var/lib/hifi-wifi' "$bashrc" 2>/dev/null; then
        echo -e "${GREEN}PATH already configured in .bashrc${NC}"
        return 0
    fi
    
    # Add to bashrc
    echo "" >> "$bashrc"
    echo "# hifi-wifi CLI access (survives SteamOS updates)" >> "$bashrc"
    echo "$path_line" >> "$bashrc"
    
    # Fix ownership if running as root
    if [[ $EUID -eq 0 ]] && [[ -n "$SUDO_USER" ]]; then
        chown "$SUDO_USER:$SUDO_USER" "$bashrc"
    fi
    
    echo -e "${GREEN}Added /var/lib/hifi-wifi to PATH in .bashrc${NC}"
    echo -e "${YELLOW}Note: Run 'source ~/.bashrc' or open a new terminal to use 'hifi-wifi' command${NC}"
}

# Apply initial optimizations
apply_optimizations() {
    local run_as_root=""
    [[ $EUID -ne 0 ]] && run_as_root="sudo"
    
    echo -e "${BLUE}Applying initial optimizations...${NC}"
    $run_as_root /var/lib/hifi-wifi/hifi-wifi apply
    echo ""
}

# Offer reboot
offer_reboot() {
    echo -e "${GREEN}Success! hifi-wifi v3.0.0-beta3 is installed and active.${NC}\n"
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
    
    # First check if binary exists in bin/ (release package)
    local script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    if [[ -f "$script_dir/bin/hifi-wifi" ]]; then
        echo -e "${GREEN}Found release binary: bin/hifi-wifi${NC}"
        
        # Verify architecture
        if ! file "$script_dir/bin/hifi-wifi" | grep -q "x86-64"; then
            echo -e "${RED}Error: Binary is not x86_64 architecture${NC}"
            exit 1
        fi
        
        # Copy to target/release for install step
        mkdir -p target/release
        cp "$script_dir/bin/hifi-wifi" target/release/hifi-wifi
        chmod +x target/release/hifi-wifi
        echo -e "${GREEN}Using release binary (skipping build)${NC}\n"
    else    echo -e "${YELLOW}No pre-compiled binary found. Will build from source.${NC}\n"
            
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
        
        # Pre-release warning for source builds
        echo -e "${YELLOW}╔══════════════════════════════════════════════════════════════╗${NC}"
        echo -e "${YELLOW}║              ⚠️  PRE-RELEASE SOFTWARE WARNING  ⚠️              ║${NC}"
        echo -e "${YELLOW}╠══════════════════════════════════════════════════════════════╣${NC}"
        echo -e "${YELLOW}║  This is hifi-wifi v3.0.0-beta3 - a TESTING release.         ║${NC}"
        echo -e "${YELLOW}║                                                              ║${NC}"
        echo -e "${YELLOW}║  • NOT recommended for production use                        ║${NC}"
        echo -e "${YELLOW}║  • May contain bugs or unexpected behavior                   ║${NC}"
        echo -e "${YELLOW}║  • Intended for testing and feedback purposes only           ║${NC}"
        echo -e "${YELLOW}╚══════════════════════════════════════════════════════════════╝${NC}\n"
        
        # SteamOS-specific: Homebrew build info
        if [[ "$distro_id" == "steamos" ]]; then
            echo -e "${BLUE}SteamOS Build Info:${NC} First-time setup uses Homebrew (~10 min)."
            echo -e "Build environment persists across SteamOS updates.\n"
        fi
        
            read -p "I understand this is pre-release software for testing only. Continue? [y/N] " -n 1 -r
            echo
            [[ ! $REPLY =~ ^[Yy]$ ]] && exit 1
            
            # Step 2: Setup build environment (SteamOS uses Homebrew, others use system packages)
            if [[ "$distro_id" == "steamos" ]]; then
                echo -e "${BLUE}[2/5] Setting up Homebrew build environment...${NC}"
                setup_steamos_build_env
            elif [[ "$distro_id" == *"arch"* ]]; then
                if ! command -v cc &>/dev/null; then
                    echo -e "${BLUE}[2/5] Setting up build environment...${NC}"
                    # Arch but not SteamOS - use pacman directly
                    sudo pacman -Sy --noconfirm --needed base-devel
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
    fi
    
    # Step 5: Install
    echo -e "${BLUE}[5/5] Installing service...${NC}"
    install_service
    setup_user_path

    # Ensure service is enabled and started for persistence
    local run_as_root=""
    [[ $EUID -ne 0 ]] && run_as_root="sudo"
    $run_as_root systemctl enable --now hifi-wifi.service >/dev/null 2>&1 || true

    apply_optimizations
    
    # Offer reboot
    offer_reboot
}

# Run main
main
