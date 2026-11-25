import SwiftUI

/// A running stream, edge to edge.
///
/// Design doc 5.3: the top of the screen belongs to the remote window, so there
/// is no toolbar; the floating ball is the only persistent control. Design doc
/// 5.4: a stall never blacks out the picture or blocks it with a modal; it
/// shows a small banner over the last frame.
struct StreamView: View {
    @ObservedObject var session: StreamSession
    @EnvironmentObject private var preferences: StreamPreferences

    var body: some View {
        StreamContent(session: session, input: session.input, preferences: preferences)
            .onAppear { AppDelegate.lock(.landscape) }
            .onDisappear { AppDelegate.lock(.all) }
    }
}

private struct StreamContent: View {
    @ObservedObject var session: StreamSession
    @ObservedObject var input: InputController
    @ObservedObject var preferences: StreamPreferences
    @State private var panelOpen = false
    @State private var keyboard = false

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            StreamSurface(session: session, geometry: input.geometry)
                .ignoresSafeArea()
                .accessibilityIdentifier("stream-surface")

            if input.mode == .trackpad {
                LocalCursor(position: input.geometry.surfacePoint(x: input.cursor.x, y: input.cursor.y))
                    .ignoresSafeArea()
            } else if input.directPointerActive {
                DirectTouchReticle(position: input.geometry.surfacePoint(x: input.cursor.x, y: input.cursor.y))
                    .ignoresSafeArea()
            }

            if session.stage != .connecting {
                HotkeyLayer(preset: preferences.preset, input: input)
            }

            SoftKeyboard(visible: $keyboard, input: input)
                .frame(width: 1, height: 1)
                .opacity(0)

            if case .stalled = session.stage {
                StallBanner()
                    .frame(maxHeight: .infinity, alignment: .top)
                    .padding(.top, 12)
                    .allowsHitTesting(false)
            }

            if session.stage == .connecting {
                ConnectingOverlay(host: session.host.name, progress: session.progress) {
                    session.hangUp(lock: false)
                }
            }

            FloatingBall(health: health) {
                withAnimation(.easeOut(duration: 0.14)) { panelOpen.toggle() }
            }
            .ignoresSafeArea()

            if panelOpen {
                Color.black.opacity(0.58)
                    .ignoresSafeArea()
                    .transition(.opacity)
                    .onTapGesture {
                        withAnimation(.easeOut(duration: 0.12)) { panelOpen = false }
                    }
                ControlPanel(
                    session: session, input: input,
                    showKeyboard: { keyboard = true },
                    close: {
                        withAnimation(.easeOut(duration: 0.12)) { panelOpen = false }
                    })
                    .transition(.opacity)
            }
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        .defersSystemGestures(on: .all)
    }

    private var health: FloatingBall.Health {
        switch session.stage {
        case .stalled: return .stalled
        case .connecting: return .degraded
        default: return session.path.relayed ? .degraded : .good
        }
    }
}

/// Precision reticle that tracks the remote cursor in trackpad mode so motion
/// feels immediate before the encoded frame catches up.
private struct LocalCursor: View {
    var position: CGPoint

    var body: some View {
        ZStack {
            Circle()
                .stroke(.black.opacity(0.75), lineWidth: 2.5)
                .frame(width: 14, height: 14)
            Circle()
                .strokeBorder(.white.opacity(0.95), lineWidth: 1.25)
                .background(Circle().fill(.black.opacity(0.18)))
                .frame(width: 14, height: 14)
            Circle()
                .fill(.white)
                .frame(width: 2.5, height: 2.5)
        }
        .position(position)
        .allowsHitTesting(false)
    }
}

/// Subtle zero-latency touch feedback while hovering or dragging in direct mode.
private struct DirectTouchReticle: View {
    var position: CGPoint

    var body: some View {
        ZStack {
            Circle()
                .stroke(.black.opacity(0.45), lineWidth: 2)
                .frame(width: 18, height: 18)
            Circle()
                .strokeBorder(.white.opacity(0.75), lineWidth: 1)
                .frame(width: 18, height: 18)
        }
        .position(position)
        .allowsHitTesting(false)
    }
}

private struct StallBanner: View {
    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(WaystoneTheme.coral)
                .frame(width: 5, height: 5)
            Text("网络波动，正在恢复画面…")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(WaystoneTheme.textPrimary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .consolePanel(cornerRadius: 6, opacity: 0.94)
        .transition(.move(edge: .top).combined(with: .opacity))
    }
}

private struct ConnectingOverlay: View {
    var host: String
    var progress: String
    var cancel: () -> Void

    var body: some View {
        ZStack {
            WaystoneTheme.canvas.ignoresSafeArea()

            VStack(alignment: .leading, spacing: 14) {
                Text(host)
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(WaystoneTheme.textPrimary)
                    .lineLimit(1)

                ConsoleProgressBar()

                HStack(alignment: .center) {
                    Text(progress)
                        .font(.system(size: 12, weight: .regular, design: .monospaced))
                        .foregroundStyle(WaystoneTheme.textSecondary)
                        .lineLimit(1)

                    Spacer(minLength: 12)

                    Button {
                        Haptics.tap()
                        cancel()
                    } label: {
                        Text("取消")
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(WaystoneTheme.textSecondary)
                            .padding(.horizontal, 14)
                            .frame(height: 30)
                            .background(
                                RoundedRectangle(cornerRadius: 5, style: .continuous)
                                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                            )
                    }
                    .buttonStyle(ConsoleButtonStyle())
                }
            }
            .frame(maxWidth: 320)
            .padding(.horizontal, 24)
        }
        .accessibilityIdentifier("connecting")
    }
}
