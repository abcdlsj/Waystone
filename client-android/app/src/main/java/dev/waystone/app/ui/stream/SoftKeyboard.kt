package dev.waystone.app.ui.stream

import android.content.Context
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.automirrored.outlined.ArrowForward
import androidx.compose.material.icons.outlined.ArrowDownward
import androidx.compose.material.icons.outlined.ArrowUpward
import androidx.compose.material.icons.outlined.KeyboardHide
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import dev.waystone.app.input.InputController
import dev.waystone.app.protocol.KeyCode
import dev.waystone.app.protocol.Modifiers
import dev.waystone.app.ui.shared.HairlineDivider
import dev.waystone.app.ui.shared.Haptics
import dev.waystone.app.ui.shared.WaystoneTheme

/**
 * System soft keyboard bridge plus the PC special-key bar (Esc, Tab, Ctrl/Alt/Shift/Win,
 * arrows, F1-F12). Mirrors `client-ios/Waystone/UI/Stream/SoftKeyboard.swift`.
 */
@Composable
fun SoftKeyboardOverlay(
    visible: Boolean,
    input: InputController,
    haptics: Haptics,
    onDismiss: () -> Unit,
    modifier: Modifier = Modifier,
) {
    if (!visible) return

    Column(
        modifier = modifier
            .fillMaxWidth()
            .imePadding(),
    ) {
        HiddenImeBridge(input = input)
        KeyboardBar(input = input, haptics = haptics, onDismiss = onDismiss)
    }
}

@Composable
private fun HiddenImeBridge(input: InputController) {
    val editTextHolder = remember { arrayOfNulls<EditText>(1) }

    DisposableEffect(Unit) {
        onDispose {
            editTextHolder[0]?.let { et ->
                val imm = et.context.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager
                imm?.hideSoftInputFromWindow(et.windowToken, 0)
            }
            editTextHolder[0] = null
        }
    }

    AndroidView(
        modifier = Modifier
            .size(1.dp)
            .alpha(0f),
        factory = { ctx ->
            var resetting = false
            val sentinel = " "
            EditText(ctx).apply {
                editTextHolder[0] = this
                inputType = InputType.TYPE_CLASS_TEXT or
                    InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
                    InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI
                isSingleLine = true
                setText(sentinel)
                setSelection(sentinel.length)

                setOnKeyListener { _, keyCode, event ->
                    if (event.action == KeyEvent.ACTION_DOWN) {
                        when (keyCode) {
                            KeyEvent.KEYCODE_DEL -> {
                                input.tap(KeyCode.BACKSPACE)
                                return@setOnKeyListener true
                            }
                            KeyEvent.KEYCODE_ENTER -> {
                                input.tap(KeyCode.ENTER)
                                return@setOnKeyListener true
                            }
                        }
                    }
                    false
                }

                addTextChangedListener(object : TextWatcher {
                    override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit
                    override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit
                    override fun afterTextChanged(s: Editable?) {
                        if (resetting) return
                        val current = s?.toString().orEmpty()
                        if (current.isEmpty()) {
                            input.tap(KeyCode.BACKSPACE)
                        } else if (current != sentinel) {
                            val typed = if (current.startsWith(sentinel)) current.substring(1) else current
                            if (typed.isNotEmpty()) {
                                if (typed == "\n") {
                                    input.tap(KeyCode.ENTER)
                                } else {
                                    input.type(typed)
                                }
                            }
                        }
                        resetting = true
                        setText(sentinel)
                        setSelection(sentinel.length)
                        resetting = false
                    }
                })

                post {
                    requestFocus()
                    val imm = ctx.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager
                    imm?.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
                }
            }
        },
    )
}

@Composable
private fun KeyboardBar(
    input: InputController,
    haptics: Haptics,
    onDismiss: () -> Unit,
) {
    val armedModifiers by input.armedModifiers.collectAsState()

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(WaystoneTheme.Canvas),
    ) {
        HairlineDivider(color = WaystoneTheme.Border)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(44.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Row(
                modifier = Modifier
                    .weight(1f)
                    .horizontalScroll(rememberScrollState())
                    .padding(horizontal = 10.dp, vertical = 6.dp),
                horizontalArrangement = Arrangement.spacedBy(5.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                BarKey("Esc", haptics) { input.tap(KeyCode.ESCAPE) }
                BarKey("Tab", haptics) { input.tap(KeyCode.TAB) }

                BarDivider()

                ModifierKey("Ctrl", Modifiers.CONTROL, armedModifiers, haptics) { input.toggleArmed(Modifiers.CONTROL) }
                ModifierKey("Alt", Modifiers.ALT, armedModifiers, haptics) { input.toggleArmed(Modifiers.ALT) }
                ModifierKey("Shift", Modifiers.SHIFT, armedModifiers, haptics) { input.toggleArmed(Modifiers.SHIFT) }
                ModifierKey("Win", Modifiers.META, armedModifiers, haptics) { input.toggleArmed(Modifiers.META) }

                BarDivider()

                SymbolKey(Icons.AutoMirrored.Outlined.ArrowBack, "左", haptics) { input.tap(KeyCode.LEFT) }
                SymbolKey(Icons.Outlined.ArrowUpward, "上", haptics) { input.tap(KeyCode.UP) }
                SymbolKey(Icons.Outlined.ArrowDownward, "下", haptics) { input.tap(KeyCode.DOWN) }
                SymbolKey(Icons.AutoMirrored.Outlined.ArrowForward, "右", haptics) { input.tap(KeyCode.RIGHT) }

                BarDivider()

                for (n in 1..12) {
                    BarKey("F$n", haptics) { KeyCode.function(n)?.let { input.tap(it) } }
                }
            }

            // Pinned dismiss button
            Row(
                modifier = Modifier
                    .fillMaxHeight()
                    .background(WaystoneTheme.Canvas),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    modifier = Modifier
                        .width(0.75.dp)
                        .height(20.dp)
                        .background(WaystoneTheme.BorderSubtle),
                )
                Box(
                    modifier = Modifier
                        .fillMaxHeight()
                        .clickable {
                            haptics.tap()
                            onDismiss()
                        }
                        .padding(horizontal = 14.dp),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Outlined.KeyboardHide,
                        contentDescription = "收起键盘",
                        tint = WaystoneTheme.TextPrimary,
                        modifier = Modifier.size(18.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun BarDivider() {
    Box(
        modifier = Modifier
            .padding(horizontal = 3.dp)
            .width(0.75.dp)
            .height(16.dp)
            .background(WaystoneTheme.BorderSubtle),
    )
}

@Composable
private fun BarKey(
    title: String,
    haptics: Haptics,
    onClick: () -> Unit,
) {
    val shape = RoundedCornerShape(4.dp)
    Box(
        modifier = Modifier
            .defaultMinSize(minWidth = 38.dp, minHeight = 30.dp)
            .clip(shape)
            .background(WaystoneTheme.SurfaceRaised, shape)
            .border(0.75.dp, WaystoneTheme.Border, shape)
            .clickable {
                haptics.tap()
                onClick()
            }
            .padding(horizontal = 6.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = title,
            fontSize = 11.5.sp,
            fontWeight = FontWeight.Medium,
            fontFamily = FontFamily.Monospace,
            color = WaystoneTheme.TextPrimary,
        )
    }
}

@Composable
private fun SymbolKey(
    icon: ImageVector,
    description: String,
    haptics: Haptics,
    onClick: () -> Unit,
) {
    val shape = RoundedCornerShape(4.dp)
    Box(
        modifier = Modifier
            .size(width = 34.dp, height = 30.dp)
            .clip(shape)
            .background(WaystoneTheme.SurfaceRaised, shape)
            .border(0.75.dp, WaystoneTheme.Border, shape)
            .clickable {
                haptics.tap()
                onClick()
            },
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            imageVector = icon,
            contentDescription = description,
            tint = WaystoneTheme.TextPrimary,
            modifier = Modifier.size(14.dp),
        )
    }
}

@Composable
private fun ModifierKey(
    title: String,
    flag: Int,
    armedMask: Int,
    haptics: Haptics,
    onClick: () -> Unit,
) {
    val armed = (armedMask and flag) != 0
    val shape = RoundedCornerShape(4.dp)
    Box(
        modifier = Modifier
            .defaultMinSize(minWidth = 42.dp, minHeight = 30.dp)
            .clip(shape)
            .background(if (armed) WaystoneTheme.Amber else WaystoneTheme.SurfaceRaised, shape)
            .then(
                if (armed) Modifier else Modifier.border(0.75.dp, WaystoneTheme.Border, shape)
            )
            .clickable {
                haptics.selection()
                onClick()
            }
            .padding(horizontal = 6.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = title,
            fontSize = 11.5.sp,
            fontWeight = if (armed) FontWeight.SemiBold else FontWeight.Medium,
            fontFamily = FontFamily.Monospace,
            color = if (armed) WaystoneTheme.AmberInk else WaystoneTheme.TextPrimary,
        )
    }
}
