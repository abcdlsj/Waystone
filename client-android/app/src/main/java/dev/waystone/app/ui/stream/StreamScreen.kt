package dev.waystone.app.ui.stream

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.waystone.app.input.HotkeyPreset
import dev.waystone.app.input.InputController
import dev.waystone.app.input.Point2D
import dev.waystone.app.stream.StreamPreferences
import dev.waystone.app.stream.StreamSession
import dev.waystone.app.ui.shared.ConsoleProgressBar
import dev.waystone.app.ui.shared.Haptics
import dev.waystone.app.ui.shared.WaystoneTheme
import dev.waystone.app.ui.shared.consolePanel

/**
 * Full-screen landscape stream view matching `client-ios/Waystone/UI/Stream/StreamView.swift`.
 */
@Composable
fun StreamScreen(
    session: StreamSession,
    preferences: StreamPreferences,
    haptics: Haptics,
) {
    val stage by session.stage.collectAsState()
    val path by session.path.collectAsState()
    val progress by session.progress.collectAsState()

    val input = session.input
    val geometry by input.geometry.collectAsState()
    val touchMode by input.mode.collectAsState()
    val cursor by input.cursor.collectAsState()
    val directPointerActive by input.directPointerActive.collectAsState()
    val presetId by preferences.presetId.collectAsState()

    var panelOpen by remember { mutableStateOf(false) }
    var keyboardVisible by remember { mutableStateOf(false) }

    BackHandler {
        when {
            keyboardVisible -> keyboardVisible = false
            panelOpen -> panelOpen = false
            else -> panelOpen = true
        }
    }

    val health = when (stage) {
        is StreamSession.Stage.Stalled -> BallHealth.STALLED
        StreamSession.Stage.Connecting -> BallHealth.DEGRADED
        else -> if (path.relayed) BallHealth.DEGRADED else BallHealth.GOOD
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black),
    ) {
        StreamSurface(
            session = session,
            geometry = geometry,
            modifier = Modifier.fillMaxSize(),
        )

        val pointerPos = geometry.surfacePoint(cursor.first, cursor.second)
        if (touchMode == InputController.Mode.TRACKPAD) {
            LocalCursor(position = pointerPos)
        } else if (directPointerActive) {
            DirectTouchReticle(position = pointerPos)
        }

        if (stage != StreamSession.Stage.Connecting) {
            HotkeyLayer(
                preset = HotkeyPreset.named(presetId),
                input = input,
                haptics = haptics,
            )
        }

        if (stage is StreamSession.Stage.Stalled) {
            StallBanner(
                modifier = Modifier
                    .align(Alignment.TopCenter)
                    .padding(top = 12.dp),
            )
        }

        if (stage == StreamSession.Stage.Connecting) {
            ConnectingOverlay(
                host = session.host.name,
                progress = progress,
                onCancel = {
                    haptics.tap()
                    session.hangUp(lock = false)
                },
            )
        }

        FloatingBall(
            health = health,
            preferences = preferences,
            haptics = haptics,
            onTap = { panelOpen = !panelOpen },
        )

        SoftKeyboardOverlay(
            visible = keyboardVisible,
            input = input,
            haptics = haptics,
            onDismiss = { keyboardVisible = false },
            modifier = Modifier.align(Alignment.BottomCenter),
        )

        AnimatedVisibility(
            visible = panelOpen,
            enter = fadeIn(),
            exit = fadeOut(),
        ) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(Color.Black.copy(alpha = 0.58f))
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                        onClick = { panelOpen = false },
                    ),
                contentAlignment = Alignment.Center,
            ) {
                Box(
                    modifier = Modifier.clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                        onClick = {},
                    ),
                ) {
                    ControlPanel(
                        session = session,
                        input = input,
                        preferences = preferences,
                        haptics = haptics,
                        onShowKeyboard = { keyboardVisible = true },
                        onClose = { panelOpen = false },
                    )
                }
            }
        }
    }
}

@Composable
private fun LocalCursor(position: Point2D) {
    Box(
        modifier = Modifier
            .offset(x = (position.x - 7f).dp, y = (position.y - 7f).dp)
            .size(14.dp),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(14.dp)
                .clip(CircleShape)
                .background(Color.Black.copy(alpha = 0.18f), CircleShape)
                .border(2.5.dp, Color.Black.copy(alpha = 0.75f), CircleShape)
                .border(1.25.dp, Color.White.copy(alpha = 0.95f), CircleShape),
        )
        Box(
            modifier = Modifier
                .size(2.5.dp)
                .clip(CircleShape)
                .background(Color.White),
        )
    }
}

@Composable
private fun DirectTouchReticle(position: Point2D) {
    Box(
        modifier = Modifier
            .offset(x = (position.x - 9f).dp, y = (position.y - 9f).dp)
            .size(18.dp)
            .clip(CircleShape)
            .border(2.dp, Color.Black.copy(alpha = 0.45f), CircleShape)
            .border(1.dp, Color.White.copy(alpha = 0.75f), CircleShape),
    )
}

@Composable
private fun StallBanner(modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .consolePanel(cornerRadius = 6.dp, alpha = 0.94f)
            .padding(horizontal = 12.dp, vertical = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(5.dp)
                .clip(CircleShape)
                .background(WaystoneTheme.Coral),
        )
        Text(
            text = "网络波动，正在恢复画面…",
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
            color = WaystoneTheme.TextPrimary,
        )
    }
}

@Composable
private fun ConnectingOverlay(
    host: String,
    progress: String,
    onCancel: () -> Unit,
) {
    val cancelShape = RoundedCornerShape(5.dp)
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(WaystoneTheme.Canvas),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            modifier = Modifier
                .widthIn(max = 320.dp)
                .fillMaxWidth()
                .padding(horizontal = 24.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(
                text = host,
                fontSize = 17.sp,
                fontWeight = FontWeight.SemiBold,
                color = WaystoneTheme.TextPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )

            ConsoleProgressBar()

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = progress,
                    fontSize = 12.sp,
                    fontFamily = FontFamily.Monospace,
                    color = WaystoneTheme.TextSecondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )

                Spacer(modifier = Modifier.weight(1f))

                Box(
                    modifier = Modifier
                        .height(30.dp)
                        .clip(cancelShape)
                        .border(0.75.dp, WaystoneTheme.Border, cancelShape)
                        .clickable(onClick = onCancel)
                        .padding(horizontal = 14.dp),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        text = "取消",
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                        color = WaystoneTheme.TextSecondary,
                    )
                }
            }
        }
    }
}
