// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "TaskChampionFFI",
    platforms: [
        .iOS(.v13),
    ],
    products: [
        .library(
            name: "TaskChampionFFI",
            targets: ["TaskChampionFFI"]
        ),
    ],
    targets: [
        // Generated Swift bindings that call into the C FFI layer
        .target(
            name: "TaskChampionFFI",
            dependencies: ["TaskChampionFFIFFI"],
            path: "Sources/TaskChampionFFI"
        ),
        // Pre-built static library + C headers.
        // Downloaded from GitHub Releases. The release workflow auto-updates url + checksum.
        // NOTE: Placeholder values below — auto-replaced by release workflow on each tag push.
        // Do NOT use the main branch as an SPM dependency; always pin to a tagged version.
        .binaryTarget(
            name: "TaskChampionFFIFFI",
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.21/TaskChampionFFIFFI.xcframework.zip",
            checksum: "5a2b67d012343fdb3c26e3f98b54e18376a52c620259303599584299c77460c4"
        ),
    ]
)
