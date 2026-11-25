package dev.waystone.app.input

import dev.waystone.app.protocol.InputEvent
import kotlin.math.hypot
import kotlin.math.max

/**
 * Trackpad mode: the screen is a laptop touchpad (design doc section 5.2).
 */
class TrackpadEngine : TouchEngine {
    sealed class State {
        data object Idle : State()
        data class Pending(val touch: Int, val start: Point2D, val at: Double) : State()
        data class Moving(val touch: Int) : State()
        data class Dragging(val touch: Int) : State()
        data class TwoFingers(val gesture: TwoFingerGesture) : State()
        data object Draining : State()
    }

    var state: State = State.Idle
        private set

    private val active = mutableMapOf<Int, Point2D>()
    private var lastMove: Pair<Point2D, Double>? = null

    override val deadline: Double?
        get() = (state as? State.Pending)?.let { it.at + HOLD_DURATION }

    override fun began(touch: Int, at: Point2D, time: Double): List<PointerOutput> {
        active[touch] = at
        return when (val current = state) {
            State.Idle -> {
                state = State.Pending(touch, at, time)
                lastMove = at to time
                emptyList()
            }
            is State.Pending -> {
                val firstPoint = active[current.touch] ?: return emptyList()
                state = State.TwoFingers(TwoFingerGesture(current.touch to firstPoint, touch to at, time))
                emptyList()
            }
            is State.Moving -> {
                val firstPoint = active[current.touch] ?: return emptyList()
                state = State.TwoFingers(TwoFingerGesture(current.touch to firstPoint, touch to at, time))
                emptyList()
            }
            is State.Dragging, is State.TwoFingers, State.Draining -> emptyList()
        }
    }

    override fun moved(touches: Map<Int, Point2D>, time: Double): List<PointerOutput> {
        active.putAll(touches)
        return when (val current = state) {
            is State.Pending -> {
                val point = touches[current.touch] ?: return emptyList()
                if (distance(point, current.start) <= TouchTuning.SLOP) return emptyList()
                state = State.Moving(current.touch)
                relative(point, time)
            }
            is State.Moving -> {
                val point = touches[current.touch] ?: return emptyList()
                relative(point, time)
            }
            is State.Dragging -> {
                val point = touches[current.touch] ?: return emptyList()
                relative(point, time)
            }
            is State.TwoFingers -> current.gesture.moved(active)
            State.Idle, State.Draining -> emptyList()
        }
    }

    override fun ended(touch: Int, at: Point2D, time: Double): List<PointerOutput> {
        active.remove(touch)
        val outputs = when (val current = state) {
            is State.Pending -> {
                if (current.touch != touch) {
                    emptyList()
                } else {
                    state = State.Idle
                    if (time - current.at < TAP_DURATION) {
                        listOf(PointerOutput.Click(InputEvent.MouseButton.LEFT, PointerOutput.Target.Cursor))
                    } else {
                        emptyList()
                    }
                }
            }
            is State.Moving -> {
                if (current.touch == touch) {
                    state = State.Idle
                }
                emptyList()
            }
            is State.Dragging -> {
                if (current.touch != touch) {
                    emptyList()
                } else {
                    state = State.Draining
                    listOf(PointerOutput.Release(InputEvent.MouseButton.LEFT, PointerOutput.Target.Cursor))
                }
            }
            is State.TwoFingers -> {
                state = State.Draining
                if (current.gesture.isTap(time)) {
                    listOf(
                        PointerOutput.Click(InputEvent.MouseButton.RIGHT, PointerOutput.Target.Cursor),
                        PointerOutput.HapticFeedback(PointerOutput.Haptic.SECONDARY),
                    )
                } else {
                    emptyList()
                }
            }
            else -> emptyList()
        }
        if (active.isEmpty()) state = State.Idle
        return outputs
    }

    override fun cancelled(): List<PointerOutput> {
        active.clear()
        val prev = state
        state = State.Idle
        return if (prev is State.Dragging) {
            listOf(PointerOutput.Release(InputEvent.MouseButton.LEFT, PointerOutput.Target.Cursor))
        } else {
            emptyList()
        }
    }

    override fun tick(time: Double): List<PointerOutput> {
        val current = state as? State.Pending ?: return emptyList()
        if (time - current.at < HOLD_DURATION) return emptyList()
        state = State.Dragging(current.touch)
        return listOf(
            PointerOutput.Press(InputEvent.MouseButton.LEFT, PointerOutput.Target.Cursor),
            PointerOutput.HapticFeedback(PointerOutput.Haptic.GRAB),
        )
    }

    private fun relative(point: Point2D, time: Double): List<PointerOutput> {
        val prev = lastMove
        lastMove = point to time
        if (prev == null) return emptyList()
        val dx = point.x - prev.first.x
        val dy = point.y - prev.first.y
        val elapsed = max(time - prev.second, 1.0 / 240.0).toFloat()
        val speed = hypot(dx, dy) / elapsed
        val gain = gain(speed)
        return listOf(PointerOutput.MoveBy(Vector2D(dx * gain, dy * gain)))
    }

    companion object {
        const val HOLD_DURATION: Double = 0.35
        const val TAP_DURATION: Double = 0.25

        fun gain(speed: Float): Float = (0.8f + speed / 700f).coerceIn(0.8f, 3f)
    }
}
