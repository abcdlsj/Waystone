package dev.waystone.app.ui.home

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.Computer
import androidx.compose.material.icons.outlined.Delete
import androidx.compose.material.icons.outlined.DesktopWindows
import androidx.compose.material.icons.outlined.Dns
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.LaptopMac
import androidx.compose.material.icons.outlined.MoreHoriz
import androidx.compose.material.icons.outlined.QrCodeScanner
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.waystone.app.app.AppModel
import dev.waystone.app.pool.Host
import dev.waystone.app.pool.PoolConnection
import dev.waystone.app.protocol.Wire
import dev.waystone.app.stream.StreamPreferences
import dev.waystone.app.ui.shared.HairlineDivider
import dev.waystone.app.ui.shared.WaystoneMark
import dev.waystone.app.ui.shared.WaystoneTheme
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * Home screen: every machine in every enrolled pool, one tap to stream.
 * Mirrors `client-ios/Waystone/UI/Home/HomeView.swift`.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HomeScreen(model: AppModel) {
    val pools by model.pools.collectAsState()
    val pairing by model.pairing.collectAsState()
    val notice by model.notice.collectAsState()

    var addingServer by remember { mutableStateOf(false) }
    var showingSettings by remember { mutableStateOf(false) }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(WaystoneTheme.Canvas)
            .statusBarsPadding()
            .navigationBarsPadding(),
    ) {
        Column(modifier = Modifier.fillMaxSize()) {
            TopBar(
                onSettings = {
                    model.haptics.tap()
                    showingSettings = true
                },
                onAdd = {
                    model.haptics.tap()
                    addingServer = true
                },
            )

            if (pools.isEmpty()) {
                EmptyPoolView(
                    onAdd = {
                        model.haptics.tap()
                        addingServer = true
                    }
                )
            } else {
                PoolList(pools = pools, model = model)
            }
        }

        NoticeOverlay(
            pairing = pairing,
            notice = notice,
            onDismissNotice = { model.clearNotice() },
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(horizontal = 16.dp, vertical = 16.dp),
        )
    }

    if (addingServer) {
        val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
        ModalBottomSheet(
            onDismissRequest = { addingServer = false },
            sheetState = sheetState,
            containerColor = WaystoneTheme.Canvas,
            scrimColor = Color.Black.copy(alpha = 0.72f),
            dragHandle = null,
        ) {
            PairingSheetContent(
                model = model,
                onDismiss = { addingServer = false },
            )
        }
    }

    if (showingSettings) {
        val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
        ModalBottomSheet(
            onDismissRequest = { showingSettings = false },
            sheetState = sheetState,
            containerColor = WaystoneTheme.Canvas,
            scrimColor = Color.Black.copy(alpha = 0.72f),
            dragHandle = null,
        ) {
            SettingsSheetContent(
                preferences = model.preferences,
                onDismiss = {
                    model.haptics.tap()
                    showingSettings = false
                },
            )
        }
    }

    val failed = pairing as? AppModel.Pairing.Failed
    if (failed != null) {
        AlertDialog(
            onDismissRequest = { model.dismissPairingError() },
            containerColor = WaystoneTheme.SurfaceRaised,
            titleContentColor = WaystoneTheme.TextPrimary,
            textContentColor = WaystoneTheme.TextSecondary,
            title = { Text("添加失败", fontWeight = FontWeight.SemiBold) },
            text = { Text(failed.reason) },
            confirmButton = {
                TextButton(onClick = { model.dismissPairingError() }) {
                    Text("好", color = WaystoneTheme.TextPrimary)
                }
            },
        )
    }
}

@Composable
private fun TopBar(
    onSettings: () -> Unit,
    onAdd: () -> Unit,
) {
    val buttonShape = RoundedCornerShape(5.dp)
    Column(modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(48.dp)
                .padding(horizontal = 18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Row(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                WaystoneMark(
                    modifier = Modifier.size(15.dp),
                    color = WaystoneTheme.TextPrimary,
                )
                Text(
                    text = "WAYSTONE",
                    fontSize = 13.sp,
                    fontWeight = FontWeight.Bold,
                    fontFamily = FontFamily.Monospace,
                    letterSpacing = 1.8.sp,
                    color = WaystoneTheme.TextPrimary,
                )
            }

            Spacer(modifier = Modifier.weight(1f))

            Row(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    modifier = Modifier
                        .size(28.dp)
                        .clip(buttonShape)
                        .border(0.75.dp, WaystoneTheme.Border, buttonShape)
                        .clickable(onClick = onSettings),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.Settings,
                        contentDescription = "设置",
                        tint = WaystoneTheme.TextSecondary,
                        modifier = Modifier.size(15.dp),
                    )
                }

                Row(
                    modifier = Modifier
                        .height(28.dp)
                        .clip(buttonShape)
                        .border(0.75.dp, WaystoneTheme.Border, buttonShape)
                        .clickable(onClick = onAdd)
                        .padding(horizontal = 10.dp),
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.Add,
                        contentDescription = null,
                        tint = WaystoneTheme.TextPrimary,
                        modifier = Modifier.size(13.dp),
                    )
                    Text(
                        text = "添加",
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                        color = WaystoneTheme.TextPrimary,
                    )
                }
            }
        }
        HairlineDivider()
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PoolList(
    pools: List<PoolConnection>,
    model: AppModel,
) {
    var refreshing by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    PullToRefreshBox(
        isRefreshing = refreshing,
        onRefresh = {
            model.haptics.tap()
            refreshing = true
            model.refresh()
            scope.launch {
                delay(400)
                refreshing = false
            }
        },
        modifier = Modifier.fillMaxSize(),
    ) {
        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(top = 16.dp, bottom = 36.dp),
            verticalArrangement = Arrangement.spacedBy(24.dp),
        ) {
            items(pools, key = { it.id }) { pool ->
                PoolSection(pool = pool, model = model)
            }
        }
    }
}

@Composable
private fun PoolSection(
    pool: PoolConnection,
    model: AppModel,
) {
    val status by pool.status.collectAsState()
    val hosts by pool.hosts.collectAsState()
    var menuExpanded by remember { mutableStateOf(false) }
    var confirmForget by remember { mutableStateOf(false) }

    Column(modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 18.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = pool.instance.endpoint,
                fontSize = 11.5.sp,
                fontWeight = FontWeight.SemiBold,
                fontFamily = FontFamily.Monospace,
                color = WaystoneTheme.TextSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )

            PoolStatusLabel(
                status = status,
                onRetry = {
                    model.haptics.tap()
                    pool.reconnectNow()
                },
            )

            Box {
                Box(
                    modifier = Modifier
                        .size(width = 24.dp, height = 22.dp)
                        .clickable { menuExpanded = true },
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.MoreHoriz,
                        contentDescription = "服务器选项",
                        tint = WaystoneTheme.TextTertiary,
                        modifier = Modifier.size(16.dp),
                    )
                }

                DropdownMenu(
                    expanded = menuExpanded,
                    onDismissRequest = { menuExpanded = false },
                    containerColor = WaystoneTheme.SurfaceRaised,
                ) {
                    DropdownMenuItem(
                        text = { Text("重新连接", color = WaystoneTheme.TextPrimary, fontSize = 13.sp) },
                        leadingIcon = {
                            Icon(Icons.Outlined.Refresh, contentDescription = null, tint = WaystoneTheme.TextSecondary, modifier = Modifier.size(16.dp))
                        },
                        onClick = {
                            menuExpanded = false
                            pool.reconnectNow()
                        },
                    )
                    DropdownMenuItem(
                        text = { Text("移除此服务器", color = WaystoneTheme.Coral, fontSize = 13.sp) },
                        leadingIcon = {
                            Icon(Icons.Outlined.Delete, contentDescription = null, tint = WaystoneTheme.Coral, modifier = Modifier.size(16.dp))
                        },
                        onClick = {
                            menuExpanded = false
                            confirmForget = true
                        },
                    )
                }
            }
        }

        Spacer(modifier = Modifier.height(8.dp))

        Column(
            modifier = Modifier
                .fillMaxWidth()
                .background(WaystoneTheme.Ledger),
        ) {
            HairlineDivider()

            val currentStatus = status
            when {
                currentStatus is PoolConnection.Status.Online && hosts.isEmpty() -> EmptyHostsRow()
                currentStatus is PoolConnection.Status.Rejected -> RejectedRow(reason = currentStatus.reason)
                hosts.isEmpty() && currentStatus is PoolConnection.Status.Connecting -> ConnectingRow()
                hosts.isEmpty() -> OfflineRow(onRetry = {
                    model.haptics.tap()
                    pool.reconnectNow()
                })
                else -> {
                    hosts.forEachIndexed { index, host ->
                        if (index > 0) HairlineDivider()
                        HostRow(
                            host = host,
                            reachable = currentStatus is PoolConnection.Status.Online,
                            onConnect = {
                                model.haptics.tap()
                                model.connect(host, pool)
                            },
                        )
                    }
                }
            }

            HairlineDivider()
        }
    }

    if (confirmForget) {
        AlertDialog(
            onDismissRequest = { confirmForget = false },
            containerColor = WaystoneTheme.SurfaceRaised,
            titleContentColor = WaystoneTheme.TextPrimary,
            textContentColor = WaystoneTheme.TextSecondary,
            title = { Text("移除 ${pool.instance.label}？", fontWeight = FontWeight.SemiBold) },
            text = { Text("移除后需重新扫码添加。") },
            dismissButton = {
                TextButton(onClick = { confirmForget = false }) {
                    Text("取消", color = WaystoneTheme.TextSecondary)
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    confirmForget = false
                    model.forget(pool)
                }) {
                    Text("移除", color = WaystoneTheme.Coral, fontWeight = FontWeight.SemiBold)
                }
            },
        )
    }
}

@Composable
private fun PoolStatusLabel(
    status: PoolConnection.Status,
    onRetry: () -> Unit,
) {
    when (status) {
        PoolConnection.Status.Online -> {
            Text(
                text = "在线",
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                color = WaystoneTheme.TextTertiary,
            )
        }
        PoolConnection.Status.Connecting -> {
            Row(
                horizontalArrangement = Arrangement.spacedBy(5.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                CircularProgressIndicator(
                    modifier = Modifier.size(10.dp),
                    strokeWidth = 1.5.dp,
                    color = WaystoneTheme.TextSecondary,
                )
                Text(
                    text = "连接中",
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    color = WaystoneTheme.TextSecondary,
                )
            }
        }
        is PoolConnection.Status.Offline -> {
            Text(
                text = status.reason,
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                color = WaystoneTheme.Amber,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.clickable(onClick = onRetry),
            )
        }
        is PoolConnection.Status.Rejected -> {
            Text(
                text = "已移除",
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                color = WaystoneTheme.Coral,
            )
        }
    }
}

@Composable
private fun EmptyHostsRow() {
    val cmdShape = RoundedCornerShape(4.dp)
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 18.dp, vertical = 14.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            text = "暂无主机",
            fontSize = 14.sp,
            fontWeight = FontWeight.SemiBold,
            color = WaystoneTheme.TextPrimary,
        )
        Text(
            text = "在电脑上运行以下命令并粘贴主机配对码：",
            fontSize = 12.5.sp,
            color = WaystoneTheme.TextSecondary,
        )
        Text(
            text = "waystone-host pair",
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
            fontFamily = FontFamily.Monospace,
            color = WaystoneTheme.TextPrimary,
            modifier = Modifier
                .clip(cmdShape)
                .background(Color.Black, cmdShape)
                .border(0.75.dp, WaystoneTheme.Border, cmdShape)
                .padding(horizontal = 8.dp, vertical = 5.dp),
        )
    }
}

@Composable
private fun RejectedRow(reason: String) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 18.dp, vertical = 14.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = Icons.Outlined.ErrorOutline,
            contentDescription = null,
            tint = WaystoneTheme.Coral,
            modifier = Modifier.size(16.dp),
        )
        Text(
            text = reason,
            fontSize = 13.sp,
            fontWeight = FontWeight.Medium,
            color = WaystoneTheme.TextPrimary,
        )
    }
}

@Composable
private fun ConnectingRow() {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 18.dp, vertical = 14.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CircularProgressIndicator(
            modifier = Modifier.size(14.dp),
            strokeWidth = 1.5.dp,
            color = WaystoneTheme.TextSecondary,
        )
        Text(
            text = "连接中…",
            fontSize = 13.sp,
            color = WaystoneTheme.TextSecondary,
        )
    }
}

@Composable
private fun OfflineRow(onRetry: () -> Unit) {
    val btnShape = RoundedCornerShape(5.dp)
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 18.dp, vertical = 13.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = "未连接",
            fontSize = 13.sp,
            color = WaystoneTheme.TextSecondary,
        )
        Spacer(modifier = Modifier.weight(1f))
        Box(
            modifier = Modifier
                .height(28.dp)
                .clip(btnShape)
                .background(WaystoneTheme.SurfaceRaised, btnShape)
                .border(0.75.dp, WaystoneTheme.Border, btnShape)
                .clickable(onClick = onRetry)
                .padding(horizontal = 12.dp),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = "重试",
                fontSize = 12.sp,
                fontWeight = FontWeight.Medium,
                color = WaystoneTheme.TextPrimary,
            )
        }
    }
}

@Composable
private fun HostRow(
    host: Host,
    reachable: Boolean,
    onConnect: () -> Unit,
) {
    val available = reachable && host.online && !host.busy
    val subtitleText = when {
        !reachable -> "服务器未连接"
        !host.online -> "离线"
        host.busy -> "占用中"
        else -> when (host.platform) {
            Wire.Platform.MACOS -> "macOS"
            Wire.Platform.WINDOWS -> "Windows"
            Wire.Platform.LINUX -> "Linux"
            else -> "在线"
        }
    }
    val subtitleColor = when {
        !reachable || !host.online -> WaystoneTheme.TextTertiary
        host.busy -> WaystoneTheme.Amber
        else -> WaystoneTheme.TextTertiary
    }
    val icon: ImageVector = when (host.platform) {
        Wire.Platform.MACOS -> Icons.Outlined.LaptopMac
        Wire.Platform.WINDOWS -> Icons.Outlined.DesktopWindows
        Wire.Platform.LINUX -> Icons.Outlined.Dns
        else -> Icons.Outlined.Computer
    }

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(enabled = available, onClick = onConnect)
            .padding(horizontal = 18.dp, vertical = 14.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = icon,
            contentDescription = null,
            tint = if (available) WaystoneTheme.TextPrimary else WaystoneTheme.TextTertiary,
            modifier = Modifier.size(18.dp),
        )

        Column(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(2.5.dp),
        ) {
            Text(
                text = host.name,
                fontSize = 15.sp,
                fontWeight = if (available) FontWeight.SemiBold else FontWeight.Medium,
                color = if (available) WaystoneTheme.TextPrimary else WaystoneTheme.TextSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                text = subtitleText,
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                color = subtitleColor,
                maxLines = 1,
            )
        }

        if (available) {
            Row(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (host.rttMs != null) {
                    Text(
                        text = "${host.rttMs} ms",
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                        fontFamily = FontFamily.Monospace,
                        color = WaystoneTheme.latencyColor(host.rttMs),
                    )
                }
                Text(
                    text = "→",
                    fontSize = 13.sp,
                    fontFamily = FontFamily.Monospace,
                    color = WaystoneTheme.TextTertiary,
                )
            }
        } else {
            Text(
                text = "—",
                fontSize = 12.sp,
                fontFamily = FontFamily.Monospace,
                color = WaystoneTheme.TextQuaternary,
            )
        }
    }
}

@Composable
private fun EmptyPoolView(onAdd: () -> Unit) {
    val buttonShape = RoundedCornerShape(6.dp)
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 24.dp),
    ) {
        Spacer(modifier = Modifier.weight(1f))

        Text(
            text = "暂无服务器",
            fontSize = 20.sp,
            fontWeight = FontWeight.SemiBold,
            color = WaystoneTheme.TextPrimary,
            modifier = Modifier.padding(bottom = 18.dp),
        )

        Column(modifier = Modifier.fillMaxWidth().padding(bottom = 22.dp)) {
            HairlineDivider()
            StepRow(index = "1", text = "在 Waystone 面板点击「添加我的设备」")
            HairlineDivider()
            StepRow(index = "2", text = "扫码或粘贴配对链接")
            HairlineDivider()
        }

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(44.dp)
                .clip(buttonShape)
                .background(WaystoneTheme.Accent, buttonShape)
                .clickable(onClick = onAdd),
            horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterHorizontally),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                imageVector = Icons.Outlined.QrCodeScanner,
                contentDescription = null,
                tint = WaystoneTheme.AccentInk,
                modifier = Modifier.size(17.dp),
            )
            Text(
                text = "扫码添加",
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
                color = WaystoneTheme.AccentInk,
            )
        }

        Spacer(modifier = Modifier.weight(2f))
    }
}

@Composable
private fun StepRow(index: String, text: String) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.Top,
    ) {
        Text(
            text = index,
            fontSize = 11.sp,
            fontFamily = FontFamily.Monospace,
            color = WaystoneTheme.TextTertiary,
        )
        Text(
            text = text,
            fontSize = 12.5.sp,
            color = WaystoneTheme.TextSecondary,
        )
    }
}

@Composable
private fun NoticeOverlay(
    pairing: AppModel.Pairing,
    notice: AppModel.Notice?,
    onDismissNotice: () -> Unit,
    modifier: Modifier = Modifier,
) {
    LaunchedEffect(notice?.id) {
        if (notice != null) {
            delay(6000)
            onDismissNotice()
        }
    }

    Column(
        modifier = modifier,
        verticalArrangement = Arrangement.spacedBy(8.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        val inProgress = pairing as? AppModel.Pairing.InProgress
        AnimatedVisibility(
            visible = inProgress != null,
            enter = slideInVertically(initialOffsetY = { it / 2 }) + fadeIn(),
            exit = slideOutVertically(targetOffsetY = { it / 2 }) + fadeOut(),
        ) {
            if (inProgress != null) {
                NoticeCard(
                    showProgress = true,
                    title = "正在添加 ${inProgress.endpoint}…",
                    message = null,
                )
            }
        }

        AnimatedVisibility(
            visible = notice != null,
            enter = slideInVertically(initialOffsetY = { it / 2 }) + fadeIn(),
            exit = slideOutVertically(targetOffsetY = { it / 2 }) + fadeOut(),
        ) {
            if (notice != null) {
                NoticeCard(
                    showProgress = false,
                    title = notice.title,
                    message = notice.message,
                    modifier = Modifier.clickable(onClick = onDismissNotice),
                )
            }
        }
    }
}

@Composable
private fun NoticeCard(
    showProgress: Boolean,
    title: String,
    message: String?,
    modifier: Modifier = Modifier,
) {
    val shape = RoundedCornerShape(8.dp)
    Row(
        modifier = modifier
            .widthIn(max = 460.dp)
            .fillMaxWidth()
            .clip(shape)
            .background(WaystoneTheme.SurfaceRaised, shape)
            .border(0.75.dp, WaystoneTheme.BorderStrong, shape)
            .padding(horizontal = 14.dp, vertical = 11.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (showProgress) {
            CircularProgressIndicator(
                modifier = Modifier.size(16.dp),
                strokeWidth = 1.75.dp,
                color = WaystoneTheme.TextPrimary,
            )
        } else {
            Icon(
                imageVector = Icons.Outlined.Info,
                contentDescription = null,
                tint = WaystoneTheme.Amber,
                modifier = Modifier.size(16.dp),
            )
        }

        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(
                text = title,
                fontSize = 13.sp,
                fontWeight = FontWeight.SemiBold,
                color = WaystoneTheme.TextPrimary,
            )
            if (message != null) {
                Text(
                    text = message,
                    fontSize = 12.sp,
                    color = WaystoneTheme.TextSecondary,
                )
            }
        }
    }
}

@Composable
private fun SettingsSheetContent(
    preferences: StreamPreferences,
    onDismiss: () -> Unit,
) {
    val preventSleep by preferences.preventSleep.collectAsState()

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(WaystoneTheme.Canvas)
            .navigationBarsPadding()
            .padding(bottom = 28.dp),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(start = 18.dp, end = 18.dp, top = 20.dp, bottom = 14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "设置",
                fontSize = 16.sp,
                fontWeight = FontWeight.SemiBold,
                color = WaystoneTheme.TextPrimary,
            )
            Spacer(modifier = Modifier.weight(1f))
            Text(
                text = "完成",
                fontSize = 13.sp,
                fontWeight = FontWeight.Medium,
                color = WaystoneTheme.TextSecondary,
                modifier = Modifier.clickable(onClick = onDismiss),
            )
        }

        HairlineDivider()

        Text(
            text = "电源与显示",
            fontSize = 11.5.sp,
            fontWeight = FontWeight.SemiBold,
            fontFamily = FontFamily.Monospace,
            color = WaystoneTheme.TextSecondary,
            modifier = Modifier.padding(start = 18.dp, end = 18.dp, top = 18.dp, bottom = 8.dp),
        )

        Column(
            modifier = Modifier
                .fillMaxWidth()
                .background(WaystoneTheme.Ledger),
        ) {
            HairlineDivider()

            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 18.dp, vertical = 14.dp),
                horizontalArrangement = Arrangement.spacedBy(14.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(3.dp),
                ) {
                    Text(
                        text = "防止自动休眠",
                        fontSize = 14.5.sp,
                        fontWeight = FontWeight.Medium,
                        color = WaystoneTheme.TextPrimary,
                    )
                    Text(
                        text = "应用在前台时保持屏幕常亮，避免串流或等待时自动锁屏。",
                        fontSize = 12.sp,
                        color = WaystoneTheme.TextSecondary,
                    )
                }

                Switch(
                    checked = preventSleep,
                    onCheckedChange = { preferences.setPreventSleep(it) },
                    colors = SwitchDefaults.colors(
                        checkedThumbColor = Color.White,
                        checkedTrackColor = WaystoneTheme.Emerald,
                        uncheckedThumbColor = WaystoneTheme.TextSecondary,
                        uncheckedTrackColor = WaystoneTheme.SurfaceRaised,
                        uncheckedBorderColor = WaystoneTheme.Border,
                    ),
                )
            }

            HairlineDivider()
        }
    }
}
