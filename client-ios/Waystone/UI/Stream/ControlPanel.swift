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
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(.horizontal, 14)
                .padding(.vertical, 11)

            Rectangle()
                .fill(WaystoneTheme.borderSubtle)
                .frame(height: 0.75)

            VStack(spacing: 7) {
                controlRow("触控") {
                    WaystoneSegmentedControl(
                        options: InputController.Mode.allCases,
                        selection: binding(\.touchMode, session.setTouchMode),
                        title: \.title
                    )
                }

                controlRow("热键") {
                    WaystoneSegmentedControl(
                        options: HotkeyPreset.all,
                        selection: Binding(
                            get: { preferences.preset },
                            set: { session.setPreset($0) }),
                        title: \.name
                    )
                }

                controlRow("分辨率") {
                    WaystoneSegmentedControl(
                        options: StreamPreferences.Resolution.allCases,
                        selection: binding(\.resolution, session.setResolution),
                        title: \.title
                    )
                }

                controlRow("帧率") {
                    WaystoneSegmentedControl(
                        options: StreamPreferences.FrameRate.allCases,
                        selection: binding(\.frameRate, session.setFrameRate),
                        title: \.title
                    )
                }

                controlRow("声音") {
                    WaystoneSegmentedControl(
                        options: StreamPreferences.Sound.allCases,
                        selection: binding(\.sound, session.setSound),
                        title: \.title
                    )
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            Rectangle()
                .fill(WaystoneTheme.borderSubtle)
                .frame(height: 0.75)

            actionFooter
                .padding(.horizontal, 14)
                .padding(.vertical, 9)
                .background(WaystoneTheme.footerSurface)
        }
        .frame(width: 456)
        .consolePanel(cornerRadius: 8, opacity: 0.96)
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text(session.host.name)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .lineLimit(1)

            PathCapsule(stage: session.stage, path: session.path)

            if let streamStats = streamStatsLabel {
                Text(streamStats)
                    .font(.system(size: 10.5, weight: .regular, design: .monospaced))
                    .monospacedDigit()
                    .foregroundStyle(WaystoneTheme.textTertiary)
                    .lineLimit(1)
            }

            Spacer(minLength: 6)

            if input.geometry.isZoomed {
                Button {
                    Haptics.tap()
                    input.resetZoom()
                } label: {
                    Text("还原缩放")
                        .font(.system(size: 11.5, weight: .medium))
                        .foregroundStyle(WaystoneTheme.textPrimary)
                        .padding(.horizontal, 8)
                        .frame(height: 24)
                        .background(
                            RoundedRectangle(cornerRadius: 4, style: .continuous)
                                .fill(WaystoneTheme.surfaceRaised)
                                .overlay(
                                    RoundedRectangle(cornerRadius: 4, style: .continuous)
                                        .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                                )
                        )
                }
                .buttonStyle(ConsoleButtonStyle())
            }

            Button {
                Haptics.tap()
                close()
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 10.5, weight: .semibold))
                    .foregroundStyle(WaystoneTheme.textSecondary)
                    .frame(width: 24, height: 24)
                    .background(
                        RoundedRectangle(cornerRadius: 4, style: .continuous)
                            .fill(WaystoneTheme.surfaceRaised)
                            .overlay(
                                RoundedRectangle(cornerRadius: 4, style: .continuous)
                                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                            )
                    )
            }
            .buttonStyle(ConsoleButtonStyle())
            .accessibilityLabel("收起")
        }
    }

    private var actionFooter: some View {
        HStack(spacing: 8) {
            Button {
                Haptics.tap()
                close()
                showKeyboard()
            } label: {
                Text("键盘")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(WaystoneTheme.textPrimary)
                    .padding(.horizontal, 12)
                    .frame(height: 32)
                    .background(
                        RoundedRectangle(cornerRadius: 5, style: .continuous)
                            .fill(WaystoneTheme.surfaceRaised)
                            .overlay(
                                RoundedRectangle(cornerRadius: 5, style: .continuous)
                                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                            )
                    )
            }
            .buttonStyle(ConsoleButtonStyle())

            Spacer(minLength: 4)

            Button {
                Haptics.tap()
                session.hangUp(lock: false)
            } label: {
                Text("仅断开，不锁屏")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(WaystoneTheme.textSecondary)
                    .padding(.horizontal, 11)
                    .frame(height: 32)
                    .background(
                        RoundedRectangle(cornerRadius: 5, style: .continuous)
                            .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                    )
            }
            .buttonStyle(ConsoleButtonStyle())

            Button(role: .destructive) {
                Haptics.tap()
                session.hangUp(lock: true)
            } label: {
                Text("断开并锁屏")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 12)
                    .frame(height: 32)
                    .background(
                        RoundedRectangle(cornerRadius: 5, style: .continuous)
                            .fill(WaystoneTheme.coral)
                    )
            }
            .buttonStyle(ConsoleButtonStyle())
        }
    }

    private var streamStatsLabel: String? {
        let size = input.geometry.videoSize
        guard size.width > 0, size.height > 0 else { return nil }
        let res = "\(Int(size.width))×\(Int(size.height))"
        if session.liveFps > 0 {
            return "\(res) · \(session.liveFps) fps"
        }
        return "\(res) · \(preferences.frameRate.rawValue) Hz"
    }

    private func controlRow<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(spacing: 10) {
            Text(title)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(WaystoneTheme.textSecondary)
                .frame(width: 40, alignment: .leading)
            content()
        }
    }

    private func binding<Value>(
        _ keyPath: ReferenceWritableKeyPath<StreamPreferences, Value>, _ apply: @escaping (Value) -> Void
    ) -> Binding<Value> {
        Binding(get: { preferences[keyPath: keyPath] }, set: apply)
    }
}

/// "P2P 直连 · 12 ms" in emerald, "TURN 中继 · 40 ms" in amber (design doc 3.3).
struct PathCapsule: View {
    var stage: StreamSession.Stage
    var path: StreamSession.Path

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(color)
                .frame(width: 5, height: 5)
            Text(label)
                .font(.system(size: 10.5, weight: .regular, design: .monospaced))
                .monospacedDigit()
                .foregroundStyle(WaystoneTheme.textSecondary)
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(
            RoundedRectangle(cornerRadius: 4, style: .continuous)
                .fill(Color.black)
                .overlay(
                    RoundedRectangle(cornerRadius: 4, style: .continuous)
                        .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                )
        )
    }

    private var isStalled: Bool {
        if case .stalled = stage { return true }
        return false
    }

    private var label: String {
        if isStalled { return "网络波动" }
        let kind = path.relayed ? "TURN 中继" : "P2P 直连"
        guard let rtt = path.rttMs else { return kind }
        return "\(kind) · \(rtt) ms"
    }

    private var color: Color {
        if isStalled { return WaystoneTheme.coral }
        return path.relayed ? WaystoneTheme.amber : WaystoneTheme.emerald
    }
}
