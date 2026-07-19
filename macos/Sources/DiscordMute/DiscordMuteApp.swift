import SwiftUI

@main
struct DiscordMuteApp: App {
    @StateObject private var model = AppModel()
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        // `isInserted` lets Quit pull the icon out of the menu bar immediately,
        // rather than leaving it sitting there during teardown.
        MenuBarExtra(isInserted: .constant(!model.isQuitting)) {
            PanelView(model: model)
        } label: {
            // The label exists as soon as the status item does, so this starts
            // the server at launch. Hanging it off the panel instead would
            // delay startup until the first time the popover was opened.
            Image(systemName: model.menuBarSymbol)
                .task {
                    delegate.model = model
                    model.start()
                }
        }
        // `.window` gives the popover panel rather than a list of menu items,
        // which is what lets the hero control exist at all.
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView(model: model)
        }
    }
}

/// Owns start and stop. A `Scene` has no termination hook, and the server is a
/// child process that must not outlive the app.
final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor var model: AppModel? {
        didSet {
            guard oldValue == nil else { return }
            model?.start()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated {
            model?.shutdown()
        }
    }
}
