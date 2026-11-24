import SwiftUI

/// The floating ball's panel: the five switches from design doc 5.3, plus
/// sound, and the way out.
struct ControlPanel: View {
    @ObservedObject var session: StreamSession
    @ObservedObject var input: InputController
    @EnvironmentObject private var preferences: StreamPreferences
    var showKeyboard: () -> Void
    var close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header

            row("触控") {
                Picker("触控", selection: binding(\.touchMode, session.setTouchMode)) {
                    ForEach(InputController.Mode.allCases) { Text($0.title).tag($0) }
                }
            }
            row("热键") {
                Picker("热键", selection: Binding(
                    get: { preferences.presetId },
                    set: { session.setPreset(HotkeyPreset.named($0)) })
                ) {
                    ForEach(HotkeyPreset.all) { Text($0.name).tag($0.id) }
                }
            }
            row("画质") {
                Picker("画质", selection: binding(\.frameRate, session.setFrameRate)) {
                    ForEach(StreamPreferences.FrameRate.allCases) { Text($0.title).tag($0) }
                }
            }
            row("声音") {
                Picker("声音", selection: binding(\.sound, session.setSound)) {
                    ForEach(StreamPreferences.Sound.allCases) { Text($0.title).tag($0) }
                }
            }

            HStack(spacing: 10) {
                Button {
                    close()
                    showKeyboard()
                } label: {
                    Label("键盘", systemImage: "keyboard")
                        .frame(maxWidth: .infinity)
                }
                if input.geometry.isZoomed {
                    Button {
                        input.resetZoom()
                    } label: {
                        Label("还原缩放", systemImage: "arrow.down.right.and.arrow.up.left")
                            .frame(maxWidth: .infinity)
                    }
                }
            }
            .buttonStyle(.bordered)

            VStack(spacing: 8) {
                Button(role: .destructive) {
                    session.hangUp(lock: true)
                } label: {
                    Label("断开并锁屏", systemImage: "lock.fill")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .tint(.red)
                Button("仅断开，不锁屏") { session.hangUp(lock: false) }
                    .font(.footnote)
                    .frame(maxWidth: .infinity)
            }
        }
        .pickerStyle(.segmented)
        .padding(16)
        .frame(width: 340)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 20))
        .shadow(radius: 20)
    }

    private var header: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                Text(session.host.name).font(.headline)
                PathCapsule(stage: session.stage, path: session.path)
            }
            Spacer()
            Button(action: close) {
                Image(systemName: "xmark.circle.fill")
                    .font(.title2)
                    .symbolRenderingMode(.hierarchical)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("收起")
        }
    }

    private func row<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(spacing: 10) {
            Text(title)
                .font(.caption.weight(.medium))
                .foregroundStyle(.secondary)
                .frame(width: 30, alignment: .leading)
            content()
        }
    }

    private func binding<Value>(
        _ keyPath: ReferenceWritableKeyPath<StreamPreferences, Value>, _ apply: @escaping (Value) -> Void
    ) -> Binding<Value> {
        Binding(get: { preferences[keyPath: keyPath] }, set: apply)
    }
}

/// "直连 · 12 ms" in green, "中继 · 40 ms" in yellow (design doc 3.3).
struct PathCapsule: View {
    var stage: StreamSession.Stage
    var path: StreamSession.Path

    var body: some View {
        HStack(spacing: 5) {
            Circle().fill(color).frame(width: 7, height: 7)
            Text(label).font(.caption2.weight(.medium)).monospacedDigit()
        }
    }

    private var label: String {
        if case .stalled = stage { return "网络波动" }
        let kind = path.relayed ? "中继" : "直连"
        guard let rtt = path.rttMs else { return kind }
        return "\(kind) · \(rtt) ms"
    }

    private var color: Color {
        if case .stalled = stage { return .orange }
        return path.relayed ? .yellow : .green
    }
}
