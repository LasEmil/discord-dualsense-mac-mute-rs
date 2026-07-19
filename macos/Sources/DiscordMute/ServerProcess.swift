import Foundation

/// Supervises the Rust server, which does all the actual work.
///
/// The server is spawned as a child of this app so that macOS attributes its
/// HID access to this bundle — that is what makes the controller readable
/// without granting permission to a separate binary.
@MainActor
final class ServerProcess: ObservableObject {
    /// The address the server reported binding, once it is up.
    @Published private(set) var address: String?
    @Published private(set) var failure: String?

    private var process: Process?
    private var pendingOutput = Data()

    /// The server prints this as its first line so we can learn which port it
    /// actually got. We ask for port 0, so we cannot know it in advance.
    private static let listeningPrefix = "DISCORD_MUTE_API_LISTENING="

    /// The server binary sits next to us inside `Contents/MacOS`. Resolving it
    /// relative to our own executable also works for `swift run` during
    /// development, where there is no bundle at all.
    private var binaryURL: URL? {
        Bundle.main.executableURL?
            .deletingLastPathComponent()
            .appendingPathComponent("discord-mute-rs")
    }

    func start() {
        guard process == nil else { return }
        guard let binary = binaryURL, FileManager.default.fileExists(atPath: binary.path) else {
            failure = "The discord-mute-rs binary is missing from the app bundle."
            return
        }

        let task = Process()
        task.executableURL = binary

        // Extend the inherited environment rather than replacing it. A fresh
        // dictionary would drop HOME, and the server resolves its config
        // directory from HOME — without it, it cannot find your credentials.
        var environment = ProcessInfo.processInfo.environment
        // Port 0 means "any free port", so relaunching can never collide with a
        // server that outlived a previous run.
        environment["DISCORD_MUTE_API_ADDR"] = "127.0.0.1:0"
        // Belt and braces: if this app is force quit and never runs its own
        // cleanup, the server notices it has been orphaned and exits.
        environment["DISCORD_MUTE_EXIT_WITH_PARENT"] = "1"
        task.environment = environment

        // The server resolves `./static` relative to the working directory, so
        // point it at Resources where the web UI is bundled.
        task.currentDirectoryURL = Bundle.main.resourceURL

        let pipe = Pipe()
        task.standardOutput = pipe
        task.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            guard !chunk.isEmpty else { return }
            Task { @MainActor in self?.consume(chunk) }
        }

        task.terminationHandler = { [weak self] finished in
            Task { @MainActor in
                self?.process = nil
                self?.address = nil
                if finished.terminationStatus != 0 {
                    self?.failure =
                        "The server exited unexpectedly (status \(finished.terminationStatus))."
                }
            }
        }

        do {
            try task.run()
            process = task
            failure = nil
        } catch {
            failure = "Could not start the server: \(error.localizedDescription)"
        }
    }

    /// Accumulates stdout and pulls the listening address out of it. Reads
    /// arrive in arbitrary chunks, so buffer until a newline rather than
    /// assuming one read is one line.
    private func consume(_ chunk: Data) {
        pendingOutput.append(chunk)

        while let newline = pendingOutput.firstIndex(of: UInt8(ascii: "\n")) {
            let lineData = pendingOutput[pendingOutput.startIndex..<newline]
            pendingOutput.removeSubrange(pendingOutput.startIndex...newline)

            guard let line = String(data: lineData, encoding: .utf8) else { continue }
            if line.hasPrefix(Self.listeningPrefix) {
                address = String(line.dropFirst(Self.listeningPrefix.count))
                    .trimmingCharacters(in: .whitespaces)
            }
        }
    }

    /// Stops the server, fast.
    ///
    /// Signals rather than asking over HTTP: `POST /quit` costs a request plus
    /// a graceful shutdown, and the user is waiting for the app to disappear
    /// the whole time. Nothing here needs the graceful path — the OS releases
    /// the HID device and the socket on exit — so SIGTERM is both correct and
    /// immediate. If the process somehow ignores it, SIGKILL follows shortly.
    ///
    /// Runs synchronously: it is called while the app is terminating, and an
    /// async cleanup would not get the chance to finish.
    func shutdown() {
        guard let task = process else { return }
        process = nil
        guard task.isRunning else { return }

        task.terminate()
        if !waitForExit(task, timeout: 0.5) {
            kill(task.processIdentifier, SIGKILL)
        }
    }

    private func waitForExit(_ task: Process, timeout: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if !task.isRunning { return true }
            usleep(50_000)
        }
        return !task.isRunning
    }
}
