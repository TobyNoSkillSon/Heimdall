// swift-tools-version: 6.2
import PackageDescription
let package = Package(
    name: "HeimdallEmbedding", platforms: [.macOS(.v14)],
    products: [.executable(name: "heimdall-embedding-worker", targets: ["EmbeddingWorker"])],
    dependencies: [
        .package(url: "https://github.com/ml-explore/mlx-swift-lm.git", revision: "c53d7d480147684f42dac9d959f648dc35f021d0", traits: []),
        .package(url: "https://github.com/ml-explore/mlx-swift", exact: "0.32.3"),
        .package(url: "https://github.com/huggingface/swift-transformers.git", revision: "150169bfba0889c229a2ce7494cf8949f18e6906")
    ],
    targets: [.executableTarget(name: "EmbeddingWorker", dependencies: [
        .product(name: "MLXLMCommon", package: "mlx-swift-lm"),
        .product(name: "MLX", package: "mlx-swift"), .product(name: "MLXNN", package: "mlx-swift"),
        .product(name: "MLXFFT", package: "mlx-swift"), .product(name: "Tokenizers", package: "swift-transformers")
    ])], swiftLanguageModes: [.v5])
