package dev.waystone.app.stream

import android.content.Context
import android.content.SharedPreferences
import dev.waystone.app.input.HotkeyPreset
import dev.waystone.app.input.InputController
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Stream and app preferences persisted across sessions in SharedPreferences.
 */
class StreamPreferences(context: Context) {
    enum class FrameRate(val fps: Int) {
        FPS_30(30),
        FPS_45(45),
        FPS_60(60);

        val title: String get() = "$fps Hz"

        companion object {
            val SMOOTH = FPS_60
            val COOL = FPS_45

            fun fromFps(value: Int): FrameRate = entries.firstOrNull { it.fps == value } ?: SMOOTH
        }
    }

    enum class Resolution(val key: String, val title: String, val maxSize: Pair<Int, Int>?) {
        AUTO("auto", "原生", null),
        P720("p720", "720p", 1280 to 720),
        P1080("p1080", "1080p", 1920 to 1080),
        P1440("p1440", "2K", 2560 to 1440);

        companion object {
            fun fromKey(key: String?): Resolution = entries.firstOrNull { it.key == key } ?: AUTO
        }
    }

    enum class Sound(val key: String, val title: String) {
        AUTO("auto", "跟随静音键"),
        ON("on", "开"),
        OFF("off", "关");

        companion object {
            fun fromKey(key: String?): Sound = entries.firstOrNull { it.key == key } ?: AUTO
        }
    }

    private val prefs: SharedPreferences =
        context.getSharedPreferences("waystone_prefs", Context.MODE_PRIVATE)

    private val _frameRate = MutableStateFlow(FrameRate.fromFps(prefs.getInt(KEY_FRAME_RATE, FrameRate.SMOOTH.fps)))
    val frameRate: StateFlow<FrameRate> = _frameRate.asStateFlow()

    private val _resolution = MutableStateFlow(Resolution.fromKey(prefs.getString(KEY_RESOLUTION, null)))
    val resolution: StateFlow<Resolution> = _resolution.asStateFlow()

    private val _sound = MutableStateFlow(Sound.fromKey(prefs.getString(KEY_SOUND, null)))
    val sound: StateFlow<Sound> = _sound.asStateFlow()

    private val _touchMode = MutableStateFlow(InputController.Mode.fromKey(prefs.getString(KEY_TOUCH_MODE, null)))
    val touchMode: StateFlow<InputController.Mode> = _touchMode.asStateFlow()

    private val _presetId = MutableStateFlow(prefs.getString(KEY_PRESET, null) ?: HotkeyPreset.CARDS.id)
    val presetId: StateFlow<String> = _presetId.asStateFlow()

    private val _preventSleep = MutableStateFlow(prefs.getBoolean(KEY_PREVENT_SLEEP, true))
    val preventSleep: StateFlow<Boolean> = _preventSleep.asStateFlow()

    private val _ballTrailing = MutableStateFlow(prefs.getBoolean(KEY_BALL_TRAILING, true))
    val ballTrailing: StateFlow<Boolean> = _ballTrailing.asStateFlow()

    private val _ballHeight = MutableStateFlow(prefs.getFloat(KEY_BALL_HEIGHT, 0.5f))
    val ballHeight: StateFlow<Float> = _ballHeight.asStateFlow()

    val currentPreset: HotkeyPreset get() = HotkeyPreset.named(_presetId.value)

    fun setFrameRate(rate: FrameRate) {
        _frameRate.value = rate
        prefs.edit().putInt(KEY_FRAME_RATE, rate.fps).apply()
    }

    fun setResolution(res: Resolution) {
        _resolution.value = res
        prefs.edit().putString(KEY_RESOLUTION, res.key).apply()
    }

    fun setSound(sound: Sound) {
        _sound.value = sound
        prefs.edit().putString(KEY_SOUND, sound.key).apply()
    }

    fun setTouchMode(mode: InputController.Mode) {
        _touchMode.value = mode
        prefs.edit().putString(KEY_TOUCH_MODE, mode.key).apply()
    }

    fun setPresetId(id: String) {
        _presetId.value = id
        prefs.edit().putString(KEY_PRESET, id).apply()
    }

    fun setPreventSleep(prevent: Boolean) {
        _preventSleep.value = prevent
        prefs.edit().putBoolean(KEY_PREVENT_SLEEP, prevent).apply()
    }

    fun setBallPosition(trailing: Boolean, heightFraction: Float) {
        val clamped = heightFraction.coerceIn(0.08f, 0.92f)
        _ballTrailing.value = trailing
        _ballHeight.value = clamped
        prefs.edit()
            .putBoolean(KEY_BALL_TRAILING, trailing)
            .putFloat(KEY_BALL_HEIGHT, clamped)
            .apply()
    }

    companion object {
        private const val KEY_FRAME_RATE = "stream.frameRate"
        private const val KEY_RESOLUTION = "stream.resolution"
        private const val KEY_SOUND = "stream.sound"
        private const val KEY_TOUCH_MODE = "stream.touchMode"
        private const val KEY_PRESET = "stream.preset"
        private const val KEY_PREVENT_SLEEP = "app.preventSleep"
        private const val KEY_BALL_TRAILING = "ball.trailing"
        private const val KEY_BALL_HEIGHT = "ball.height"
    }
}
