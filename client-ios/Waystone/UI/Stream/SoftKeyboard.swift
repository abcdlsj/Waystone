import SwiftUI
import UIKit

/// The system keyboard, wired to the host, with a bar of the keys a phone
/// keyboard lacks (design doc 5.3: Esc, Tab, Win, Ctrl…).
///
/// An invisible `UIKeyInput` view becomes first responder; typed text goes out
/// as text frames, Return and Backspace as keys. Modifiers on the bar arm for
/// the next key, so Ctrl then C sends Ctrl+C.
struct SoftKeyboard: UIViewRepresentable {
    @Binding var visible: Bool
    let input: InputController

    func makeUIView(context: Context) -> KeyInputView {
        let view = KeyInputView()
        view.input = input
        view.onResign = { visible = false }
        let bar = UIHostingController(rootView: KeyboardBar(input: input) { visible = false })
        bar.view.backgroundColor = .black
        bar.view.frame = CGRect(x: 0, y: 0, width: 0, height: 44)
        bar.view.autoresizingMask = [.flexibleWidth]
        view.accessory = bar
        return view
    }

    func updateUIView(_ view: KeyInputView, context: Context) {
        if visible, !view.isFirstResponder {
            view.becomeFirstResponder()
        } else if !visible, view.isFirstResponder {
            view.resignFirstResponder()
        }
    }
}

final class KeyInputView: UIView, UIKeyInput {
    weak var input: InputController?
    var accessory: UIHostingController<KeyboardBar>?
    var onResign: (() -> Void)?

    override var canBecomeFirstResponder: Bool { true }
    override var inputAccessoryView: UIView? { accessory?.view }

    var hasText: Bool { true }
    var autocorrectionType: UITextAutocorrectionType { get { .no } set {} }
    var autocapitalizationType: UITextAutocapitalizationType { get { .none } set {} }
    var spellCheckingType: UITextSpellCheckingType { get { .no } set {} }
    var smartQuotesType: UITextSmartQuotesType { get { .no } set {} }
    var keyboardAppearance: UIKeyboardAppearance { get { .dark } set {} }

    func insertText(_ text: String) {
        MainActor.assumeIsolated {
            if text == "\n" {
                input?.tap(.enter)
            } else {
                input?.type(text)
            }
        }
    }

    func deleteBackward() {
        MainActor.assumeIsolated { input?.tap(.backspace) }
    }

    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { onResign?() }
        return resigned
    }
}

struct KeyboardBar: View {
    @ObservedObject var input: InputController
    var dismiss: () -> Void

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 5) {
                key("Esc") { input.tap(.escape) }
                key("Tab") { input.tap(.tab) }

                barDivider

                modifier("Ctrl", .control)
                modifier("Alt", .alt)
                modifier("Shift", .shift)
                modifier("Win", .meta)

                barDivider

                symbolKey("arrow.left", label: "左") { input.tap(.left) }
                symbolKey("arrow.up", label: "上") { input.tap(.up) }
                symbolKey("arrow.down", label: "下") { input.tap(.down) }
                symbolKey("arrow.right", label: "右") { input.tap(.right) }

                barDivider

                ForEach(1...12, id: \.self) { number in
                    key("F\(number)") { input.tap(KeyCode.function(number)!) }
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
        .safeAreaInset(edge: .trailing, spacing: 0) {
            HStack(spacing: 0) {
                Rectangle()
                    .fill(WaystoneTheme.borderSubtle)
                    .frame(width: 0.75, height: 20)
                Button {
                    Haptics.tap()
                    dismiss()
                } label: {
                    Image(systemName: "keyboard.chevron.compact.down")
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(WaystoneTheme.textPrimary)
                        .padding(.horizontal, 14)
                        .frame(maxHeight: .infinity)
                }
                .buttonStyle(ConsoleButtonStyle())
                .accessibilityLabel("收起键盘")
            }
            .background(WaystoneTheme.canvas)
        }
        .frame(height: 44)
        .background(WaystoneTheme.canvas)
        .overlay(alignment: .top) {
            Rectangle()
                .fill(WaystoneTheme.border)
                .frame(height: 0.75)
        }
    }

    private var barDivider: some View {
        Rectangle()
            .fill(WaystoneTheme.borderSubtle)
            .frame(width: 0.75, height: 16)
            .padding(.horizontal, 3)
    }

    private func key(_ title: String, action: @escaping () -> Void) -> some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            Text(title)
                .font(.system(size: 11.5, weight: .medium, design: .monospaced))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .frame(minWidth: 38, minHeight: 30)
                .padding(.horizontal, 6)
                .background(keycapSurface)
        }
        .buttonStyle(ConsoleButtonStyle())
    }

    private func symbolKey(_ systemName: String, label: String, action: @escaping () -> Void) -> some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            Image(systemName: systemName)
                .font(.system(size: 11.5, weight: .semibold))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .frame(width: 34, height: 30)
                .background(keycapSurface)
        }
        .buttonStyle(ConsoleButtonStyle())
        .accessibilityLabel(label)
    }

    private func modifier(_ title: String, _ modifier: Modifiers) -> some View {
        let armed = input.armedModifiers.contains(modifier)
        return Button {
            Haptics.selection()
            input.toggleArmed(modifier)
        } label: {
            Text(title)
                .font(.system(size: 11.5, weight: armed ? .semibold : .medium, design: .monospaced))
                .foregroundStyle(armed ? WaystoneTheme.amberInk : WaystoneTheme.textPrimary)
                .frame(minWidth: 42, minHeight: 30)
                .padding(.horizontal, 6)
                .background {
                    let shape = RoundedRectangle(cornerRadius: 4, style: .continuous)
                    if armed {
                        shape.fill(WaystoneTheme.amber)
                    } else {
                        keycapSurface
                    }
                }
        }
        .buttonStyle(ConsoleButtonStyle())
    }

    private var keycapSurface: some View {
        RoundedRectangle(cornerRadius: 4, style: .continuous)
            .fill(WaystoneTheme.surfaceRaised)
            .overlay(
                RoundedRectangle(cornerRadius: 4, style: .continuous)
                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
            )
    }
}
