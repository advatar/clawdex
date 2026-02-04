#!/bin/bash
set -euo pipefail

# Build a universal2 clawdexd and place it in Resources/prebuilt/.
# Usage:
#   macClawdex/Scripts/build_prebuilt_clawdexd.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRCROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${SRCROOT}/.." && pwd)"

CARGO_BIN="${CARGO_BIN:-cargo}"
CLAWDEX_CARGO_ROOT="${CLAWDEX_CARGO_ROOT:-${REPO_ROOT}/clawdex}"
PREBUILT_DIR="${PREBUILT_DIR:-${SRCROOT}/Resources/prebuilt}"

if ! command -v "${CARGO_BIN}" >/dev/null 2>&1; then
  echo "[clawdex][error] cargo not found. Install Rust or set CARGO_BIN." >&2
  exit 1
fi

if [[ ! -f "${CLAWDEX_CARGO_ROOT}/Cargo.toml" ]]; then
  echo "[clawdex][error] Missing Cargo.toml in ${CLAWDEX_CARGO_ROOT}" >&2
  exit 1
fi

mkdir -p "${PREBUILT_DIR}"

echo "[clawdex] Building clawdexd for aarch64-apple-darwin..."
pushd "${CLAWDEX_CARGO_ROOT}" >/dev/null
"${CARGO_BIN}" build --release --bin clawdexd --target aarch64-apple-darwin

echo "[clawdex] Building clawdexd for x86_64-apple-darwin..."
"${CARGO_BIN}" build --release --bin clawdexd --target x86_64-apple-darwin
popd >/dev/null

UNIVERSAL_BIN="${PREBUILT_DIR}/clawdexd"
echo "[clawdex] Creating universal2 clawdexd at ${UNIVERSAL_BIN}..."
/usr/bin/lipo -create \
  "${CLAWDEX_CARGO_ROOT}/target/aarch64-apple-darwin/release/clawdexd" \
  "${CLAWDEX_CARGO_ROOT}/target/x86_64-apple-darwin/release/clawdexd" \
  -output "${UNIVERSAL_BIN}"

/bin/chmod +x "${UNIVERSAL_BIN}"
/usr/bin/file "${UNIVERSAL_BIN}"

echo "[clawdex] Done."
