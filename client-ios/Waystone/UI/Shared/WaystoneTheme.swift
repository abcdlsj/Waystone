import SwiftUI

/// Waystone pure-black monochrome console visual tokens and shared controls.
enum WaystoneTheme {
    // Surfaces — pure OLED black with restrained neutral elevation steps.
    static let canvas = Color.black
    static let ledger = Color(white: 0.02)
    static let surface = Color(white: 0.043)
    static let surfaceRaised = Color(white: 0.078)
    static let hudSurface = Color(white: 0.024)
    static let footerSurface = Color(white: 0.012)

    // Structural hairlines
    static let borderSubtle = Color(white: 0.11)
    static let border = Color(white: 0.15)
    static let borderStrong = Color(white: 0.23)

    // Ink & high-contrast monochrome primary accent
    static let textPrimary = Color(white: 0.96)
    static let textSecondary = Color(white: 0.54)
    static let textTertiary = Color(white: 0.31)
    static let textQuaternary = Color(white: 0.18)
    static let accent = Color.white
    static let accentInk = Color.black

    // Functional telemetry & alert colors
    static let emerald = Color(red: 0.133, green: 0.773, blue: 0.369)
    static let amber = Color(red: 0.961, green: 0.620, blue: 0.043)
    static let amberInk = Color.black
    static let coral = Color(red: 0.863, green: 0.149, blue: 0.149)

    /// Latency tint used on home telemetry and stream status readouts.
    static func latencyColor(for rttMs: UInt32) -> Color {
        switch rttMs {
        case ..<35: return emerald
        case ..<90: return amber
        default: return coral
        }
    }
}

/// Standing boundary stone silhouette matching `docs/brand/icon.svg`.
struct WaystoneMark: Shape {
    func path(in rect: CGRect) -> Path {
        let sx = rect.width / 1024
        let sy = rect.height / 1024
        func pt(_ x: CGFloat, _ y: CGFloat) -> CGPoint {
            CGPoint(x: rect.minX + x * sx, y: rect.minY + y * sy)
        }
        var path = Path()
        // Outer monolith
        path.move(to: pt(330, 840))
        path.addLine(to: pt(366, 352))
        path.addCurve(to: pt(658, 352), control1: pt(380, 190), control2: pt(644, 190))
        path.addLine(to: pt(694, 840))
        path.closeSubpath()

        // Carved play mark (even-odd fill rule cuts this out)
        path.move(to: pt(462, 488))
        path.addLine(to: pt(462, 652))
        path.addLine(to: pt(598, 570))
        path.closeSubpath()
        return path
    }
}

/// Immediate press feedback without spring bounce.
struct ConsoleButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .opacity(configuration.isPressed ? 0.70 : 1)
            .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
    }
}

/// High-contrast monochrome segmented control for landscape stream HUDs.
///
/// Renders each option as a distinct `Button` so both VoiceOver and UI tests
/// address segments by their exact titles.
struct WaystoneSegmentedControl<Option: Identifiable & Equatable>: View {
    let options: [Option]
    @Binding var selection: Option
    let title: (Option) -> String

    @Namespace private var indicator

    var body: some View {
        HStack(spacing: 2) {
            ForEach(options) { option in
                let selected = option == selection
                Button {
                    guard !selected else { return }
                    Haptics.selection()
                    withAnimation(.easeOut(duration: 0.12)) {
                        selection = option
                    }
                } label: {
                    Text(title(option))
                        .font(.system(size: 11.5, weight: selected ? .semibold : .medium))
                        .lineLimit(1)
                        .minimumScaleFactor(0.84)
                        .foregroundStyle(selected ? WaystoneTheme.accentInk : WaystoneTheme.textSecondary)
                        .frame(maxWidth: .infinity)
                        .frame(height: 26)
                        .padding(.horizontal, 6)
                        .background {
                            if selected {
                                RoundedRectangle(cornerRadius: 3.5, style: .continuous)
                                    .fill(WaystoneTheme.accent)
                                    .matchedGeometryEffect(id: "segment-pill", in: indicator)
                            }
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(selected ? [.isSelected] : [])
            }
        }
        .padding(2)
        .background(
            RoundedRectangle(cornerRadius: 5, style: .continuous)
                .fill(Color.black)
                .overlay(
                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                        .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                )
        )
    }
}

/// Crisp 2 pt linear console progress bar used during connection setup.
struct ConsoleProgressBar: View {
    var body: some View {
        TimelineView(.animation(minimumInterval: 1.0 / 60.0)) { context in
            let cycle = 1.4
            let t = context.date.timeIntervalSinceReferenceDate
                .truncatingRemainder(dividingBy: cycle) / cycle
            let eased = t < 0.5 ? 2 * t * t : 1 - pow(-2 * t + 2, 2) / 2
            GeometryReader { proxy in
                let width = proxy.size.width
                let barWidth = width * 0.36
                let x = -barWidth + (width + barWidth) * eased
                ZStack(alignment: .leading) {
                    Rectangle().fill(WaystoneTheme.borderSubtle)
                    Rectangle()
                        .fill(WaystoneTheme.accent)
                        .frame(width: barWidth)
                        .offset(x: x)
                }
            }
        }
        .frame(height: 2)
        .clipped()
    }
}

extension View {
    /// Flat matte-black console panel with a crisp 0.75 pt hairline border.
    func consolePanel(cornerRadius: CGFloat = 8, opacity: Double = 0.96) -> some View {
        self
            .background(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .fill(WaystoneTheme.hudSurface.opacity(opacity))
            )
            .clipShape(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .strokeBorder(WaystoneTheme.borderStrong, lineWidth: 0.75)
            )
    }
}
