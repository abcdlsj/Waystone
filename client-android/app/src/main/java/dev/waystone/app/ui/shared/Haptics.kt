package dev.waystone.app.ui.shared

import android.content.Context
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import dev.waystone.app.input.PointerOutput

/** Tactile feedback for pointer gestures and console UI controls. */
class Haptics(context: Context) {
    private val vibrator: Vibrator? = runCatching {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            (context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as? VibratorManager)?.defaultVibrator
        } else {
            @Suppress("DEPRECATION")
            context.getSystemService(Context.VIBRATOR_SERVICE) as? Vibrator
        }
    }.getOrNull()

    fun play(kind: PointerOutput.Haptic) {
        when (kind) {
            PointerOutput.Haptic.GRAB -> vibrate(VibrationEffect.EFFECT_CLICK)
            PointerOutput.Haptic.SECONDARY -> vibrate(VibrationEffect.EFFECT_HEAVY_CLICK)
        }
    }

    fun selection() = vibrate(VibrationEffect.EFFECT_TICK)

    fun tap() = vibrate(VibrationEffect.EFFECT_TICK)

    fun success() = vibrate(VibrationEffect.EFFECT_CLICK)

    private fun vibrate(effectId: Int) {
        val v = vibrator ?: return
        if (!v.hasVibrator()) return
        runCatching {
            v.vibrate(VibrationEffect.createPredefined(effectId))
        }
    }
}
