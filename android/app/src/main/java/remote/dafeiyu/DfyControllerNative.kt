package remote.dafeiyu

import android.os.Handler
import android.os.Looper

/** JNI bridge for the viewer role. It shares the library, but none of the host state, with DfyNative. */
object DfyControllerNative {
    @Volatile private var listener: Listener? = null
    private val main = Handler(Looper.getMainLooper())
    private val feedbackLock = Any()
    private val feedbackWindow = StreamFeedbackWindow()
    private var feedbackEnabled = false
    private var feedbackScheduled = false

    interface Listener {
        fun onState(state: String, detail: String)
        fun onFrame(data: ByteArray, ptsUs: Long, keyframe: Boolean): Boolean
        fun onVideoConfig(width: Int, height: Int, fps: Int, bitrateKbps: Int)
        fun onSession(permissions: Int, capabilities: Set<String>)
        fun onControlAllowed(allowed: Boolean)
    }

    init {
        System.loadLibrary("dfy_android")
        setCallback(this)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    fun connect(host: String, port: Int, password: String, fingerprint: String) {
        resetFeedback()
        val pin = ViewerHelpers.normalizeFingerprint(fingerprint)
            ?: throw IllegalArgumentException("Fingerprint must contain exactly 64 hexadecimal characters")
        val bytes = password.toByteArray(Charsets.UTF_8)
        try {
            connect(host.trim(), port, bytes, pin)
        } finally {
            bytes.fill(0)
        }
    }

    /** Called on a Rust worker thread. Never log frame contents or connection secrets. */
    @Suppress("UNUSED")
    fun onState(state: String, detail: String) {
        if (state == "disconnected" || state == "error") resetFeedback()
        val value = listener ?: return
        main.post { value.onState(state, detail) }
    }

    @Suppress("UNUSED")
    fun onFrame(data: ByteArray, ptsUs: Long, keyframe: Boolean): Boolean =
        listener?.onFrame(data, ptsUs, keyframe) == true

    @Suppress("UNUSED")
    fun onVideoConfig(width: Int, height: Int, fps: Int, bitrateKbps: Int) {
        val value = listener ?: return
        main.post { value.onVideoConfig(width, height, fps, bitrateKbps) }
    }

    @Suppress("UNUSED")
    fun onSession(permissions: Int, capabilities: String) {
        val value = listener
        val parsed = capabilities.split('\n').filter { it.isNotBlank() }.toSet()
        synchronized(feedbackLock) {
            feedbackEnabled = CAPABILITY_STREAM_FEEDBACK in parsed
            feedbackWindow.reset()
            feedbackScheduled = false
            main.removeCallbacks(feedbackFlush)
        }
        if (value != null) main.post { value.onSession(permissions, parsed) }
    }

    /** Called for each decoded video DATAGRAM on a Rust worker thread. */
    @Suppress("UNUSED")
    fun onVideoPacket(
        datagramBytes: Long,
        framesAssembled: Int,
        framesDelivered: Int,
        droppedFrames: Long,
    ) {
        synchronized(feedbackLock) {
            if (!feedbackEnabled) return
            feedbackWindow.notePacket(
                System.nanoTime() / 1_000_000,
                datagramBytes,
                framesAssembled,
                framesDelivered,
                droppedFrames,
            )
            if (!feedbackScheduled) {
                feedbackScheduled = true
                main.postDelayed(feedbackFlush, feedbackWindow.interval)
            }
        }
    }

    private val feedbackFlush = object : Runnable {
        override fun run() {
            synchronized(feedbackLock) {
                if (!feedbackEnabled) return
                feedbackScheduled = false
                val feedback = feedbackWindow.take(System.nanoTime() / 1_000_000) ?: return
                sendStreamFeedback(
                    feedback.windowMs,
                    feedback.payloadBytes,
                    feedback.packets,
                    feedback.framesAssembled,
                    feedback.framesDelivered,
                    feedback.droppedFrames,
                    feedback.stallMs,
                )
                feedbackScheduled = true
                main.postDelayed(this, feedbackWindow.interval)
            }
        }
    }

    private fun resetFeedback() {
        synchronized(feedbackLock) {
            feedbackEnabled = false
            feedbackWindow.reset()
            feedbackScheduled = false
            main.removeCallbacks(feedbackFlush)
        }
    }

    @Suppress("UNUSED")
    fun onControlAllowed(allowed: Boolean) {
        val value = listener ?: return
        main.post { value.onControlAllowed(allowed) }
    }

    private external fun setCallback(callback: Any)
    private external fun connect(host: String, port: Int, password: ByteArray, fingerprint: String): String
    fun disconnect() {
        resetFeedback()
        disconnectNative()
    }
    private external fun disconnectNative()
    external fun setControlAllowed(allowed: Boolean)
    external fun pointer(x: Int, y: Int)
    external fun button(button: Int, down: Boolean, x: Int, y: Int)
    external fun wheel(dx: Int, dy: Int)
    external fun key(usage: Int, down: Boolean, modifiers: Int)
    external fun systemAction(action: Int)
    external fun releaseInput()
    external fun requestKeyframe()
    fun setQualityControl(width: Int, height: Int, fps: Int, bitrateKbps: Int): Boolean =
        setQualityControl(width.toLong(), height.toLong(), fps.toLong(), bitrateKbps.toLong())
    private external fun setQualityControl(width: Long, height: Long, fps: Long, bitrateKbps: Long): Boolean
    private external fun sendStreamFeedback(
        windowMs: Long,
        payloadBytes: Long,
        packets: Long,
        framesAssembled: Long,
        framesDelivered: Long,
        droppedFrames: Long,
        stallMs: Long,
    ): Boolean
    external fun stats(): String

    const val PERM_VIEW = 1 shl 0
    const val PERM_CONTROL = 1 shl 1
    const val PERM_SYSTEM_ACTION = 1 shl 7
    const val CAPABILITY_SYSTEM_ACTION = "system-action.back-home.v1"
    const val CAPABILITY_STREAM_FEEDBACK = "stream-feedback.v1"
    const val CAPABILITY_QUALITY_CONTROL = "quality-control.v1"
    const val ACTION_BACK = 0
    const val ACTION_HOME = 1

    fun systemActionAuthorized(permissions: Int, capabilities: Set<String>, localControl: Boolean): Boolean =
        ViewerHelpers.systemActionAuthorized(permissions, capabilities, localControl, CAPABILITY_SYSTEM_ACTION)
}
