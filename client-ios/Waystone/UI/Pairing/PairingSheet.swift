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
        NavigationStack {
            VStack(spacing: 16) {
                if cameraAvailable && !typing {
                    QRScanner { code in submit(code) }
                        .clipShape(RoundedRectangle(cornerRadius: 16))
                        .overlay(alignment: .bottom) {
                            Text("对准服务器面板上的二维码")
                                .font(.footnote)
                                .padding(8)
                                .background(.ultraThinMaterial, in: Capsule())
                                .padding()
                        }
                } else {
                    manualEntry
                }

                if let hint {
                    Text(hint).font(.footnote).foregroundStyle(.orange)
                }

                HStack {
                    PasteButton(payloadType: String.self) { values in
                        if let value = values.first { submit(value) }
                    }
                    .labelStyle(.titleAndIcon)
                    if cameraAvailable {
                        Button(typing ? "用相机扫码" : "手动输入") { typing.toggle() }
                            .buttonStyle(.bordered)
                    }
                }
            }
            .padding()
            .navigationTitle("添加服务器")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("取消") { dismiss() }
                }
            }
        }
    }

    private var manualEntry: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("粘贴面板上的配对链接")
                .font(.footnote)
                .foregroundStyle(.secondary)
            TextField("waystone://pair?…", text: $manual, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .font(.system(.footnote, design: .monospaced))
                .lineLimit(3...6)
            Button("添加") { submit(manual) }
                .buttonStyle(.borderedProminent)
                .disabled(manual.isEmpty)
            Spacer()
        }
    }

    private func submit(_ text: String) {
        guard PairingLink(text) != nil else {
            hint = "这不是 Waystone 配对码。"
            return
        }
        model.pair(with: text)
        dismiss()
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
