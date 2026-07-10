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
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.58-snapshot.20260710035218.2c24303/TaskChampionCore.xcframework.zip",
            checksum: "c0be56918a5bce1cfff5eaefa29dc0b8efc49c866a5aa27961b0142b362c4b37"
        ),
    ]
)
