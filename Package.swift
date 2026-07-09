// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "TaskChampionFFI",
    platforms: [
        .iOS(.v14),
        .macOS(.v14),
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
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.56-dynamic/TaskChampionFFIFFI.dynamic.xcframework.zip",
            checksum: "5426de7ffe1bbb61e41e3aba8e53ca3c45bd2f0688951c531a9c241870e2f6d0"
        ),
    ]
)
