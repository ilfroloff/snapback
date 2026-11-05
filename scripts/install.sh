#!/bin/sh
set -e

# snapback — install the latest release from GitHub
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ilfroloff/snapback/main/scripts/install.sh | sh
#
# Environment variables:
#   SNAPBACK_INSTALL_DIR  Override the install directory (default: ~/.local/bin)
#
# This script is hosted at:
#   https://raw.githubusercontent.com/ilfroloff/snapback/main/scripts/install.sh
#
# It downloads the prebuilt binary for the current platform from the latest
# GitHub Release (https://github.com/ilfroloff/snapback/releases/latest),
# verifies its SHA-256 checksum against the SHA256SUMS file published with
# the release, and installs it as both `snapback` and `sb`.

REPO="ilfroloff/snapback"
BASE_URL="https://github.com/${REPO}/releases"
API_URL="https://api.github.com/repos/${REPO}/releases/latest"

# ── Helpers ──────────────────────────────────────────────────────────────────

info() {
    printf '\033[1;34m==>\033[0m %s\n' "$1"
}

warn() {
    printf '\033[1;33m==>\033[0m %s\n' "$1" >&2
}

error() {
    printf '\033[1;31merror:\033[0m %s\n' "$1" >&2
    exit 1
}

# ── Detect platform ──────────────────────────────────────────────────────────

detect_os() {
    os="$(uname -s)"
    case "$os" in
        Darwin) printf 'darwin' ;;
        Linux)  printf 'linux'  ;;
        *)      error "unsupported operating system: $os (only macOS and Linux are supported)" ;;
    esac
}

detect_arch() {
    arch="$(uname -m)"
    case "$arch" in
        arm64|aarch64) printf 'arm64' ;;
        x86_64|amd64)  printf 'x64'   ;;
        *)             error "unsupported architecture: $arch (only arm64 and x64 are supported)" ;;
    esac
}

# ── Resolve latest version ───────────────────────────────────────────────────

get_latest_version() {
    # Follow the /releases/latest redirect to extract the tag from the
    # Location header. This avoids parsing JSON and needs no jq dependency.
    location="$(curl -sSfI "${BASE_URL}/latest" 2>/dev/null \
        | grep -i '^location:' \
        | head -n 1 \
        | sed 's/.*\/tag\///' \
        | tr -d '\r\n ')"

    if [ -z "$location" ]; then
        error "could not determine the latest release version"
    fi

    printf '%s' "$location"
}

# ── Download with retry ──────────────────────────────────────────────────────

download() {
    url="$1"
    dest="$2"

    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$url" -O "$dest"
    else
        error "either curl or wget is required to download files"
    fi
}

# ── Checksum verification ────────────────────────────────────────────────────

compute_sha256() {
    file="$1"

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | cut -d' ' -f1
    else
        error "sha256sum or shasum is required for checksum verification"
    fi
}

verify_checksum() {
    binary_path="$1"
    sums_path="$2"
    asset_name="$3"

    # The SHA256SUMS file uses the format: <hash>  <filename>
    # Extract the expected hash for our binary.
    expected="$(grep "  ${asset_name}\$" "$sums_path" | cut -d' ' -f1)"

    if [ -z "$expected" ]; then
        # Try the binary format: <hash> *<filename>
        expected="$(grep "\*${asset_name}\$" "$sums_path" | cut -d' ' -f1)"
    fi

    if [ -z "$expected" ]; then
        warn "checksum for ${asset_name} not found in SHA256SUMS — skipping verification"
        return 0
    fi

    actual="$(compute_sha256 "$binary_path")"

    if [ "$actual" != "$expected" ]; then
        error "checksum mismatch for ${asset_name}
  expected: ${expected}
  actual:   ${actual}
The downloaded binary may be corrupted or tampered with."
    fi

    info "checksum verified"
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    info "detecting platform..."
    os="$(detect_os)"
    arch="$(detect_arch)"
    platform="${os}-${arch}"
    info "platform: ${platform}"

    info "resolving latest version..."
    version="$(get_latest_version)"
    info "latest version: ${version}"

    asset_name="snapback-${version}-${platform}"
    binary_url="${BASE_URL}/download/${version}/${asset_name}"
    sums_url="${BASE_URL}/download/${version}/SHA256SUMS"

    # Create a temporary directory for downloads
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    binary_path="${tmpdir}/snapback"
    sums_path="${tmpdir}/SHA256SUMS"

    info "downloading ${asset_name}..."
    if ! download "$binary_url" "$binary_path"; then
        error "failed to download ${binary_url}
This may mean the latest release does not include a binary for ${platform}.
Check ${BASE_URL}/latest for available assets."
    fi

    info "downloading SHA256SUMS..."
    if download "$sums_url" "$sums_path" 2>/dev/null; then
        verify_checksum "$binary_path" "$sums_path" "$asset_name"
    else
        warn "SHA256SUMS not found for this release — skipping checksum verification"
    fi

    # Determine install directory
    install_dir="${SNAPBACK_INSTALL_DIR:-${HOME}/.local/bin}"

    info "installing to ${install_dir}..."
    mkdir -p "$install_dir"

    # Install the binary
    cp "$binary_path" "${install_dir}/snapback"
    chmod +x "${install_dir}/snapback"

    # Create the `sb` alias as a copy (same as the npm installer does)
    cp "${install_dir}/snapback" "${install_dir}/sb"
    chmod +x "${install_dir}/sb"

    # Check if install_dir is on PATH
    case ":${PATH}:" in
        *":${install_dir}:"*)
            ;;
        *)
            warn "${install_dir} is not on your PATH"
            info "add it by running:"
            printf '    export PATH="%s:$PATH"\n' "$install_dir"
            info "or add the above line to your shell profile (~/.bashrc, ~/.zshrc, etc.)"
            ;;
    esac

    printf '\n'
    info "snapback ${version} installed successfully!"
    printf '\n'
    info "next steps:"
    printf '    snapback          # launch the board\n'
    printf '    sb                # same thing, shorter name\n'
    printf '    snapback --help   # see all options\n'
    printf '\n'
}

main
