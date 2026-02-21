// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "PhotoFerrySwift",
    platforms: [.macOS(.v13)],
    products: [
        .library(
            name: "PhotoFerrySwift",
            type: .static,
            targets: ["PhotoFerrySwift"]
        ),
    ],
    dependencies: [
        .package(url: "https://github.com/Brendonovich/swift-rs", from: "1.0.7"),
    ],
    targets: [
        .target(
            name: "PhotoFerrySwift",
            dependencies: [
                .product(name: "SwiftRs", package: "swift-rs"),
            ],
            path: "Sources"
        ),
    ]
)
