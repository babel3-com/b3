#!/bin/sh
# Babel3 installer — detects OS/arch, downloads binary from babel3.com.
# Usage: curl -fsSL https://babel3.com/install.sh | bash
set -e

BASE_URL="https://babel3.com/releases/latest"
INSTALL_DIR="${B3_INSTALL_DIR:-/usr/local/bin}"
BINARY="b3"

# --- helpers ---

info()  { printf '\033[1;34m==>\033[0m %s\n' "$1"; }
warn()  { printf '\033[1;33mWARN:\033[0m %s\n' "$1"; }
error() { printf '\033[1;31mERROR:\033[0m %s\n' "$1" >&2; exit 1; }

need_cmd() {
    if ! command -v "$1" > /dev/null 2>&1; then
        error "Need '$1' (command not found)"
    fi
}

# --- detect platform ---

detect_platform() {
    local os arch

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)   os="linux" ;;
        Darwin)  os="darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="windows" ;;
        *)       error "Unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)   arch="amd64" ;;
        aarch64|arm64)   arch="arm64" ;;
        *)               error "Unsupported architecture: $arch" ;;
    esac

    PLATFORM="${os}-${arch}"

    if [ "$os" = "windows" ]; then
        ASSET="${BINARY}-${PLATFORM}.exe"
    else
        ASSET="${BINARY}-${PLATFORM}"
    fi
}

# --- download + install ---

download_and_install() {
    local url tmpdir tmpfile

    url="${BASE_URL}/${ASSET}"

    info "Downloading ${BINARY} for ${PLATFORM}..."
    info "  ${url}"

    tmpdir="$(mktemp -d)"
    tmpfile="${tmpdir}/${ASSET}"

    if command -v curl > /dev/null 2>&1; then
        curl -fsSL -o "$tmpfile" "$url" || error "Download failed. No release available for ${PLATFORM}."
    elif command -v wget > /dev/null 2>&1; then
        wget -q -O "$tmpfile" "$url" || error "Download failed."
    else
        error "Need curl or wget"
    fi

    # Verify checksum if available
    local checksum_url="${BASE_URL}/${ASSET}.sha256"
    local checksum_file="${tmpdir}/${ASSET}.sha256"
    if curl -fsSL -o "$checksum_file" "$checksum_url" 2>/dev/null; then
        if command -v sha256sum > /dev/null 2>&1; then
            info "Verifying checksum..."
            (cd "$tmpdir" && sha256sum -c "${ASSET}.sha256") || error "Checksum verification failed!"
        elif command -v shasum > /dev/null 2>&1; then
            info "Verifying checksum..."
            local expected
            expected=$(awk '{print $1}' "$checksum_file")
            local actual
            actual=$(shasum -a 256 "$tmpfile" | awk '{print $1}')
            if [ "$expected" != "$actual" ]; then
                error "Checksum mismatch! Expected ${expected}, got ${actual}"
            fi
        else
            warn "No sha256sum or shasum found — skipping checksum verification"
        fi
    fi

    chmod +x "$tmpfile"

    # Install to target directory
    if [ -w "$INSTALL_DIR" ]; then
        mv "$tmpfile" "${INSTALL_DIR}/${BINARY}"
    else
        # Fall back to ~/.local/bin instead of prompting for sudo
        INSTALL_DIR="${HOME}/.local/bin"
        mkdir -p "$INSTALL_DIR"
        mv "$tmpfile" "${INSTALL_DIR}/${BINARY}"
        # Add to PATH permanently
        local path_line='export PATH="$HOME/.local/bin:$PATH"'
        local profile=""
        if [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = "zsh" ]; then
            profile="${HOME}/.zshrc"
        elif [ -f "${HOME}/.bashrc" ]; then
            profile="${HOME}/.bashrc"
        elif [ -f "${HOME}/.bash_profile" ]; then
            profile="${HOME}/.bash_profile"
        elif [ -f "${HOME}/.profile" ]; then
            profile="${HOME}/.profile"
        fi

        if [ -n "$profile" ] && ! grep -qF '.local/bin' "$profile" 2>/dev/null; then
            echo "" >> "$profile"
            echo "# Added by Babel3 installer" >> "$profile"
            echo "$path_line" >> "$profile"
            info "Added ~/.local/bin to PATH in ${profile}"
        fi
    fi

    rm -rf "$tmpdir"
}

# --- verify ---

verify_install() {
    if ! command -v "$BINARY" > /dev/null 2>&1; then
        warn "${BINARY} installed to ${INSTALL_DIR}/${BINARY} but not in PATH"
        warn "Add ${INSTALL_DIR} to your PATH, or set B3_INSTALL_DIR"
        return
    fi

    info "Installed ${BINARY} to ${INSTALL_DIR}/${BINARY}"
}

# --- main ---

main() {
    info "Babel3 Installer"
    echo

    detect_platform
    download_and_install
    verify_install

    echo
    if ! command -v "$BINARY" > /dev/null 2>&1; then
        # Binary installed to ~/.local/bin but not yet in PATH for this shell
        export PATH="${HOME}/.local/bin:${PATH}"
    fi

    if command -v "$BINARY" > /dev/null 2>&1; then
        info "Installed. Starting Babel3..."
        echo
        # Redirect stdin from /dev/tty so interactive prompts (token input)
        # work even when install.sh is piped from curl (curl | bash).
        # In Docker/CI, /dev/tty exists as a device node but can't be opened.
        # Test with actual open, not just -e (existence check).
        if (exec < /dev/tty) 2>/dev/null; then
            exec "$BINARY" start < /dev/tty
        else
            info "Run 'b3 start' to begin."
        fi
    else
        info "Installed to ${INSTALL_DIR}/${BINARY}"
        info "Reload your shell, then run: b3 start"
    fi
}

main "$@"
