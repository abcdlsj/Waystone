package dev.waystone.app.input

import dev.waystone.app.protocol.InputEvent
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.max

/** What a touch engine wants done. Positions are surface dp coordinates. */
sealed class PointerOutput {
    sealed class Target {
        data class Point(val point: Point2D) : Target()
        data object Cursor : Target()
    }

    enum class Haptic {
        GRAB,
        SECONDARY,
    }

    data class Move(val to: Point2D) : PointerOutput()
    data class MoveBy(val delta: Vector2D) : PointerOutput()
    data class Press(val button: InputEvent.MouseButton, val target: Target) : PointerOutput()
    data class Release(val button: InputEvent.MouseButton, val target: Target) : PointerOutput()
    data class Click(val button: InputEvent.MouseButton, val target: Target) : PointerOutput()
    data class Scroll(val dx: Float, val dy: Float) : PointerOutput()
    data class Zoom(val factor: Float, val focus: Point2D, val translation: Vector2D) : PointerOutput()
    data class HapticFeedback(val kind: Haptic) : PointerOutput()
}

/** A touch-to-pointer state machine. Times are seconds on any monotonic clock. */
interface TouchEngine {
    fun began(touch: Int, at: Point2D, time: Double): List<PointerOutput>
    fun moved(touches: Map<Int, Point2D>, time: Double): List<PointerOutput>
    fun ended(touch: Int, at: Point2D, time: Double): List<PointerOutput>
    fun cancelled(): List<PointerOutput>
    fun tick(time: Double): List<PointerOutput>
    val deadline: Double?
}

/** Thresholds shared by both engines; from design doc section 5.2 (in dp). */
object TouchTuning {
    const val HOLD_DURATION: Double = 0.16
    const val SLOP: Float = 8f
    const val TWO_FINGER_TAP_DURATION: Double = 0.2
    const val TWO_FINGER_SLOP: Float = 10f
}

/** Two fingers on the glass: tap (right click), scroll, or pinch. */
class TwoFingerGesture(
    a: Pair<Int, Point2D>,
    b: Pair<Int, Point2D>,
    val startedAt: Double,
) {
    enum class Mode { UNDECIDED, SCROLL, PINCH }

    val ids: Pair<Int, Int> = a.first to b.first
    var mode: Mode = Mode.UNDECIDED
        private set

    private val startCentroid: Point2D = centroid(a.second, b.second)
    private val startSpan: Float = span(a.second, b.second)
    private var lastCentroid: Point2D = startCentroid
    private var lastSpan: Float = startSpan

    val centroid: Point2D get() = lastCentroid

    fun moved(touches: Map<Int, Point2D>): List<PointerOutput> {
        val a = touches[ids.first] ?: return emptyList()
        val b = touches[ids.second] ?: return emptyList()
        val currentCentroid = centroid(a, b)
        val currentSpan = span(a, b)
        val prevCentroid = lastCentroid
        val prevSpan = lastSpan
        lastCentroid = currentCentroid
        lastSpan = currentSpan

        if (mode == Mode.UNDECIDED) {
            val travel = hypot(currentCentroid.x - startCentroid.x, currentCentroid.y - startCentroid.y)
            val stretch = abs(currentSpan - startSpan)
            if (max(travel, stretch) <= TouchTuning.TWO_FINGER_SLOP) return emptyList()
            mode = if (stretch > travel) Mode.PINCH else Mode.SCROLL
        }

        return when (mode) {
            Mode.UNDECIDED -> emptyList()
            Mode.SCROLL -> listOf(
                PointerOutput.Scroll(
                    dx = currentCentroid.x - prevCentroid.x,
                    dy = currentCentroid.y - prevCentroid.y,
                )
            )
            Mode.PINCH -> {
                if (prevSpan <= 0f) {
                    emptyList()
                } else {
                    listOf(
                        PointerOutput.Zoom(
                            factor = currentSpan / prevSpan,
                            focus = prevCentroid,
                            translation = Vector2D(
                                dx = currentCentroid.x - prevCentroid.x,
                                dy = currentCentroid.y - prevCentroid.y,
                            ),
                        )
                    )
                }
            }
        }
    }

    fun isTap(endedAt: Double): Boolean =
        mode == Mode.UNDECIDED && (endedAt - startedAt) < TouchTuning.TWO_FINGER_TAP_DURATION

    companion object {
        private fun centroid(a: Point2D, b: Point2D): Point2D =
            Point2D((a.x + b.x) / 2f, (a.y + b.y) / 2f)

        private fun span(a: Point2D, b: Point2D): Float =
            hypot(a.x - b.x, a.y - b.y)
    }
}

fun distance(a: Point2D, b: Point2D): Float = hypot(a.x - b.x, a.y - b.y)
