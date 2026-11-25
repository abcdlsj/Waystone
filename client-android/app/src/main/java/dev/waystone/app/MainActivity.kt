package dev.waystone.app

import android.content.Intent
import android.content.pm.ActivityInfo
import android.graphics.Color
import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import dev.waystone.app.ui.home.HomeScreen
import dev.waystone.app.ui.shared.WaystoneTheme
import dev.waystone.app.ui.stream.StreamScreen

class MainActivity : ComponentActivity() {
    private val appModel by lazy { (application as WaystoneApp).model }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)

        handlePairingIntent(intent)

        setContent {
            val session by appModel.session.collectAsState()
            val preventSleep by appModel.preferences.preventSleep.collectAsState()
            val streaming = session != null

            LaunchedEffect(streaming, preventSleep) {
                applyWindowMode(streaming = streaming, keepScreenOn = streaming || preventSleep)
            }

            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(WaystoneTheme.Canvas),
            ) {
                val activeSession = session
                if (activeSession != null) {
                    StreamScreen(
                        session = activeSession,
                        preferences = appModel.preferences,
                        haptics = appModel.haptics,
                    )
                } else {
                    HomeScreen(model = appModel)
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handlePairingIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        appModel.onForeground()
    }

    override fun onStop() {
        super.onStop()
        appModel.onBackground()
    }

    private fun handlePairingIntent(intent: Intent?) {
        val data = intent?.dataString ?: return
        if (data.startsWith("waystone://pair")) {
            appModel.pair(data)
        }
    }

    private fun applyWindowMode(streaming: Boolean, keepScreenOn: Boolean) {
        requestedOrientation = if (streaming) {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        } else {
            ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
        }

        if (keepScreenOn) {
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }

        val controller = WindowCompat.getInsetsController(window, window.decorView)
        controller.isAppearanceLightStatusBars = false
        controller.isAppearanceLightNavigationBars = false
        if (streaming) {
            controller.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            controller.hide(WindowInsetsCompat.Type.systemBars())
        } else {
            controller.show(WindowInsetsCompat.Type.systemBars())
        }
    }
}
