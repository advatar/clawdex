\
#!/bin/bash
set -euo pipefail

# -----------------------------------------------------------------------------
# Build and embed Rust tools into the macOS app bundle.
#
# Expected layout (adjust as needed):
#   repo/
#     codex/              (upstream codex cli source)
#     clawdex/        (your clawdex daemon / MCP server)
#     macClawdex/        (this project)
#
# If you're using a single Cargo workspace at repo root, set REPO_ROOT accordingly.
# -----------------------------------------------------------------------------

REPO_ROOT="${REPO_ROOT:-${SRCROOT}/..}"
CARGO_BIN="${CARGO_BIN:-cargo}"

if [[ "${SKIP_RUST_EMBED:-}" == "1" ]]; then
  echo "[clawdex] SKIP_RUST_EMBED=1; skipping Rust build/embed."
  exit 0
fi

if [[ ! -f "${REPO_ROOT}/Cargo.toml" ]]; then
  echo "[clawdex][error] Missing Cargo.toml in ${REPO_ROOT}."
  echo "[clawdex][error] Set REPO_ROOT to your Cargo workspace or set SKIP_RUST_EMBED=1 for UI-only builds."
  exit 1
fi

# The Cargo package names to build.
# Adjust these to match your workspace (e.g. "codex-cli" instead of "codex").
RUST_PACKAGES=(${RUST_PACKAGES:-codex clawdex})

# The final executable names produced by cargo (often same as package name).
# If your binary name differs from package name, update this list accordingly.
RUST_BINARIES=(${RUST_BINARIES:-codex clawdex})

# Where to place intermediate artifacts
ARTIFACT_DIR="${SRCROOT}/BuildArtifacts"
UNIVERSAL_DIR="${ARTIFACT_DIR}/universal"
mkdir -p "${UNIVERSAL_DIR}"

# Build per-arch
ARCH_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
for TARGET in "${ARCH_TARGETS[@]}"; do
  echo "[clawdex] Building Rust packages for ${TARGET}..."
  pushd "${REPO_ROOT}" >/dev/null
  for PKG in "${RUST_PACKAGES[@]}"; do
    "${CARGO_BIN}" build --release --target "${TARGET}" -p "${PKG}"
  done
  popd >/dev/null
done

make_universal () {
  local bin="$1"
  local out="${UNIVERSAL_DIR}/${bin}"

  local a="${REPO_ROOT}/target/aarch64-apple-darwin/release/${bin}"
  local x="${REPO_ROOT}/target/x86_64-apple-darwin/release/${bin}"

  if [[ ! -f "${a}" ]]; then
    echo "[clawdex][error] Missing ${a} (check your Cargo package/bin names)"
    exit 1
  fi
  if [[ ! -f "${x}" ]]; then
    echo "[clawdex][error] Missing ${x} (check your Cargo package/bin names)"
    exit 1
  fi

  echo "[clawdex] Lipo ${bin} -> universal2"
  /usr/bin/lipo -create "${a}" "${x}" -output "${out}"
  /bin/chmod +x "${out}"
}

for BIN in "${RUST_BINARIES[@]}"; do
  make_universal "${BIN}"
done

# Copy into the app bundle
APP_RES_DIR="${TARGET_BUILD_DIR}/${UNLOCALIZED_RESOURCES_FOLDER_PATH}"
BIN_DIR="${APP_RES_DIR}/bin"
mkdir -p "${BIN_DIR}"

for BIN in "${RUST_BINARIES[@]}"; do
  /bin/cp -f "${UNIVERSAL_DIR}/${BIN}" "${BIN_DIR}/${BIN}"
  /bin/chmod +x "${BIN_DIR}/${BIN}"
done

# Codesign the embedded executables so they can be executed from a sandboxed app.
# NOTE: For Debug builds you may not have an expanded signing identity; skip in that case.
if [[ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]]; then
  echo "[clawdex] Codesigning embedded tools..."
  for BIN in "${RUST_BINARIES[@]}"; do
    /usr/bin/codesign --force \
      --sign "${EXPANDED_CODE_SIGN_IDENTITY}" \
      --entitlements "${SRCROOT}/Resources/HelperTool.entitlements" \
      --options runtime \
      --timestamp \
      "${BIN_DIR}/${BIN}"
  done
else
  echo "[clawdex] EXPANDED_CODE_SIGN_IDENTITY not set; skipping embedded-tool codesign (Debug?)"
fi

echo "[clawdex] Done."
