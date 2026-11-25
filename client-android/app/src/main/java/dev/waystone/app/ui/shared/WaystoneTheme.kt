package dev.waystone.app.ui.shared

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Waystone pure-black monochrome console visual tokens and shared controls,
 * mirroring `client-ios/Waystone/UI/Shared/WaystoneTheme.swift`.
 */
object WaystoneTheme {
    // Surfaces — pure OLED black with restrained neutral elevation steps.
    val Canvas = Color(0xFF000000)
    val Ledger = Color(0xFF050505)
    val Surface = Color(0xFF0B0B0B)
    val SurfaceRaised = Color(0xFF141414)
    val HudSurface = Color(0xFF060606)
    val FooterSurface = Color(0xFF030303)

    // Structural hairlines
    val BorderSubtle = Color(0xFF1C1C1C)
    val Border = Color(0xFF262626)
    val BorderStrong = Color(0xFF3B3B3B)

    // Ink & high-contrast monochrome primary accent
    val TextPrimary = Color(0xFFF5F5F5)
    val TextSecondary = Color(0xFF8A8A8A)
    val TextTertiary = Color(0xFF4F4F4F)
    val TextQuaternary = Color(0xFF2E2E2E)
    val Accent = Color.White
    val AccentInk = Color.Black

    // Functional telemetry & alert colors
    val Emerald = Color(0xFF22C55E)
    val Amber = Color(0xFFF59E0B)
    val AmberInk = Color.Black
    val Coral = Color(0xFFDC2626)

    fun latencyColor(rttMs: Int): Color = when {
        rttMs < 35 -> Emerald
        rttMs < 90 -> Amber
        else -> Coral
    }
}

/** Standing boundary stone silhouette matching `docs/brand/icon.svg`. */
@Composable
fun WaystoneMark(
    modifier: Modifier = Modifier,
    color: Color = WaystoneTheme.TextPrimary,
) {
    Canvas(modifier = modifier) {
        val sx = size.width / 1024f
        val sy = size.height / 1024f
        val path = Path().apply {
            fillType = PathFillType.EvenOdd
            // Outer monolith
            moveTo(330f * sx, 840f * sy)
            lineTo(366f * sx, 352f * sy)
            cubicTo(
                380f * sx, 190f * sy,
                644f * sx, 190f * sy,
                658f * sx, 352f * sy,
            )
            lineTo(694f * sx, 840f * sy)
            close()

            // Carved play mark cutout
            moveTo(462f * sx, 488f * sy)
            lineTo(462f * sx, 652f * sy)
            lineTo(598f * sx, 570f * sy)
            close()
        }
        drawPath(path = path, color = color)
    }
}

@Composable
fun HairlineDivider(
    modifier: Modifier = Modifier,
    color: Color = WaystoneTheme.BorderSubtle,
) {
    Box(
        modifier = modifier
            .fillMaxWidth()
            .height(0.75.dp)
            .background(color)
    )
}

/** Flat matte-black console panel with a crisp 0.75 dp hairline border. */
fun Modifier.consolePanel(
    cornerRadius: Dp = 8.dp,
    alpha: Float = 0.96f,
): Modifier {
    val shape = RoundedCornerShape(cornerRadius)
    return this
        .clip(shape)
        .background(WaystoneTheme.HudSurface.copy(alpha = alpha), shape)
        .border(0.75.dp, WaystoneTheme.BorderStrong, shape)
}

/** High-contrast monochrome segmented control for landscape stream HUDs. */
@Composable
fun <T> WaystoneSegmentedControl(
    options: List<T>,
    selection: T,
    title: (T) -> String,
    onSelect: (T) -> Unit,
    modifier: Modifier = Modifier,
) {
    val outerShape = RoundedCornerShape(5.dp)
    val pillShape = RoundedCornerShape(3.5.dp)
    Row(
        modifier = modifier
            .clip(outerShape)
            .background(Color.Black, outerShape)
            .border(0.75.dp, WaystoneTheme.Border, outerShape)
            .padding(2.dp),
        horizontalArrangement = Arrangement.spacedBy(2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        options.forEach { option ->
            val selected = option == selection
            val interaction = remember { MutableInteractionSource() }
            Box(
                modifier = Modifier
                    .weight(1f)
                    .height(26.dp)
                    .clip(pillShape)
                    .background(if (selected) WaystoneTheme.Accent else Color.Transparent, pillShape)
                    .clickable(interactionSource = interaction, indication = null) {
                        if (!selected) onSelect(option)
                    }
                    .padding(horizontal = 4.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = title(option),
                    fontSize = 11.5.sp,
                    fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Medium,
                    color = if (selected) WaystoneTheme.AccentInk else WaystoneTheme.TextSecondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

/** Crisp 2 dp linear console progress bar used during connection setup. */
@Composable
fun ConsoleProgressBar(modifier: Modifier = Modifier) {
    val transition = rememberInfiniteTransition(label = "console-progress")
    val t by transition.animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 1400, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "sweep",
    )
    val eased = if (t < 0.5f) 2f * t * t else 1f - (-2f * t + 2f).let { it * it } / 2f

    Canvas(
        modifier = modifier
            .fillMaxWidth()
            .height(2.dp)
            .clip(RoundedCornerShape(0.dp)),
    ) {
        drawRect(color = WaystoneTheme.BorderSubtle)
        val barWidth = size.width * 0.36f
        val x = -barWidth + (size.width + barWidth) * eased
        drawRect(
            color = WaystoneTheme.Accent,
            topLeft = Offset(x, 0f),
            size = Size(barWidth, size.height),
        )
    }
}
