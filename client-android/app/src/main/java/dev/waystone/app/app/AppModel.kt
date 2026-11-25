package dev.waystone.app.app

import android.content.Context
import android.util.Log
import dev.waystone.app.input.PointerOutput
import dev.waystone.app.pool.DeviceIdentity
import dev.waystone.app.pool.FileIdentityStorage
import dev.waystone.app.pool.Host
import dev.waystone.app.pool.Instance
import dev.waystone.app.pool.InstanceStore
import dev.waystone.app.pool.PoolConnection
import dev.waystone.app.protocol.PairingLink
import dev.waystone.app.stream.StreamPreferences
import dev.waystone.app.stream.StreamSession
import dev.waystone.app.ui.shared.Haptics
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import java.io.File
import java.util.UUID

/**
 * Root app state: enrolled servers, live pool connections, pairing, and the
 * single stream session that may be running.
 */
class AppModel(private val appContext: Context) {
    sealed class Pairing {
        data object Idle : Pairing()
        data class InProgress(val endpoint: String) : Pairing()
        data class Failed(val reason: String) : Pairing()
    }

    data class Notice(
        val id: String = UUID.randomUUID().toString(),
        val title: String,
        val message: String? = null,
    )

    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    val preferences = StreamPreferences(appContext)
    val haptics = Haptics(appContext)

    private val store = InstanceStore(File(appContext.filesDir, "instances.json"))
    private val identity: DeviceIdentity? = try {
        DeviceIdentity.loadOrCreate("primary", FileIdentityStorage(appContext.filesDir))
    } catch (e: Exception) {
        Log.e(TAG, "cannot load device identity: ${e.message}")
        null
    }

    private val _pools = MutableStateFlow<List<PoolConnection>>(emptyList())
    val pools: StateFlow<List<PoolConnection>> = _pools.asStateFlow()

    private val _session = MutableStateFlow<StreamSession?>(null)
    val session: StateFlow<StreamSession?> = _session.asStateFlow()

    private val _pairing = MutableStateFlow<Pairing>(Pairing.Idle)
    val pairing: StateFlow<Pairing> = _pairing.asStateFlow()

    private val _notice = MutableStateFlow<Notice?>(null)
    val notice: StateFlow<Notice?> = _notice.asStateFlow()

    private var pendingPairing: PoolConnection? = null
    private var sessionJob: Job? = null

    init {
        if (identity != null) {
            _pools.value = store.instances.map { PoolConnection(it, identity, scope) }
        }
    }

    fun onForeground() {
        _pools.value.forEach {
            it.start()
            it.reconnectNow()
        }
    }

    fun onBackground() {
        if (_session.value != null) return
        _pools.value.forEach { it.stop() }
    }

    fun refresh() {
        _pools.value.forEach { it.reconnectNow() }
    }

    fun clearNotice() {
        _notice.value = null
    }

    fun pair(text: String) {
        val currentIdentity = identity
        if (currentIdentity == null) {
            _pairing.value = Pairing.Failed("无法读取本机身份，请重新安装 App")
            return
        }
        val link = PairingLink.parse(text)
        if (link == null) {
            _pairing.value = Pairing.Failed("这不是 Waystone 配对码")
            return
        }
        val instance = Instance.fromLink(link)
        val existing = _pools.value.firstOrNull { it.id == instance.id }
        if (existing != null) {
            existing.stop()
            _pools.value = _pools.value.filterNot { it.id == instance.id }
        }
        val pool = PoolConnection(
            instance = instance,
            identity = currentIdentity,
            scope = scope,
            pairingToken = link.token,
        )
        _pairing.value = Pairing.InProgress(link.endpoint)
        pool.onPaired = {
            store.upsert(pool.instance)
            _pools.value = _pools.value + pool
            _pairing.value = Pairing.Idle
            pendingPairing = null
            _notice.value = Notice(title = "已添加 ${pool.instance.label}")
        }
        pool.onPairingFailed = { reason ->
            _pairing.value = Pairing.Failed(reason)
            pendingPairing = null
        }
        pool.start()
        pendingPairing = pool
    }

    fun dismissPairingError() {
        if (_pairing.value is Pairing.Failed) {
            _pairing.value = Pairing.Idle
        }
    }

    fun forget(pool: PoolConnection) {
        pool.stop()
        _pools.value = _pools.value.filterNot { it.id == pool.id }
        store.remove(pool.id)
    }

    fun connect(host: Host, pool: PoolConnection) {
        if (_session.value != null || !host.online) return
        val newSession = StreamSession(
            context = appContext,
            host = host,
            pool = pool,
            preferences = preferences,
            scope = scope,
            haptics = { kind: PointerOutput.Haptic -> haptics.play(kind) },
        )
        _session.value = newSession
        newSession.start()
        sessionJob?.cancel()
        sessionJob = scope.launch {
            newSession.stage.collect { stage ->
                if (stage is StreamSession.Stage.Ended) {
                    sessionEnded(stage.ending)
                    sessionJob?.cancel()
                }
            }
        }
    }

    private fun sessionEnded(ending: StreamSession.Ending) {
        _session.value = null
        sessionJob = null
        if (ending != StreamSession.Ending.HUNG_UP) {
            _notice.value = Notice(title = ending.title, message = ending.message)
        }
    }

    companion object {
        private const val TAG = "WaystoneApp"
    }
}
