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
                    .padding(.top, 8)
                    .allowsHitTesting(false)
            }

            if session.stage == .connecting {
                ConnectingOverlay(host: session.host.name, progress: session.progress) {
                    session.hangUp(lock: false)
                }
            }

            FloatingBall(health: health) {
                withAnimation(.spring(duration: 0.25)) { panelOpen.toggle() }
            }
            .ignoresSafeArea()

            if panelOpen {
                Color.black.opacity(0.25)
                    .ignoresSafeArea()
                    .onTapGesture { withAnimation { panelOpen = false } }
                ControlPanel(
                    session: session, input: input,
                    showKeyboard: { keyboard = true },
                    close: { withAnimation { panelOpen = false } })
                .transition(.scale(scale: 0.9).combined(with: .opacity))
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

/// A small ring that follows the remote cursor in trackpad mode, so the
/// cursor moves under the finger before the video catches up.
private struct LocalCursor: View {
    var position: CGPoint

    var body: some View {
        Circle()
            .strokeBorder(.white, lineWidth: 2)
            .background(Circle().fill(.black.opacity(0.25)))
            .frame(width: 18, height: 18)
            .shadow(radius: 2)
            .position(position)
            .allowsHitTesting(false)
    }
}

private struct StallBanner: View {
    var body: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text("网络波动，正在恢复画面…")
                .font(.footnote.weight(.medium))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(.ultraThinMaterial, in: Capsule())
        .transition(.move(edge: .top).combined(with: .opacity))
    }
}

private struct ConnectingOverlay: View {
    var host: String
    var progress: String
    var cancel: () -> Void

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 40))
                .foregroundStyle(.secondary)
            Text(host).font(.title3.weight(.semibold))
            HStack(spacing: 8) {
                ProgressView()
                Text(progress).foregroundStyle(.secondary)
            }
            Button("取消", action: cancel)
                .buttonStyle(.bordered)
                .padding(.top, 8)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
        .accessibilityIdentifier("connecting")
    }
}
