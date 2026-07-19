import SwiftUI

/// The Settings window (⌘,). Everything configurable lives here so the popover
/// stays a control surface rather than a form.
struct SettingsView: View {
    @ObservedObject var model: AppModel
    @StateObject private var loginItem = LoginItem()

    @State private var clientId = ""
    @State private var clientSecret = ""
    @State private var config: ConfigStatus?
    @State private var saveError: String?
    @State private var didSave = false
    @State private var isSaving = false

    var body: some View {
        Form {
            discordSection
            generalSection
            serverSection
        }
        .formStyle(.grouped)
        .frame(width: 460)
        .fixedSize(horizontal: false, vertical: true)
        .task { await loadConfig() }
    }

    // MARK: Discord

    private var discordSection: some View {
        Section {
            TextField("Client ID", text: $clientId, prompt: Text("1234567890123456789"))
                .textContentType(.username)
            SecureField("Client secret", text: $clientSecret, prompt: Text("••••••••••••"))

            HStack {
                statusLine
                Spacer()
                Button(isSaving ? "Saving…" : "Save") {
                    Task { await save() }
                }
                .buttonStyle(.glassProminent)
                .disabled(!canSave)
            }

            if let saveError {
                Label(saveError, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } header: {
            Text("Discord application")
        } footer: {
            Text(
                "Create an application at discord.com/developers/applications and add "
                    + "http://localhost as an OAuth2 redirect URI."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var statusLine: some View {
        if didSave {
            Label("Saved", systemImage: "checkmark.circle.fill")
                .font(.caption)
                .foregroundStyle(.green)
        } else if config?.configured == true {
            Label("Configured", systemImage: "checkmark.circle.fill")
                .font(.caption)
                .foregroundStyle(.secondary)
        } else {
            Label("Not configured", systemImage: "circle.dashed")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var canSave: Bool {
        !isSaving
            && !clientId.trimmingCharacters(in: .whitespaces).isEmpty
            && !clientSecret.trimmingCharacters(in: .whitespaces).isEmpty
    }

    // MARK: General

    private var generalSection: some View {
        Section("General") {
            Toggle(
                "Launch at login",
                isOn: Binding(
                    get: { loginItem.isEnabled },
                    set: { loginItem.set($0) }
                )
            )

            if let problem = loginItem.problem {
                Text(problem)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    // MARK: Server

    private var serverSection: some View {
        Section("Server") {
            LabeledContent("Address", value: model.status?.api ?? "—")
            LabeledContent("Process", value: model.status.map { "\($0.pid)" } ?? "—")
            LabeledContent("Uptime", value: uptimeText)
            if let path = config?.configPath {
                LabeledContent("Config", value: path)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .monospacedDigit()
        .foregroundStyle(.secondary)
    }

    private var uptimeText: String {
        guard let seconds = model.status?.uptimeSeconds else { return "—" }
        let hours = seconds / 3600
        let minutes = (seconds % 3600) / 60
        let secs = seconds % 60
        return String(format: "%02d:%02d:%02d", hours, minutes, secs)
    }

    // MARK: Work

    private func loadConfig() async {
        config = try? await model.client.fetchConfig()
    }

    private func save() async {
        isSaving = true
        saveError = nil
        do {
            try await model.client.saveConfig(clientId: clientId, clientSecret: clientSecret)
            clientId = ""
            clientSecret = ""
            didSave = true
            await loadConfig()
        } catch {
            saveError = error.localizedDescription
        }
        isSaving = false
    }
}
