#!/usr/bin/env bash
# ==============================================================================
# uj installation and environment setup script for Linux
# ==============================================================================
set -euo pipefail

DEST_DIR="/usr/local/bin"
APPARMOR_DIR="/etc/apparmor.d"
UJ_BIN="${DEST_DIR}/uj"

echo "=== Installing uj (Universal Judge) ==="

# Check architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        TARGET="x86_64-unknown-linux-musl"
        ;;
    aarch64|arm64)
        TARGET="aarch64-unknown-linux-musl"
        ;;
    *)
        echo "Error: Unsupported architecture: $ARCH" >&2
        exit 1
        ;;
esac

# Check for root/sudo
IS_ROOT=false
if [ "$(id -u)" -eq 0 ]; then
    IS_ROOT=true
fi

# 1. Install Binary
if [ -f "./target/release/uj" ]; then
    echo "Installing compiled binary from ./target/release/uj..."
    if [ "$IS_ROOT" = true ]; then
        install -m 755 "./target/release/uj" "$UJ_BIN"
    else
        sudo install -m 755 "./target/release/uj" "$UJ_BIN"
    fi
elif [ -f "./uj" ]; then
    echo "Installing binary from ./uj..."
    if [ "$IS_ROOT" = true ]; then
        install -m 755 "./uj" "$UJ_BIN"
    else
        sudo install -m 755 "./uj" "$UJ_BIN"
    fi
else
    echo "Building release binary with cargo..."
    cargo build --release
    if [ "$IS_ROOT" = true ]; then
        install -m 755 "./target/release/uj" "$UJ_BIN"
    else
        sudo install -m 755 "./target/release/uj" "$UJ_BIN"
    fi
fi

echo "uj installed to $UJ_BIN"

# 2. Ubuntu 23.10+ / 24.04 AppArmor unprivileged userns configuration
APPARMOR_RESTRICT="/proc/sys/kernel/apparmor_restrict_unprivileged_userns"
if [ -f "$APPARMOR_RESTRICT" ] && [ "$(cat "$APPARMOR_RESTRICT")" = "1" ]; then
    echo "Detected Ubuntu 24.04+ AppArmor unprivileged user namespace restriction."
    PROFILE_FILE="${APPARMOR_DIR}/uj"
    
    echo "Installing AppArmor profile to $PROFILE_FILE..."
    PROFILE_CONTENT="# AppArmor profile for uj (Universal Judge)
abi <abi/4.0>,
include <tunables/global>

${UJ_BIN} flags=(unconfined) {
    userns,
}
"
    if [ "$IS_ROOT" = true ]; then
        echo "$PROFILE_CONTENT" > "$PROFILE_FILE"
        if command -v apparmor_parser >/dev/null 2>&1; then
            apparmor_parser -r "$PROFILE_FILE" || true
        fi
    else
        echo "$PROFILE_CONTENT" | sudo tee "$PROFILE_FILE" >/dev/null
        if command -v apparmor_parser >/dev/null 2>&1; then
            sudo apparmor_parser -r "$PROFILE_FILE" || true
        fi
    fi
    echo "AppArmor profile successfully loaded."
fi

# 3. Systemd cgroup delegation check
echo "Verifying systemd user cgroup delegation..."
SYSTEMD_USER_CONF="/etc/systemd/system/user@.service.d/delegate.conf"
if [ ! -f "$SYSTEMD_USER_CONF" ]; then
    echo "Note: For optimal rootless cgroup v2 judging, systemd user delegation can be enabled:"
    echo "  sudo mkdir -p /etc/systemd/system/user@.service.d/"
    echo "  echo -e '[Service]\nDelegate=cpu cpuset memory pids' | sudo tee $SYSTEMD_USER_CONF"
    echo "  sudo systemctl daemon-reload"
fi

# 4. Verify installation
echo "Running 'uj doctor'..."
"$UJ_BIN" doctor || true

echo "=== uj installation completed successfully! ==="
