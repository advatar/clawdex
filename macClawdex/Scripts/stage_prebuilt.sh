#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREBUILT_DIR="${PREBUILT_DIR:-${ROOT}/Resources/prebuilt}"

CODEX_BIN_DEFAULT="${ROOT}/../codex/codex-rs/target/release/codex"
CLAWDEX_BIN_DEFAULT="${ROOT}/../clawdex/target/release/clawdex"

CODEX_BIN="${CODEX_BIN:-${CODEX_BIN_DEFAULT}}"
CLAWDEX_BIN="${CLAWDEX_BIN:-${CLAWDEX_BIN_DEFAULT}}"

log() {
  echo "[clawdex] $*"
}

die() {
  echo "[clawdex][error] $*" >&2
  exit 1
}

usage() {
  cat <<USAGE
Usage: stage_prebuilt.sh [--codex <path>] [--clawdex <path>] [--prebuilt-dir <dir>]

Copies prebuilt Mach-O binaries into macClawdex/Resources/prebuilt for Xcode builds
that don't have Rust installed.

Defaults:
  codex   = ${CODEX_BIN_DEFAULT}
  clawdex = ${CLAWDEX_BIN_DEFAULT}
  prebuilt-dir = ${PREBUILT_DIR}
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --codex)
      CODEX_BIN="$2"
      shift 2
      ;;
    --clawdex)
      CLAWDEX_BIN="$2"
      shift 2
      ;;
    --prebuilt-dir)
      PREBUILT_DIR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "Unknown argument: $1"
      ;;
  esac
done

is_macho() {
  /usr/bin/file "$1" | /usr/bin/grep -q "Mach-O"
}

stage_one() {
  local name="$1"
  local src="$2"
  local dest="$3"

  if [[ ! -f "${src}" ]]; then
    die "${name} binary not found at ${src}"
  fi
  if [[ ! -x "${src}" ]]; then
    die "${name} binary is not executable: ${src}"
  fi
  if ! is_macho "${src}"; then
    die "${name} binary is not a Mach-O. Build a macOS binary first."
  fi

  /bin/mkdir -p "${PREBUILT_DIR}"
  /bin/cp -f "${src}" "${dest}"
  /bin/chmod +x "${dest}"
  log "Staged ${name} -> ${dest}"
}

stage_one "codex" "${CODEX_BIN}" "${PREBUILT_DIR}/codex"
stage_one "clawdex" "${CLAWDEX_BIN}" "${PREBUILT_DIR}/clawdex"

log "Done."
