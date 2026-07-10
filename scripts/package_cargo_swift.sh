#!/usr/bin/env bash
#
# Build the Swift package and TaskChampionCore XCFramework with cargo-swift.
#
# This script is intended for macOS. It keeps the checked-in ffi crate unchanged
# and patches a temporary copy so cargo-swift can package this workspace layout.
#
# Usage:
#   ./scripts/package_cargo_swift.sh [output-dir]
#
# Environment:
#   TASKCHAMPION_FFI_LINKAGE=static|dynamic  Defaults to static.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUTPUT_DIR="${1:-${PROJECT_ROOT}/target/cargo-swift}"
LINKAGE="${TASKCHAMPION_FFI_LINKAGE:-static}"

case "${LINKAGE}" in
  dynamic | static) ;;
  *)
    echo "ERROR: TASKCHAMPION_FFI_LINKAGE must be 'dynamic' or 'static', got '${LINKAGE}'" >&2
    exit 1
    ;;
esac

command -v cargo >/dev/null || { echo "ERROR: cargo is required" >&2; exit 1; }
command -v cargo-swift >/dev/null || { echo "ERROR: cargo-swift is required; install with: cargo install cargo-swift@0.11.1 --locked" >&2; exit 1; }

workdir="$(mktemp -d "${TMPDIR:-/tmp}/taskchampion-ffi.XXXXXX")"
trap 'rm -rf "${workdir}"' EXIT

mkdir -p "${OUTPUT_DIR}"
rm -rf "${OUTPUT_DIR}/TaskChampionFFI"

rsync -a --exclude target "${PROJECT_ROOT}/ffi/" "${workdir}/"

python3 - "${PROJECT_ROOT}" "${workdir}/Cargo.toml" "${workdir}/uniffi.toml" <<'PY'
import sys
from pathlib import Path

repo = Path(sys.argv[1])
cargo_toml = Path(sys.argv[2])
uniffi_toml = Path(sys.argv[3])

content = cargo_toml.read_text()
content = content.replace('path = ".."', f'path = "{repo}"')
content = content.replace('path = "../praxis"', f'path = "{repo / "praxis"}"')
content = content.replace(
    'crate-type = ["cdylib", "staticlib", "rlib"]',
    'crate-type = ["lib", "cdylib", "staticlib", "rlib"]',
)
cargo_toml.write_text(content)

content = uniffi_toml.read_text()
content = content.replace(
    'module_name = "TaskChampionFFI"',
    'ffi_module_name = "TaskChampionCore"',
    1,
)
uniffi_toml.write_text(content)
PY

cd "${workdir}"
cat uniffi.toml

cargo swift package \
  -p ios@14 macos@14 \
  -n TaskChampionFFI \
  --release \
  --lib-type "${LINKAGE}" \
  --bundle-identifier com.guion.taskchampion \
  --swift-tools-version 5.9 \
  -y --silent

cp -R TaskChampionFFI "${OUTPUT_DIR}/TaskChampionFFI"
echo "Generated ${OUTPUT_DIR}/TaskChampionFFI"
