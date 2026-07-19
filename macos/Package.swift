// swift-tools-version:5.9
import PackageDescription

// Targets macOS 26 deliberately: the panel is built on the Liquid Glass APIs
// (`glassEffect`, `GlassEffectContainer`, `.buttonStyle(.glass)`), which exist
// only from Tahoe onward. Nothing here degrades gracefully to an older system,
// so there is no point pretending to support one.
let package = Package(
    name: "DiscordMute",
    platforms: [.macOS("26.0")],
    targets: [
        .executableTarget(name: "DiscordMute", path: "Sources/DiscordMute")
    ]
)
