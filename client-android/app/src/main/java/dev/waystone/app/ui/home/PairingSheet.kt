package dev.waystone.app.ui.home

import android.Manifest
import android.content.pm.PackageManager
import android.util.Size
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ContentPaste
import androidx.compose.material.icons.outlined.WarningAmber
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer
import dev.waystone.app.app.AppModel
import dev.waystone.app.protocol.PairingLink
import dev.waystone.app.ui.shared.HairlineDivider
import dev.waystone.app.ui.shared.WaystoneTheme
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.min

/**
 * Add a server: scan the panel's QR code with CameraX + ZXing, paste, or enter manually.
 * Mirrors `client-ios/Waystone/UI/Pairing/PairingSheet.swift`.
 */
@Composable
fun PairingSheetContent(
    model: AppModel,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val clipboard = LocalClipboardManager.current
    val hasCameraFeature = remember {
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)
    }

    var cameraGranted by remember {
        mutableStateOf(
            ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
        )
    }
    var typing by remember { mutableStateOf(!hasCameraFeature) }
    var manual by remember { mutableStateOf("") }
    var hint by remember { mutableStateOf<String?>(null) }

    val permissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission(),
    ) { granted ->
        cameraGranted = granted
        if (!granted) typing = true
    }

    LaunchedEffect(hasCameraFeature, typing) {
        if (hasCameraFeature && !typing && !cameraGranted) {
            permissionLauncher.launch(Manifest.permission.CAMERA)
        }
    }

    fun submit(text: String) {
        val trimmed = text.trim()
        if (PairingLink.parse(trimmed) == null) {
            hint = "配对码无效"
            return
        }
        hint = null
        model.pair(trimmed)
        onDismiss()
    }

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(WaystoneTheme.Canvas)
            .navigationBarsPadding(),
    ) {
        // Sheet header
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(48.dp)
                .padding(horizontal = 18.dp),
        ) {
            Text(
                text = "取消",
                fontSize = 13.5.sp,
                color = WaystoneTheme.TextSecondary,
                modifier = Modifier
                    .align(Alignment.CenterStart)
                    .clickable(onClick = onDismiss),
            )
            Text(
                text = "添加服务器",
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
                color = WaystoneTheme.TextPrimary,
                modifier = Modifier.align(Alignment.Center),
            )
        }
        HairlineDivider()

        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            if (hasCameraFeature && cameraGranted && !typing) {
                ScannerCard(
                    onCode = { code ->
                        model.haptics.success()
                        submit(code)
                    },
                )
            } else {
                ManualEntry(
                    manual = manual,
                    onManualChange = {
                        manual = it
                        hint = null
                    },
                    onSubmit = {
                        model.haptics.tap()
                        submit(manual)
                    },
                )
            }

            if (hint != null) {
                val hintShape = RoundedCornerShape(6.dp)
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .clip(hintShape)
                        .background(WaystoneTheme.Surface, hintShape)
                        .border(0.75.dp, WaystoneTheme.Coral.copy(alpha = 0.55f), hintShape)
                        .padding(horizontal = 12.dp, vertical = 10.dp),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.WarningAmber,
                        contentDescription = null,
                        tint = WaystoneTheme.Coral,
                        modifier = Modifier.size(15.dp),
                    )
                    Text(
                        text = hint.orEmpty(),
                        fontSize = 12.5.sp,
                        fontWeight = FontWeight.Medium,
                        color = WaystoneTheme.TextPrimary,
                    )
                }
            }

            // Action bar: Paste + Toggle scan/manual
            val actionShape = RoundedCornerShape(6.dp)
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Row(
                    modifier = Modifier
                        .weight(1f)
                        .height(38.dp)
                        .clip(actionShape)
                        .background(WaystoneTheme.SurfaceRaised, actionShape)
                        .border(0.75.dp, WaystoneTheme.Border, actionShape)
                        .clickable {
                            model.haptics.tap()
                            val pasted = clipboard.getText()?.text
                            if (!pasted.isNullOrBlank()) {
                                submit(pasted)
                            } else {
                                hint = "剪贴板为空"
                            }
                        },
                    horizontalArrangement = Arrangement.spacedBy(6.dp, Alignment.CenterHorizontally),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.ContentPaste,
                        contentDescription = null,
                        tint = WaystoneTheme.TextPrimary,
                        modifier = Modifier.size(14.dp),
                    )
                    Text(
                        text = "粘贴",
                        fontSize = 13.sp,
                        fontWeight = FontWeight.Medium,
                        color = WaystoneTheme.TextPrimary,
                    )
                }

                if (hasCameraFeature) {
                    Box(
                        modifier = Modifier
                            .weight(1f)
                            .height(38.dp)
                            .clip(actionShape)
                            .background(WaystoneTheme.SurfaceRaised, actionShape)
                            .border(0.75.dp, WaystoneTheme.Border, actionShape)
                            .clickable {
                                model.haptics.selection()
                                typing = !typing
                                hint = null
                            },
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(
                            text = if (typing) "扫码" else "手动输入",
                            fontSize = 13.sp,
                            fontWeight = FontWeight.Medium,
                            color = WaystoneTheme.TextPrimary,
                        )
                    }
                }
            }

            Text(
                text = "在面板「添加我的设备」中生成配对码",
                fontSize = 11.5.sp,
                color = WaystoneTheme.TextTertiary,
                textAlign = TextAlign.Center,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(bottom = 4.dp),
            )
        }
    }
}

@Composable
private fun ManualEntry(
    manual: String,
    onManualChange: (String) -> Unit,
    onSubmit: () -> Unit,
) {
    val fieldShape = RoundedCornerShape(6.dp)
    Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            text = "配对链接",
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
            color = WaystoneTheme.TextSecondary,
        )

        Box(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 104.dp)
                .clip(fieldShape)
                .background(WaystoneTheme.HudSurface, fieldShape)
                .border(0.75.dp, WaystoneTheme.Border, fieldShape)
                .padding(12.dp),
        ) {
            if (manual.isEmpty()) {
                Text(
                    text = "waystone://pair?…",
                    fontSize = 12.5.sp,
                    fontFamily = FontFamily.Monospace,
                    color = WaystoneTheme.TextTertiary,
                )
            }
            BasicTextField(
                value = manual,
                onValueChange = onManualChange,
                textStyle = TextStyle(
                    fontSize = 12.5.sp,
                    fontFamily = FontFamily.Monospace,
                    color = WaystoneTheme.TextPrimary,
                ),
                cursorBrush = SolidColor(WaystoneTheme.Accent),
                modifier = Modifier.fillMaxWidth(),
            )
        }

        val enabled = manual.isNotBlank()
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(44.dp)
                .clip(fieldShape)
                .background(
                    if (enabled) WaystoneTheme.Accent else WaystoneTheme.SurfaceRaised,
                    fieldShape,
                )
                .clickable(enabled = enabled, onClick = onSubmit),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = "添加服务器",
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
                color = if (enabled) WaystoneTheme.AccentInk else WaystoneTheme.TextTertiary,
            )
        }
    }
}

@Composable
private fun ScannerCard(onCode: (String) -> Unit) {
    val cardShape = RoundedCornerShape(8.dp)
    val badgeShape = RoundedCornerShape(5.dp)

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .aspectRatio(1f)
            .clip(cardShape)
            .background(Color.Black, cardShape)
            .border(0.75.dp, WaystoneTheme.Border, cardShape),
    ) {
        QrCameraPreview(
            onCode = onCode,
            modifier = Modifier.fillMaxSize(),
        )

        ScanReticleOverlay(modifier = Modifier.fillMaxSize())

        Box(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(bottom = 14.dp)
                .clip(badgeShape)
                .background(Color.Black.copy(alpha = 0.88f), badgeShape)
                .border(0.75.dp, WaystoneTheme.Border, badgeShape)
                .padding(horizontal = 12.dp, vertical = 6.dp),
        ) {
            Text(
                text = "对准二维码",
                fontSize = 12.sp,
                fontWeight = FontWeight.Medium,
                color = WaystoneTheme.TextPrimary,
            )
        }
    }
}

@Composable
private fun ScanReticleOverlay(modifier: Modifier = Modifier) {
    Canvas(modifier = modifier) {
        val side = min(size.width, size.height) * 0.58f
        val left = (size.width - side) / 2f
        val top = (size.height - side) / 2f - 10.dp.toPx()
        val right = left + side
        val bottom = top + side
        val len = 18.dp.toPx()
        val stroke = 2.dp.toPx()
        val color = Color.White.copy(alpha = 0.85f)

        // Top-left
        drawLine(color, Offset(left, top + len), Offset(left, top), stroke, StrokeCap.Square)
        drawLine(color, Offset(left, top), Offset(left + len, top), stroke, StrokeCap.Square)
        // Top-right
        drawLine(color, Offset(right - len, top), Offset(right, top), stroke, StrokeCap.Square)
        drawLine(color, Offset(right, top), Offset(right, top + len), stroke, StrokeCap.Square)
        // Bottom-right
        drawLine(color, Offset(right, bottom - len), Offset(right, bottom), stroke, StrokeCap.Square)
        drawLine(color, Offset(right, bottom), Offset(right - len, bottom), stroke, StrokeCap.Square)
        // Bottom-left
        drawLine(color, Offset(left + len, bottom), Offset(left, bottom), stroke, StrokeCap.Square)
        drawLine(color, Offset(left, bottom), Offset(left, bottom - len), stroke, StrokeCap.Square)
    }
}

@Composable
private fun QrCameraPreview(
    onCode: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val executor = remember { Executors.newSingleThreadExecutor() }
    val delivered = remember { AtomicBoolean(false) }
    val providerHolder = remember { arrayOfNulls<ProcessCameraProvider>(1) }

    DisposableEffect(Unit) {
        onDispose {
            runCatching { providerHolder[0]?.unbindAll() }
            providerHolder[0] = null
            executor.shutdown()
        }
    }

    AndroidView(
        modifier = modifier,
        factory = { ctx ->
            val previewView = PreviewView(ctx).apply {
                scaleType = PreviewView.ScaleType.FILL_CENTER
            }
            val providerFuture = ProcessCameraProvider.getInstance(ctx)
            providerFuture.addListener(
                {
                    runCatching {
                        val provider = providerFuture.get()
                        providerHolder[0] = provider
                        val preview = Preview.Builder().build().also {
                            it.surfaceProvider = previewView.surfaceProvider
                        }
                        val reader = MultiFormatReader().apply {
                            setHints(
                                mapOf(
                                    DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE),
                                )
                            )
                        }
                        @Suppress("DEPRECATION")
                        val analysis = ImageAnalysis.Builder()
                            .setTargetResolution(Size(960, 720))
                            .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                            .build()
                        analysis.setAnalyzer(executor) { image ->
                            decodeQrFrame(image, reader)?.let { code ->
                                if (PairingLink.parse(code) != null && delivered.compareAndSet(false, true)) {
                                    ContextCompat.getMainExecutor(ctx).execute { onCode(code) }
                                }
                            }
                            image.close()
                        }
                        provider.unbindAll()
                        provider.bindToLifecycle(
                            lifecycleOwner,
                            CameraSelector.DEFAULT_BACK_CAMERA,
                            preview,
                            analysis,
                        )
                    }
                },
                ContextCompat.getMainExecutor(ctx),
            )
            previewView
        },
    )
}

private fun decodeQrFrame(image: ImageProxy, reader: MultiFormatReader): String? {
    val plane = image.planes.firstOrNull() ?: return null
    val buffer = plane.buffer
    val data = ByteArray(buffer.remaining())
    buffer.get(data)
    val width = image.width
    val height = image.height
    val rowStride = plane.rowStride
    val yBytes = if (rowStride == width) {
        data
    } else {
        ByteArray(width * height).also { compact ->
            for (row in 0 until height) {
                System.arraycopy(data, row * rowStride, compact, row * width, width)
            }
        }
    }
    val source = PlanarYUVLuminanceSource(yBytes, width, height, 0, 0, width, height, false)
    val bitmap = BinaryBitmap(HybridBinarizer(source))
    return try {
        reader.decodeWithState(bitmap).text
    } catch (_: Exception) {
        null
    } finally {
        reader.reset()
    }
}
