#!/bin/sh
# Devbox installer — downloads the latest release binary from GitHub.
# Usage: curl -fsSL https://raw.githubusercontent.com/ethannortharc/devbox/main/install.sh | sh

set -e

REPO="ethannortharc/devbox"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

main() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) binary="devbox-darwin-arm64" ;;
                x86_64)
                    echo "Error: macOS x86_64 builds are not available. Apple Silicon (arm64) only." >&2
                    exit 1
                    ;;
                *)
                    echo "Error: unsupported architecture: $arch" >&2
                    exit 1
                    ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64) binary="devbox-linux-amd64" ;;
                *)
                    echo "Error: unsupported architecture: $arch" >&2
                    exit 1
                    ;;
            esac
            ;;
        *)
            echo "Error: unsupported OS: $os" >&2
            exit 1
            ;;
    esac

    # Get latest release tag
    echo "Fetching latest release..."
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*: *"//;s/".*//')"

    if [ -z "$tag" ]; then
        echo "Error: could not determine latest release." >&2
        exit 1
    fi

    url="https://github.com/${REPO}/releases/download/${tag}/${binary}"
    echo "Downloading devbox ${tag} (${binary})..."

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    curl -fsSL -o "${tmpdir}/devbox" "$url"
    chmod +x "${tmpdir}/devbox"

    # Install
    if [ -w "$INSTALL_DIR" ]; then
        mv "${tmpdir}/devbox" "${INSTALL_DIR}/devbox"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${tmpdir}/devbox" "${INSTALL_DIR}/devbox"
    fi

    echo ""
    echo "devbox ${tag} installed to ${INSTALL_DIR}/devbox"
    echo ""
    echo "Get started:"
    echo "  cd your-project"
    echo "  devbox"
}

main
