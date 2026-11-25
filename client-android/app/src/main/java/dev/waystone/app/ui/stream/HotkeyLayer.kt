package dev.waystone.app.ui.stream

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.waystone.app.input.EdgeZone
import dev.waystone.app.input.HotkeyButton
import dev.waystone.app.input.HotkeyGroup
import dev.waystone.app.input.HotkeyPreset
import dev.waystone.app.input.InputController
import dev.waystone.app.input.JoystickLayout
import dev.waystone.app.input.KeyAction
import dev.waystone.app.protocol.KeyCode
import dev.waystone.app.ui.shared.Haptics
import dev.waystone.app.ui.shared.WaystoneTheme
import kotlin.math.hypot
import kotlin.math.roundToInt

/**
 * Renders a `HotkeyPreset`: keycaps, edge zones, and the virtual 8-way joystick.
 * Mirrors `client-ios/Waystone/UI/Stream/HotkeyLayer.swift`.
 */
@Composable
fun HotkeyLayer(
    preset: HotkeyPreset,
    input: InputController,
    haptics: Haptics,
) {
    Box(modifier = Modifier.fillMaxSize()) {
        preset.edgeZones.forEach { zone ->
            EdgeZoneView(
                zone = zone,
                input = input,
                modifier = Modifier.align(
                    if (zone.side == EdgeZone.Side.LEADING) Alignment.CenterStart else Alignment.CenterEnd
                ),
            )
        }

        preset.groups.forEach { group ->
            val alignment = when (group.anchor) {
                HotkeyGroup.Anchor.LEADING -> Alignment.CenterStart
                HotkeyGroup.Anchor.TRAILING -> Alignment.CenterEnd
                HotkeyGroup.Anchor.TOP -> Alignment.TopCenter
                HotkeyGroup.Anchor.BOTTOM_TRAILING -> Alignment.BottomEnd
            }
            val padModifier = when (group.anchor) {
                HotkeyGroup.Anchor.TRAILING -> Modifier.padding(end = 54.dp)
                HotkeyGroup.Anchor.LEADING -> Modifier.padding(start = 14.dp)
                HotkeyGroup.Anchor.TOP -> Modifier.padding(top = 12.dp)
                HotkeyGroup.Anchor.BOTTOM_TRAILING -> Modifier.padding(end = 54.dp, bottom = 20.dp)
            }
            Box(modifier = Modifier.align(alignment).then(padModifier)) {
                when (group.anchor) {
                    HotkeyGroup.Anchor.LEADING, HotkeyGroup.Anchor.TRAILING -> {
                        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            group.buttons.forEach { button ->
                                HotkeyCapsule(button, CapsuleStyle.SIDE_STACK, input, haptics)
                            }
                        }
                    }
                    HotkeyGroup.Anchor.TOP -> {
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            group.buttons.forEach { button ->
                                HotkeyCapsule(button, CapsuleStyle.TOP_PILL, input, haptics)
                            }
                        }
                    }
                    HotkeyGroup.Anchor.BOTTOM_TRAILING -> {
                        ThumbCluster(group.buttons, input, haptics)
                    }
                }
            }
        }

        if (preset.joystick != null) {
            JoystickView(
                layout = preset.joystick,
                input = input,
                modifier = Modifier
                    .align(Alignment.BottomStart)
                    .padding(start = 36.dp, bottom = 26.dp),
            )
        }
    }
}

private enum class CapsuleStyle { SIDE_STACK, TOP_PILL, THUMB_KEY }

@Composable
private fun ThumbCluster(
    buttons: List<HotkeyButton>,
    input: InputController,
    haptics: Haptics,
) {
    val rows = buttons.chunked(4)
    Column(
        horizontalAlignment = Alignment.End,
        verticalArrangement = Arrangement.spacedBy(7.dp),
    ) {
        rows.forEach { row ->
            Row(horizontalArrangement = Arrangement.spacedBy(7.dp)) {
                row.forEach { button ->
                    HotkeyCapsule(button, CapsuleStyle.THUMB_KEY, input, haptics)
                }
            }
        }
    }
}

@Composable
private fun HotkeyCapsule(
    button: HotkeyButton,
    style: CapsuleStyle,
    input: InputController,
    haptics: Haptics,
) {
    val latchedKeys by input.latchedKeys.collectAsState()
    var pressed by remember { mutableStateOf(false) }
    val latched = (button.action as? KeyAction.Toggle)?.key in latchedKeys

    val minW = when (style) {
        CapsuleStyle.SIDE_STACK -> 74.dp
        CapsuleStyle.TOP_PILL -> 80.dp
        CapsuleStyle.THUMB_KEY -> if (button.cap == null) 40.dp else 58.dp
    }
    val minH = when (style) {
        CapsuleStyle.SIDE_STACK -> 40.dp
        CapsuleStyle.TOP_PILL -> 28.dp
        CapsuleStyle.THUMB_KEY -> if (button.cap == null) 34.dp else 40.dp
    }

    val shape = RoundedCornerShape(6.dp)
    val bgColor = when {
        latched -> WaystoneTheme.Amber
        pressed -> Color.Black.copy(alpha = 0.92f)
        else -> Color.Black.copy(alpha = 0.78f)
    }
    val borderColor = if (latched) WaystoneTheme.Amber else Color.White.copy(alpha = 0.22f)
    val fgColor = if (latched) WaystoneTheme.AmberInk else WaystoneTheme.TextPrimary
    val capColor = if (latched) WaystoneTheme.AmberInk.copy(alpha = 0.75f) else WaystoneTheme.TextSecondary

    Box(
        modifier = Modifier
            .defaultMinSize(minWidth = minW, minHeight = minH)
            .alpha(if (pressed || latched) 0.96f else 0.84f)
            .clip(shape)
            .background(bgColor, shape)
            .border(0.75.dp, borderColor, shape)
            .pointerInput(button) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    pressed = true
                    if (button.action !is KeyAction.Click) haptics.tap()
                    input.perform(button.action, down = true)
                    waitForUpOrCancellation()
                    pressed = false
                    input.perform(button.action, down = false)
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        when (style) {
            CapsuleStyle.TOP_PILL -> {
                Row(
                    modifier = Modifier.padding(horizontal = 12.dp, vertical = 5.dp),
                    horizontalArrangement = Arrangement.spacedBy(7.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = button.title,
                        fontSize = 11.5.sp,
                        fontWeight = FontWeight.SemiBold,
                        color = fgColor,
                        maxLines = 1,
                    )
                    if (button.cap != null) {
                        Text(
                            text = button.cap.uppercase(),
                            fontSize = 9.5.sp,
                            fontWeight = FontWeight.Medium,
                            fontFamily = FontFamily.Monospace,
                            color = capColor,
                            maxLines = 1,
                        )
                    }
                }
            }
            CapsuleStyle.SIDE_STACK -> {
                Column(
                    modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(1.5.dp),
                ) {
                    Text(
                        text = button.title,
                        fontSize = 11.5.sp,
                        fontWeight = FontWeight.SemiBold,
                        color = fgColor,
                        maxLines = 1,
                    )
                    if (button.cap != null) {
                        Text(
                            text = button.cap.uppercase(),
                            fontSize = 9.sp,
                            fontWeight = FontWeight.Medium,
                            fontFamily = FontFamily.Monospace,
                            color = capColor,
                            maxLines = 1,
                        )
                    }
                }
            }
            CapsuleStyle.THUMB_KEY -> {
                if (button.cap != null) {
                    Column(
                        modifier = Modifier.padding(horizontal = 9.dp, vertical = 6.dp),
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(1.5.dp),
                    ) {
                        Text(
                            text = button.title,
                            fontSize = 11.5.sp,
                            fontWeight = FontWeight.SemiBold,
                            color = fgColor,
                            maxLines = 1,
                        )
                        Text(
                            text = button.cap.uppercase(),
                            fontSize = 9.sp,
                            fontWeight = FontWeight.Medium,
                            fontFamily = FontFamily.Monospace,
                            color = capColor,
                            maxLines = 1,
                        )
                    }
                } else {
                    Text(
                        text = button.title,
                        fontSize = 12.5.sp,
                        fontWeight = FontWeight.SemiBold,
                        fontFamily = FontFamily.Monospace,
                        color = fgColor,
                        maxLines = 1,
                    )
                }
            }
        }
    }
}

@Composable
private fun EdgeZoneView(
    zone: EdgeZone,
    input: InputController,
    modifier: Modifier = Modifier,
) {
    var flash by remember { mutableStateOf(false) }
    val alpha = if (flash) 0.14f else 0.01f
    val colors = if (zone.side == EdgeZone.Side.LEADING) {
        listOf(Color.White.copy(alpha = alpha), Color.Transparent)
    } else {
        listOf(Color.Transparent, Color.White.copy(alpha = alpha))
    }

    Box(
        modifier = modifier
            .fillMaxHeight()
            .width(zone.widthDp.dp)
            .background(Brush.horizontalGradient(colors))
            .pointerInput(zone) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    flash = true
                    input.perform(zone.action, down = true)
                    waitForUpOrCancellation()
                    input.perform(zone.action, down = false)
                    flash = false
                }
            },
    )
}

@Composable
private fun JoystickView(
    layout: JoystickLayout,
    input: InputController,
    modifier: Modifier = Modifier,
) {
    val radiusDp = 54.dp
    val density = LocalDensity.current
    val radiusPx = with(density) { radiusDp.toPx() }
    val allKeys = remember(layout) { setOf(layout.up, layout.left, layout.down, layout.right) }

    var knob by remember { mutableStateOf(Offset.Zero) }
    var active by remember { mutableStateOf(false) }

    fun keysFor(vector: Offset): Set<KeyCode> {
        val threshold = radiusPx / 3f
        val result = mutableSetOf<KeyCode>()
        if (vector.y < -threshold) result.add(layout.up)
        if (vector.y > threshold) result.add(layout.down)
        if (vector.x < -threshold) result.add(layout.left)
        if (vector.x > threshold) result.add(layout.right)
        return result
    }

    fun clamp(vector: Offset): Offset {
        val len = hypot(vector.x, vector.y)
        return if (len > radiusPx && len > 0f) {
            Offset(vector.x / len * radiusPx, vector.y / len * radiusPx)
        } else {
            vector
        }
    }

    val activeKeys = keysFor(knob)

    Box(
        modifier = modifier
            .size(radiusDp * 2)
            .pointerInput(layout) {
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    val origin = down.position
                    active = true
                    while (true) {
                        val event = awaitPointerEvent()
                        val change = event.changes.firstOrNull { it.id == down.id } ?: break
                        if (!change.pressed) break
                        change.consume()
                        val clamped = clamp(change.position - origin)
                        knob = clamped
                        input.hold(keysFor(clamped), among = allKeys)
                    }
                    knob = Offset.Zero
                    active = false
                    input.hold(emptySet(), among = allKeys)
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        // Outer ring
        Box(
            modifier = Modifier
                .fillMaxSize()
                .clip(CircleShape)
                .background(Color.Black.copy(alpha = if (active) 0.72f else 0.58f), CircleShape)
                .border(0.75.dp, Color.White.copy(alpha = 0.22f), CircleShape),
        )

        // Inner deadzone ring
        Box(
            modifier = Modifier
                .size(radiusDp * 0.88f)
                .clip(CircleShape)
                .border(0.75.dp, Color.White.copy(alpha = if (active) 0.14f else 0.08f), CircleShape),
        )

        // Four cardinal ticks
        val tickDist = radiusDp - 10.dp
        CardinalTick(engaged = layout.up in activeKeys, modifier = Modifier.offset(y = -tickDist))
        CardinalTick(engaged = layout.down in activeKeys, modifier = Modifier.offset(y = tickDist))
        CardinalTick(engaged = layout.left in activeKeys, modifier = Modifier.offset(x = -tickDist))
        CardinalTick(engaged = layout.right in activeKeys, modifier = Modifier.offset(x = tickDist))

        // Knob
        val knobGray = if (active) 0.86f else 0.60f
        Box(
            modifier = Modifier
                .offset { IntOffset(knob.x.roundToInt(), knob.y.roundToInt()) }
                .size(34.dp)
                .clip(CircleShape)
                .background(Color(knobGray, knobGray, knobGray), CircleShape)
                .border(1.dp, Color.Black, CircleShape),
        )
    }
}

@Composable
private fun CardinalTick(
    engaged: Boolean,
    modifier: Modifier = Modifier,
) {
    Box(
        modifier = modifier
            .size(if (engaged) 5.dp else 3.5.dp)
            .clip(CircleShape)
            .background(if (engaged) Color.White else Color.White.copy(alpha = 0.24f)),
    )
}
