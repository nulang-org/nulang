// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "NulangUIHost",
    platforms: [
        .iOS(.v15),
        .macOS(.v13),
    ],
    products: [
        .library(name: "NulangUIProtocol", targets: ["NulangUIProtocol"]),
        .library(name: "NulangUIHost", targets: ["NulangUIHost"]),
    ],
    targets: [
        .target(name: "NulangUIProtocol"),
        .target(
            name: "NulangUIHost",
            dependencies: ["NulangUIProtocol"]
        ),
        .testTarget(
            name: "NulangUIProtocolTests",
            dependencies: ["NulangUIProtocol"]
        ),
        .testTarget(
            name: "NulangUIHostTests",
            dependencies: ["NulangUIHost", "NulangUIProtocol"]
        ),
    ]
)
