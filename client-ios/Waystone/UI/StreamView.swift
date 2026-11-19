import SwiftUI

/// Full screen stream: Metal view, touch surface, floating ball and capsules.
///
/// The rule from the design doc is that the top of the screen belongs to the
/// remote window, so there is no toolbar at all; every control lives in the
/// floating ball on the side.
struct StreamView: View {
    @EnvironmentObject private var controller: SessionController
    @State private var panelOpen = false
    @State private var ballHidden = false
    @State private var pinchScale: CGFloat = 1

    var body: some View {
        GeometryReader { geometry in
            ZStack {
                Color.black.ignoresSafeArea()

                RemoteSurface(controller: controller)
                    .ignoresSafeArea()
                    .scaleEffect(pinchScale)
                    .gesture(
                        MagnificationGesture()
                            .onChanged { value in
                                pinchScale = controller.pinch(scale: value, ended: false)
                            }
                            .onEnded { _ in
                                pinchScale = controller.pinch(scale: 1, ended: true)
                            }
                    )
                    .simultaneousGesture(
                        SpatialTapGesture(count: 2).onEnded { _ in
                            controller.requestKeyframe()
                        }
                    )

                // Edge capsules, one column per side, tuned per preset.
                HStack {
                    capsuleColumn(alignment: .leading)
                    Spacer()
                    capsuleColumn(alignment: .trailing)
                }
                .padding(.horizontal, 2)
                .allowsHitTesting(true)

                // Floating ball is the only persistent control.
                ball
                    .position(
                        x: geometry.size.width - 28,
                        y: geometry.size.height / 2
                    )
                    .opacity(ballHidden ? 0.3 : 0.85)
                    .animation(.easeInOut(duration: 0.2), value: ballHidden)

                statusCapsule
                    .padding(.top, 8)
                    .frame(maxHeight: .infinity, alignment: .top)

                if case .ended(let reason) = controller.phase {
                    disconnectedOverlay(reason: reason)
                }
            }
            .statusBarHidden(true)
            .persistentSystemOverlays(.hidden)
        }
    }

    // MARK: - Capsules

    @ViewBuilder
    private func capsuleColumn(alignment: HorizontalAlignment) -> some View {
        VStack(spacing: 8) {
            ForEach(controller.selectedPreset.capsules) { capsule in
                CapsuleButton(capsule: capsule) {
                    controller.press(capsule)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: alignment == .leading ? .leading : .trailing)
        .padding(.vertical, 60)
    }

    private var ball: some View {
        Button {
            panelOpen.toggle()
            ballHidden = false
        } label: {
            Circle()
                .fill(.ultraThinMaterial)
                .overlay(
                    Image(systemName: "circle.grid.cross")
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(.white.opacity(0.9))
                )
                .frame(width: 36, height: 36)
        }
        .buttonStyle(.plain)
        .popover(isPresented: $panelOpen, arrowEdge: .trailing) {
            ControlPanel(controller: controller)
                .presentationCompactAdaptation(.popover)
        }
    }

    private var statusCapsule: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(controller.path.isHealthy ? Color.green : Color.yellow)
                .frame(width: 7, height: 7)
            Text(controller.path.label)
                .font(.caption2.weight(.medium))
                .foregroundStyle(.white)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        .background(.ultraThinMaterial, in: Capsule())
        .opacity(0.9)
    }

    private func disconnectedOverlay(reason: String) -> some View {
        VStack(spacing: 12) {
            Image(systemName: "wifi.exclamationmark")
                .font(.system(size: 32))
                .foregroundStyle(.orange)
            Text(reason)
                .font(.callout)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 24)
            Button("Back") {
                controller.hangUp(lock: false)
            }
            .buttonStyle(.borderedProminent)
        }
        .padding(24)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16))
    }
}

/// One edge capsule.
private struct CapsuleButton: View {
    var capsule: SessionController.HotkeyCapsule
    var action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(capsule.title)
                .font(.caption2.weight(.semibold))
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(.ultraThinMaterial, in: Capsule())
                .foregroundStyle(.white.opacity(0.9))
        }
        .buttonStyle(.plain)
        .frame(width: 56)
    }
}

/// The Metal backed surface that receives touches.
private struct RemoteSurface: UIViewRepresentable {
    var controller: SessionController

    func makeUIView(context: Context) -> TouchView {
        let view = TouchView()
        view.backgroundColor = .black
        view.isMultipleTouchEnabled = true
        view.onTouches = { event, point in
            let size = view.bounds.size
            switch event {
            case .began: controller.touchBegan(at: point, in: size)
            case .moved: controller.touchMoved(to: point, in: size)
            case .ended: controller.touchEnded(at: point, in: size)
            case .cancelled: controller.touchCancelled()
            }
        }
        return view
    }

    func updateUIView(_ view: TouchView, context: Context) {
        if view.videoView.superview == nil {
            view.addSubview(view.videoView)
            view.videoView.frame = view.bounds
            view.videoView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            controller.attach(view: view.videoView)
        }
    }
}

/// Hosts the `VideoView` and forwards raw touches.
///
/// `UIView` touches are used instead of SwiftUI gestures because the gesture state
/// machine needs every touch phase with sub-frame timing.
private final class TouchView: UIView {
    enum TouchEvent {
        case began
        case moved
        case ended
        case cancelled
    }

    let videoView = VideoView(frame: .zero)
    var onTouches: ((TouchEvent, CGPoint) -> Void)?

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesBegan(touches, with: event)
        guard let touch = touches.first else { return }
        onTouches?(.began, touch.location(in: self))
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesMoved(touches, with: event)
        guard let touch = touches.first else { return }
        onTouches?(.moved, touch.location(in: self))
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesEnded(touches, with: event)
        guard let touch = touches.first else { return }
        onTouches?(.ended, touch.location(in: self))
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesCancelled(touches, with: event)
        onTouches?(.cancelled, .zero)
    }
}

/// The floating ball panel: touch mode, preset, frame rate, audio, hang up.
private struct ControlPanel: View {
    @ObservedObject var controller: SessionController

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Stream")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)

            Toggle("Sound", isOn: Binding(
                get: { !controller.audioMuted },
                set: { controller.setAudioMuted(!$0) }
            ))
            .toggleStyle(.switch)

            Picker("Frames", selection: Binding(
                get: { controller.frameRate },
                set: { controller.setFrameRate($0) }
            )) {
                ForEach(SessionController.FrameRateTier.allCases, id: \.rawValue) { tier in
                    Text(tier.label).tag(tier)
                }
            }
            .pickerStyle(.segmented)

            Picker("Keys", selection: $controller.selectedPreset) {
                ForEach(SessionController.HotkeyPreset.allCases) { preset in
                    Text(preset.label).tag(preset)
                }
            }
            .pickerStyle(.segmented)

            Picker("Touch", selection: $controller.touchMode) {
                ForEach(SessionController.TouchMode.allCases, id: \.rawValue) { mode in
                    Text(mode.label).tag(mode)
                }
            }
            .pickerStyle(.segmented)

            Button(role: .destructive) {
                controller.hangUp(lock: true)
            } label: {
                Label("Disconnect and lock", systemImage: "lock")
                    .font(.footnote)
            }
        }
        .padding(16)
        .frame(width: 260)
    }
}
