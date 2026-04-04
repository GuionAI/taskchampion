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
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.37/TaskChampionFFIFFI.xcframework.zip",
            checksum: "4c0e575639bc612f5a1c2758642836c7b58bff56bd26493076b8659faca8891e"
        ),
    ]
)
