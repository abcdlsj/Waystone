package dev.waystone.app.input

import kotlin.math.max
import kotlin.math.min

data class Point2D(val x: Float, val y: Float) {
    companion object {
        val ZERO = Point2D(0f, 0f)
    }
}

data class Size2D(val width: Float, val height: Float) {
    companion object {
        val ZERO = Size2D(0f, 0f)
    }
}

data class Vector2D(val dx: Float, val dy: Float) {
    companion object {
        val ZERO = Vector2D(0f, 0f)
    }
}

data class Rect2D(val x: Float, val y: Float, val width: Float, val height: Float) {
    val minX: Float get() = x
    val minY: Float get() = y
    val size: Size2D get() = Size2D(width, height)

    companion object {
        val ZERO = Rect2D(0f, 0f, 0f, 0f)
    }
}

/**
 * Where the remote picture sits on the touch surface, and how to map a touch
 * back to the remote screen.
 *
 * The picture is aspect-fitted into the surface and then zoomed and panned locally.
 */
data class PictureGeometry(
    val bounds: Size2D = Size2D.ZERO,
    val videoSize: Size2D = Size2D.ZERO,
    val scale: Float = 1f,
    val offset: Vector2D = Vector2D.ZERO,
) {
    /** The aspect-fitted picture before zoom, in surface coordinates. */
    val contentRect: Rect2D
        get() {
            if (bounds.width <= 0f || bounds.height <= 0f) return Rect2D.ZERO
            if (videoSize.width <= 0f || videoSize.height <= 0f) {
                return Rect2D(0f, 0f, bounds.width, bounds.height)
            }
            val fit = min(bounds.width / videoSize.width, bounds.height / videoSize.height)
            val width = videoSize.width * fit
            val height = videoSize.height * fit
            return Rect2D(
                x = (bounds.width - width) / 2f,
                y = (bounds.height - height) / 2f,
                width = width,
                height = height,
            )
        }

    /** The picture as drawn, after zoom and pan. */
    val displayedRect: Rect2D
        get() {
            val content = contentRect
            val centerX = bounds.width / 2f
            val centerY = bounds.height / 2f
            return Rect2D(
                x = centerX + (content.minX - centerX) * scale + offset.dx,
                y = centerY + (content.minY - centerY) * scale + offset.dy,
                width = content.width * scale,
                height = content.height * scale,
            )
        }

    val isZoomed: Boolean get() = scale > 1.001f

    val displayedSize: Size2D get() = displayedRect.size

    /** Remote position, `0..1` on both axes, for a point on the surface. */
    fun normalized(point: Point2D): Pair<Double, Double> {
        val rect = displayedRect
        if (rect.width <= 0f || rect.height <= 0f) return 0.5 to 0.5
        val x = ((point.x - rect.minX) / rect.width).coerceIn(0f, 1f).toDouble()
        val y = ((point.y - rect.minY) / rect.height).coerceIn(0f, 1f).toDouble()
        return x to y
    }

    /** Surface position of a remote position; used to draw the local cursor. */
    fun surfacePoint(x: Double, y: Double): Point2D {
        val rect = displayedRect
        return Point2D(
            x = rect.minX + x.toFloat() * rect.width,
            y = rect.minY + y.toFloat() * rect.height,
        )
    }

    /** Zooms by `factor` around `focus` and pans by `translation`. */
    fun zoomed(factor: Float, focus: Point2D, translation: Vector2D): PictureGeometry {
        val newScale = (scale * factor).coerceIn(1f, MAX_SCALE)
        val centerX = bounds.width / 2f
        val centerY = bounds.height / 2f
        val anchorX = centerX + (focus.x - centerX - offset.dx) / scale
        val anchorY = centerY + (focus.y - centerY - offset.dy) / scale
        val rawDx = focus.x + translation.dx - centerX - (anchorX - centerX) * newScale
        val rawDy = focus.y + translation.dy - centerY - (anchorY - centerY) * newScale
        val content = contentRect
        val maxX = max(0f, (content.width * newScale - bounds.width) / 2f)
        val maxY = max(0f, (content.height * newScale - bounds.height) / 2f)
        return copy(
            scale = newScale,
            offset = Vector2D(
                dx = rawDx.coerceIn(-maxX, maxX),
                dy = rawDy.coerceIn(-maxY, maxY),
            ),
        )
    }

    fun resetZoom(): PictureGeometry = copy(scale = 1f, offset = Vector2D.ZERO)

    companion object {
        const val MAX_SCALE: Float = 4f
    }
}
