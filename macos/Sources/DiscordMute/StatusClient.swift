import Foundation

// MARK: - Wire types

/// Mirrors the `listener` object in the server's status snapshot.
struct ListenerStatus: Decodable, Equatable {
    let running: Bool
    let lastError: String?
}

/// Mirrors `StatusResponse` in `src/api.rs`.
struct StatusSnapshot: Decodable, Equatable {
    let pid: Int
    let uptimeSeconds: Int
    let api: String
    let muted: Bool?
    let controllerConnected: Bool
    let controllerError: String?
    let listener: ListenerStatus?
}

/// Mirrors `ConfigStatus` in `src/api.rs`.
struct ConfigStatus: Decodable, Equatable {
    let configured: Bool
    let configPath: String
    let tokenPath: String
}

// MARK: - Client

/// Talks to the local server: a WebSocket for live state, plain requests for
/// actions.
@MainActor
final class StatusClient: ObservableObject {
    @Published private(set) var status: StatusSnapshot?
    @Published private(set) var isConnected = false

    private var address: String?
    private var socket: URLSessionWebSocketTask?
    private var reconnectDelay: TimeInterval = 0.5
    private var isStopped = false

    private let decoder = JSONDecoder()

    func connect(to address: String) {
        self.address = address
        isStopped = false
        openSocket()
    }

    func stop() {
        isStopped = true
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        isConnected = false
    }

    private func openSocket() {
        guard !isStopped, let address, let url = URL(string: "ws://\(address)/ws") else { return }

        let task = URLSession.shared.webSocketTask(with: url)
        socket = task
        task.resume()
        receiveNext()
    }

    private func receiveNext() {
        socket?.receive { [weak self] result in
            Task { @MainActor in
                guard let self else { return }
                switch result {
                case .success(let message):
                    self.isConnected = true
                    self.reconnectDelay = 0.5
                    self.handle(message)
                    self.receiveNext()
                case .failure:
                    // The server is gone or still starting; back off and retry
                    // rather than leaving the UI permanently stale.
                    self.isConnected = false
                    self.scheduleReconnect()
                }
            }
        }
    }

    private func handle(_ message: URLSessionWebSocketTask.Message) {
        let data: Data?
        switch message {
        case .string(let text): data = text.data(using: .utf8)
        case .data(let raw): data = raw
        @unknown default: data = nil
        }

        guard let data, let snapshot = try? decoder.decode(StatusSnapshot.self, from: data) else {
            return  // Ignore a malformed frame rather than tearing the socket down.
        }
        status = snapshot
    }

    private func scheduleReconnect() {
        guard !isStopped else { return }
        let delay = reconnectDelay
        reconnectDelay = min(reconnectDelay * 2, 5)

        socket?.cancel()
        socket = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
            Task { @MainActor in self?.openSocket() }
        }
    }

    // MARK: Actions

    func toggleMute() async throws {
        _ = try await send("/discord/toggle", method: "POST")
    }

    func startListener() async throws {
        _ = try await send("/listeners/mute", method: "POST")
    }

    func stopListener() async throws {
        _ = try await send("/listeners/current", method: "DELETE")
    }

    func fetchConfig() async throws -> ConfigStatus {
        let data = try await send("/config", method: "GET")
        return try decoder.decode(ConfigStatus.self, from: data)
    }

    func saveConfig(clientId: String, clientSecret: String) async throws {
        let body = try JSONSerialization.data(withJSONObject: [
            "clientId": clientId,
            "clientSecret": clientSecret,
        ])
        _ = try await send("/config", method: "PUT", body: body)
    }

    @discardableResult
    private func send(_ path: String, method: String, body: Data? = nil) async throws -> Data {
        guard let address, let url = URL(string: "http://\(address)\(path)") else {
            throw ClientError.notRunning
        }

        var request = URLRequest(url: url)
        request.httpMethod = method
        request.timeoutInterval = 15
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }

        let (data, response) = try await URLSession.shared.data(for: request)
        let code = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(code) else {
            // The server reports failures as {"ok":false,"error":"..."}; prefer
            // that message over a bare status code.
            let detail =
                (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"]
                as? String
            throw ClientError.server(detail ?? "Request failed with status \(code)")
        }
        return data
    }

    enum ClientError: LocalizedError {
        case notRunning
        case server(String)

        var errorDescription: String? {
            switch self {
            case .notRunning: return "The server is not running yet."
            case .server(let message): return message
            }
        }
    }
}
