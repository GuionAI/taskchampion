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
            dependencies: ["TaskChampionCore"],
            path: "Sources/TaskChampionFFI"
        ),
        // Pre-built framework + C headers. Tags containing "dynamic" use dynamic linkage;
        // other release tags use static linkage.
        // Downloaded from GitHub Releases. The release workflow auto-updates url + checksum.
        // NOTE: Placeholder values below — auto-replaced by release workflow on each tag push.
        // Do NOT use the main branch as an SPM dependency; always pin to a tagged version.
        .binaryTarget(
            name: "TaskChampionCore",
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.56-dynamic/TaskChampionCore.xcframework.zip",
            checksum: "00ac3dfe70a706d7df44abc249aa08e42e24b11dec630dca54f6581d4a2d3279"
        ),
    ]
)
