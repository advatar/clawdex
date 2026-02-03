\
#!/bin/bash
set -euo pipefail

# -----------------------------------------------------------------------------
# Build and embed Rust tools into the macOS app bundle.
#
# Expected layout (adjust as needed):
#   repo/
#     codex/              (upstream codex cli source)
#     codex-clawd/        (your clawd daemon / MCP server)
#     apps/macos/ClawdApp (this project)
#
# If you're using a single Cargo workspace at repo root, set REPO_ROOT accordingly.
# -----------------------------------------------------------------------------

REPO_ROOT="${REPO_ROOT:-${SRCROOT}/..}"
CARGO_BIN="${CARGO_BIN:-cargo}"

# The Cargo package names to build.
# Adjust these to match your workspace (e.g. "codex-cli" instead of "codex").
RUST_PACKAGES=(${RUST_PACKAGES:-codex codex-clawd})

# The final executable names produced by cargo (often same as package name).
# If your binary name differs from package name, update this list accordingly.
RUST_BINARIES=(${RUST_BINARIES:-codex codex-clawd})

# Where to place intermediate artifacts
ARTIFACT_DIR="${SRCROOT}/BuildArtifacts"
UNIVERSAL_DIR="${ARTIFACT_DIR}/universal"
mkdir -p "${UNIVERSAL_DIR}"

# Build per-arch
ARCH_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
for TARGET in "${ARCH_TARGETS[@]}"; do
  echo "[clawd] Building Rust packages for ${TARGET}..."
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
    echo "[clawd][error] Missing ${a} (check your Cargo package/bin names)"
    exit 1
  fi
  if [[ ! -f "${x}" ]]; then
    echo "[clawd][error] Missing ${x} (check your Cargo package/bin names)"
    exit 1
  fi

  echo "[clawd] Lipo ${bin} -> universal2"
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
  echo "[clawd] Codesigning embedded tools..."
  for BIN in "${RUST_BINARIES[@]}"; do
    /usr/bin/codesign --force \
      --sign "${EXPANDED_CODE_SIGN_IDENTITY}" \
      --entitlements "${SRCROOT}/Resources/HelperTool.entitlements" \
      --options runtime \
      --timestamp \
      "${BIN_DIR}/${BIN}"
  done
else
  echo "[clawd] EXPANDED_CODE_SIGN_IDENTITY not set; skipping embedded-tool codesign (Debug?)"
fi

echo "[clawd] Done."
