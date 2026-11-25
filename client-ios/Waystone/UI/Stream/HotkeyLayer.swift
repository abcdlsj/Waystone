import SwiftUI

/// Renders a `HotkeyPreset`: keycaps, edge zones and the virtual joystick.
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
                    .padding(.leading, 36)
                    .padding(.bottom, 26)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
            }
        }
    }
}

private struct CapsuleGroup: View {
    let group: HotkeyGroup
    @ObservedObject var input: InputController

    var body: some View {
        layout
            .padding(padding)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: alignment)
    }

    @ViewBuilder
    private var layout: some View {
        switch group.anchor {
        case .leading, .trailing:
            VStack(spacing: 8) {
                ForEach(group.buttons) { button in
                    HotkeyCapsule(button: button, style: .sideStack, input: input)
                }
            }
        case .top:
            HStack(spacing: 8) {
                ForEach(group.buttons) { button in
                    HotkeyCapsule(button: button, style: .topPill, input: input)
                }
            }
        case .bottomTrailing:
            ThumbCluster(buttons: group.buttons, input: input)
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
        case .trailing: return EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 54)
        case .leading: return EdgeInsets(top: 0, leading: 14, bottom: 0, trailing: 0)
        case .top: return EdgeInsets(top: 12, leading: 0, bottom: 0, trailing: 0)
        case .bottomTrailing: return EdgeInsets(top: 0, leading: 0, bottom: 20, trailing: 54)
        }
    }
}

/// Buttons in reading order, in rows of four, each row against the trailing
/// edge. A preset lists its secondary keys first so the last, possibly short
/// row holds the main actions right under the thumb.
private struct ThumbCluster: View {
    static let perRow = 4
    let buttons: [HotkeyButton]
    @ObservedObject var input: InputController

    var body: some View {
        VStack(alignment: .trailing, spacing: 7) {
            ForEach(rows, id: \.self) { range in
                HStack(spacing: 7) {
                    ForEach(buttons[range]) { button in
                        HotkeyCapsule(button: button, style: .thumbKey, input: input)
                    }
                }
            }
        }
    }

    /// Seven buttons give rows 0..<4 and 4..<7.
    private var rows: [Range<Int>] {
        stride(from: 0, to: buttons.count, by: Self.perRow).map { $0..<min($0 + Self.perRow, buttons.count) }
    }
}

/// Flat translucent black keycap. Toggles highlight in solid amber while latched.
private struct HotkeyCapsule: View {
    enum Style {
        case sideStack
        case topPill
        case thumbKey
    }

    let button: HotkeyButton
    let style: Style
    @ObservedObject var input: InputController
    @State private var pressed = false

    private var latched: Bool {
        if case .toggle(let key) = button.action { return input.latchedKeys.contains(key) }
        return false
    }

    var body: some View {
        labelContent
            .foregroundStyle(latched ? WaystoneTheme.amberInk : WaystoneTheme.textPrimary)
            .frame(minWidth: minWidth, minHeight: minHeight)
            .background { backgroundShape }
            .overlay { borderOverlay }
            .opacity(pressed || latched ? 0.96 : 0.84)
            .contentShape(hitShape)
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { _ in
                        guard !pressed else { return }
                        pressed = true
                        if case .click = button.action {
                            // `InputController.perform(.click)` emits its own haptic.
                        } else {
                            Haptics.tap()
                        }
                        input.perform(button.action, down: true)
                    }
                    .onEnded { _ in
                        pressed = false
                        input.perform(button.action, down: false)
                    }
            )
            .animation(.easeOut(duration: 0.08), value: pressed)
            .animation(.easeOut(duration: 0.12), value: latched)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(button.title)
            .accessibilityValue(latched ? "已按住" : "")
            .accessibilityAddTraits(.isButton)
    }

    @ViewBuilder
    private var labelContent: some View {
        switch style {
        case .topPill:
            HStack(spacing: 7) {
                Text(button.title)
                    .font(.system(size: 11.5, weight: .semibold))
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
                if let cap = button.cap {
                    Text(cap.uppercased())
                        .font(.system(size: 9.5, weight: .medium, design: .monospaced))
                        .foregroundStyle(latched ? WaystoneTheme.amberInk.opacity(0.75) : WaystoneTheme.textSecondary)
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                }
            }
            .fixedSize(horizontal: true, vertical: false)
            .padding(.horizontal, 12)
            .padding(.vertical, 5)

        case .sideStack:
            VStack(spacing: 1.5) {
                Text(button.title)
                    .font(.system(size: 11.5, weight: .semibold))
                    .lineLimit(1)
                if let cap = button.cap {
                    Text(cap.uppercased())
                        .font(.system(size: 9, weight: .medium, design: .monospaced))
                        .foregroundStyle(latched ? WaystoneTheme.amberInk.opacity(0.75) : WaystoneTheme.textSecondary)
                        .lineLimit(1)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)

        case .thumbKey:
            if let cap = button.cap {
                VStack(spacing: 1.5) {
                    Text(button.title)
                        .font(.system(size: 11.5, weight: .semibold))
                        .lineLimit(1)
                    Text(cap.uppercased())
                        .font(.system(size: 9, weight: .medium, design: .monospaced))
                        .foregroundStyle(latched ? WaystoneTheme.amberInk.opacity(0.75) : WaystoneTheme.textSecondary)
                        .lineLimit(1)
                }
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
            } else {
                Text(button.title)
                    .font(.system(size: 12.5, weight: .semibold, design: .monospaced))
                    .lineLimit(1)
            }
        }
    }

    private var minWidth: CGFloat {
        switch style {
        case .sideStack: return 74
        case .topPill: return 80
        case .thumbKey: return button.cap == nil ? 40 : 58
        }
    }

    private var minHeight: CGFloat {
        switch style {
        case .sideStack: return 40
        case .topPill: return 28
        case .thumbKey: return button.cap == nil ? 34 : 40
        }
    }

    private var cornerRadius: CGFloat { 6 }

    private var hitShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
    }

    @ViewBuilder
    private var backgroundShape: some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
        if latched {
            shape.fill(WaystoneTheme.amber)
        } else {
            shape.fill(Color.black.opacity(pressed ? 0.92 : 0.78))
        }
    }

    @ViewBuilder
    private var borderOverlay: some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
        if latched {
            shape.strokeBorder(WaystoneTheme.amber, lineWidth: 0.75)
        } else {
            shape.strokeBorder(Color.white.opacity(0.22), lineWidth: 0.75)
        }
    }
}

/// A nearly invisible strip that thumbs can hit without looking.
private struct EdgeZoneView: View {
    let zone: EdgeZone
    @ObservedObject var input: InputController
    @State private var flash = false

    var body: some View {
        LinearGradient(
            colors: [
                Color.white.opacity(flash ? 0.14 : 0.01),
                .clear,
            ],
            startPoint: zone.side == .leading ? .leading : .trailing,
            endPoint: zone.side == .leading ? .trailing : .leading)
            .frame(width: zone.width)
            .contentShape(Rectangle())
            // Same touch handling as the capsules: fire on touch-down, which a
            // SwiftUI tap on a near-transparent strip does not reliably do.
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { _ in
                        guard !flash else { return }
                        flash = true
                        input.perform(zone.action, down: true)
                    }
                    .onEnded { _ in
                        input.perform(zone.action, down: false)
                        withAnimation(.easeOut(duration: 0.2)) { flash = false }
                    }
            )
            .accessibilityElement()
            .accessibilityLabel("下一句")
            .accessibilityAddTraits(.isButton)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: zone.side == .leading ? .leading : .trailing)
            // A thumb finds the physical edge, not the safe area's.
            .ignoresSafeArea()
    }
}

/// An on-screen stick that holds WASD-style keys in eight directions.
struct JoystickView: View {
    let layout: JoystickLayout
    @ObservedObject var input: InputController
    @State private var knob: CGSize = .zero
    @State private var active = false

    private let radius: CGFloat = 54
    private var allKeys: Set<KeyCode> { [layout.up, layout.left, layout.down, layout.right] }

    var body: some View {
        let activeKeys = keys(for: knob)
        ZStack {
            Circle()
                .fill(Color.black.opacity(active ? 0.72 : 0.58))
                .overlay(Circle().strokeBorder(Color.white.opacity(0.22), lineWidth: 0.75))
                .frame(width: radius * 2, height: radius * 2)

            Circle()
                .strokeBorder(.white.opacity(active ? 0.14 : 0.08), lineWidth: 0.75)
                .frame(width: radius * 0.88, height: radius * 0.88)

            cardinalTick(engaged: activeKeys.contains(layout.up), offset: CGSize(width: 0, height: -(radius - 10)))
            cardinalTick(engaged: activeKeys.contains(layout.down), offset: CGSize(width: 0, height: radius - 10))
            cardinalTick(engaged: activeKeys.contains(layout.left), offset: CGSize(width: -(radius - 10), height: 0))
            cardinalTick(engaged: activeKeys.contains(layout.right), offset: CGSize(width: radius - 10, height: 0))

            Circle()
                .fill(Color(white: active ? 0.86 : 0.60))
                .overlay(Circle().strokeBorder(Color.black, lineWidth: 1))
                .frame(width: 34, height: 34)
                .offset(knob)
        }
        .contentShape(Circle().scale(1.4))
        .gesture(
            DragGesture(minimumDistance: 0)
                .onChanged { value in
                    if !active { active = true }
                    let vector = clamp(value.translation)
                    knob = vector
                    input.hold(keys(for: vector), among: allKeys)
                }
                .onEnded { _ in
                    withAnimation(.easeOut(duration: 0.14)) {
                        knob = .zero
                        active = false
                    }
                    input.hold([], among: allKeys)
                }
        )
        .animation(.easeOut(duration: 0.1), value: active)
        .accessibilityLabel("方向摇杆")
    }

    private func cardinalTick(engaged: Bool, offset: CGSize) -> some View {
        Circle()
            .fill(engaged ? Color.white : .white.opacity(0.24))
            .frame(width: engaged ? 5 : 3.5, height: engaged ? 5 : 3.5)
            .offset(offset)
            .animation(.easeOut(duration: 0.08), value: engaged)
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
