package dev.waystone.app.input

import dev.waystone.app.protocol.InputEvent
import dev.waystone.app.protocol.KeyCode
import dev.waystone.app.protocol.Modifiers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlin.math.roundToInt

/**
 * Turns touches and on-screen controls into input frames for the host.
 */
class InputController(
    initialMode: Mode,
    private val haptics: (PointerOutput.Haptic) -> Unit = {},
) {
    enum class Mode(val key: String, val title: String) {
        DIRECT("direct", "直点"),
        TRACKPAD("trackpad", "触控板");

        companion object {
            fun fromKey(key: String?): Mode = entries.firstOrNull { it.key == key } ?: DIRECT
        }
    }

    private val _geometry = MutableStateFlow(PictureGeometry())
    val geometry: StateFlow<PictureGeometry> = _geometry.asStateFlow()

    private val _mode = MutableStateFlow(initialMode)
    val mode: StateFlow<Mode> = _mode.asStateFlow()

    private val _cursor = MutableStateFlow(0.5 to 0.5)
    val cursor: StateFlow<Pair<Double, Double>> = _cursor.asStateFlow()

    private val _directPointerActive = MutableStateFlow(false)
    val directPointerActive: StateFlow<Boolean> = _directPointerActive.asStateFlow()

    private val _latchedKeys = MutableStateFlow<Set<KeyCode>>(emptySet())
    val latchedKeys: StateFlow<Set<KeyCode>> = _latchedKeys.asStateFlow()

    private val _armedModifiers = MutableStateFlow(Modifiers.NONE)
    val armedModifiers: StateFlow<Int> = _armedModifiers.asStateFlow()

    private var engine: TouchEngine = makeEngine(initialMode)
    private val activeTouches = mutableSetOf<Int>()
    private val heldKeys = mutableSetOf<KeyCode>()

    var sink: (InputEvent) -> Unit = {}

    val deadline: Double? get() = engine.deadline

    fun layout(bounds: Size2D) {
        if (_geometry.value.bounds == bounds) return
        _geometry.value = _geometry.value.copy(bounds = bounds).resetZoom()
    }

    fun videoSizeChanged(size: Size2D) {
        if (_geometry.value.videoSize == size) return
        _geometry.value = _geometry.value.copy(videoSize = size).resetZoom()
    }

    fun resetZoom() {
        _geometry.value = _geometry.value.resetZoom()
    }

    fun touchBegan(id: Int, at: Point2D, time: Double) {
        activeTouches.add(id)
        applyOutputs(engine.began(id, at, time))
    }

    fun touchesMoved(touches: Map<Int, Point2D>, time: Double) {
        applyOutputs(engine.moved(touches, time))
    }

    fun touchEnded(id: Int, at: Point2D, time: Double) {
        activeTouches.remove(id)
        applyOutputs(engine.ended(id, at, time))
        if (activeTouches.isEmpty()) {
            _directPointerActive.value = false
        }
    }

    fun touchesCancelled() {
        activeTouches.clear()
        _directPointerActive.value = false
        applyOutputs(engine.cancelled())
    }

    fun tick(time: Double) {
        applyOutputs(engine.tick(time))
    }

    fun setMode(mode: Mode) {
        if (mode == _mode.value) return
        activeTouches.clear()
        _directPointerActive.value = false
        applyOutputs(engine.cancelled())
        _mode.value = mode
        engine = makeEngine(mode)
    }

    fun perform(action: KeyAction, down: Boolean) {
        when (action) {
            is KeyAction.Tap -> {
                if (down) tap(action.key)
            }
            is KeyAction.Toggle -> {
                if (!down) return
                val current = _latchedKeys.value
                if (action.key in current) {
                    _latchedKeys.value = current - action.key
                    keyUp(action.key)
                } else {
                    _latchedKeys.value = current + action.key
                    keyDown(action.key)
                }
            }
            is KeyAction.Hold -> {
                if (down) keyDown(action.key) else keyUp(action.key)
            }
            is KeyAction.Click -> {
                if (!down) return
                click(action.button, _cursor.value)
                haptics(PointerOutput.Haptic.SECONDARY)
            }
        }
    }

    /** Holds exactly `keys` down, releasing the rest; drives the joystick. */
    fun hold(keys: Set<KeyCode>, among: Set<KeyCode>) {
        for (key in among - keys) {
            if (key in heldKeys) keyUp(key)
        }
        for (key in keys) {
            if (key !in heldKeys) keyDown(key)
        }
    }

    fun toggleArmed(modifier: Int) {
        val current = _armedModifiers.value
        _armedModifiers.value = if ((current and modifier) != 0) {
            current and modifier.inv()
        } else {
            current or modifier
        }
    }

    fun tap(key: KeyCode) {
        val mods = _armedModifiers.value
        _armedModifiers.value = Modifiers.NONE
        val modifierKeys = keysFor(mods)
        modifierKeys.forEach { keyDown(it) }
        keyDown(key, mods)
        keyUp(key, mods)
        modifierKeys.asReversed().forEach { keyUp(it) }
    }

    fun type(text: String) {
        val armed = _armedModifiers.value
        if (armed == Modifiers.NONE) {
            sink(InputEvent.Text(text))
            return
        }
        for (ch in text) {
            val key = KeyCode.letter(ch) ?: ch.digitToIntOrNull()?.let { KeyCode.digit(it) }
            if (key != null) {
                _armedModifiers.value = armed
                tap(key)
            } else {
                sink(InputEvent.Text(ch.toString()))
            }
        }
        _armedModifiers.value = Modifiers.NONE
    }

    fun releaseAll() {
        applyOutputs(engine.cancelled())
        heldKeys.clear()
        _latchedKeys.value = emptySet()
        _armedModifiers.value = Modifiers.NONE
        sink(InputEvent.ReleaseAll)
    }

    private fun applyOutputs(outputs: List<PointerOutput>) {
        for (output in outputs) {
            when (output) {
                is PointerOutput.Move -> {
                    val pos = _geometry.value.normalized(output.to)
                    _cursor.value = pos
                    if (_mode.value == Mode.DIRECT && activeTouches.isNotEmpty()) {
                        _directPointerActive.value = true
                    }
                    sink(InputEvent.MouseMove(pos.first, pos.second))
                }
                is PointerOutput.MoveBy -> {
                    val size = _geometry.value.displayedSize
                    if (size.width <= 0f || size.height <= 0f) continue
                    val current = _cursor.value
                    val next = (current.first + output.delta.dx / size.width).coerceIn(0.0, 1.0) to
                        (current.second + output.delta.dy / size.height).coerceIn(0.0, 1.0)
                    _cursor.value = next
                    sink(InputEvent.MouseMove(next.first, next.second))
                }
                is PointerOutput.Press -> {
                    val at = resolveTarget(output.target)
                    sink(InputEvent.MouseButtonEvent(output.button, down = true, x = at.first, y = at.second))
                }
                is PointerOutput.Release -> {
                    val at = resolveTarget(output.target)
                    sink(InputEvent.MouseButtonEvent(output.button, down = false, x = at.first, y = at.second))
                }
                is PointerOutput.Click -> {
                    click(output.button, resolveTarget(output.target))
                }
                is PointerOutput.Scroll -> {
                    val wheelX = (output.dx * SCROLL_GAIN).roundToInt()
                        .coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt()).toShort()
                    val wheelY = (output.dy * SCROLL_GAIN).roundToInt()
                        .coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt()).toShort()
                    if (wheelX.toInt() == 0 && wheelY.toInt() == 0) continue
                    sink(InputEvent.Wheel(wheelX, wheelY))
                }
                is PointerOutput.Zoom -> {
                    _geometry.value = _geometry.value.zoomed(output.factor, output.focus, output.translation)
                }
                is PointerOutput.HapticFeedback -> haptics(output.kind)
            }
        }
    }

    private fun resolveTarget(target: PointerOutput.Target): Pair<Double, Double> = when (target) {
        is PointerOutput.Target.Point -> {
            val pos = _geometry.value.normalized(target.point)
            _cursor.value = pos
            pos
        }
        PointerOutput.Target.Cursor -> _cursor.value
    }

    private fun click(button: InputEvent.MouseButton, point: Pair<Double, Double>) {
        sink(InputEvent.MouseButtonEvent(button, down = true, x = point.first, y = point.second))
        sink(InputEvent.MouseButtonEvent(button, down = false, x = point.first, y = point.second))
    }

    private fun keyDown(key: KeyCode, modifiers: Int = Modifiers.NONE) {
        heldKeys.add(key)
        sink(InputEvent.Key(key, down = true, modifiers = modifiers))
    }

    private fun keyUp(key: KeyCode, modifiers: Int = Modifiers.NONE) {
        heldKeys.remove(key)
        sink(InputEvent.Key(key, down = false, modifiers = modifiers))
    }

    companion object {
        const val SCROLL_GAIN: Float = 2f

        private fun keysFor(modifiers: Int): List<KeyCode> = buildList {
            if ((modifiers and Modifiers.CONTROL) != 0) add(KeyCode.LEFT_CONTROL)
            if ((modifiers and Modifiers.ALT) != 0) add(KeyCode.LEFT_ALT)
            if ((modifiers and Modifiers.SHIFT) != 0) add(KeyCode.LEFT_SHIFT)
            if ((modifiers and Modifiers.META) != 0) add(KeyCode.LEFT_META)
        }

        private fun makeEngine(mode: Mode): TouchEngine = when (mode) {
            Mode.DIRECT -> DirectTouchEngine()
            Mode.TRACKPAD -> TrackpadEngine()
        }
    }
}
