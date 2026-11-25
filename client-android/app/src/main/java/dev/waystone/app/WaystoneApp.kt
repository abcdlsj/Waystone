package dev.waystone.app

import android.app.Application
import dev.waystone.app.app.AppModel

class WaystoneApp : Application() {
    lateinit var model: AppModel
        private set

    override fun onCreate() {
        super.onCreate()
        model = AppModel(applicationContext)
    }
}
