import SwiftUI
import VisionKit

/// Add a server: scan the panel's QR code, or paste the link.
///
/// The camera is the default because it needs no typing at all; pasting is
/// offered as soon as the clipboard holds a pairing link (copied from the
/// panel on the same phone), and always as a fallback.
struct PairingSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var manual = ""
    @State private var typing = false
    @State private var hint: String?

    private var cameraAvailable: Bool {
        DataScannerViewController.isSupported && DataScannerViewController.isAvailable
    }

    var body: some View {
        VStack(spacing: 0) {
            sheetHeader

            VStack(spacing: 14) {
                if cameraAvailable && !typing {
                    scannerCard
                } else {
                    manualEntry
                }

                if let hint {
                    HStack(spacing: 8) {
                        Image(systemName: "exclamationmark.triangle")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(WaystoneTheme.coral)
                        Text(hint)
                            .font(.system(size: 12.5, weight: .medium))
                            .foregroundStyle(WaystoneTheme.textPrimary)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(WaystoneTheme.surface)
                            .overlay(
                                RoundedRectangle(cornerRadius: 6, style: .continuous)
                                    .strokeBorder(WaystoneTheme.coral.opacity(0.55), lineWidth: 0.75)
                            )
                    )
                    .transition(.opacity)
                }

                actionBar

                Text("在面板「添加我的设备」中生成配对码")
                    .font(.system(size: 11.5))
                    .foregroundStyle(WaystoneTheme.textTertiary)
                    .multilineTextAlignment(.center)
                    .padding(.bottom, 4)
            }
            .padding(18)
            .animation(.easeOut(duration: 0.14), value: hint)
        }
        .background(WaystoneTheme.canvas.ignoresSafeArea())
    }

    private var sheetHeader: some View {
        VStack(spacing: 0) {
            ZStack {
                HStack {
                    Button("取消") { dismiss() }
                        .font(.system(size: 13.5))
                        .foregroundStyle(WaystoneTheme.textSecondary)
                    Spacer()
                }
                Text("添加服务器")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(WaystoneTheme.textPrimary)
            }
            .padding(.horizontal, 18)
            .frame(height: 48)

            Rectangle()
                .fill(WaystoneTheme.borderSubtle)
                .frame(height: 0.75)
        }
    }

    private var scannerCard: some View {
        QRScanner { code in submit(code) }
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
            .overlay {
                ScanReticleOverlay()
            }
            .overlay(alignment: .bottom) {
                Text("对准二维码")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(WaystoneTheme.textPrimary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                    .background(
                        RoundedRectangle(cornerRadius: 5, style: .continuous)
                            .fill(Color.black.opacity(0.88))
                            .overlay(
                                RoundedRectangle(cornerRadius: 5, style: .continuous)
                                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                            )
                    )
                    .padding(.bottom, 14)
            }
            .overlay(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
            )
    }

    private var manualEntry: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("配对链接")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(WaystoneTheme.textSecondary)

            TextField("waystone://pair?…", text: $manual, axis: .vertical)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .font(.system(size: 12.5, design: .monospaced))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .lineLimit(4...6)
                .padding(12)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(WaystoneTheme.hudSurface)
                        .overlay(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                        )
                )

            Button {
                Haptics.tap()
                submit(manual)
            } label: {
                Text("添加服务器")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(manual.isEmpty ? WaystoneTheme.textTertiary : WaystoneTheme.accentInk)
                    .frame(maxWidth: .infinity)
                    .frame(height: 44)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(manual.isEmpty ? WaystoneTheme.surfaceRaised : WaystoneTheme.accent)
                    )
            }
            .buttonStyle(ConsoleButtonStyle())
            .disabled(manual.isEmpty)

            Spacer(minLength: 0)
        }
        .frame(maxHeight: .infinity, alignment: .top)
    }

    private var actionBar: some View {
        HStack(spacing: 10) {
            PasteButton(payloadType: String.self) { values in
                if let value = values.first { submit(value) }
            }
            .buttonBorderShape(.roundedRectangle(radius: 6))
            .tint(WaystoneTheme.surfaceRaised)

            if cameraAvailable {
                Button {
                    Haptics.selection()
                    withAnimation(.easeOut(duration: 0.14)) {
                        typing.toggle()
                        hint = nil
                    }
                } label: {
                    Text(typing ? "扫码" : "手动输入")
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(WaystoneTheme.textPrimary)
                        .frame(maxWidth: .infinity)
                        .frame(height: 38)
                        .background(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .fill(WaystoneTheme.surfaceRaised)
                                .overlay(
                                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                                        .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                                )
                        )
                }
                .buttonStyle(ConsoleButtonStyle())
            }
        }
    }

    private func submit(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard PairingLink(trimmed) != nil else {
            hint = "配对码无效"
            return
        }
        hint = nil
        model.pair(with: trimmed)
        dismiss()
    }
}

/// Four sharp right-angle corner brackets guiding QR placement.
private struct ScanReticleOverlay: View {
    var body: some View {
        GeometryReader { proxy in
            let side = min(proxy.size.width, proxy.size.height) * 0.58
            ReticleCorners(length: 18)
                .stroke(Color.white.opacity(0.85), style: StrokeStyle(lineWidth: 2, lineCap: .square))
                .frame(width: side, height: side)
                .position(x: proxy.size.width / 2, y: proxy.size.height / 2 - 10)
        }
        .allowsHitTesting(false)
    }
}

private struct ReticleCorners: Shape {
    var length: CGFloat

    func path(in rect: CGRect) -> Path {
        var p = Path()
        let l = length

        p.move(to: CGPoint(x: rect.minX, y: rect.minY + l))
        p.addLine(to: CGPoint(x: rect.minX, y: rect.minY))
        p.addLine(to: CGPoint(x: rect.minX + l, y: rect.minY))

        p.move(to: CGPoint(x: rect.maxX - l, y: rect.minY))
        p.addLine(to: CGPoint(x: rect.maxX, y: rect.minY))
        p.addLine(to: CGPoint(x: rect.maxX, y: rect.minY + l))

        p.move(to: CGPoint(x: rect.maxX, y: rect.maxY - l))
        p.addLine(to: CGPoint(x: rect.maxX, y: rect.maxY))
        p.addLine(to: CGPoint(x: rect.maxX - l, y: rect.maxY))

        p.move(to: CGPoint(x: rect.minX + l, y: rect.maxY))
        p.addLine(to: CGPoint(x: rect.minX, y: rect.maxY))
        p.addLine(to: CGPoint(x: rect.minX, y: rect.maxY - l))

        return p
    }
}

/// `DataScannerViewController` limited to QR codes.
private struct QRScanner: UIViewControllerRepresentable {
    var onCode: (String) -> Void

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            isHighlightingEnabled: true)
        scanner.delegate = context.coordinator
        try? scanner.startScanning()
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {}

    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) {
        scanner.stopScanning()
    }

    func makeCoordinator() -> Coordinator { Coordinator(onCode: onCode) }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let onCode: (String) -> Void
        private var delivered = false

        init(onCode: @escaping (String) -> Void) { self.onCode = onCode }

        func dataScanner(
            _ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]
        ) {
            guard !delivered else { return }
            for case .barcode(let barcode) in items {
                guard let value = barcode.payloadStringValue, PairingLink(value) != nil else { continue }
                delivered = true
                UINotificationFeedbackGenerator().notificationOccurred(.success)
                onCode(value)
                return
            }
        }
    }
}
