package dev.waystone.app.protocol

import java.io.ByteArrayOutputStream
import kotlin.math.roundToInt

/**
 * Input frames, byte for byte the layout of `crates/waystone-proto/src/input.rs`.
 *
 * Positions are normalized to the streamed picture (`0.0..1.0` here, `0..65535` on the wire).
 */
sealed class InputEvent {
    enum class MouseButton(val rawValue: Int) {
        LEFT(0),
        RIGHT(1),
        MIDDLE(2),
    }

    data class MouseMove(val x: Double, val y: Double) : InputEvent()
    data class MouseButtonEvent(
        val button: MouseButton,
        val down: Boolean,
        val x: Double,
        val y: Double,
    ) : InputEvent()
    data class Wheel(val dx: Short, val dy: Short) : InputEvent()
    data class Key(
        val code: KeyCode,
        val down: Boolean,
        val modifiers: Int = 0,
    ) : InputEvent()
    data class Text(val value: String) : InputEvent()
    data object ReleaseAll : InputEvent()

    /** Edges must arrive; superseded move samples may be dropped. */
    val isReliable: Boolean
        get() = this !is MouseMove

    fun encoded(): ByteArray {
        val out = ByteArrayOutputStream(16)
        when (this) {
            is MouseMove -> {
                out.write(0x01)
                out.writeU16BE(axis(x))
                out.writeU16BE(axis(y))
            }
            is MouseButtonEvent -> {
                out.write(0x02)
                out.write(button.rawValue)
                out.write(if (down) 1 else 0)
                out.writeU16BE(axis(x))
                out.writeU16BE(axis(y))
            }
            is Wheel -> {
                out.write(0x03)
                out.writeU16BE(dx.toInt() and 0xffff)
                out.writeU16BE(dy.toInt() and 0xffff)
            }
            is Key -> {
                out.write(0x04)
                out.writeU32BE(code.rawValue)
                out.write(if (down) 1 else 0)
                out.write(modifiers and 0xff)
            }
            is Text -> {
                out.write(0x05)
                val bytes = value.toByteArray(Charsets.UTF_8)
                out.write(bytes, 0, bytes.size)
            }
            ReleaseAll -> {
                out.write(0x06)
            }
        }
        return out.toByteArray()
    }

    companion object {
        private fun axis(value: Double): Int =
            (value.coerceIn(0.0, 1.0) * 65535.0).roundToInt()

        private fun ByteArrayOutputStream.writeU16BE(value: Int) {
            write((value ushr 8) and 0xff)
            write(value and 0xff)
        }

        private fun ByteArrayOutputStream.writeU32BE(value: Int) {
            write((value ushr 24) and 0xff)
            write((value ushr 16) and 0xff)
            write((value ushr 8) and 0xff)
            write(value and 0xff)
        }
    }
}

/** Modifier bit flags, `waystone_proto::input::modifiers`. */
object Modifiers {
    const val NONE: Int = 0
    const val SHIFT: Int = 1 shl 0
    const val CONTROL: Int = 1 shl 1
    const val ALT: Int = 1 shl 2
    const val META: Int = 1 shl 3
}

/**
 * USB HID usage IDs (Keyboard/Keypad page 0x07), matching `waystone_proto::input::keys`.
 */
@JvmInline
value class KeyCode(val rawValue: Int) {
    val modifierFlag: Int
        get() = when (this) {
            LEFT_CONTROL -> Modifiers.CONTROL
            LEFT_SHIFT -> Modifiers.SHIFT
            LEFT_ALT -> Modifiers.ALT
            LEFT_META -> Modifiers.META
            else -> Modifiers.NONE
        }

    companion object {
        val ENTER = KeyCode(0x28)
        val ESCAPE = KeyCode(0x29)
        val BACKSPACE = KeyCode(0x2a)
        val TAB = KeyCode(0x2b)
        val SPACE = KeyCode(0x2c)
        val RIGHT = KeyCode(0x4f)
        val LEFT = KeyCode(0x50)
        val DOWN = KeyCode(0x51)
        val UP = KeyCode(0x52)
        val LEFT_CONTROL = KeyCode(0xe0)
        val LEFT_SHIFT = KeyCode(0xe1)
        val LEFT_ALT = KeyCode(0xe2)
        val LEFT_META = KeyCode(0xe3)

        fun letter(ch: Char): KeyCode? {
            val lower = ch.lowercaseChar()
            if (lower !in 'a'..'z') return null
            return KeyCode(0x04 + (lower - 'a'))
        }

        fun digit(value: Int): KeyCode? {
            if (value !in 0..9) return null
            return KeyCode(if (value == 0) 0x27 else 0x1d + value)
        }

        fun function(number: Int): KeyCode? {
            if (number !in 1..12) return null
            return KeyCode(0x39 + number)
        }
    }
}
