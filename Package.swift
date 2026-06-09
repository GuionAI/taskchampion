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
            url: "https://github.com/GuionAI/taskchampion/releases/download/v3.0.2-guion.55/TaskChampionFFIFFI.xcframework.zip",
            checksum: "588b8e88f2076c3d1dc02a4d802c46f539d28cc9428decc9bf7efcd68e6b5366"
        ),
    ]
)
