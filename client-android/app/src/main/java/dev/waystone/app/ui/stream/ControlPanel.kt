package dev.waystone.app.ui.stream

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.waystone.app.input.HotkeyPreset
import dev.waystone.app.input.InputController
import dev.waystone.app.stream.StreamPreferences
import dev.waystone.app.stream.StreamSession
import dev.waystone.app.ui.shared.HairlineDivider
import dev.waystone.app.ui.shared.Haptics
import dev.waystone.app.ui.shared.WaystoneSegmentedControl
import dev.waystone.app.ui.shared.WaystoneTheme
import dev.waystone.app.ui.shared.consolePanel

/**
 * The floating ball's panel: 5 switches + keyboard + hangup / lock actions.
 * Mirrors `client-ios/Waystone/UI/Stream/ControlPanel.swift`.
 */
@Composable
fun ControlPanel(
    session: StreamSession,
    input: InputController,
    preferences: StreamPreferences,
    haptics: Haptics,
    onShowKeyboard: () -> Unit,
    onClose: () -> Unit,
) {
    val stage by session.stage.collectAsState()
    val path by session.path.collectAsState()
    val liveFps by session.liveFps.collectAsState()
    val geometry by input.geometry.collectAsState()

    val touchMode by preferences.touchMode.collectAsState()
    val presetId by preferences.presetId.collectAsState()
    val resolution by preferences.resolution.collectAsState()
    val frameRate by preferences.frameRate.collectAsState()
    val sound by preferences.sound.collectAsState()

    val videoSize = geometry.videoSize
    val statsLabel = if (videoSize.width > 0f && videoSize.height > 0f) {
        val res = "${videoSize.width.toInt()}×${videoSize.height.toInt()}"
        if (liveFps > 0) "$res · $liveFps fps" else "$res · ${frameRate.fps} Hz"
    } else {
        null
    }

    Column(
        modifier = Modifier
            .padding(horizontal = 16.dp)
            .widthIn(max = 456.dp)
            .fillMaxWidth()
            .consolePanel(cornerRadius = 8.dp, alpha = 0.96f),
    ) {
        // Header
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 14.dp, vertical = 11.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = session.host.name,
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
                color = WaystoneTheme.TextPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )

            PathCapsule(stage = stage, path = path)

            if (statsLabel != null) {
                Text(
                    text = statsLabel,
                    fontSize = 10.5.sp,
                    fontFamily = FontFamily.Monospace,
                    color = WaystoneTheme.TextTertiary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }

            Spacer(modifier = Modifier.weight(1f))

            if (geometry.isZoomed) {
                val resetShape = RoundedCornerShape(4.dp)
                Box(
                    modifier = Modifier
                        .height(24.dp)
                        .clip(resetShape)
                        .background(WaystoneTheme.SurfaceRaised, resetShape)
                        .border(0.75.dp, WaystoneTheme.Border, resetShape)
                        .clickable {
                            haptics.tap()
                            input.resetZoom()
                        }
                        .padding(horizontal = 8.dp),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        text = "还原缩放",
                        fontSize = 11.5.sp,
                        fontWeight = FontWeight.Medium,
                        color = WaystoneTheme.TextPrimary,
                    )
                }
            }

            val closeShape = RoundedCornerShape(4.dp)
            Box(
                modifier = Modifier
                    .size(24.dp)
                    .clip(closeShape)
                    .background(WaystoneTheme.SurfaceRaised, closeShape)
                    .border(0.75.dp, WaystoneTheme.Border, closeShape)
                    .clickable {
                        haptics.tap()
                        onClose()
                    },
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    imageVector = Icons.Outlined.Close,
                    contentDescription = "收起",
                    tint = WaystoneTheme.TextSecondary,
                    modifier = Modifier.size(13.dp),
                )
            }
        }

        HairlineDivider()

        // 5 segmented rows
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 14.dp, vertical = 10.dp),
            verticalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            ControlRow("触控") {
                WaystoneSegmentedControl(
                    options = InputController.Mode.entries,
                    selection = touchMode,
                    title = { it.title },
                    onSelect = {
                        haptics.selection()
                        session.setTouchMode(it)
                    },
                    modifier = Modifier.weight(1f),
                )
            }

            ControlRow("热键") {
                WaystoneSegmentedControl(
                    options = HotkeyPreset.ALL,
                    selection = HotkeyPreset.named(presetId),
                    title = { it.name },
                    onSelect = {
                        haptics.selection()
                        session.setPreset(it)
                    },
                    modifier = Modifier.weight(1f),
                )
            }

            ControlRow("分辨率") {
                WaystoneSegmentedControl(
                    options = StreamPreferences.Resolution.entries,
                    selection = resolution,
                    title = { it.title },
                    onSelect = {
                        haptics.selection()
                        session.setResolution(it)
                    },
                    modifier = Modifier.weight(1f),
                )
            }

            ControlRow("帧率") {
                WaystoneSegmentedControl(
                    options = StreamPreferences.FrameRate.entries,
                    selection = frameRate,
                    title = { it.title },
                    onSelect = {
                        haptics.selection()
                        session.setFrameRate(it)
                    },
                    modifier = Modifier.weight(1f),
                )
            }

            ControlRow("声音") {
                WaystoneSegmentedControl(
                    options = StreamPreferences.Sound.entries,
                    selection = sound,
                    title = { it.title },
                    onSelect = {
                        haptics.selection()
                        session.setSound(it)
                    },
                    modifier = Modifier.weight(1f),
                )
            }
        }

        HairlineDivider()

        // Action footer
        val btnShape = RoundedCornerShape(5.dp)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .background(WaystoneTheme.FooterSurface)
                .padding(horizontal = 14.dp, vertical = 9.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .height(32.dp)
                    .clip(btnShape)
                    .background(WaystoneTheme.SurfaceRaised, btnShape)
                    .border(0.75.dp, WaystoneTheme.Border, btnShape)
                    .clickable {
                        haptics.tap()
                        onClose()
                        onShowKeyboard()
                    }
                    .padding(horizontal = 12.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = "键盘",
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Medium,
                    color = WaystoneTheme.TextPrimary,
                )
            }

            Spacer(modifier = Modifier.weight(1f))

            Box(
                modifier = Modifier
                    .height(32.dp)
                    .clip(btnShape)
                    .border(0.75.dp, WaystoneTheme.Border, btnShape)
                    .clickable {
                        haptics.tap()
                        session.hangUp(lock = false)
                    }
                    .padding(horizontal = 11.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = "仅断开，不锁屏",
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Medium,
                    color = WaystoneTheme.TextSecondary,
                )
            }

            Box(
                modifier = Modifier
                    .height(32.dp)
                    .clip(btnShape)
                    .background(WaystoneTheme.Coral, btnShape)
                    .clickable {
                        haptics.tap()
                        session.hangUp(lock = true)
                    }
                    .padding(horizontal = 12.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = "断开并锁屏",
                    fontSize = 12.sp,
                    fontWeight = FontWeight.SemiBold,
                    color = Color.White,
                )
            }
        }
    }
}

@Composable
private fun ControlRow(
    title: String,
    content: @Composable androidx.compose.foundation.layout.RowScope.() -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = title,
            fontSize = 11.5.sp,
            fontWeight = FontWeight.Medium,
            color = WaystoneTheme.TextSecondary,
            modifier = Modifier.width(40.dp),
        )
        content()
    }
}

/** "P2P 直连 · 12 ms" in emerald, "TURN 中继 · 40 ms" in amber, or "网络波动" in coral. */
@Composable
fun PathCapsule(
    stage: StreamSession.Stage,
    path: StreamSession.PathInfo,
) {
    val stalled = stage is StreamSession.Stage.Stalled
    val label = when {
        stalled -> "网络波动"
        path.rttMs != null -> "${if (path.relayed) "TURN 中继" else "P2P 直连"} · ${path.rttMs} ms"
        else -> if (path.relayed) "TURN 中继" else "P2P 直连"
    }
    val dotColor = when {
        stalled -> WaystoneTheme.Coral
        path.relayed -> WaystoneTheme.Amber
        else -> WaystoneTheme.Emerald
    }
    val shape = RoundedCornerShape(4.dp)
    Row(
        modifier = Modifier
            .clip(shape)
            .background(Color.Black, shape)
            .border(0.75.dp, WaystoneTheme.Border, shape)
            .padding(horizontal = 7.dp, vertical = 3.dp),
        horizontalArrangement = Arrangement.spacedBy(5.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(5.dp)
                .clip(CircleShape)
                .background(dotColor),
        )
        Text(
            text = label,
            fontSize = 10.5.sp,
            fontFamily = FontFamily.Monospace,
            color = WaystoneTheme.TextSecondary,
            maxLines = 1,
        )
    }
}
