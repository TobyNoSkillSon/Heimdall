// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "FloatingIslands",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "FloatingIslands", targets: ["FloatingIslands"]),
        .executable(name: "islands", targets: ["islands"]),
    ],
    targets: [
        .target(name: "IslandModel"),
        .executableTarget(name: "FloatingIslands", dependencies: ["IslandModel"]),
        .executableTarget(name: "islands", dependencies: ["IslandModel"]),
        .testTarget(name: "IslandModelTests", dependencies: ["IslandModel"]),
    ]
)
