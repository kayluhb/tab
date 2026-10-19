#!/usr/bin/env bash
# Build the app locally and ad-hoc sign it. No Apple Developer ID needed.
#
# Usage: scripts/build-local.sh [extra build-scripts/main.py build args]
#   e.g. scripts/build-local.sh --skip-tests --skip-lints

set -euo pipefail

cd "$(dirname "$0")/.."

missing=()
for tool in cargo rustup pnpm protoc cargo-tauri python3.11; do
  command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if ((${#missing[@]})); then
  echo "Missing tools: ${missing[*]}" >&2
  echo "Run scripts/setup.sh; cargo-tauri needs: cargo +1.79.0 install tauri-cli@1.6.0 --locked" >&2
  exit 1
fi

[ -d .venv ] || python3.11 -m venv .venv
.venv/bin/pip install -q -r build-scripts/requirements.txt

# Build only for this Mac's architecture, which halves the release build.
# Set TAB_BUILD_TARGET_TRIPLE=universal-apple-darwin for a universal build.
# Asks the hardware rather than rustc, which reports x86_64 when installed under Rosetta.
if [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
  native_triple=aarch64-apple-darwin
else
  native_triple=x86_64-apple-darwin
fi
export TAB_BUILD_TARGET_TRIPLE="${TAB_BUILD_TARGET_TRIPLE:-$native_triple}"

.venv/bin/python build-scripts/main.py build "$@"

app_name="$(cd build-scripts && ../.venv/bin/python -c 'import const; print(const.APP_NAME)')"
app="build/${app_name}.app"

# Accessibility permission is tied to the code signature. Ad-hoc signatures
# change on every build, so prefer the stable local certificate from
# scripts/create-signing-cert.sh when it exists.
identity="Tab Local Code Signing"
if security find-identity -p codesigning | grep -qF "\"$identity\""; then
  signed_with="\"$identity\""
else
  identity="-"
  signed_with="ad-hoc (run scripts/create-signing-cert.sh to keep Accessibility across rebuilds)"
fi
# The hardened runtime blocks dylib injection into a process that holds Accessibility
codesign --force --deep --options runtime \
  --entitlements crates/tab_desktop/entitlements.plist --sign "$identity" "$app"
codesign --verify --deep --strict "$app"

echo
echo "Built and signed with $signed_with: $app"
echo "Install: scripts/install-local.sh"
