// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "AgentGitMenuBar",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "AgentGitMenuBar",
            path: "Sources/AgentGitMenuBar"
        ),
    ]
)
