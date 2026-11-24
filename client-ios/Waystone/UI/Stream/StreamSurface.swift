import SwiftUI
import UIKit

/// The remote picture plus the raw touch handling on top of it.
///
/// UIKit touches are used instead of SwiftUI gestures because the touch
/// engines need every phase of every finger with its exact timestamp.
struct StreamSurface: UIViewRepresentable {
    let session: StreamSession
    let geometry: PictureGeometry

    func makeUIView(context: Context) -> TouchSurfaceView {
        let view = TouchSurfaceView(input: session.input)
        session.attach(view.videoView)
        return view
    }

    func updateUIView(_ view: TouchSurfaceView, context: Context) {
        view.place(geometry)
    }
}

final class TouchSurfaceView: UIView {
    let videoView = VideoView(frame: .zero)
    private let input: InputController
    private var tickTimer: Timer?

    init(input: InputController) {
        self.input = input
        super.init(frame: .zero)
        backgroundColor = .black
        isMultipleTouchEnabled = true
        addSubview(videoView)
    }

    required init?(coder: NSCoder) { fatalError("not used from a storyboard") }

    override func layoutSubviews() {
        super.layoutSubviews()
        MainActor.assumeIsolated { input.layout(bounds: bounds.size) }
        place(MainActor.assumeIsolated { input.geometry })
    }

    /// Positions the picture where the geometry says it is drawn.
    func place(_ geometry: PictureGeometry) {
        let frame = geometry.displayedRect
        guard frame.width > 0, videoView.frame != frame else { return }
        // The drawable keeps the unzoomed size: zooming magnifies the stream's
        // own pixels, so a larger drawable would only cost memory.
        UIView.performWithoutAnimation { videoView.frame = frame }
    }

    // MARK: - Touches

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        MainActor.assumeIsolated {
            for touch in touches {
                input.touchBegan(Self.id(touch), at: touch.location(in: self), time: touch.timestamp)
            }
            scheduleTick()
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        MainActor.assumeIsolated {
            var points: [Int: CGPoint] = [:]
            var time: TimeInterval = 0
            for touch in event?.allTouches ?? touches where touch.view === self {
                points[Self.id(touch)] = touch.location(in: self)
                time = max(time, touch.timestamp)
            }
            input.touchesMoved(points, time: time)
            scheduleTick()
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        MainActor.assumeIsolated {
            for touch in touches {
                input.touchEnded(Self.id(touch), at: touch.location(in: self), time: touch.timestamp)
            }
            scheduleTick()
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        MainActor.assumeIsolated {
            input.touchesCancelled()
            scheduleTick()
        }
    }

    /// Long presses fire without movement, so the engine's deadline is turned
    /// into a timer. `UITouch.timestamp` and `systemUptime` share a clock.
    @MainActor
    private func scheduleTick() {
        tickTimer?.invalidate()
        tickTimer = nil
        guard let deadline = input.deadline else { return }
        let delay = max(0, deadline - ProcessInfo.processInfo.systemUptime)
        tickTimer = Timer.scheduledTimer(withTimeInterval: delay, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.input.tick(time: ProcessInfo.processInfo.systemUptime)
                self.scheduleTick()
            }
        }
    }

    private static func id(_ touch: UITouch) -> Int {
        ObjectIdentifier(touch).hashValue
    }
}
