package dev.waystone.app.input

import dev.waystone.app.protocol.InputEvent
import dev.waystone.app.protocol.KeyCode

/** What an on-screen control sends. */
sealed class KeyAction {
    data class Tap(val key: KeyCode) : KeyAction()
    data class Toggle(val key: KeyCode) : KeyAction()
    data class Hold(val key: KeyCode) : KeyAction()
    data class Click(val button: InputEvent.MouseButton) : KeyAction()
}

data class HotkeyButton(
    val title: String,
    val cap: String? = null,
    val action: KeyAction,
) {
    val id: String get() = title
}

data class HotkeyGroup(
    val anchor: Anchor,
    val buttons: List<HotkeyButton>,
) {
    enum class Anchor { TRAILING, LEADING, TOP, BOTTOM_TRAILING }

    val id: String get() = buttons.joinToString("|") { it.id }
}

data class EdgeZone(
    val side: Side,
    val widthDp: Float,
    val action: KeyAction,
) {
    enum class Side { LEADING, TRAILING }

    val id: String get() = if (side == Side.LEADING) "leading" else "trailing"
}

data class JoystickLayout(
    val up: KeyCode,
    val left: KeyCode,
    val down: KeyCode,
    val right: KeyCode,
)

/**
 * Built-in hotkey presets from design doc section 5.3, matching iOS 1:1.
 */
data class HotkeyPreset(
    val id: String,
    val name: String,
    val groups: List<HotkeyGroup> = emptyList(),
    val edgeZones: List<EdgeZone> = emptyList(),
    val joystick: JoystickLayout? = null,
) {
    companion object {
        val CARDS = HotkeyPreset(
            id = "cards",
            name = "卡牌",
            groups = listOf(
                HotkeyGroup(
                    anchor = HotkeyGroup.Anchor.TRAILING,
                    buttons = listOf(
                        HotkeyButton("结束回合", cap = "Space", action = KeyAction.Tap(KeyCode.SPACE)),
                        HotkeyButton("取消", cap = "右键", action = KeyAction.Click(InputEvent.MouseButton.RIGHT)),
                        HotkeyButton("菜单", cap = "Esc", action = KeyAction.Tap(KeyCode.ESCAPE)),
                    ),
                )
            ),
        )

        val NOVEL = HotkeyPreset(
            id = "novel",
            name = "视觉小说",
            groups = listOf(
                HotkeyGroup(
                    anchor = HotkeyGroup.Anchor.TOP,
                    buttons = listOf(
                        HotkeyButton("快进", cap = "Ctrl", action = KeyAction.Toggle(KeyCode.LEFT_CONTROL)),
                    ),
                )
            ),
            edgeZones = listOf(
                EdgeZone(EdgeZone.Side.LEADING, widthDp = 48f, action = KeyAction.Tap(KeyCode.ENTER)),
                EdgeZone(EdgeZone.Side.TRAILING, widthDp = 48f, action = KeyAction.Tap(KeyCode.ENTER)),
            ),
        )

        val ROUTINE = HotkeyPreset(
            id = "routine",
            name = "日常",
            groups = listOf(
                HotkeyGroup(
                    anchor = HotkeyGroup.Anchor.BOTTOM_TRAILING,
                    buttons = listOf(
                        HotkeyButton("1", action = KeyAction.Tap(KeyCode.digit(1)!!)),
                        HotkeyButton("2", action = KeyAction.Tap(KeyCode.digit(2)!!)),
                        HotkeyButton("3", action = KeyAction.Tap(KeyCode.digit(3)!!)),
                        HotkeyButton("4", action = KeyAction.Tap(KeyCode.digit(4)!!)),
                        HotkeyButton("地图", cap = "M", action = KeyAction.Tap(KeyCode.letter('m')!!)),
                        HotkeyButton("跳跃", cap = "Space", action = KeyAction.Tap(KeyCode.SPACE)),
                        HotkeyButton("交互", cap = "F", action = KeyAction.Tap(KeyCode.letter('f')!!)),
                    ),
                )
            ),
            joystick = JoystickLayout(
                up = KeyCode.letter('w')!!,
                left = KeyCode.letter('a')!!,
                down = KeyCode.letter('s')!!,
                right = KeyCode.letter('d')!!,
            ),
        )

        val NONE = HotkeyPreset(id = "none", name = "无")

        val ALL: List<HotkeyPreset> = listOf(CARDS, NOVEL, ROUTINE, NONE)

        fun named(id: String): HotkeyPreset = ALL.firstOrNull { it.id == id } ?: CARDS
    }
}
