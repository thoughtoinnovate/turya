#!/bin/bash
# turya installer script
# Usage: curl -fsSL https://raw.githubusercontent.com/thoughtoinnovate/turya/main/install.sh | bash
#
# Security: This script verifies SHA256 checksums to ensure binary integrity

set -e

# Version selection is dynamic: unless pinned via --version, the installer
# resolves the latest GitHub release tag at runtime, so this script never
# goes stale and doubles as an updater (skips when already current).
PINNED_VERSION=""
FORCE_REINSTALL="false"
CHECK_ONLY="false"
REPO="thoughtoinnovate/turya"
BINARY_NAME="turya"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
SKIP_VERIFY="${SKIP_VERIFY:-false}"
GITHUB_TOKEN="${GITHUB_TOKEN:-}"
PROMPT_FOR_TOKEN="${PROMPT_FOR_TOKEN:-false}"
TOKEN_FROM_STDIN="${TOKEN_FROM_STDIN:-false}"
CONNECT_TIMEOUT_SECONDS="${CONNECT_TIMEOUT_SECONDS:-15}"
DOWNLOAD_TIMEOUT_SECONDS="${DOWNLOAD_TIMEOUT_SECONDS:-120}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

error() {
    echo -e "${RED}[ERROR]${NC} $1"
    exit 1
}

security() {
    echo -e "${CYAN}[SECURITY]${NC} $1"
}

read_github_token() {
    if [ -n "$GITHUB_TOKEN" ]; then
        return 0
    fi

    if [ "$TOKEN_FROM_STDIN" = "true" ]; then
        info "Reading GitHub token from stdin..."
        IFS= read -r GITHUB_TOKEN || true
    elif [ "$PROMPT_FOR_TOKEN" = "true" ]; then
        if [ -t 0 ]; then
            printf "GitHub token (input hidden): "
            stty -echo
            IFS= read -r GITHUB_TOKEN || true
            stty echo
            printf "\n"
        else
            warn "Cannot prompt for token in non-interactive mode. Use GITHUB_TOKEN env or --token-stdin."
            return 1
        fi
    fi

    if [ -z "$GITHUB_TOKEN" ]; then
        return 1
    fi
    return 0
}

download_file() {
    local url="$1"
    local output="$2"

    if command -v curl &> /dev/null; then
        if [ -n "$GITHUB_TOKEN" ]; then
            curl -fsSL \
                --connect-timeout "${CONNECT_TIMEOUT_SECONDS}" \
                --max-time "${DOWNLOAD_TIMEOUT_SECONDS}" \
                -H "Authorization: Bearer ${GITHUB_TOKEN}" \
                -H "Accept: application/octet-stream" \
                "$url" \
                -o "$output" \
                2>/dev/null
        else
            curl -fsSL \
                --connect-timeout "${CONNECT_TIMEOUT_SECONDS}" \
                --max-time "${DOWNLOAD_TIMEOUT_SECONDS}" \
                "$url" \
                -o "$output" \
                2>/dev/null
        fi
        return $?
    fi

    if command -v wget &> /dev/null; then
        if [ -n "$GITHUB_TOKEN" ]; then
            wget -q \
                --timeout="${CONNECT_TIMEOUT_SECONDS}" \
                --header="Authorization: Bearer ${GITHUB_TOKEN}" \
                --header="Accept: application/octet-stream" \
                "$url" \
                -O "$output" \
                2>/dev/null
        else
            wget -q \
                --timeout="${CONNECT_TIMEOUT_SECONDS}" \
                "$url" \
                -O "$output" \
                2>/dev/null
        fi
        return $?
    fi

    error "Neither curl nor wget found. Please install one of them."
}

download_with_auth_retry() {
    local url="$1"
    local output="$2"

    if download_file "$url" "$output"; then
        return 0
    fi

    # If initial unauthenticated download fails, try token once.
    if [ -z "$GITHUB_TOKEN" ]; then
        warn "Download failed without authentication."
        if read_github_token; then
            info "Retrying download with GitHub token..."
            download_file "$url" "$output"
            return $?
        fi
    fi

    return 1
}

validate_github_token_access() {
    # Only validate when token is present and curl exists.
    if [ -z "$GITHUB_TOKEN" ] || ! command -v curl &> /dev/null; then
        return 0
    fi

    local code
    code=$(curl -sS -o /dev/null -w "%{http_code}" \
        -H "Authorization: Bearer ${GITHUB_TOKEN}" \
        "https://api.github.com/repos/${REPO}" || true)

    case "$code" in
        200) return 0 ;;
        401) error "GitHub token is invalid or expired (HTTP 401). Create a new token with Contents: Read." ;;
        403) error "GitHub token is not authorized for this repo (HTTP 403). Check org SSO authorization and repo access." ;;
        404) error "Repo ${REPO} is not accessible with this token (HTTP 404). Check repo selection in fine-grained token." ;;
        000) error "Network error while validating token access to GitHub." ;;
        *) error "Unexpected GitHub response while validating token access: HTTP ${code}." ;;
    esac
}

diagnose_download_failure() {
    local url="$1"

    if ! command -v curl &> /dev/null; then
        warn "Download failed. Install curl for detailed diagnostics."
        return
    fi

    local code
    if [ -n "$GITHUB_TOKEN" ]; then
        code=$(curl -sS -o /dev/null -w "%{http_code}" \
            -H "Authorization: Bearer ${GITHUB_TOKEN}" \
            -H "Accept: application/octet-stream" \
            "$url" || true)
    else
        code=$(curl -sS -o /dev/null -w "%{http_code}" \
            -H "Accept: application/octet-stream" \
            "$url" || true)
    fi

    case "$code" in
        401) warn "Download failed: invalid or expired token (HTTP 401)." ;;
        403) warn "Download failed: token lacks permission, SSO not authorized, or API rate-limited (HTTP 403)." ;;
        404) warn "Download failed: release asset not found or repo not accessible (HTTP 404). Has any release been published yet?" ;;
        000) warn "Download failed: network/connectivity issue (HTTP 000)." ;;
        *) warn "Download failed with HTTP ${code}." ;;
    esac
}

# Detect OS and architecture
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)   os="linux" ;;
        Darwin*)  os="darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="windows" ;;
        *)        error "Unsupported operating system: $(uname -s)

Supported: Linux, macOS (Darwin), Windows" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="arm64" ;;
        *)             error "Unsupported architecture: $(uname -m)" ;;
    esac

    echo "${os}-${arch}"
}

# Get the download URL for a specific version
get_download_url() {
    local version="$1"
    local asset_name="$2"
    echo "https://github.com/${REPO}/releases/download/${version}/${asset_name}"
}

# Asset names mirror release.yml. Linux builds are statically linked
# (musl), macOS/Windows use the platform toolchain.
get_asset_name() {
    local platform="$1"
    local asset_name=""

    case "$platform" in
        linux-x86_64)  asset_name="turya-linux-x86_64-musl" ;;
        linux-arm64)   asset_name="turya-linux-arm64-musl" ;;
        darwin-x86_64) asset_name="turya-darwin-x86_64" ;;
        darwin-arm64)  asset_name="turya-darwin-arm64" ;;
        windows-x86_64) asset_name="turya-windows-x86_64.exe" ;;
        windows-arm64)  asset_name="turya-windows-arm64.exe" ;;
        *) error "No release asset for platform: ${platform}" ;;
    esac

    echo "${asset_name}"
}

# Fetch the latest release tag (e.g. v0.1.0) from the GitHub API.
# Prefers python3, then jq, then a sed fallback. Fails with a clear
# remediation message instead of installing a stale version.
# NOTE: /releases/latest excludes prereleases (incl. nightly builds).
fetch_latest_tag() {
    local url="https://api.github.com/repos/${REPO}/releases/latest"
    local response=""

    if command -v curl &> /dev/null; then
        if [ -n "$GITHUB_TOKEN" ]; then
            response=$(curl -fsSL \
                --connect-timeout "${CONNECT_TIMEOUT_SECONDS}" \
                --max-time "${DOWNLOAD_TIMEOUT_SECONDS}" \
                -H "Authorization: Bearer ${GITHUB_TOKEN}" \
                "$url" 2>/dev/null) || response=""
        else
            response=$(curl -fsSL \
                --connect-timeout "${CONNECT_TIMEOUT_SECONDS}" \
                --max-time "${DOWNLOAD_TIMEOUT_SECONDS}" \
                "$url" 2>/dev/null) || response=""
        fi
    elif command -v wget &> /dev/null; then
        response=$(wget -qO- \
            --timeout="${CONNECT_TIMEOUT_SECONDS}" \
            ${GITHUB_TOKEN:+--header="Authorization: Bearer ${GITHUB_TOKEN}"} \
            "$url" 2>/dev/null) || response=""
    fi

    if [ -z "$response" ]; then
        return 1
    fi

    local tag=""
    if command -v python3 &> /dev/null; then
        tag=$(echo "$response" | python3 -c 'import json, sys; print(json.load(sys.stdin).get("tag_name", ""))' 2>/dev/null) || tag=""
    elif command -v jq &> /dev/null; then
        tag=$(echo "$response" | jq -r '.tag_name // empty' 2>/dev/null) || tag=""
    else
        tag=$(echo "$response" | grep -m1 '"tag_name":' | sed -E 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/')
    fi

    if [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]]; then
        echo "$tag"
        return 0
    fi
    return 1
}

# Strip a leading 'v' for numeric comparison (v0.1.0 -> 0.1.0).
normalize_version() {
    echo "${1#v}"
}

# Compare two versions (leading 'v' tolerated).
# Echoes -1 if $1 < $2, 0 if equal, 1 if $1 > $2. Pure bash (no sort -V,
# which BSD/macOS sort lacks).
compare_versions() {
    local a b i av bv
    a=$(normalize_version "$1")
    b=$(normalize_version "$2")
    local IFS='.'
    # shellcheck disable=SC2206
    local a_parts=($a) b_parts=($b)
    for ((i = 0; i < 3; i++)); do
        av=${a_parts[$i]:-0}
        bv=${b_parts[$i]:-0}
        # Force base-10 (avoid octal interpretation of 08/09).
        av=$((10#$av)); bv=$((10#$bv))
        if ((av < bv)); then echo -1; return 0; fi
        if ((av > bv)); then echo 1; return 0; fi
    done
    echo 0
    return 0
}

# Installed turya version (bare number, e.g. 0.1.0), or empty if absent.
installed_version() {
    if ! command -v "$BINARY_NAME" &> /dev/null; then
        echo ""
        return 1
    fi
    local reported
    reported=$("$BINARY_NAME" --version 2>/dev/null | awk '{print $2}') || true
    if [[ "$reported" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
        echo "$reported"
        return 0
    fi
    echo ""
    return 1
}

# Resolve which version to install: explicit pin wins, otherwise latest.
resolve_target_version() {
    if [ -n "$PINNED_VERSION" ]; then
        if [[ "$PINNED_VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+ ]]; then
            echo "$PINNED_VERSION"
            return 0
        fi
        error "Invalid --version '${PINNED_VERSION}'. Expected form: v0.1.0"
    fi
    local latest
    if latest=$(fetch_latest_tag); then
        echo "$latest"
        return 0
    fi
    error "Could not determine the latest release (network/API issue? no releases published yet?). Pin one explicitly: install.sh --version v0.1.0"
}

fetch_release_metadata() {
    local version="$1"
    local url="https://api.github.com/repos/${REPO}/releases/tags/${version}"

    if command -v curl &> /dev/null; then
        if [ -n "$GITHUB_TOKEN" ]; then
            curl -fsSL \
                --connect-timeout "${CONNECT_TIMEOUT_SECONDS}" \
                --max-time "${DOWNLOAD_TIMEOUT_SECONDS}" \
                -H "Authorization: Bearer ${GITHUB_TOKEN}" \
                "$url" \
                2>/dev/null
        else
            curl -fsSL \
                --connect-timeout "${CONNECT_TIMEOUT_SECONDS}" \
                --max-time "${DOWNLOAD_TIMEOUT_SECONDS}" \
                "$url" \
                2>/dev/null
        fi
    else
        return 1
    fi
}

resolve_release_asset_id() {
    local version="$1"
    local asset_name="$2"
    local metadata

    metadata=$(fetch_release_metadata "$version") || return 1

    if command -v python3 &> /dev/null; then
        echo "$metadata" | python3 -c '
import json, sys
name = sys.argv[1]
data = json.load(sys.stdin)
for asset in data.get("assets", []):
    if asset.get("name") == name:
        print(asset.get("id", ""))
        break
' "$asset_name"
        return 0
    fi

    if command -v jq &> /dev/null; then
        echo "$metadata" | jq -r --arg name "$asset_name" '.assets[] | select(.name == $name) | .id' | head -n 1
        return 0
    fi

    # Minimal fallback parser when python3/jq are unavailable.
    echo "$metadata" | sed -n "/\"name\": \"${asset_name//\//\\/}\"/,/\"id\":/p" | grep -m1 '"id":' | sed -E 's/.*"id":[[:space:]]*([0-9]+).*/\1/'
}

get_release_asset_url() {
    local version="$1"
    local asset_name="$2"

    # For private repos, prefer GitHub API asset downloads with token.
    if [ -n "$GITHUB_TOKEN" ]; then
        local asset_id
        asset_id=$(resolve_release_asset_id "$version" "$asset_name")
        if [ -n "$asset_id" ]; then
            echo "https://api.github.com/repos/${REPO}/releases/assets/${asset_id}"
            return 0
        fi
    fi

    get_download_url "$version" "$asset_name"
}

# Get the checksum URL for a binary
get_checksum_url() {
    local binary_url="$1"
    echo "${binary_url}.sha256"
}

# Verify SHA256 checksum
verify_checksum() {
    local file="$1"
    local expected_checksum="$2"
    local actual_checksum

    if command -v sha256sum &> /dev/null; then
        actual_checksum=$(sha256sum "$file" | cut -d ' ' -f 1)
    elif command -v shasum &> /dev/null; then
        actual_checksum=$(shasum -a 256 "$file" | cut -d ' ' -f 1)
    else
        warn "Neither sha256sum nor shasum found. Skipping verification."
        return 0
    fi

    if [ "$actual_checksum" = "$expected_checksum" ]; then
        return 0
    else
        return 1
    fi
}

# Download checksum file and extract the hash
download_checksum() {
    local checksum_url="$1"
    local tmp_dir="$2"
    local checksum

    if download_with_auth_retry "$checksum_url" "${tmp_dir}/checksum.sha256"; then
        # Extract just the hash (first field)
        checksum=$(cut -d ' ' -f 1 "${tmp_dir}/checksum.sha256")
        echo "$checksum"
    else
        echo ""
    fi
}

# Attempt to download a version
try_download() {
    local version="$1"
    local platform="$2"
    local tmp_dir="$3"
    local asset_name checksum_asset_name
    asset_name=$(get_asset_name "$platform")
    checksum_asset_name="${asset_name}.sha256"

    info "Attempting to download version ${version}..."
    local download_url
    download_url=$(get_release_asset_url "$version" "$asset_name")
    info "Download URL: ${download_url}"

    # Download checksum first
    local checksum_url expected_checksum
    if [ "$SKIP_VERIFY" != "true" ]; then
        security "Downloading checksum for verification..."
        checksum_url=$(get_release_asset_url "$version" "$checksum_asset_name")
        expected_checksum=$(download_checksum "$checksum_url" "$tmp_dir")

        if [ -n "$expected_checksum" ]; then
            security "Expected SHA256: ${expected_checksum}"
        else
            warn "Could not download checksum file for version ${version}. Binary verification will be skipped."
        fi
    else
        warn "Checksum verification skipped (SKIP_VERIFY=true)"
    fi

    info "Downloading ${BINARY_NAME}..."
    if ! download_with_auth_retry "$download_url" "${tmp_dir}/${BINARY_NAME}"; then
        warn "Failed to download version ${version}."
        diagnose_download_failure "$download_url"
        if [ -z "$GITHUB_TOKEN" ]; then
            warn "If this repository is private, set GITHUB_TOKEN, use --prompt-token, or use --token-stdin."
        fi
        return 1
    fi

    # Verify checksum
    if [ "$SKIP_VERIFY" != "true" ] && [ -n "$expected_checksum" ]; then
        security "Verifying binary integrity..."
        if verify_checksum "${tmp_dir}/${BINARY_NAME}" "$expected_checksum"; then
            success "Checksum verified! Binary is authentic."
        else
            error "SECURITY ALERT: Checksum verification FAILED for version ${version}!"
            return 1
        fi
    fi

    success "Successfully downloaded version ${version}."
    echo "$version"
    return 0
}

# Download and install (doubles as updater: skips when already current).
install() {
    local platform tmp_dir installed_version target_version current cmp

    info "Detecting platform..."
    platform=$(detect_platform)
    info "Platform: ${platform}"

    # If caller explicitly asked for token mode, get it before network calls.
    if [ "$PROMPT_FOR_TOKEN" = "true" ] || [ "$TOKEN_FROM_STDIN" = "true" ]; then
        read_github_token || error "Token mode requested, but no GitHub token was provided."
    fi

    # Fail fast for invalid token/access issues before asset downloads.
    validate_github_token_access

    # Resolve which version to install: explicit pin wins, else latest tag.
    target_version=$(resolve_target_version)
    info "Target version: ${target_version}"

    # Update behavior: skip when already current (unless pinned or forced).
    current=$(installed_version || true)
    if [ -z "$PINNED_VERSION" ] && [ -n "$current" ]; then
        cmp=$(compare_versions "$current" "$(normalize_version "$target_version")")
        if [ "$cmp" = "0" ] && [ "$FORCE_REINSTALL" != "true" ]; then
            success "turya ${current} is already up to date (latest: ${target_version}). Use --force to reinstall."
            exit 0
        fi
        if [ "$cmp" = "1" ]; then
            warn "Installed turya ${current} is newer than latest release ${target_version}; leaving it in place. Use --force to reinstall."
            exit 0
        fi
        info "Updating turya ${current} -> ${target_version}..."
    elif [ -n "$current" ]; then
        info "Installed version: ${current}; (re)installing ${target_version}..."
    fi

    # Create temp directory
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

    # Single attempt against the resolved version; failures diagnose clearly.
    if installed_version_raw=$(try_download "$target_version" "$platform" "$tmp_dir"); then
        installed_version=$(echo "$installed_version_raw" | tail -n 1)
    else
        error "Failed to download ${target_version} for ${platform}. Aborting."
    fi

    # Make executable
    chmod +x "${tmp_dir}/${BINARY_NAME}"

    # Install
    info "Installing to ${INSTALL_DIR}..."
    if [ -w "$INSTALL_DIR" ]; then
        mv "${tmp_dir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    else
        warn "Need sudo to install to ${INSTALL_DIR}"
        sudo mv "${tmp_dir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    fi

    # Verify installation
    if command -v "$BINARY_NAME" &> /dev/null; then
        success "turya installed successfully!"
        info "Installed Version: ${installed_version}"
        actual_version=$($BINARY_NAME --version)
        info "Reported Version: ${actual_version}"

        local installed_version_for_compare
        installed_version_for_compare=$(normalize_version "$installed_version")
        if [ "$installed_version_for_compare" != "$(echo "$actual_version" | awk '{print $2}')" ]; then
             warn "Installed version (${installed_version}) does not match reported version (${actual_version})."
        fi

        if [ "$SKIP_VERIFY" != "true" ]; then
            security "Installation verified with SHA256 checksum"
        fi

        echo ""
        info "Next steps:"
        echo "  1. Set your API key:"
        echo "     export ANTHROPIC_API_KEY='sk-ant-...'"
        echo ""
        echo "  2. Run turya:"
        echo "     turya"
        echo ""
        if [[ "$platform" == darwin* ]]; then
            warn "macOS Gatekeeper: binaries downloaded outside the App Store may be quarantined."
            echo "  If macOS refuses to run turya, clear the quarantine flag:"
            echo "     xattr -d com.apple.quarantine ${INSTALL_DIR}/${BINARY_NAME}"
        fi
    else
        warn "Installation completed, but ${BINARY_NAME} not found in PATH"
        info "You may need to add ${INSTALL_DIR} to your PATH"
    fi
}

# Report latest vs installed versions without changing anything.
check_update() {
    local target_version current cmp
    target_version=$(resolve_target_version)
    current=$(installed_version || true)
    if [ -z "$current" ]; then
        info "turya is not installed. Latest release: ${target_version}."
        exit 0
    fi
    cmp=$(compare_versions "$current" "$(normalize_version "$target_version")")
    if [ "$cmp" = "0" ]; then
        success "turya ${current} is up to date (latest: ${target_version})."
    elif [ "$cmp" = "1" ]; then
        info "Installed turya ${current} is newer than latest release ${target_version}."
    else
        info "Update available: turya ${current} -> ${target_version}. Re-run without --check to install."
    fi
    exit 0
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version|-v)
            if [ -z "${2:-}" ]; then
                error "--version needs a value (e.g. --version v0.1.0)"
            fi
            PINNED_VERSION="$2"
            shift 2
            ;;
        --force|-f)
            FORCE_REINSTALL="true"
            shift
            ;;
        --check|-c)
            CHECK_ONLY="true"
            shift
            ;;
        --install-dir|-d)
            INSTALL_DIR="$2"
            shift 2
            ;;
        --skip-verify)
            SKIP_VERIFY="true"
            shift
            ;;
        --prompt-token)
            PROMPT_FOR_TOKEN="true"
            shift
            ;;
        --token-stdin)
            TOKEN_FROM_STDIN="true"
            shift
            ;;
        --help|-h)
            echo "turya installer and updater"
            echo ""
            echo "Usage: install.sh [OPTIONS]"
            echo ""
            echo "Without options, installs the latest release, or updates an"
            echo "existing installation when a newer release is available."
            echo ""
            echo "Options:"
            echo "  -v, --version VERSION   Install a specific version (default: latest release)"
            echo "  -f, --force             Reinstall even when already up to date"
            echo "  -c, --check             Report latest vs installed versions, change nothing"
            echo "  -d, --install-dir DIR   Installation directory (default: /usr/local/bin)"
            echo "  --skip-verify           Skip SHA256 checksum verification (not recommended)"
            echo "  --prompt-token          Prompt securely for GitHub token (for private repos)"
            echo "  --token-stdin           Read GitHub token from stdin (first line)"
            echo "  -h, --help              Show this help"
            echo ""
            echo "Security:"
            echo "  This installer verifies SHA256 checksums to ensure binary integrity."
            echo "  If verification fails, installation is aborted for your protection."
            echo "  For private repos, prefer GITHUB_TOKEN env var or --prompt-token."
            echo ""
            echo "Network tuning:"
            echo "  CONNECT_TIMEOUT_SECONDS   Connect timeout per request (default: 15)"
            echo "  DOWNLOAD_TIMEOUT_SECONDS  Total request timeout (default: 120)"
            echo ""
            echo "Examples:"
            echo "  curl -fsSL https://raw.githubusercontent.com/thoughtoinnovate/turya/main/install.sh | bash"
            echo "  ./install.sh --check"
            echo "  ./install.sh --version v0.1.0"
            echo "  GITHUB_TOKEN=ghp_xxx ./install.sh"
            echo "  ./install.sh --prompt-token"
            echo "  printf '%s\n' \"\$GITHUB_TOKEN\" | ./install.sh --token-stdin"
            echo "  ./install.sh --install-dir ~/.local/bin"
            exit 0
            ;;
        *)
            error "Unknown option: $1"
            ;;
    esac
done

# Run check or installation
if [ "$CHECK_ONLY" = "true" ]; then
    check_update
fi
install
