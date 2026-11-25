package dev.waystone.app.input

import dev.waystone.app.protocol.InputEvent

/**
 * Direct touch: the finger is the cursor (design doc section 5.2).
 *
 * - lift quickly without moving: click;
 * - move before hold time: hover only (lifting never clicks);
 * - hold still for hold time: grab (left button down) and drag;
 * - two fingers: right-click tap, scroll, or pinch zoom.
 */
class DirectTouchEngine : TouchEngine {
    sealed class State {
        data object Idle : State()
        data class Pending(val touch: Int, val start: Point2D, val at: Double) : State()
        data class Hovering(val touch: Int) : State()
        data class Dragging(val touch: Int, val last: Point2D) : State()
        data class TwoFingers(val gesture: TwoFingerGesture) : State()
        data object Draining : State()
    }

    var state: State = State.Idle
        private set

    private val active = mutableMapOf<Int, Point2D>()

    override val deadline: Double?
        get() = (state as? State.Pending)?.let { it.at + TouchTuning.HOLD_DURATION }

    override fun began(touch: Int, at: Point2D, time: Double): List<PointerOutput> {
        active[touch] = at
        return when (val current = state) {
            State.Idle -> {
                state = State.Pending(touch, at, time)
                emptyList()
            }
            is State.Pending -> {
                val firstPoint = active[current.touch] ?: return emptyList()
                state = State.TwoFingers(TwoFingerGesture(current.touch to firstPoint, touch to at, time))
                emptyList()
            }
            is State.Hovering -> {
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
                if (distance(point, current.start) > TouchTuning.SLOP) {
                    state = State.Hovering(current.touch)
                    listOf(PointerOutput.Move(point))
                } else if (time - current.at >= TouchTuning.HOLD_DURATION) {
                    grab(current.touch, point)
                } else {
                    emptyList()
                }
            }
            is State.Hovering -> {
                val point = touches[current.touch] ?: return emptyList()
                listOf(PointerOutput.Move(point))
            }
            is State.Dragging -> {
                val point = touches[current.touch] ?: return emptyList()
                state = State.Dragging(current.touch, point)
                listOf(PointerOutput.Move(point))
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
                    if (time - current.at >= TouchTuning.HOLD_DURATION) {
                        grab(touch, at) + PointerOutput.Release(
                            InputEvent.MouseButton.LEFT,
                            PointerOutput.Target.Point(at),
                        )
                    } else {
                        listOf(
                            PointerOutput.Move(at),
                            PointerOutput.Click(
                                InputEvent.MouseButton.LEFT,
                                PointerOutput.Target.Point(at),
                            ),
                        )
                    }
                }
            }
            is State.Hovering -> {
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
                    listOf(
                        PointerOutput.Release(
                            InputEvent.MouseButton.LEFT,
                            PointerOutput.Target.Point(at),
                        )
                    )
                }
            }
            is State.TwoFingers -> {
                state = State.Draining
                if (current.gesture.isTap(time)) {
                    val point = current.gesture.centroid
                    listOf(
                        PointerOutput.Move(point),
                        PointerOutput.Click(
                            InputEvent.MouseButton.RIGHT,
                            PointerOutput.Target.Point(point),
                        ),
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
            listOf(
                PointerOutput.Release(
                    InputEvent.MouseButton.LEFT,
                    PointerOutput.Target.Point(prev.last),
                )
            )
        } else {
            emptyList()
        }
    }

    override fun tick(time: Double): List<PointerOutput> {
        val current = state as? State.Pending ?: return emptyList()
        if (time - current.at < TouchTuning.HOLD_DURATION) return emptyList()
        val point = active[current.touch] ?: return emptyList()
        return grab(current.touch, point)
    }

    private fun grab(touch: Int, at: Point2D): List<PointerOutput> {
        state = State.Dragging(touch, at)
        return listOf(
            PointerOutput.Move(at),
            PointerOutput.Press(InputEvent.MouseButton.LEFT, PointerOutput.Target.Point(at)),
            PointerOutput.HapticFeedback(PointerOutput.Haptic.GRAB),
        )
    }
}
