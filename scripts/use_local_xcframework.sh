#!/usr/bin/env bash
#
# Point Package.swift at a locally built XCFramework.
#
# Run this after ./scripts/build_xcframework.sh when testing this repo as a
# local Swift package in Xcode. Restore the release URL with:
#
#   git restore Package.swift
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
XCFRAMEWORK_PATH="${1:-TaskChampionFFIFFI.xcframework}"
PACKAGE_SWIFT="${PROJECT_ROOT}/Package.swift"

if [ ! -d "${PROJECT_ROOT}/${XCFRAMEWORK_PATH}" ]; then
  echo "ERROR: ${XCFRAMEWORK_PATH} does not exist. Run ./scripts/build_xcframework.sh first." >&2
  exit 1
fi

python3 - "${PACKAGE_SWIFT}" "${XCFRAMEWORK_PATH}" <<'PY'
import re
import sys
from pathlib import Path

package_swift = Path(sys.argv[1])
xcframework_path = sys.argv[2]

content = package_swift.read_text()
replacement = f'''.binaryTarget(
            name: "TaskChampionFFIFFI",
            path: "{xcframework_path}"
        )'''

pattern = re.compile(
    r'''\.binaryTarget\(
            name: "TaskChampionFFIFFI",
            url: "[^"]+",
            checksum: "[0-9a-f]{64}"
        \)''',
    re.MULTILINE,
)

content, count = pattern.subn(replacement, content)
if count == 0:
    path_pattern = re.compile(
        r'''\.binaryTarget\(
            name: "TaskChampionFFIFFI",
            path: "[^"]+"
        \)''',
        re.MULTILINE,
    )
    content, count = path_pattern.subn(replacement, content)

if count != 1:
    print(f"ERROR: expected one TaskChampionFFIFFI binaryTarget block, found {count}", file=sys.stderr)
    sys.exit(1)

package_swift.write_text(content)
PY

echo "Package.swift now points at ${XCFRAMEWORK_PATH}"
echo "Restore the release URL with: git restore Package.swift"
