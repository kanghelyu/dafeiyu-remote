package remote.dafeiyu

import android.content.Context

import java.util.concurrent.CopyOnWriteArrayList

object DfyNative {
    init { System.loadLibrary("dfy_android") }

    private val listeners = CopyOnWriteArrayList<(String, String) -> Unit>()
    @Volatile private var runtimeSettingsCallback: ((Long, Int, Int, Int, Int) -> IntArray)? = null
    @Volatile private var captureActivityCallback: ((Long, Boolean) -> Unit)? = null
    @Volatile private var inputCallback: ((Long, ByteArray) -> Unit)? = null
    @Volatile private var releaseInputCallback: ((Long) -> Unit)? = null
    @Volatile private var stateCallback: ((Long, String, String) -> Unit)? = null

    @Suppress("unused")
    private external fun setCallback(callback: Any)
    external fun start(
        host: String,
        port: Int,
        password: ByteArray,
        identityKey: ByteArray,
        allowInput: Boolean,
        width: Int,
        height: Int,
        fps: Int,
        bitrateKbps: Int,
        sourceWidth: Int,
        sourceHeight: Int
    ): String
    external fun stop()
    external fun hostEpoch(): Long
    external fun pushFrame(data: ByteArray, keyframe: Boolean, ptsUs: Long, hostEpoch: Long)
    external fun isKeyframeRequested(hostEpoch: Long): Boolean
    external fun stats(): String

    init { setCallback(this) }

    fun addListener(listener: (String, String) -> Unit) {
        listeners.add(listener)
    }

    fun removeListener(listener: (String, String) -> Unit) {
        listeners.remove(listener)
    }

    fun setRuntimeSettingsCallback(callback: ((Long, Int, Int, Int, Int) -> IntArray)?) {
        runtimeSettingsCallback = callback
    }

    fun setCaptureActivityCallback(callback: ((Long, Boolean) -> Unit)?) {
        captureActivityCallback = callback
    }

    fun setInputCallback(callback: ((Long, ByteArray) -> Unit)?) {
        inputCallback = callback
    }

    fun setReleaseInputCallback(callback: ((Long) -> Unit)?) {
        releaseInputCallback = callback
    }

    fun setStateCallback(callback: ((Long, String, String) -> Unit)?) {
        stateCallback = callback
    }

    @Suppress("unused")
    fun loadOrCreateHostIdentity(context: Context): ByteArray = HostIdentityStore.loadOrCreate(context)

    @Suppress("unused")
    fun onCaptureActivity(hostEpoch: Long, active: Boolean) {
        captureActivityCallback?.invoke(hostEpoch, active)
    }

    @Suppress("unused")
    fun applyRuntimeSettings(
        hostEpoch: Long,
        width: Int,
        height: Int,
        fps: Int,
        bitrateKbps: Int
    ): IntArray {
        val callback = runtimeSettingsCallback
            ?: throw IllegalStateException("Android 运行时编码器控制不可用")
        return callback(hostEpoch, width, height, fps, bitrateKbps)
    }

    @Suppress("unused")
    fun onState(hostEpoch: Long, state: String, detail: String) {
        stateCallback?.invoke(hostEpoch, state, detail)
    }

    @Suppress("unused")
    fun onInput(hostEpoch: Long, data: ByteArray) {
        inputCallback?.invoke(hostEpoch, data)
    }

    @Suppress("unused")
    fun onReleaseInput(hostEpoch: Long) {
        releaseInputCallback?.invoke(hostEpoch)
    }

    fun reportLocal(state: String, detail: String) {
        listeners.forEach { it(state, detail) }
    }

    @Suppress("unused")
    fun isInputAllowed(): Boolean = RemoteAccessibilityService.isActive
}
