import SwiftUI

/// The only persistent control on a stream (design doc 5.3).
///
/// A 36 pt ball docked to a side edge. It can be dragged along and across
/// the screen and snaps to the nearest side; after 3 s untouched it tucks half
/// behind the edge at 30 % opacity so it never covers the game. Its dot shows
/// the connection health.
struct FloatingBall: View {
    var health: Health
    var onTap: () -> Void

    enum Health {
        case good, degraded, stalled

        var color: Color {
            switch self {
            case .good: return WaystoneTheme.emerald
            case .degraded: return WaystoneTheme.amber
            case .stalled: return WaystoneTheme.coral
            }
        }
    }

    @AppStorage("ball.trailing") private var trailing = true
    @AppStorage("ball.height") private var height = 0.5
    @State private var drag: CGSize?
    @State private var tucked = false
    @State private var lastTouch = Date()
    @State private var pulse = false

    static let size: CGFloat = 36

    var body: some View {
        GeometryReader { proxy in
            let area = proxy.size
            ball
                .position(position(in: area))
                .gesture(
                    DragGesture(minimumDistance: 4)
                        .onChanged { value in
                            touched()
                            drag = value.translation
                        }
                        .onEnded { _ in
                            let end = position(in: area)
                            trailing = end.x > area.width / 2
                            height = Double(min(max(end.y / area.height, 0.08), 0.92))
                            withAnimation(.easeOut(duration: 0.16)) { drag = nil }
                        }
                )
                .simultaneousGesture(TapGesture().onEnded {
                    touched()
                    Haptics.tap()
                    onTap()
                })
        }
        .task(id: lastTouch) {
            try? await Task.sleep(for: .seconds(3))
            withAnimation(.easeInOut(duration: 0.25)) { tucked = true }
        }
        .onAppear { pulse = true }
    }

    private var ball: some View {
        Circle()
            .fill(Color.black.opacity(0.88))
            .overlay(Circle().strokeBorder(WaystoneTheme.borderStrong, lineWidth: 0.75))
            .overlay(
                Image(systemName: "line.3.horizontal")
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(WaystoneTheme.textPrimary.opacity(0.92))
            )
            .overlay(alignment: .topTrailing) {
                Circle()
                    .fill(health.color)
                    .frame(width: 8, height: 8)
                    .overlay(Circle().strokeBorder(Color.black, lineWidth: 1.5))
                    .opacity(health == .good || pulse ? 1 : 0.30)
                    .animation(
                        health == .good ? .default : .easeInOut(duration: 0.75).repeatForever(autoreverses: true),
                        value: pulse)
            }
            .frame(width: Self.size, height: Self.size)
            .opacity(tucked && drag == nil ? 0.30 : 0.94)
            .contentShape(Circle().inset(by: -10))
            .accessibilityLabel("串流菜单")
            .accessibilityAddTraits(.isButton)
    }

    private func position(in area: CGSize) -> CGPoint {
        let half = Self.size / 2
        let inset: CGFloat = tucked && drag == nil ? -half / 2 : 8
        var x = trailing ? area.width - half - inset : half + inset
        var y = area.height * height
        if let drag {
            x += drag.width
            y += drag.height
        }
        // Tucked, half the ball sits past the edge on purpose.
        let edge: CGFloat = tucked && drag == nil ? 0 : half
        return CGPoint(x: min(max(x, edge), area.width - edge), y: min(max(y, half), area.height - half))
    }

    private func touched() {
        lastTouch = Date()
        withAnimation(.easeOut(duration: 0.12)) { tucked = false }
    }
}
