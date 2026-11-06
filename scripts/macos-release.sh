#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) macOS release of live-mcp with the LiveMCP Ear plugin
# inside, signs it, and notarizes it.
#
# Usage: scripts/macos-release.sh <out-dir>
#
# Environment:
#   SIGN_IDENTITY  codesign identity, e.g. "Developer ID Application: Name (TEAMID)".
#                  Default "-": ad-hoc signing, for testing the build without a certificate.
#   NOTARY_KEY, NOTARY_KEY_ID, NOTARY_ISSUER
#                  App Store Connect API key (path to the .p8 file), its key ID and issuer ID.
#                  Without them the build is not notarized.
#
# The plugin is signed, notarized and stapled before it is embedded, so the copy that
# `live-mcp install` writes is the notarized one. Then the binary is signed and notarized.
set -euo pipefail

out=${1:?usage: $0 <out-dir>}
identity=${SIGN_IDENTITY:--}
targets=(aarch64-apple-darwin x86_64-apple-darwin)
bundle="target/bundled/LiveMCP Ear.vst3"

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
notarizing=false
if [[ -n ${NOTARY_KEY:-} && -n ${NOTARY_KEY_ID:-} && -n ${NOTARY_ISSUER:-} ]]; then
    notarizing=true
fi
if [[ $identity == "-" && $notarizing == true ]]; then
    echo "error: notarization needs a Developer ID identity in SIGN_IDENTITY" >&2
    exit 1
fi

sign() {
    if [[ $identity == "-" ]]; then
        codesign --force --sign - "$1"
    else
        codesign --force --options runtime --timestamp --sign "$identity" "$1"
    fi
    codesign --verify --strict --verbose=1 "$1"
}

notarize() {
    if [[ $notarizing != true ]]; then
        echo "Not notarizing $1 (no NOTARY_KEY, NOTARY_KEY_ID and NOTARY_ISSUER)"
        return
    fi
    local dir zip result id status
    dir=$(mktemp -d)
    zip="$dir/$(basename "$1").zip"
    ditto -c -k --keepParent "$1" "$zip"
    # Usually minutes, but Apple sometimes takes over an hour, especially for a new team.
    result=$(xcrun notarytool submit "$zip" --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" \
        --issuer "$NOTARY_ISSUER" --wait --timeout 3h --output-format json) || {
        echo "Notarization of $1 failed or timed out: $result" >&2
        exit 1
    }
    id=$(python3 -I -c 'import json, sys; print(json.load(sys.stdin)["id"])' <<<"$result")
    status=$(python3 -I -c 'import json, sys; print(json.load(sys.stdin)["status"])' <<<"$result")
    echo "Notarization of $1: $status ($id)"
    if [[ $status != Accepted ]]; then
        xcrun notarytool log "$id" --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" >&2
        exit 1
    fi
    rm -rf "$dir"
}

rustup target add "${targets[@]}"

echo "== LiveMCP Ear $version"
cargo xtask bundle-universal live-mcp-ear --release
lipo -archs "$bundle/Contents/MacOS/LiveMCP Ear"
sign "$bundle"
notarize "$bundle"
if [[ $notarizing == true ]]; then
    xcrun stapler staple "$bundle"
    xcrun stapler validate "$bundle"
fi

echo "== live-mcp $version"
target_args=()
for target in "${targets[@]}"; do
    target_args+=(--target "$target")
done
LIVE_MCP_EAR_BUNDLE="$bundle" cargo build --release --locked -p live-mcp-server "${target_args[@]}"
mkdir -p "$out"
stage=$(mktemp -d)
binaries=()
for target in "${targets[@]}"; do
    binaries+=("target/$target/release/live-mcp")
done
lipo -create "${binaries[@]}" -output "$stage/live-mcp"
lipo -archs "$stage/live-mcp"
sign "$stage/live-mcp"
notarize "$stage/live-mcp"

echo "== Checking the installer"
scratch=$(mktemp -d)
mkdir -p "$scratch/User Library"
"$stage/live-mcp" install --user-library "$scratch/User Library" --vst3-dir "$scratch/VST3" >/dev/null
diff -r "$bundle" "$scratch/VST3/LiveMCP Ear.vst3"
codesign --verify --strict "$scratch/VST3/LiveMCP Ear.vst3"
rm -rf "$scratch"

archive="live-mcp-macos-universal.tar.gz"
tar -czf "$out/$archive" -C "$stage" live-mcp
(cd "$out" && shasum -a 256 "$archive" >"$archive.sha256")
rm -rf "$stage"
echo "== Built $out/$archive"
