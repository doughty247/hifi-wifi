#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION=$(grep '^version' "$SCRIPT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
GIT_HASH=$(git rev-parse --short HEAD)
PACKAGE_NAME="hifi-wifi-${VERSION%%-*}-${GIT_HASH}"
BUILD_DIR="$SCRIPT_DIR/release-build"
PACKAGE_DIR="$BUILD_DIR/$PACKAGE_NAME"

echo "Building v$VERSION ($GIT_HASH)..."
cargo build --release

rm -rf "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR/bin"

cp target/release/hifi-wifi "$PACKAGE_DIR/bin/"
chmod +x "$PACKAGE_DIR/bin/hifi-wifi"
cp install.sh README.md "$PACKAGE_DIR/"
chmod +x "$PACKAGE_DIR/install.sh"
[[ -f uninstall.sh ]] && cp uninstall.sh "$PACKAGE_DIR/" && chmod +x "$PACKAGE_DIR/uninstall.sh"

cat > "$PACKAGE_DIR/TESTING-NOTES.md" << 'EOF'
# Testing Notes

## Install
```bash
sudo ./install.sh
```

## Validate
```bash
systemctl status hifi-wifi
journalctl -u hifi-wifi -n 50
```

## Test
- Ping gateway: `ping -c 100 <gateway>`
- Check CAKE RTT in logs
- Test streaming/gaming

## Uninstall
```bash
sudo ./uninstall.sh
```
EOF

cd "$BUILD_DIR"
TARBALL="hifi-wifi-v$VERSION-$GIT_HASH.tar.gz"
tar -czf "$TARBALL" "$PACKAGE_NAME"
sha256sum "$TARBALL" > "$TARBALL.sha256"

echo "Created: $TARBALL"
cat "$TARBALL.sha256"
