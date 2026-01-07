#!/bin/bash
# SteamOS Preparation Script for hifi-wifi
# Run this ONCE before running install.sh on a fresh SteamOS install

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${BLUE}=== SteamOS Preparation for hifi-wifi ===${NC}"
echo ""

# Must be root
if [[ $EUID -ne 0 ]]; then
    echo -e "${RED}This script must be run as root.${NC}"
    echo -e "Try: ${BLUE}sudo ./steamos-prep.sh${NC}"
    exit 1
fi

# Check for sysexts
echo -e "${BLUE}[1/6] Checking for system extensions...${NC}"
if systemd-sysext status | grep -q "merged"; then
    echo -e "${YELLOW}System extensions detected. Unmerging...${NC}"
    systemd-sysext unmerge || true
    sleep 1
fi

# Disable readonly
echo -e "${BLUE}[2/6] Disabling readonly filesystem...${NC}"
steamos-readonly disable

# Wait and verify
sleep 2
if mount | grep -q "/ type btrfs.*ro,"; then
    echo -e "${RED}Filesystem is still read-only after disable!${NC}"
    echo -e "You may need to reboot and try again."
    exit 1
fi

# Initialize pacman
echo -e "${BLUE}[3/6] Initializing pacman...${NC}"
if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
    pacman-key --init
fi

# Populate keys
echo -e "${BLUE}[4/6] Populating pacman keys...${NC}"
pacman-key --populate archlinux holo 2>/dev/null || pacman-key --populate archlinux

# Update package database
echo -e "${BLUE}[5/6] Syncing package database...${NC}"
pacman -Sy

# Install build tools
echo -e "${BLUE}[6/6] Installing build dependencies...${NC}"
pacman -S --noconfirm --needed base-devel glibc linux-api-headers

echo ""
echo -e "${GREEN}✓ SteamOS preparation complete!${NC}"
echo ""
echo -e "You can now run: ${BLUE}./install.sh${NC}"
echo ""
echo -e "${YELLOW}Note:${NC} After a SteamOS update, you may need to run this prep script again"
echo -e "      before reinstalling hifi-wifi."
