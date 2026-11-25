package dev.waystone.app.ui.stream

import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Menu
import androidx.compose.material3.Icon
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import dev.waystone.app.stream.StreamPreferences
import dev.waystone.app.ui.shared.Haptics
import dev.waystone.app.ui.shared.WaystoneTheme
import kotlinx.coroutines.delay
import kotlin.math.roundToInt

enum class BallHealth(val color: Color) {
    GOOD(WaystoneTheme.Emerald),
    DEGRADED(WaystoneTheme.Amber),
    STALLED(WaystoneTheme.Coral),
}

/**
 * The only persistent control on a stream (design doc 5.3).
 *
 * A 36 dp ball docked to a side edge. Can be dragged along and across the
 * screen and snaps to the nearest side; after 3 s untouched it tucks half
 * behind the edge at 30 % opacity.
 */
@Composable
fun FloatingBall(
    health: BallHealth,
    preferences: StreamPreferences,
    haptics: Haptics,
    onTap: () -> Unit,
) {
    val trailing by preferences.ballTrailing.collectAsState()
    val heightFraction by preferences.ballHeight.collectAsState()

    var dragging by remember { mutableStateOf(false) }
    var dragDx by remember { mutableFloatStateOf(0f) }
    var dragDy by remember { mutableFloatStateOf(0f) }
    var tucked by remember { mutableStateOf(false) }
    var touchCounter by remember { mutableIntStateOf(0) }

    LaunchedEffect(touchCounter) {
        tucked = false
        delay(3000)
        if (!dragging) tucked = true
    }

    val pulseTransition = rememberInfiniteTransition(label = "ball-pulse")
    val pulseAlpha by pulseTransition.animateFloat(
        initialValue = 1f,
        targetValue = 0.30f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 750),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "pulse",
    )

    val ballAlpha by animateFloatAsState(
        targetValue = if (tucked && !dragging) 0.30f else 0.94f,
        animationSpec = tween(durationMillis = 220),
        label = "ball-alpha",
    )

    val density = LocalDensity.current
    val sizeDp = 36.dp
    val sizePx = with(density) { sizeDp.toPx() }
    val halfPx = sizePx / 2f
    val insetPx = with(density) { if (tucked && !dragging) -sizeDp.toPx() / 4f else 8.dp.toPx() }

    BoxWithConstraints(modifier = Modifier.fillMaxSize()) {
        val areaW = constraints.maxWidth.toFloat()
        val areaH = constraints.maxHeight.toFloat()

        val baseCenterX = if (trailing) areaW - halfPx - insetPx else halfPx + insetPx
        val baseCenterY = areaH * heightFraction
        val edge = if (tucked && !dragging) 0f else halfPx
        val centerX = (baseCenterX + if (dragging) dragDx else 0f).coerceIn(edge, (areaW - edge).coerceAtLeast(edge))
        val centerY = (baseCenterY + if (dragging) dragDy else 0f).coerceIn(halfPx, (areaH - halfPx).coerceAtLeast(halfPx))

        val animatedX by animateFloatAsState(
            targetValue = centerX,
            animationSpec = tween(durationMillis = if (dragging) 0 else 160),
            label = "ball-x",
        )
        val animatedY by animateFloatAsState(
            targetValue = centerY,
            animationSpec = tween(durationMillis = if (dragging) 0 else 160),
            label = "ball-y",
        )

        Box(
            modifier = Modifier
                .offset {
                    IntOffset(
                        x = (animatedX - halfPx).roundToInt(),
                        y = (animatedY - halfPx).roundToInt(),
                    )
                }
                .size(sizeDp)
                .alpha(ballAlpha)
                .pointerInput(areaW, areaH, trailing, heightFraction) {
                    detectTapGestures(
                        onTap = {
                            touchCounter += 1
                            haptics.tap()
                            onTap()
                        }
                    )
                }
                .pointerInput(areaW, areaH, trailing, heightFraction) {
                    detectDragGestures(
                        onDragStart = {
                            dragging = true
                            tucked = false
                            dragDx = 0f
                            dragDy = 0f
                        },
                        onDragEnd = {
                            val endX = (baseCenterX + dragDx).coerceIn(halfPx, (areaW - halfPx).coerceAtLeast(halfPx))
                            val endY = (baseCenterY + dragDy).coerceIn(halfPx, (areaH - halfPx).coerceAtLeast(halfPx))
                            preferences.setBallPosition(
                                trailing = endX > areaW / 2f,
                                heightFraction = if (areaH > 0f) endY / areaH else 0.5f,
                            )
                            dragging = false
                            dragDx = 0f
                            dragDy = 0f
                            touchCounter += 1
                        },
                        onDragCancel = {
                            dragging = false
                            dragDx = 0f
                            dragDy = 0f
                            touchCounter += 1
                        },
                        onDrag = { change, amount ->
                            change.consume()
                            dragDx += amount.x
                            dragDy += amount.y
                        },
                    )
                },
        ) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .clip(CircleShape)
                    .background(Color.Black.copy(alpha = 0.88f), CircleShape)
                    .border(0.75.dp, WaystoneTheme.BorderStrong, CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    imageVector = Icons.Outlined.Menu,
                    contentDescription = "串流菜单",
                    tint = WaystoneTheme.TextPrimary.copy(alpha = 0.92f),
                    modifier = Modifier.size(16.dp),
                )
            }

            // Health dot at top-right
            Box(
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .size(8.dp)
                    .alpha(if (health == BallHealth.GOOD) 1f else pulseAlpha)
                    .clip(CircleShape)
                    .background(health.color, CircleShape)
                    .border(1.5.dp, Color.Black, CircleShape),
            )
        }
    }
}
