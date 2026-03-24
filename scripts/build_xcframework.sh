#!/usr/bin/env bash
#
# Build an XCFramework containing the taskchampion-ffi static library
# for iOS device + simulator (arm64) targets, plus generate Swift bindings.
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

echo "==> Building static libraries (parallel)..."
pids=()
for target in "${TARGETS[@]}"; do
  echo "    Spawning build for ${target}..."
  cargo build \
    -p taskchampion-ffi \
    --lib \
    --release \
    --target "${target}" \
    --manifest-path "${PROJECT_ROOT}/Cargo.toml" &
  pids+=($!)
done
for pid in "${pids[@]}"; do
  wait "$pid"
done

# --- Generate Swift bindings ---

echo "==> Generating Swift bindings..."
# uniffi-bindgen reads type metadata from the compiled library — architecture
# doesn't matter, so we reuse the already-built iOS device lib instead of
# compiling a redundant host-native build.
METADATA_LIB="${PROJECT_ROOT}/target/aarch64-apple-ios/release/libtaskchampion_ffi.a"
if [ ! -f "${METADATA_LIB}" ]; then
  echo "ERROR: iOS device lib not found at ${METADATA_LIB} — did the cargo build step fail?" >&2
  exit 1
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
  --library "${METADATA_LIB}" \
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

# --- Prepare simulator library ---

echo "==> Preparing simulator library..."
mkdir -p "${BUILD_DIR}/ios-simulator"
cp "${PROJECT_ROOT}/target/aarch64-apple-ios-sim/release/libtaskchampion_ffi.a" \
   "${BUILD_DIR}/ios-simulator/libtaskchampion_ffi.a"

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
echo "    Tag a version and push to create a GitHub Release. SPM consumers add:
    https://github.com/tta-lab/taskchampion.git"
