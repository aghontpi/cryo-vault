#!/bin/bash

set -e
export PATH="$HOME/.cargo/bin:$PATH"

APP_NAME="cryo-vault"
VERSION=$(grep '^version =' Cargo.toml | cut -d '"' -f2)
DIST_DIR="dist"

mkdir -p "$DIST_DIR"

echo "Building $APP_NAME v$VERSION..."

# Check for cargo-zigbuild
if ! command -v cargo-zigbuild &> /dev/null; then
    echo "❌ cargo-zigbuild could not be found!"
    echo "Please install it:"
    echo "  brew install zig"
    echo "  cargo install cargo-zigbuild"
    exit 1
fi

# Build a single target for a specific platform
# Arguments:
#   $1 - TARGET: Rust target triple (e.g., x86_64-apple-darwin)
#   $2 - OS: Operating system name (e.g., macos, linux, windows)
#   $3 - ARCH: Architecture (e.g., x64, arm64)
#   $4 - BUILDER: Build tool to use (cargo or zigbuild)
#   $5 - EXTENSION: Binary extension (empty for Unix, .exe for Windows)
build_target() {
    local TARGET=$1
    local OS=$2
    local ARCH=$3
    local BUILDER=$4
    local EXTENSION=$5

    echo "--------------------------------------------------"
    echo "Building for $OS ($ARCH) - Target: $TARGET..."
    
    if [ "$BUILDER" = "cargo" ]; then
        cargo build --release --target "$TARGET"
    else
        cargo zigbuild --release --target "$TARGET"
    fi

    local BINARIES=("cryo-vault" "cryo-vault-mcp")

    for BIN_NAME in "${BINARIES[@]}"; do
        local SRC_BIN="target/$TARGET/release/$BIN_NAME$EXTENSION"
        local DEST_BIN="$DIST_DIR/$BIN_NAME-v$VERSION-$OS-$ARCH$EXTENSION"

        if [ -f "$SRC_BIN" ]; then
            cp "$SRC_BIN" "$DEST_BIN"
            echo "✅ Success! Artifact: $DEST_BIN"
            
            # Show file info
            file "$DEST_BIN"
        else
            echo "❌ Error: Binary not found at $SRC_BIN"
            exit 1
        fi
    done
}


# Ensure all targets are installed
echo "Ensuring Rust targets are installed..."
rustup target add x86_64-apple-darwin 2>/dev/null || true
rustup target add aarch64-apple-darwin 2>/dev/null || true
rustup target add x86_64-unknown-linux-gnu 2>/dev/null || true
rustup target add aarch64-unknown-linux-gnu 2>/dev/null || true
rustup target add x86_64-pc-windows-gnu 2>/dev/null || true
rustup target add aarch64-pc-windows-gnullvm 2>/dev/null || true
rustup target add x86_64-linux-android 2>/dev/null || true
rustup target add aarch64-linux-android 2>/dev/null || true

build_target "x86_64-apple-darwin" "macos" "x64" "cargo" ""
build_target "aarch64-apple-darwin" "macos" "arm64" "cargo" ""

# --- Linux (Cross) ---
build_target "x86_64-unknown-linux-gnu" "linux" "x64" "zigbuild" ""
build_target "aarch64-unknown-linux-gnu" "linux" "arm64" "zigbuild" ""

# --- Windows (Cross) ---
# Using GNU targets for better compatibility with Zig
build_target "x86_64-pc-windows-gnu" "windows" "x64" "zigbuild" ".exe"
# ARM64 Windows GNU target
build_target "aarch64-pc-windows-gnullvm" "windows" "arm64" "zigbuild" ".exe"

# --- Android (Cross) ---
# Android builds require the Android NDK to compile C dependencies (like zstd-sys).
# We check if there is an active NDK directory.
NDK_FOUND=false
if [ -n "$ANDROID_NDK_HOME" ] && [ -d "$ANDROID_NDK_HOME" ]; then
    NDK_FOUND=true
elif [ -n "$ANDROID_HOME" ] && [ -d "$ANDROID_HOME/ndk" ] && [ "$(ls -A "$ANDROID_HOME/ndk" 2>/dev/null)" ]; then
    NDK_FOUND=true
elif [ -n "$ANDROID_HOME" ] && [ -d "$ANDROID_HOME/ndk-bundle" ] && [ "$(ls -A "$ANDROID_HOME/ndk-bundle" 2>/dev/null)" ]; then
    NDK_FOUND=true
fi

if [ "$NDK_FOUND" = true ]; then
    build_target "x86_64-linux-android" "android" "x64" "zigbuild" ""
    build_target "aarch64-linux-android" "android" "arm64" "zigbuild" ""
else
    echo "--------------------------------------------------"
    echo "⚠️ Android NDK not found. Skipping Android target builds."
    echo "To build for Android, please install the NDK and set ANDROID_NDK_HOME."
fi

echo "--------------------------------------------------"
echo "🎉 All builds completed successfully!"
ls -lh "$DIST_DIR"

