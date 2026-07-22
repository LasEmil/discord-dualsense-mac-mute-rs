import Foundation
import UserNotifications

/// Posts local notifications from the app bundle.
///
/// The Rust server used to fire these through `osascript`, which always shows
/// the script runner's icon — there's no way to override it. Posting from the
/// bundled app instead means macOS labels the banner with our own icon and
/// name for free.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    private let center = UNUserNotificationCenter.current()
    private var authorized = false

    override init() {
        super.init()
        center.delegate = self
    }

    /// Prompts once for permission. Safe to call on every launch; the system
    /// only shows the dialog the first time.
    func requestAuthorization() {
        center.requestAuthorization(options: [.alert, .sound]) { [weak self] granted, _ in
            Task { @MainActor in self?.authorized = granted }
        }
    }

    /// Delivers a banner immediately. Reusing a stable `id` per kind means a
    /// fresh banner replaces the previous one of that kind rather than stacking
    /// — rapid mute toggles collapse to the latest state.
    func post(id: String, title: String, body: String, sound: Bool = false) {
        guard authorized else { return }

        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        if sound { content.sound = .default }

        let request = UNNotificationRequest(identifier: id, content: content, trigger: nil)
        center.add(request)
    }

    // A menu bar agent is never really "frontmost", but ask for the banner
    // explicitly so the notification shows rather than only landing silently in
    // Notification Center.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}
