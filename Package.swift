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
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.35/TaskChampionFFIFFI.xcframework.zip",
            checksum: "033ebd1a44793e9b79d0bdea0e489fdf6c0744cecb868864aef35a183b221b2a"
        ),
    ]
)
