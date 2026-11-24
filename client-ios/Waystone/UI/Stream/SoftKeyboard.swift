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
        bar.view.backgroundColor = .clear
        bar.view.frame = CGRect(x: 0, y: 0, width: 0, height: 48)
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
            HStack(spacing: 6) {
                key("Esc") { input.tap(.escape) }
                key("Tab") { input.tap(.tab) }
                modifier("Ctrl", .control)
                modifier("Alt", .alt)
                modifier("Shift", .shift)
                modifier("Win", .meta)
                key("←") { input.tap(.left) }
                key("↑") { input.tap(.up) }
                key("↓") { input.tap(.down) }
                key("→") { input.tap(.right) }
                ForEach(1...12, id: \.self) { number in
                    key("F\(number)") { input.tap(KeyCode.function(number)!) }
                }
            }
            .padding(.horizontal, 8)
        }
        .safeAreaInset(edge: .trailing) {
            Button(action: dismiss) {
                Image(systemName: "keyboard.chevron.compact.down")
                    .padding(.horizontal, 12)
                    .frame(maxHeight: .infinity)
            }
            .background(.bar)
            .accessibilityLabel("收起键盘")
        }
        .frame(height: 44)
        .background(.bar)
    }

    private func key(_ title: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(.callout.weight(.medium))
                .frame(minWidth: 40, minHeight: 34)
                .padding(.horizontal, 4)
                .background(Color.white.opacity(0.12), in: RoundedRectangle(cornerRadius: 7))
        }
        .buttonStyle(.plain)
    }

    private func modifier(_ title: String, _ modifier: Modifiers) -> some View {
        let armed = input.armedModifiers.contains(modifier)
        return Button {
            input.toggleArmed(modifier)
        } label: {
            Text(title)
                .font(.callout.weight(.medium))
                .foregroundStyle(armed ? Color.black : .primary)
                .frame(minWidth: 44, minHeight: 34)
                .padding(.horizontal, 4)
                .background(armed ? Color.yellow : Color.white.opacity(0.12), in: RoundedRectangle(cornerRadius: 7))
        }
        .buttonStyle(.plain)
    }
}
