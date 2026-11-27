package remote.dafeiyu

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import android.view.WindowManager
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

class RemoteService : Service() {
    private val handler = Handler(Looper.getMainLooper())
    private val nativeStateListener: (String, String) -> Unit = { state, detail ->
        currentState = state to detail
    }
    private var projection: MediaProjection? = null
    private var projectionToken: Any? = null
    private var projectionCallback: MediaProjection.Callback? = null
    private var projectionEncoder: ProjectionEncoder? = null
    @Volatile private var hostEpoch = 0L
    @Volatile private var sourceWidth = 0
    @Volatile private var sourceHeight = 0
    @Volatile private lateinit var confirmedSettings: EncoderSettings
    @Volatile private var encoderActive = false
    private val captureActivity = CaptureActivityGate()
    private val encoderLock = Any()

    @Volatile private var running = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        DfyNative.addListener(nativeStateListener)
        DfyNative.setRuntimeSettingsCallback(::applyRuntimeSettings)
        DfyNative.setCaptureActivityCallback(::onCaptureActivity)
        DfyNative.setInputCallback(::dispatchNativeInput)
        DfyNative.setReleaseInputCallback(::releaseNativeInput)
        DfyNative.setStateCallback(::dispatchNativeState)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopCapture()
            reportLocal("stopped", "已停止")
            stopSelf()
            return START_NOT_STICKY
        }
        startForeground(
            NOTIFICATION_ID,
            buildNotification("正在准备录屏"),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        )
        if (intent?.action != ACTION_START || running) return START_NOT_STICKY
        captureActivity.reset()
        encoderActive = false
        if (projectionEncoder?.isEncoderAlive() == true) {
            reportLocal("error", "旧编码器仍在停止，请稍后重新开始")
            updateNotification("旧编码器仍在停止")
            stopSelf()
            return START_NOT_STICKY
        }
        val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        val resultData = intent.getParcelableExtra<Intent>(EXTRA_RESULT_DATA)
        val password = intent.getStringExtra(EXTRA_PASSWORD)
        if (resultCode == 0 || resultData == null || password == null) {
            fail("录屏授权数据无效，请重新开始")
            return START_NOT_STICKY
        }
        try {
            val manager = getSystemService(MediaProjectionManager::class.java)
            val newProjection = requireNotNull(manager.getMediaProjection(resultCode, resultData)) {
                "系统未返回录屏授权"
            }
            projection = newProjection
            val startedProjection = Any()
            projectionToken = startedProjection
            val allowInput = intent.getBooleanExtra(EXTRA_ALLOW_INPUT, false) &&
                RemoteAccessibilityService.isEnabled(this)
            val (width, height) = sourceDimensions()
            sourceWidth = width
            sourceHeight = height
            val initialSettings = EncoderSettings.fitWithin(
                width,
                height,
                intent.getIntExtra(EXTRA_MAX_WIDTH, DEFAULT_MAX_WIDTH),
                intent.getIntExtra(EXTRA_MAX_HEIGHT, DEFAULT_MAX_HEIGHT),
                intent.getIntExtra(EXTRA_FPS, DEFAULT_FPS),
                intent.getIntExtra(EXTRA_BITRATE_KBPS, DEFAULT_BITRATE_KBPS),
            )
            confirmedSettings = initialSettings
            projectionEncoder = ProjectionEncoder(
                newProjection,
                handler,
                onFrame = { data, keyframe, ptsUs, epoch ->
                    HostSession.requireCurrent(epoch)
                    DfyNative.pushFrame(data, keyframe, ptsUs, epoch)
                },
                isCurrent = HostSession::isCurrent,
                keyframeRequested = DfyNative::isKeyframeRequested,
                onError = { epoch, encoderWidth, encoderHeight, message ->
                    handler.post {
                        if (HostSession.isCurrent(epoch)) {
                            running = false
                            fail("${encoderWidth}×${encoderHeight} 编码失败：$message")
                        }
                    }
                },
            )
            val endpoint = DfyNative.start(
                "0.0.0.0",
                intent.getIntExtra(EXTRA_PORT, DEFAULT_PORT),
                password.toByteArray(Charsets.UTF_8),
                DfyNative.loadOrCreateHostIdentity(applicationContext),
                allowInput,
                initialSettings.width,
                initialSettings.height,
                initialSettings.fps,
                initialSettings.bitrateKbps,
                sourceWidth,
                sourceHeight
            )
            val endpointLines = endpoint.lines()
            hostEpoch = DfyNative.hostEpoch()
            check(hostEpoch > 0L) { "Android 主机代际无效" }
            HostSession.begin(hostEpoch)
            val startedEpoch = hostEpoch
            val callback = object : MediaProjection.Callback() {
                override fun onStop() {
                    Log.w("DfyProjection", "onStop epoch=$startedEpoch")
                    handler.post {
                        val current = isCurrentProjectionToken(
                            projectionToken, startedProjection, hostEpoch, startedEpoch
                        )
                        Log.i("DfyProjection", "queued onStop epoch=$startedEpoch current=$current")
                        if (current) {
                            stopCapture()
                            reportLocal("stopped", "录屏授权已撤销")
                            stopSelf()
                        }
                    }
                }
            }
            projectionCallback = callback
            newProjection.registerCallback(callback, handler)
            requireNotNull(projectionEncoder).prepareCapture()
            currentFingerprint = endpointLines.first().orEmpty()
            currentLocalAddress = endpointLines.drop(1).firstOrNull().orEmpty()
            running = true
            reconcileCaptureActivity(hostEpoch)
            if (!encoderActive) reportLocal("idle", "等待查看端连接")
            updateNotification("正在等待安全连接")
        } catch (error: Exception) {
            Log.e("DfyProjection", "start failed", error)
            running = false
            fail(error.message ?: "无法启动录屏编码器")
        }
        return START_NOT_STICKY
    }

    private fun sourceDimensions(): Pair<Int, Int> {
        val windows = getSystemService(WindowManager::class.java)
        return if (Build.VERSION.SDK_INT >= 30) {
            val metrics = windows.maximumWindowMetrics
            metrics.bounds.width() to metrics.bounds.height()
        } else {
            val metrics = android.util.DisplayMetrics()
            windows.defaultDisplay.getRealMetrics(metrics)
            metrics.widthPixels to metrics.heightPixels
        }
    }

    private fun dispatchNativeState(epoch: Long, state: String, detail: String) {
        handler.post {
            if (instance === this && hostEpoch == epoch && HostSession.isCurrent(epoch)) {
                nativeStateListener(state, detail)
            }
        }
    }

    private fun dispatchNativeInput(epoch: Long, data: ByteArray) {
        handler.post {
            if (instance !== this || hostEpoch != epoch || !HostSession.isCurrent(epoch)) return@post
            val event = InputWire.decode(data) ?: return@post
            if (RemoteAccessibilityService.isActive) RemoteAccessibilityService.instance?.handle(event)
        }
    }

    private fun releaseNativeInput(epoch: Long) {
        handler.post {
            if (instance === this && hostEpoch == epoch && HostSession.isCurrent(epoch)) {
                RemoteAccessibilityService.instance?.release()
            }
        }
    }

    private fun onCaptureActivity(epoch: Long, active: Boolean) {
        if (!captureActivity.accept(epoch, active, hostEpoch)) return
        handler.post {
            try {
                reconcileCaptureActivity(epoch)
            } catch (error: Exception) {
                if (HostSession.isCurrent(epoch)) fail("编码器启停失败：${error.message}")
            }
        }
    }

    private fun reconcileCaptureActivity(epoch: Long) {
        synchronized(encoderLock) {
            if (!running || hostEpoch != epoch || !HostSession.isCurrent(epoch)) return
            if (captureActivity.shouldRun(epoch)) {
                if (!encoderActive) {
                    val capture = startEncoder(confirmedSettings)
                    reportLocal("capturing", capture)
                    updateNotification("正在安全传输")
                }
            } else if (encoderActive) {
                suspendEncoder()
                Log.i("DfyProjection", "encoder_suspended epoch=$epoch")
                reportLocal("idle", "等待查看端连接")
                updateNotification("正在等待安全连接")
            }
        }
    }

    private fun startEncoder(settings: EncoderSettings): String {
        val epoch = hostEpoch
        requireNotNull(projectionEncoder).start(settings, epoch)
        encoderActive = true
        Log.i("DfyProjection", "encoder_started epoch=$epoch size=${settings.width}x${settings.height}")
        return "${settings.width}×${settings.height} H.264"
    }

    /** Called synchronously by a Rust worker; all MediaCodec work stays on main. */
    private fun applyRuntimeSettings(
        hostEpoch: Long,
        width: Int,
        height: Int,
        fps: Int,
        bitrateKbps: Int
    ): IntArray {
        HostSession.requireCurrent(hostEpoch)
        if (!running) throw IllegalStateException("录屏编码器已停止")
        val requested = if (width == -1 && height == -1) {
            EncoderSettings.fit(sourceWidth, sourceHeight, Int.MAX_VALUE - 1, fps, bitrateKbps)
        } else {
            EncoderSettings.fitWithin(sourceWidth, sourceHeight, width, height, fps, bitrateKbps)
        }
        if (requested == confirmedSettings) return intArrayOf(
            requested.width, requested.height, requested.fps, requested.bitrateKbps
        )
        val latch = CountDownLatch(1)
        val result = AtomicReference<IntArray?>()
        val failure = AtomicReference<Throwable?>()
        val fatal = AtomicReference(false)
        val posted = handler.post {
            try {
                synchronized(encoderLock) {
                    HostSession.requireCurrent(hostEpoch)
                    if (!running || projection == null) error("录屏编码器已停止")
                    if (!encoderActive) {
                        if (!captureActivity.shouldRun(hostEpoch)) {
                            error("查看端未连接，编码器设置尚未应用")
                        }
                        startEncoder(confirmedSettings)
                    }
                    val applied = rebuildEncoder(requested)
                    HostSession.requireCurrent(hostEpoch)
                    if (!running) error("编码器重建期间服务已停止")
                    result.set(intArrayOf(
                        applied.width, applied.height, applied.fps, applied.bitrateKbps
                    ))
                }
            } catch (error: Throwable) {
                failure.set(error)
                if (HostSession.isCurrent(hostEpoch) && !running) fatal.set(true)
            } finally {
                latch.countDown()
            }
        }
        if (!posted) throw IllegalStateException("主线程无法处理编码器设置")
        if (!latch.await(8, TimeUnit.SECONDS)) {
            // A late main-thread rebuild must not leave an unconfirmed encoder running.
            if (HostSession.isCurrent(hostEpoch)) {
                running = false
                handler.post {
                    if (HostSession.isCurrent(hostEpoch)) fail("等待编码器重建超时；设置未确认")
                }
            }
            throw IllegalStateException("等待编码器重建超时；设置未确认")
        }
        failure.get()?.let { error ->
            if (fatal.get()) handler.post {
                if (HostSession.isCurrent(hostEpoch)) fail(error.message ?: "编码器重建失败")
            }
            throw error
        }
        return result.get() ?: throw IllegalStateException("编码器重建未返回结果")
    }

    private fun rebuildEncoder(settings: EncoderSettings): EncoderSettings {
        val epoch = hostEpoch
        try {
            requireNotNull(projectionEncoder).rebuild(settings, epoch)
        } catch (error: Exception) {
            encoderActive = false
            running = false
            throw error
        }
        HostSession.requireCurrent(epoch)
        check(running) { "编码器重建期间服务已停止" }
        confirmedSettings = settings
        return settings
    }

    private fun suspendEncoder() {
        requireNotNull(projectionEncoder).suspend()
        encoderActive = false
    }

    private fun fail(detail: String) {
        stopCapture()
        if (instance !== this) {
            stopSelf()
            return
        }
        reportLocal("error", detail)
        updateNotification(detail)
        stopSelf()
    }

    private fun stopCapture() {
        val stoppedEpoch = hostEpoch
        running = false
        encoderActive = false
        captureActivity.reset()
        hostEpoch = 0
        val ownsHost = instance === this
        if (ownsHost) {
            HostSession.end(stoppedEpoch)
            currentFingerprint = ""
            currentLocalAddress = ""
            DfyNative.stop()
        }
        synchronized(encoderLock) {
            projectionEncoder?.release()
            projectionEncoder = null
        }
        projectionCallback?.let { callback ->
            runCatching { projection?.unregisterCallback(callback) }
        }
        runCatching { projection?.stop() }
        projectionCallback = null
        projectionToken = null
        projection = null
    }

    private fun reportLocal(state: String, detail: String) {
        if (instance !== this) return
        Log.i("DfyProjection", "state=$state epoch=$hostEpoch detail=$detail")
        currentState = state to detail
        DfyNative.reportLocal(state, detail)
    }

    private fun buildNotification(text: String): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= 26) {
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "远程被控服务", NotificationManager.IMPORTANCE_LOW)
            )
        }
        val stop = PendingIntent.getService(
            this,
            0,
            Intent(this, RemoteService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val content = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_menu_compass)
            .setContentTitle("大肥鱼远程被控端")
            .setContentText(text)
            .setContentIntent(content)
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, "停止", stop)
            .setOngoing(true)
            .build()
    }

    private fun updateNotification(text: String) {
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, buildNotification(text))
    }

    override fun onDestroy() {
        val ownsHost = instance === this
        stopCapture()
        DfyNative.removeListener(nativeStateListener)
        if (ownsHost) {
            DfyNative.setRuntimeSettingsCallback(null)
            DfyNative.setCaptureActivityCallback(null)
            DfyNative.setInputCallback(null)
            DfyNative.setReleaseInputCallback(null)
            DfyNative.setStateCallback(null)
            instance = null
        }
        super.onDestroy()
    }

    companion object {
        const val ACTION_START = "remote.dafeiyu.START"
        const val ACTION_STOP = "remote.dafeiyu.STOP"
        const val EXTRA_RESULT_CODE = "resultCode"
        const val EXTRA_RESULT_DATA = "resultData"
        const val EXTRA_PASSWORD = "password"
        const val EXTRA_PORT = "port"
        const val EXTRA_ALLOW_INPUT = "allowInput"
        const val EXTRA_MAX_WIDTH = "maxWidth"
        const val EXTRA_MAX_HEIGHT = "maxHeight"
        const val EXTRA_FPS = "fps"
        const val EXTRA_BITRATE_KBPS = "bitrateKbps"
        const val DEFAULT_PORT = 24443
        const val DEFAULT_MAX_WIDTH = EncoderSettings.DEFAULT_MAX_DIMENSION
        const val DEFAULT_MAX_HEIGHT = EncoderSettings.DEFAULT_MAX_DIMENSION
        const val DEFAULT_FPS = EncoderSettings.DEFAULT_FPS
        const val DEFAULT_BITRATE_KBPS = EncoderSettings.DEFAULT_BITRATE_KBPS
        private const val CHANNEL_ID = "remote_control"
        private const val NOTIFICATION_ID = 71

        @Volatile var instance: RemoteService? = null
            private set
        @Volatile var currentState: Pair<String, String> = "stopped" to "未启动"
            private set
        @Volatile var currentFingerprint: String = ""
            private set
        @Volatile var currentLocalAddress: String = ""
            private set

        fun isRunning() = instance != null && currentState.first != "stopped" && currentState.first != "error"
    }
}

internal fun annexB(data: ByteArray): ByteArray {
    if (data.isEmpty()) return data
    val startsWithStartCode = data.size >= 3 && data[0] == 0.toByte() && data[1] == 0.toByte() &&
        ((data.size >= 3 && data[2] == 1.toByte()) ||
            (data.size >= 4 && data[2] == 0.toByte() && data[3] == 1.toByte()))
    return if (startsWithStartCode) data else byteArrayOf(0, 0, 0, 1) + data
}

internal fun withSequenceHeader(config: ByteArray?, data: ByteArray, keyframe: Boolean): ByteArray =
    if (keyframe && config != null) config + data else data

internal fun isCurrentProjectionToken(
    current: Any?,
    expected: Any?,
    currentHostEpoch: Long,
    expectedHostEpoch: Long
): Boolean = current !== null && current === expected && currentHostEpoch == expectedHostEpoch &&
    HostSession.isCurrent(expectedHostEpoch)

internal object HostSession {
    @Volatile var epoch: Long = 0L
        private set

    fun begin(epoch: Long) {
        require(epoch > 0L) { "Android 主机代际无效" }
        this.epoch = epoch
    }

    fun end(expected: Long) {
        if (expected > 0L && epoch == expected) epoch = 0L
    }

    fun isCurrent(expected: Long): Boolean = expected > 0L && epoch == expected

    fun requireCurrent(expected: Long) {
        check(isCurrent(expected)) { "Android 主机代际已失效" }
    }
}
