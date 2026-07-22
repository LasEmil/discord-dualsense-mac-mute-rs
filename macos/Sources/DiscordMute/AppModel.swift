import Combine
import Foundation
import ServiceManagement
import SwiftUI

/// Ties the supervised server, the status feed, and the views together.
@MainActor
final class AppModel: ObservableObject {
    let server = ServerProcess()
    let client = StatusClient()
    let notifier = Notifier()

    /// Surfaced under whichever control the user just pressed.
    @Published var actionError: String?
    @Published var isBusy = false
    /// Drives removal of the menu bar icon ahead of the actual teardown.
    @Published private(set) var isQuitting = false

    private var addressObserver: Task<Void, Never>?
    private var cancellables = Set<AnyCancellable>()
    private var didStart = false

    /// Last mute/deafen state we notified about, so we banner transitions rather
    /// than the steady stream of identical snapshots. `nil` until the first
    /// snapshot establishes a baseline — the initial state is not a transition.
    private var lastMuted: Bool?
    private var lastDeafened: Bool?
    /// Whether we've already warned about the current low-battery episode, so a
    /// controller sitting at 5% doesn't fire on every snapshot. Re-arms once it
    /// charges or is unplugged.
    private var batteryWarned = false

    init() {
        // Nested ObservableObjects do not propagate on their own: `@Published`
        // fires when a reference is reassigned, not when the referenced
        // object's own published properties change. Without forwarding, views
        // observing this model never redraw when the socket connects or a
        // status snapshot arrives.
        for child in [server.objectWillChange, client.objectWillChange] {
            child
                .sink { [weak self] _ in self?.objectWillChange.send() }
                .store(in: &cancellables)
        }

        // React to each fresh snapshot for notifications (mute changes, low
        // battery). Separate from the redraw forwarding above because this
        // needs the snapshot value, not just the change signal.
        client.$status
            .sink { [weak self] snapshot in
                guard let snapshot else { return }
                Task { @MainActor in self?.react(to: snapshot) }
            }
            .store(in: &cancellables)
    }

    // MARK: Notifications

    private func react(to snapshot: StatusSnapshot) {
        reactToVoice(snapshot)
        reactToBattery(snapshot)
    }

    private func reactToVoice(_ snapshot: StatusSnapshot) {
        defer {
            if let muted = snapshot.muted { lastMuted = muted }
            if let deafened = snapshot.deafened { lastDeafened = deafened }
        }

        // Deafen takes priority: Discord couples the two, so deafening also
        // mutes — reporting both would fire two banners for one action.
        if let deafened = snapshot.deafened, let previous = lastDeafened, previous != deafened {
            notifier.post(
                id: "deafen-toggle",
                title: "Discord",
                body: deafened ? "Deafened" : "Undeafened"
            )
            return
        }

        if let muted = snapshot.muted, let previous = lastMuted, previous != muted {
            notifier.post(
                id: "mute-toggle",
                title: "Discord",
                body: muted ? "Microphone muted" : "Microphone unmuted"
            )
        }
    }

    private func reactToBattery(_ snapshot: StatusSnapshot) {
        guard snapshot.controllerConnected, let battery = snapshot.battery else {
            batteryWarned = false
            return
        }

        let onCable = battery.state == "charging" || battery.state == "full"
        let low = !onCable && battery.percent < 10

        if low, !batteryWarned {
            notifier.post(
                id: "battery-low",
                title: "Controller battery low",
                body: "Your DualSense is at \(battery.percent)%. Time to charge it.",
                sound: true
            )
            batteryWarned = true
        } else if !low {
            // Charging or back above the threshold: re-arm for next time.
            batteryWarned = false
        }
    }

    // MARK: Derived state

    var status: StatusSnapshot? { client.status }
    var isMuted: Bool { status?.muted ?? false }
    var isDeafened: Bool { status?.deafened ?? false }
    var muteStateKnown: Bool { status?.muted != nil }
    var listenerRunning: Bool { status?.listener?.running ?? false }
    var controllerConnected: Bool { status?.controllerConnected ?? false }
    var listenerError: String? { status?.listener?.lastError }
    var isLive: Bool { client.isConnected }

    // MARK: Battery
    //
    // macOS doesn't surface the DualSense battery over Bluetooth, so this panel
    // is the only place it shows. Present only while a controller is attached
    // and has sent a battery reading.

    var battery: BatteryStatus? {
        controllerConnected ? status?.battery : nil
    }

    var batteryCharging: Bool {
        guard let state = battery?.state else { return false }
        return state == "charging" || state == "full"
    }

    /// The level line: "Full" while topped off on the cable, otherwise a
    /// percentage.
    var batteryDetail: String {
        guard let battery else { return "—" }
        if battery.state == "full" { return "Full" }
        if battery.state == "unknown" { return "\(battery.percent)%?" }
        return "\(battery.percent)%"
    }

    /// A battery SF Symbol whose fill tracks the level, with the charging bolt
    /// when on the cable.
    var batterySymbol: String {
        guard let battery else { return "battery.0percent" }
        if batteryCharging { return "battery.100percent.bolt" }
        switch battery.percent {
        case 88...: return "battery.100percent"
        case 63...: return "battery.75percent"
        case 38...: return "battery.50percent"
        case 13...: return "battery.25percent"
        default: return "battery.0percent"
        }
    }

    /// Green normally, warmer as it runs down — but never alarming while it's
    /// on the cable and filling back up.
    var batteryColor: Color {
        guard let battery, !batteryCharging else { return .green }
        switch battery.percent {
        case ..<15: return .red
        case ..<30: return .orange
        default: return .green
        }
    }

    /// What the Controller row should say when nothing is attached.
    ///
    /// A listener with no controller is usually just an idle DualSense, but it
    /// looks the same as one we are not permitted to read — so say which.
    var controllerDetail: String {
        if controllerConnected { return "Connected" }
        guard listenerRunning else { return "—" }
        return isPermissionProblem ? "No access" : "Waiting"
    }

    /// A hint shown only when waiting is not self-explanatory.
    var controllerHint: String? {
        guard listenerRunning, !controllerConnected else { return nil }
        guard let reason = status?.controllerError else { return nil }

        return isPermissionProblem
            ? "Grant Input Monitoring to DiscordMute in System Settings › Privacy & Security."
            : reason
    }

    /// hidapi reports "no device found" when nothing is attached. Anything else
    /// means a device was seen but could not be opened, which on macOS is
    /// almost always the Input Monitoring grant.
    private var isPermissionProblem: Bool {
        guard let reason = status?.controllerError?.lowercased() else { return false }
        return !reason.contains("no sony hid device")
    }

    /// The first thing worth showing when something is wrong, in the order a
    /// failure would actually occur.
    var problem: String? {
        server.failure ?? actionError ?? listenerError
    }

    /// The menu bar glyph. Monochrome by design: menu bar icons are template
    /// images that follow the bar's own tint, so state has to read from the
    /// shape rather than from colour.
    var menuBarSymbol: String {
        guard isLive else { return "mic.badge.xmark" }
        if isMuted { return "mic.slash.fill" }
        return listenerRunning ? "mic.fill" : "mic"
    }

    var muteHeadline: String {
        guard isLive else { return "Not connected" }
        guard muteStateKnown else { return "Unknown" }
        if isDeafened { return "Deafened" }
        return isMuted ? "Muted" : "Live"
    }

    var muteHint: String {
        guard isLive else { return "waiting for the server" }
        if isDeafened { return "deafened — tap to unmute" }
        return isMuted ? "tap to unmute" : "tap to mute"
    }

    /// Label for the deafen control, reflecting the current state.
    var deafenLabel: String { isDeafened ? "Undeafen" : "Deafen" }

    // MARK: Lifecycle

    func start() {
        guard !didStart else { return }
        didStart = true
        notifier.requestAuthorization()
        server.start()

        // The port is only known once the server prints it, so wait for the
        // address to appear before opening the socket.
        addressObserver = Task { [weak self] in
            while !Task.isCancelled {
                if let address = self?.server.address {
                    self?.client.connect(to: address)
                    return
                }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
    }

    func shutdown() {
        addressObserver?.cancel()
        client.stop()
        server.shutdown()
    }

    /// Quits without making the user watch the teardown.
    ///
    /// Dropping the menu bar icon first makes the app feel gone immediately;
    /// the server is signalled a runloop turn later, once SwiftUI has had a
    /// chance to remove it. Even if this app were killed outright before
    /// getting that far, the server's own orphan watchdog would clean it up.
    func quit() {
        isQuitting = true

        Task { @MainActor in
            shutdown()
            NSApp.terminate(nil)
        }
    }

    // MARK: Actions

    func toggleMute() {
        run { try await self.client.toggleMute() }
    }

    func toggleDeafen() {
        run { try await self.client.toggleDeafen() }
    }

    func toggleListener() {
        let running = listenerRunning
        run {
            if running {
                try await self.client.stopListener()
            } else {
                try await self.client.startListener()
            }
        }
    }

    private func run(_ work: @escaping () async throws -> Void) {
        guard !isBusy else { return }
        isBusy = true
        actionError = nil

        Task {
            do {
                try await work()
            } catch {
                actionError = error.localizedDescription
            }
            isBusy = false
        }
    }
}

/// Wraps the login item registration, which is the part most likely to be
/// unhappy with an ad-hoc signed bundle — so failures are surfaced rather than
/// swallowed.
@MainActor
final class LoginItem: ObservableObject {
    @Published private(set) var isEnabled = false
    @Published private(set) var problem: String?

    init() {
        refresh()
    }

    func refresh() {
        isEnabled = SMAppService.mainApp.status == .enabled
    }

    func set(_ enabled: Bool) {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            problem = nil
        } catch {
            // Typically a signing complaint: an ad-hoc signature changes on
            // every rebuild, and the registration is tied to it.
            problem =
                "Could not \(enabled ? "enable" : "disable") launch at login: "
                + error.localizedDescription
        }
        refresh()
    }
}
