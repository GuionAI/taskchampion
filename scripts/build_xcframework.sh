#!/usr/bin/env bash
#
# Build an XCFramework containing the taskchampion-ffi static library
# for iOS device + simulator targets, plus generate Swift bindings.
#
# Prerequisites:
#   - Rust toolchain (stable)
#   - Xcode command-line tools
#   - iOS Rust targets (script installs if missing)
#
# Usage:
#   ./scripts/build_xcframework.sh
#
# Outputs:
#   TaskChampionFFIFFI.xcframework/  — XCFramework with static libs + headers
#   Sources/TaskChampionFFI/         — Generated Swift bindings
#
# Notes:
#   - The crate declares crate-type = ["cdylib", "staticlib", "rlib"]. Cargo
#     builds all three for each target. The cdylib (.dylib) output is unused —
#     only the staticlib (.a) goes into the XCFramework. Linker warnings about
#     the cdylib are expected and harmless.
#   - The XCFramework and C module are named TaskChampionFFIFFI — derived from
#     uniffi.toml module_name = "TaskChampionFFI" plus the "FFI" suffix that
#     UniFFI appends to all C-layer artifacts.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BUILD_DIR="${PROJECT_ROOT}/build"
XCFRAMEWORK_NAME="TaskChampionFFIFFI"
XCFRAMEWORK_DIR="${PROJECT_ROOT}/${XCFRAMEWORK_NAME}.xcframework"
SWIFT_OUT_DIR="${PROJECT_ROOT}/Sources/TaskChampionFFI"

# iOS targets
TARGETS=(
  aarch64-apple-ios
  aarch64-apple-ios-sim
  x86_64-apple-ios
)

# --- Ensure Rust targets are installed ---

echo "==> Checking Rust targets..."
for target in "${TARGETS[@]}"; do
  if ! rustup target list --installed | grep -q "^${target}$"; then
    echo "    Installing ${target}..."
    rustup target add "${target}"
  fi
done

# --- Build static libraries ---

echo "==> Building static libraries..."
for target in "${TARGETS[@]}"; do
  echo "    Building for ${target}..."
  cargo build \
    -p taskchampion-ffi \
    --lib \
    --release \
    --target "${target}" \
    --manifest-path "${PROJECT_ROOT}/Cargo.toml"
done

# --- Generate Swift bindings ---

echo "==> Generating Swift bindings..."
# Build host library for binding generation — uniffi-bindgen reads UniFFI
# metadata embedded in the compiled library. We use --release so the output
# lands in target/release/ alongside the cross-compiled targets, keeping the
# HOST_LIB path simple. Architecture doesn't matter for metadata extraction.
cargo build -p taskchampion-ffi --lib --release --manifest-path "${PROJECT_ROOT}/Cargo.toml"

HOST_LIB="${PROJECT_ROOT}/target/release/libtaskchampion_ffi.a"
if [ ! -f "${HOST_LIB}" ]; then
  # Fallback: try dylib
  HOST_LIB="${PROJECT_ROOT}/target/release/libtaskchampion_ffi.dylib"
fi

mkdir -p "${SWIFT_OUT_DIR}"
# uniffi-bindgen is compiled in debug mode (no --release) — it only reads
# metadata from the library, not architecture-specific code, so release
# optimisation would add build time with no benefit.
cargo run \
  -p taskchampion-ffi \
  --bin uniffi-bindgen \
  --manifest-path "${PROJECT_ROOT}/Cargo.toml" \
  -- generate \
  --library "${HOST_LIB}" \
  --language swift \
  --out-dir "${BUILD_DIR}/generated"

# Move Swift source to Sources/ directory (SPM target)
cp "${BUILD_DIR}/generated/TaskChampionFFI.swift" "${SWIFT_OUT_DIR}/TaskChampionFFI.swift"

# --- Prepare headers for XCFramework ---

echo "==> Preparing headers..."
HEADERS_DIR="${BUILD_DIR}/headers"
mkdir -p "${HEADERS_DIR}"
cp "${BUILD_DIR}/generated/${XCFRAMEWORK_NAME}.h" "${HEADERS_DIR}/${XCFRAMEWORK_NAME}.h"

# UniFFI generates a modulemap, but xcodebuild needs it named module.modulemap
cp "${BUILD_DIR}/generated/${XCFRAMEWORK_NAME}.modulemap" "${HEADERS_DIR}/module.modulemap"

# --- Create fat simulator library ---

echo "==> Creating fat simulator library (arm64 + x86_64)..."
mkdir -p "${BUILD_DIR}/ios-simulator"
lipo \
  "${PROJECT_ROOT}/target/aarch64-apple-ios-sim/release/libtaskchampion_ffi.a" \
  "${PROJECT_ROOT}/target/x86_64-apple-ios/release/libtaskchampion_ffi.a" \
  -create \
  -output "${BUILD_DIR}/ios-simulator/libtaskchampion_ffi.a"

# --- Create XCFramework ---

echo "==> Creating XCFramework..."
rm -rf "${XCFRAMEWORK_DIR}"
xcodebuild -create-xcframework \
  -library "${PROJECT_ROOT}/target/aarch64-apple-ios/release/libtaskchampion_ffi.a" \
  -headers "${HEADERS_DIR}" \
  -library "${BUILD_DIR}/ios-simulator/libtaskchampion_ffi.a" \
  -headers "${HEADERS_DIR}" \
  -output "${XCFRAMEWORK_DIR}"

# --- Cleanup ---

rm -rf "${BUILD_DIR}"

echo ""
echo "==> Done!"
echo "    XCFramework: ${XCFRAMEWORK_DIR}"
echo "    Swift sources: ${SWIFT_OUT_DIR}/TaskChampionFFI.swift"
echo ""
echo "    Add to fn-ios as a local SPM package pointing to this repo root."
