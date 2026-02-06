#!/bin/bash
set -euo pipefail

# -----------------------------------------------------------------------------
# Build and embed Codex + Clawdex Rust tools into the macOS app bundle.
#
# Expected layout (adjust as needed):
#   repo/
#     codex/              (upstream codex cli source)
#     clawdex/            (Rust clawdex CLI)
#     macClawdex/         (this project)
#
# Supports:
# - Codex: Rust build (default) or a prebuilt CODEX_BIN.
# - Clawdex: Rust build (default) or a prebuilt CLAWDEX_BIN.
# -----------------------------------------------------------------------------

REPO_ROOT="${REPO_ROOT:-${SRCROOT}/..}"
CARGO_BIN="${CARGO_BIN:-cargo}"

CODEX_CARGO_ROOT="${CODEX_CARGO_ROOT:-${REPO_ROOT}/codex/codex-rs}"
CODEX_PACKAGE="${CODEX_PACKAGE:-codex-cli}"
CODEX_BINARY="${CODEX_BINARY:-codex}"
CODEX_BIN="${CODEX_BIN:-}"

CLAWDEX_CARGO_ROOT="${CLAWDEX_CARGO_ROOT:-${REPO_ROOT}/clawdex}"
CLAWDEX_PACKAGE="${CLAWDEX_PACKAGE:-clawdex}"
CLAWDEX_BINARY="${CLAWDEX_BINARY:-clawdex}"
CLAWDEX_BIN="${CLAWDEX_BIN:-}"
CLAWDEXD_BINARY="${CLAWDEXD_BINARY:-clawdexd}"
CLAWDEXD_BIN="${CLAWDEXD_BIN:-}"
PREBUILT_DIR="${PREBUILT_DIR:-${SRCROOT}/Resources/prebuilt}"

if [[ "${SKIP_RUST_EMBED:-}" == "1" || "${SKIP_TOOLS_EMBED:-}" == "1" ]]; then
  echo "[clawdex] SKIP_RUST_EMBED=1 or SKIP_TOOLS_EMBED=1; skipping tool build/embed."
  exit 0
fi

is_macho() {
  /usr/bin/file "$1" | /usr/bin/grep -q "Mach-O"
}

log() {
  echo "[clawdex] $*"
}

die() {
  echo "[clawdex][error] $*" >&2
  exit 1
}

# Where to place intermediate artifacts
ARTIFACT_DIR="${SRCROOT}/BuildArtifacts"
BIN_STAGE_DIR="${ARTIFACT_DIR}/bin"
UNIVERSAL_DIR="${ARTIFACT_DIR}/universal"
/bin/rm -rf "${BIN_STAGE_DIR}" "${UNIVERSAL_DIR}"
/bin/mkdir -p "${BIN_STAGE_DIR}" "${UNIVERSAL_DIR}"

CARGO_AVAILABLE=1
if ! command -v "${CARGO_BIN}" >/dev/null 2>&1; then
  CARGO_AVAILABLE=0
fi

make_universal() {
  local target_dir="$1"
  local bin="$2"
  local out="$3"

  local a="${target_dir}/aarch64-apple-darwin/release/${bin}"
  local x="${target_dir}/x86_64-apple-darwin/release/${bin}"

  if [[ ! -f "${a}" ]]; then
    die "Missing ${a} (check your Cargo package/bin names)"
  fi
  if [[ ! -f "${x}" ]]; then
    die "Missing ${x} (check your Cargo package/bin names)"
  fi

  log "Lipo ${bin} -> universal2"
  /usr/bin/lipo -create "${a}" "${x}" -output "${out}"
  /bin/chmod +x "${out}"
}

build_cargo_target() {
  local cargo_root="$1"
  local package="$2"
  local target="$3"
  local bin_name="${4:-}"

  if [[ ! -f "${cargo_root}/Cargo.toml" ]]; then
    die "Missing Cargo.toml in ${cargo_root}"
  fi
  if [[ "${CARGO_AVAILABLE}" != "1" ]]; then
    die "cargo not found. Install Rust or set CODEX_BIN/CLAWDEX_BIN or place prebuilt binaries under ${PREBUILT_DIR}."
  fi

  pushd "${cargo_root}" >/dev/null
  local args=(build --release --target "${target}")
  if [[ -n "${bin_name}" ]]; then
    args+=(--bin "${bin_name}")
  fi
  if /usr/bin/grep -q "\[workspace\]" Cargo.toml; then
    "${CARGO_BIN}" "${args[@]}" -p "${package}"
  else
    "${CARGO_BIN}" "${args[@]}"
  fi
  popd >/dev/null
}

resolve_prebuilt_bin() {
  local name="$1"
  local binary="$2"
  local current="$3"
  local resolved="$current"

  if [[ -n "${resolved}" ]]; then
    echo "${resolved}"
    return 0
  fi

  local candidate
  candidate="${PREBUILT_DIR}/${binary}"
  if [[ -x "${candidate}" ]]; then
    echo "[clawdex] Using prebuilt ${name} at ${candidate}" >&2
    echo "${candidate}"
    return 0
  fi

  candidate="${SRCROOT}/Resources/bin/${binary}"
  if [[ -x "${candidate}" ]]; then
    echo "[clawdex] Using embedded ${name} at ${candidate}" >&2
    echo "${candidate}"
    return 0
  fi

  echo ""
}

stage_prebuilt() {
  local name="$1"
  local bin_path="$2"
  local dest="$3"

  if [[ ! -x "${bin_path}" ]]; then
    die "${name} binary not executable: ${bin_path}"
  fi
  if ! is_macho "${bin_path}"; then
    die "${name} binary is not a Mach-O. Provide a macOS build."
  fi
  /bin/cp -f "${bin_path}" "${dest}"
  /bin/chmod +x "${dest}"
}

log "Staging Codex..."
CODEX_BIN="$(resolve_prebuilt_bin "Codex" "${CODEX_BINARY}" "${CODEX_BIN}")"
if [[ -n "${CODEX_BIN}" ]]; then
  log "Using codex binary at ${CODEX_BIN}"
  stage_prebuilt "Codex" "${CODEX_BIN}" "${BIN_STAGE_DIR}/${CODEX_BINARY}"
else
  if [[ "${SKIP_CODEX_BUILD:-}" == "1" ]]; then
    die "SKIP_CODEX_BUILD=1 but CODEX_BIN is not set."
  fi
  if [[ "${CARGO_AVAILABLE}" != "1" ]]; then
    die "cargo not found. Set CODEX_BIN or place a prebuilt codex at ${PREBUILT_DIR}/${CODEX_BINARY}."
  fi
  ARCH_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
  for TARGET in "${ARCH_TARGETS[@]}"; do
    log "Building Codex for ${TARGET}..."
    build_cargo_target "${CODEX_CARGO_ROOT}" "${CODEX_PACKAGE}" "${TARGET}"
  done
  make_universal "${CODEX_CARGO_ROOT}/target" "${CODEX_BINARY}" "${UNIVERSAL_DIR}/${CODEX_BINARY}"
  /bin/cp -f "${UNIVERSAL_DIR}/${CODEX_BINARY}" "${BIN_STAGE_DIR}/${CODEX_BINARY}"
fi

log "Staging Clawdex..."
CLAWDEX_BIN="$(resolve_prebuilt_bin "Clawdex" "${CLAWDEX_BINARY}" "${CLAWDEX_BIN}")"
if [[ -n "${CLAWDEX_BIN}" ]]; then
  log "Using clawdex binary at ${CLAWDEX_BIN}"
  stage_prebuilt "Clawdex" "${CLAWDEX_BIN}" "${BIN_STAGE_DIR}/${CLAWDEX_BINARY}"
else
  if [[ "${SKIP_CLAWDEX_BUILD:-}" == "1" ]]; then
    die "SKIP_CLAWDEX_BUILD=1 but CLAWDEX_BIN is not set."
  fi
  if [[ "${CARGO_AVAILABLE}" != "1" ]]; then
    die "cargo not found. Set CLAWDEX_BIN or place a prebuilt clawdex at ${PREBUILT_DIR}/${CLAWDEX_BINARY}."
  fi
  ARCH_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
  for TARGET in "${ARCH_TARGETS[@]}"; do
    log "Building Clawdex for ${TARGET}..."
    build_cargo_target "${CLAWDEX_CARGO_ROOT}" "${CLAWDEX_PACKAGE}" "${TARGET}" "${CLAWDEX_BINARY}"
  done
  make_universal "${CLAWDEX_CARGO_ROOT}/target" "${CLAWDEX_BINARY}" "${UNIVERSAL_DIR}/${CLAWDEX_BINARY}"
  /bin/cp -f "${UNIVERSAL_DIR}/${CLAWDEX_BINARY}" "${BIN_STAGE_DIR}/${CLAWDEX_BINARY}"
fi

log "Staging Clawdexd..."
CLAWDEXD_BIN="$(resolve_prebuilt_bin "Clawdexd" "${CLAWDEXD_BINARY}" "${CLAWDEXD_BIN}")"
if [[ -n "${CLAWDEXD_BIN}" ]]; then
  log "Using clawdexd binary at ${CLAWDEXD_BIN}"
  stage_prebuilt "Clawdexd" "${CLAWDEXD_BIN}" "${BIN_STAGE_DIR}/${CLAWDEXD_BINARY}"
else
  if [[ "${SKIP_CLAWDEX_BUILD:-}" == "1" ]]; then
    die "SKIP_CLAWDEX_BUILD=1 but CLAWDEXD_BIN is not set."
  fi
  if [[ "${CARGO_AVAILABLE}" != "1" ]]; then
    die "cargo not found. Set CLAWDEXD_BIN or place a prebuilt clawdexd at ${PREBUILT_DIR}/${CLAWDEXD_BINARY}."
  fi
  ARCH_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
  for TARGET in "${ARCH_TARGETS[@]}"; do
    log "Building Clawdexd for ${TARGET}..."
    build_cargo_target "${CLAWDEX_CARGO_ROOT}" "${CLAWDEX_PACKAGE}" "${TARGET}" "${CLAWDEXD_BINARY}"
  done
  make_universal "${CLAWDEX_CARGO_ROOT}/target" "${CLAWDEXD_BINARY}" "${UNIVERSAL_DIR}/${CLAWDEXD_BINARY}"
  /bin/cp -f "${UNIVERSAL_DIR}/${CLAWDEXD_BINARY}" "${BIN_STAGE_DIR}/${CLAWDEXD_BINARY}"
fi

# Copy into the app bundle
APP_RES_DIR="${TARGET_BUILD_DIR}/${UNLOCALIZED_RESOURCES_FOLDER_PATH}"
BIN_DIR="${APP_RES_DIR}/bin"
/bin/rm -rf "${BIN_DIR}"
/bin/mkdir -p "${BIN_DIR}"
/usr/bin/rsync -a --delete "${BIN_STAGE_DIR}/" "${BIN_DIR}/"

# Embed OpenClaw extensions for first-run plugin install.
OPENCLAW_EXTENSIONS_SRC="${OPENCLAW_EXTENSIONS_SRC:-${REPO_ROOT}/openclaw/extensions}"
OPENCLAW_EXTENSIONS_DEST="${APP_RES_DIR}/openclaw-extensions"
if [[ -d "${OPENCLAW_EXTENSIONS_SRC}" ]]; then
  /bin/rm -rf "${OPENCLAW_EXTENSIONS_DEST}"
  /bin/mkdir -p "${OPENCLAW_EXTENSIONS_DEST}"
  /usr/bin/rsync -a --delete --exclude "node_modules" --exclude ".DS_Store" \
    "${OPENCLAW_EXTENSIONS_SRC}/" "${OPENCLAW_EXTENSIONS_DEST}/"
  log "Embedded OpenClaw extensions into ${OPENCLAW_EXTENSIONS_DEST}"
else
  log "OpenClaw extensions not found at ${OPENCLAW_EXTENSIONS_SRC}; skipping embed."
fi

# Codesign the embedded executables so they can be executed from a sandboxed app.
# NOTE: For Debug builds you may not have an expanded signing identity; skip in that case.
if [[ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]]; then
  log "Codesigning embedded tools..."
  for BIN in "${BIN_DIR}/${CODEX_BINARY}" "${BIN_DIR}/${CLAWDEX_BINARY}" "${BIN_DIR}/${CLAWDEXD_BINARY}"; do
    if [[ -f "${BIN}" ]]; then
      if is_macho "${BIN}"; then
        /usr/bin/codesign --force \
          --sign "${EXPANDED_CODE_SIGN_IDENTITY}" \
          --entitlements "${SRCROOT}/Resources/HelperTool.entitlements" \
          --options runtime \
          --timestamp \
          "${BIN}"
      else
        log "Skipping codesign for non-Mach-O tool: ${BIN}"
      fi
    fi
  done
else
  log "EXPANDED_CODE_SIGN_IDENTITY not set; skipping embedded-tool codesign (Debug?)"
fi

log "Done."
