import SwiftUI

/// Renders a `HotkeyPreset`: capsules, edge zones and the joystick.
struct HotkeyLayer: View {
    let preset: HotkeyPreset
    @ObservedObject var input: InputController

    var body: some View {
        ZStack {
            ForEach(preset.edgeZones) { zone in
                EdgeZoneView(zone: zone, input: input)
            }
            ForEach(preset.groups) { group in
                CapsuleGroup(group: group, input: input)
            }
            if let joystick = preset.joystick {
                JoystickView(layout: joystick, input: input)
                    .padding(.leading, 40)
                    .padding(.bottom, 28)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
            }
        }
    }
}

private struct CapsuleGroup: View {
    let group: HotkeyGroup
    @ObservedObject var input: InputController

    var body: some View {
        layout {
            ForEach(group.buttons) { button in
                HotkeyCapsule(button: button, input: input)
            }
        }
        .padding(padding)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: alignment)
    }

    @ViewBuilder
    private func layout<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        switch group.anchor {
        case .leading, .trailing:
            VStack(spacing: 10, content: content)
        case .top:
            HStack(spacing: 10, content: content)
        case .bottomTrailing:
            LazyVGrid(columns: Array(repeating: GridItem(.fixed(58), spacing: 8), count: 4), spacing: 8, content: content)
                .frame(width: 4 * 58 + 3 * 8)
                .environment(\.layoutDirection, .rightToLeft)
        }
    }

    private var alignment: Alignment {
        switch group.anchor {
        case .leading: return .leading
        case .trailing: return .trailing
        case .top: return .top
        case .bottomTrailing: return .bottomTrailing
        }
    }

    private var padding: EdgeInsets {
        switch group.anchor {
        // Clear of the floating ball, which lives on the trailing edge.
        case .trailing: return EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 56)
        case .leading: return EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 0)
        case .top: return EdgeInsets(top: 10, leading: 0, bottom: 0, trailing: 0)
        case .bottomTrailing: return EdgeInsets(top: 0, leading: 0, bottom: 20, trailing: 60)
        }
    }
}

/// A translucent key capsule. Toggles glow while latched.
private struct HotkeyCapsule: View {
    let button: HotkeyButton
    @ObservedObject var input: InputController
    @State private var pressed = false

    private var latched: Bool {
        if case .toggle(let key) = button.action { return input.latchedKeys.contains(key) }
        return false
    }

    var body: some View {
        VStack(spacing: 1) {
            Text(button.title)
                .font(.caption.weight(.semibold))
            if let cap = button.cap {
                Text(cap)
                    .font(.system(size: 9, weight: .medium, design: .rounded))
                    .opacity(0.7)
            }
        }
        .foregroundStyle(latched ? Color.black : .white)
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .frame(minWidth: 58, minHeight: 40)
        .background {
            Capsule().fill(latched ? AnyShapeStyle(Color.yellow) : AnyShapeStyle(.ultraThinMaterial))
        }
        .overlay(Capsule().strokeBorder(.white.opacity(0.18)))
        .opacity(pressed ? 0.95 : 0.72)
        .scaleEffect(pressed ? 0.94 : 1)
        .contentShape(Capsule())
        .gesture(
            DragGesture(minimumDistance: 0)
                .onChanged { _ in
                    guard !pressed else { return }
                    pressed = true
                    input.perform(button.action, down: true)
                }
                .onEnded { _ in
                    pressed = false
                    input.perform(button.action, down: false)
                }
        )
        .animation(.easeOut(duration: 0.08), value: pressed)
        .accessibilityLabel(button.title)
        .accessibilityAddTraits(.isButton)
    }
}

/// A nearly invisible strip that thumbs can hit without looking.
private struct EdgeZoneView: View {
    let zone: EdgeZone
    @ObservedObject var input: InputController
    @State private var flash = false

    var body: some View {
        Rectangle()
            .fill(Color.white.opacity(flash ? 0.12 : 0.015))
            .frame(width: zone.width)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: zone.side == .leading ? .leading : .trailing)
            .onTapGesture {
                input.perform(zone.action, down: true)
                input.perform(zone.action, down: false)
                flash = true
                withAnimation(.easeOut(duration: 0.25)) { flash = false }
            }
            .accessibilityLabel("下一句")
    }
}

/// An on-screen stick that holds WASD-style keys in eight directions.
struct JoystickView: View {
    let layout: JoystickLayout
    @ObservedObject var input: InputController
    @State private var knob: CGSize = .zero

    private let radius: CGFloat = 56
    private var allKeys: Set<KeyCode> { [layout.up, layout.left, layout.down, layout.right] }

    var body: some View {
        ZStack {
            Circle()
                .fill(.ultraThinMaterial)
                .overlay(Circle().strokeBorder(.white.opacity(0.18)))
                .frame(width: radius * 2, height: radius * 2)
                .opacity(0.55)
            Circle()
                .fill(.white.opacity(0.75))
                .frame(width: 44, height: 44)
                .offset(knob)
        }
        .contentShape(Circle().scale(1.4))
        .gesture(
            DragGesture(minimumDistance: 0)
                .onChanged { value in
                    let vector = clamp(value.translation)
                    knob = vector
                    input.hold(keys(for: vector), among: allKeys)
                }
                .onEnded { _ in
                    withAnimation(.spring(duration: 0.2)) { knob = .zero }
                    input.hold([], among: allKeys)
                }
        )
        .accessibilityLabel("方向摇杆")
    }

    private func clamp(_ vector: CGSize) -> CGSize {
        let length = hypot(vector.width, vector.height)
        guard length > radius else { return vector }
        return CGSize(width: vector.width / length * radius, height: vector.height / length * radius)
    }

    /// Eight-way: a direction counts once it is past a third of the radius.
    private func keys(for vector: CGSize) -> Set<KeyCode> {
        let threshold = radius / 3
        var keys: Set<KeyCode> = []
        if vector.height < -threshold { keys.insert(layout.up) }
        if vector.height > threshold { keys.insert(layout.down) }
        if vector.width < -threshold { keys.insert(layout.left) }
        if vector.width > threshold { keys.insert(layout.right) }
        return keys
    }
}
