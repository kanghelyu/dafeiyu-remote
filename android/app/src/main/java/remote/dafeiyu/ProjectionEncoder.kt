package remote.dafeiyu

import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Bundle
import android.os.Handler
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.io.ByteArrayOutputStream

internal class ProjectionEncoder(
    private val projection: MediaProjection,
    private val handler: Handler,
    private val onFrame: (ByteArray, Boolean, Long, Long) -> Unit,
    private val isCurrent: (Long) -> Boolean,
    private val keyframeRequested: (Long) -> Boolean,
    private val onError: (Long, Int, Int, String) -> Unit,
) {
    private var placeholderReader: ImageReader? = null
    private var placeholderSurface: Surface? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var codec: MediaCodec? = null
    private var codecSurface: Surface? = null
    private var thread: Thread? = null
    @Volatile private var active = false
    @Volatile private var generation = 0L
    private var released = false

    fun isEncoderAlive(): Boolean = thread?.isAlive == true

    fun prepareCapture() {
        check(!released) { "录屏编码器已释放" }
        if (virtualDisplay != null) return
        val reader = ImageReader.newInstance(1, 1, PixelFormat.RGBA_8888, 2)
        val placeholder = reader.surface
        val createdDisplay = try {
            requireNotNull(projection.createVirtualDisplay(
                "DafeiyuRemote", 1, 1, 160,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, placeholder, null, handler
            ))
        } catch (error: Exception) {
            runCatching { placeholder.release() }
            runCatching { reader.close() }
            throw error
        }
        placeholderReader = reader
        placeholderSurface = placeholder
        virtualDisplay = createdDisplay
        Log.i("DfyProjection", "virtual_display_created size=1x1")
    }

    fun start(settings: EncoderSettings, hostEpoch: Long) {
        check(!released) { "录屏编码器已释放" }
        if (active) return
        prepareCapture()
        val projectionDisplay = requireNotNull(virtualDisplay)
        val format = MediaFormat.createVideoFormat(
            MediaFormat.MIMETYPE_VIDEO_AVC, settings.width, settings.height
        ).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, settings.bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, settings.fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
        }
        val newCodec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        var newSurface: Surface? = null
        try {
            newCodec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            newSurface = requireNotNull(newCodec.createInputSurface())
            newCodec.start()
            newCodec.setParameters(Bundle().apply {
                putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
            })
            projectionDisplay.resize(settings.width, settings.height, 160)
            projectionDisplay.setSurface(newSurface)
        } catch (error: Exception) {
            runCatching { newCodec.stop() }
            runCatching { newCodec.release() }
            runCatching { newSurface?.release() }
            restorePlaceholder()
            throw error
        }
        codec = newCodec
        codecSurface = newSurface
        val startedGeneration = ++generation
        active = true
        thread = Thread(
            { encode(newCodec, settings, startedGeneration, hostEpoch) }, "DfyEncoder"
        ).also { it.start() }
        Log.i("DfyProjection", "virtual_display_attached size=${settings.width}x${settings.height}")
        Log.i("DfyProjection", "encoder_started generation=$startedGeneration size=${settings.width}x${settings.height}")
    }

    fun rebuild(settings: EncoderSettings, hostEpoch: Long) {
        suspend()
        start(settings, hostEpoch)
    }

    fun suspend() {
        if (released) return
        active = false
        generation += 1
        val oldThread = thread
        if (oldThread != null && oldThread !== Thread.currentThread()) {
            oldThread.join(2_000)
            check(!oldThread.isAlive) { "旧编码器线程未能安全停止" }
        }
        thread = null
        releaseCodec()
        restorePlaceholder()
    }

    fun release() {
        if (released) return
        active = false
        generation += 1
        val oldThread = thread
        if (oldThread != null && oldThread !== Thread.currentThread()) {
            oldThread.join(2_000)
            if (oldThread.isAlive) {
                handler.postDelayed({ release() }, 100)
                return
            }
        }
        thread = null
        releaseCodec()
        virtualDisplay?.setSurface(null)
        Log.i("DfyProjection", "virtual_display_detached")
        runCatching { virtualDisplay?.release() }
        virtualDisplay = null
        runCatching { placeholderSurface?.release() }
        placeholderSurface = null
        runCatching { placeholderReader?.close() }
        placeholderReader = null
        released = true
    }

    private fun restorePlaceholder() {
        val projectionDisplay = virtualDisplay ?: return
        val placeholder = placeholderSurface ?: return
        projectionDisplay.resize(1, 1, 160)
        projectionDisplay.setSurface(placeholder)
    }

    private fun releaseCodec() {
        codec?.let { encoder ->
            runCatching { encoder.stop() }
            runCatching { encoder.release() }
        }
        codec = null
        runCatching { codecSurface?.release() }
        codecSurface = null
        Log.i("DfyProjection", "codec_released")
    }

    private fun encode(
        encoder: MediaCodec,
        settings: EncoderSettings,
        startedGeneration: Long,
        hostEpoch: Long,
    ) {
        val info = MediaCodec.BufferInfo()
        var codecConfig: ByteArray? = null
        var lastKeyframeRequest = 0L
        try {
            while (active && startedGeneration == generation && isCurrent(hostEpoch)) {
                val elapsed = SystemClock.elapsedRealtime()
                if (elapsed - lastKeyframeRequest >= 100 && keyframeRequested(hostEpoch)) {
                    encoder.setParameters(Bundle().apply {
                        putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
                    })
                    lastKeyframeRequest = elapsed
                }
                drainOutput(encoder, info, startedGeneration, hostEpoch, codecConfig) { codecConfig = it }
            }
        } catch (error: Exception) {
            if (active && startedGeneration == generation && isCurrent(hostEpoch)) {
                handler.post {
                    if (active && startedGeneration == generation && isCurrent(hostEpoch)) {
                        onError(hostEpoch, settings.width, settings.height, error.message ?: error.javaClass.simpleName)
                    }
                }
            }
        }
    }

    private fun drainOutput(
        encoder: MediaCodec,
        info: MediaCodec.BufferInfo,
        startedGeneration: Long,
        epoch: Long,
        codecConfig: ByteArray?,
        updateConfig: (ByteArray) -> Unit,
    ) {
        var currentConfig = codecConfig
        while (active && startedGeneration == generation && isCurrent(epoch)) {
            val index = encoder.dequeueOutputBuffer(info, 10_000)
            if (index == MediaCodec.INFO_TRY_AGAIN_LATER || index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED ||
                index == MediaCodec.INFO_OUTPUT_BUFFERS_CHANGED
            ) {
                if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                    val header = ByteArrayOutputStream()
                    for (key in listOf("csd-0", "csd-1")) {
                        encoder.outputFormat.getByteBuffer(key)?.let { buffer ->
                            val bytes = ByteArray(buffer.remaining())
                            buffer.get(bytes)
                            header.write(annexB(bytes))
                        }
                    }
                    if (header.size() > 0) {
                        currentConfig = header.toByteArray()
                        updateConfig(currentConfig)
                    }
                }
                return
            }
            if (index < 0) return
            val output = encoder.getOutputBuffer(index)
            if (output != null && info.size > 0) {
                output.position(info.offset)
                output.limit(info.offset + info.size)
                val data = ByteArray(info.size)
                output.get(data)
                val keyframe = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                    currentConfig = annexB(data)
                    updateConfig(currentConfig)
                } else if (startedGeneration == generation && isCurrent(epoch)) {
                    onFrame(withSequenceHeader(currentConfig, data, keyframe), keyframe, info.presentationTimeUs, epoch)
                }
            }
            encoder.releaseOutputBuffer(index, false)
            if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) return
        }
    }
}
