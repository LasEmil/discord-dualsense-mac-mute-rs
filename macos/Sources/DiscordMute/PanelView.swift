import SwiftUI

/// The menu bar popover: glanceable state and the two controls worth reaching
/// for without opening anything else.
struct PanelView: View {
    @ObservedObject var model: AppModel
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        // A single container so the glass surfaces inside sample the same
        // backdrop and blend, instead of each compositing independently.
        GlassEffectContainer(spacing: 16) {
            VStack(spacing: 14) {
                MuteControl(model: model)

                StatusRows(model: model)

                ListenerButton(model: model)

                if let message = model.problem {
                    ErrorNote(message: message)
                }

                Divider().opacity(0.5)

                footer
            }
            .padding(16)
        }
        .frame(width: 280)
    }

    private var footer: some View {
        HStack {
            Button {
                // A menu bar app is not frontmost when the popover is open, so
                // the settings window would otherwise open behind everything.
                NSApp.activate(ignoringOtherApps: true)
                openSettings()
            } label: {
                Label("Settings…", systemImage: "gearshape")
                    .font(.callout)
            }

            Spacer()

            Button("Quit", action: model.quit)
                .font(.callout)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.secondary)
    }
}

// MARK: - Hero control

private struct MuteControl: View {
    @ObservedObject var model: AppModel

    var body: some View {
        Button(action: model.toggleMute) {
            VStack(spacing: 8) {
                Image(systemName: model.isMuted ? "mic.slash.fill" : "mic.fill")
                    .font(.system(size: 34, weight: .medium))
                    // Animate the glyph swap rather than snapping between them.
                    .contentTransition(.symbolEffect(.replace))
                    .frame(height: 40)

                VStack(spacing: 2) {
                    Text("Microphone")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Text(model.muteHeadline)
                        .font(.title3.weight(.semibold))
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 20)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .disabled(!model.isLive || model.isBusy)
        // Tint carries the state on this surface, where colour is available —
        // unlike the menu bar glyph, which has to stay monochrome.
        .glassEffect(
            .regular
                .tint(model.isMuted ? .red.opacity(0.5) : nil)
                .interactive(),
            in: .rect(cornerRadius: 20)
        )
        .animation(.smooth(duration: 0.25), value: model.isMuted)

        Text(model.muteHint)
            .font(.caption2)
            .foregroundStyle(.tertiary)
    }
}

// MARK: - Status

private struct StatusRows: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(spacing: 8) {
            StatusRow(
                title: "Controller",
                detail: model.controllerConnected ? "Connected" : "Waiting",
                isOn: model.controllerConnected
            )
            StatusRow(
                title: "Listener",
                detail: model.listenerRunning ? "Running" : "Stopped",
                isOn: model.listenerRunning
            )
        }
    }
}

private struct StatusRow: View {
    let title: String
    let detail: String
    let isOn: Bool

    var body: some View {
        HStack(spacing: 8) {
            Text(title)
                .font(.callout)
                .foregroundStyle(.secondary)
            Spacer()
            Circle()
                .fill(isOn ? Color.green : Color.secondary.opacity(0.45))
                .frame(width: 6, height: 6)
            Text(detail)
                .font(.callout.weight(.medium))
                .monospacedDigit()
        }
        .animation(.smooth(duration: 0.2), value: isOn)
    }
}

// MARK: - Listener

private struct ListenerButton: View {
    @ObservedObject var model: AppModel

    var body: some View {
        Button(action: model.toggleListener) {
            Text(model.listenerRunning ? "Stop listening" : "Start listening")
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glass)
        .controlSize(.large)
        .disabled(!model.isLive || model.isBusy)
    }
}

private struct ErrorNote: View {
    let message: String

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.caption)
        .foregroundStyle(.secondary)
    }
}
