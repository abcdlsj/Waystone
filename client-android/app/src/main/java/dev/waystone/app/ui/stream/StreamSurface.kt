package dev.waystone.app.ui.stream

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Color
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.widget.FrameLayout
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import dev.waystone.app.input.InputController
import dev.waystone.app.input.PictureGeometry
import dev.waystone.app.input.Point2D
import dev.waystone.app.input.Size2D
import dev.waystone.app.stream.StreamSession
import kotlin.math.roundToInt

/**
 * The remote picture SurfaceView plus raw multi-touch forwarding to `InputController`.
 * Mirrors `client-ios/Waystone/UI/Stream/StreamSurface.swift`.
 */
@Composable
fun StreamSurface(
    session: StreamSession,
    geometry: PictureGeometry,
    modifier: Modifier = Modifier,
) {
    AndroidView(
        modifier = modifier,
        factory = { ctx -> TouchSurfaceView(ctx, session) },
        update = { view -> view.place(geometry) },
        onRelease = { view -> view.release() },
    )
}

@SuppressLint("ViewConstructor")
private class TouchSurfaceView(
    context: Context,
    private val session: StreamSession,
) : FrameLayout(context), SurfaceHolder.Callback {

    private val input: InputController = session.input
    private val density: Float = resources.displayMetrics.density
    private val surfaceView = SurfaceView(context)
    private val mainHandler = Handler(Looper.getMainLooper())
    private var tickRunnable: Runnable? = null
    private var lastGeometry: PictureGeometry = input.geometry.value

    init {
        setBackgroundColor(Color.BLACK)
        clipChildren = true
        surfaceView.holder.addCallback(this)
        addView(surfaceView, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        if (w > 0 && h > 0) {
            input.layout(Size2D(w / density, h / density))
            place(input.geometry.value)
        }
    }

    fun place(geometry: PictureGeometry) {
        lastGeometry = geometry
        val rect = geometry.displayedRect
        if (rect.width <= 0f || rect.height <= 0f) return
        val left = (rect.x * density).roundToInt()
        val top = (rect.y * density).roundToInt()
        val w = (rect.width * density).roundToInt().coerceAtLeast(1)
        val h = (rect.height * density).roundToInt().coerceAtLeast(1)
        val params = surfaceView.layoutParams as? LayoutParams ?: LayoutParams(w, h)
        if (params.width != w || params.height != h || params.leftMargin != left || params.topMargin != top) {
            params.width = w
            params.height = h
            params.leftMargin = left
            params.topMargin = top
            surfaceView.layoutParams = params
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        session.attachSurface(holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        session.attachSurface(holder.surface)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        session.attachSurface(null)
    }

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        val timeSec = event.eventTime / 1000.0
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val index = event.actionIndex
                val id = event.getPointerId(index)
                input.touchBegan(id, pointDp(event, index), timeSec)
                scheduleTick()
            }
            MotionEvent.ACTION_MOVE -> {
                val points = HashMap<Int, Point2D>(event.pointerCount)
                for (i in 0 until event.pointerCount) {
                    points[event.getPointerId(i)] = pointDp(event, i)
                }
                input.touchesMoved(points, timeSec)
                scheduleTick()
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                val index = event.actionIndex
                val id = event.getPointerId(index)
                input.touchEnded(id, pointDp(event, index), timeSec)
                scheduleTick()
            }
            MotionEvent.ACTION_CANCEL -> {
                input.touchesCancelled()
                scheduleTick()
            }
        }
        return true
    }

    fun release() {
        cancelTick()
        surfaceView.holder.removeCallback(this)
        session.attachSurface(null)
    }

    private fun pointDp(event: MotionEvent, index: Int): Point2D =
        Point2D(event.getX(index) / density, event.getY(index) / density)

    private fun scheduleTick() {
        cancelTick()
        val deadline = input.deadline ?: return
        val delayMs = ((deadline * 1000.0).toLong() - SystemClock.uptimeMillis()).coerceAtLeast(0L)
        val runnable = Runnable {
            tickRunnable = null
            input.tick(SystemClock.uptimeMillis() / 1000.0)
            scheduleTick()
        }
        tickRunnable = runnable
        mainHandler.postDelayed(runnable, delayMs)
    }

    private fun cancelTick() {
        tickRunnable?.let { mainHandler.removeCallbacks(it) }
        tickRunnable = null
    }
}
