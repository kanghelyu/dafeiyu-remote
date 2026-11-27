package remote.dafeiyu

import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.ArrayDeque

/** Surface-backed H.264 decoder. Input is already reassembled Annex-B from the Rust client. */
class DfyH264Decoder(
    private val onError: (String) -> Unit,
    private val onDimensions: (Int, Int) -> Unit = { _, _ -> },
    private val onFramePresented: () -> Unit = {},
) {
    private data class Frame(val data: ByteArray, val ptsUs: Long, val keyframe: Boolean)

    private val lock = Any()
    private val frames = ArrayDeque<Frame>()
    private var surface: Surface? = null
    private var codec: MediaCodec? = null
    private var codecGeneration = -1
    private var retiringCodec = false
    private var worker: Thread? = null
    private var running = false
    private var latestKeyframe: Frame? = null
    private var outstandingInputs = 0
    private var outputDeadlineMs = 0L
    private var desiredWidth = 0
    private var desiredHeight = 0
    private var configuredWidth = 0
    private var configuredHeight = 0
    private var generation = 0
    private val decoderCandidates = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
        .filter { !it.isEncoder && it.supportedTypes.any(MIME::equals) }
        .map { it.name }
    private var selectedDecoderIndex = 0

    init {
        Log.i("DfyH264Decoder", "decoder_candidates=${decoderCandidates.joinToString()}")
    }

    fun setSurface(value: Surface?) {
        synchronized(lock) {
            if (surface === value) return
            surface = value
            generation++
            frames.clear()
            latestKeyframe = null
            running = value != null
            // Keep the old codec referenced until the worker has stopped and released it.
            configuredWidth = 0
            configuredHeight = 0
            selectedDecoderIndex = 0
            resetOutputStateLocked()
            retiringCodec = codec != null || worker?.isAlive == true
        }
        stopWorker()
    }

    fun setVideoConfig(width: Int, height: Int) {
        synchronized(lock) {
            if (width <= 0 || height <= 0 || (width == desiredWidth && height == desiredHeight)) return
            desiredWidth = width
            desiredHeight = height
            generation++
        }
    }

    /** Returns false when the access unit was dropped or must wait for a keyframe. */
    fun submit(data: ByteArray, ptsUs: Long, keyframe: Boolean): Boolean {
        if (data.isEmpty() || data.size > MAX_FRAME_BYTES) return false
        synchronized(lock) {
            val targetSurface = surface ?: return false
            val recoveryKeyframe = if (keyframe) Frame(data, ptsUs, true) else latestKeyframe
            if (recoveryKeyframe == null) return false
            latestKeyframe = recoveryKeyframe
            if (codec == null && retiringCodec) return false
            try {
                if (codec == null) {
                    val csd = ViewerHelpers.AnnexB.parameterSets(recoveryKeyframe.data) ?: return false
                    codec = createCodec(targetSurface, csd.first, csd.second)
                    codecGeneration = generation
                    configuredWidth = if (desiredWidth > 0) desiredWidth else FALLBACK_WIDTH
                    configuredHeight = if (desiredHeight > 0) desiredHeight else FALLBACK_HEIGHT
                    resetOutputStateLocked()
                    restoreKeyframeLocked()
                    Log.i("DfyH264Decoder", "decoder_selected name=${codec?.name} index=${selectedDecoderIndex + 1}")
                }
                if (keyframe) frames.clear()
                if (frames.size >= MAX_QUEUED_FRAMES) frames.removeFirst()
                frames.addLast(Frame(data, ptsUs, keyframe))
                running = true
                startWorkerLocked()
                return true
            } catch (error: Exception) {
                val failed = codec
                codec = null
                codecGeneration = -1
                running = false
                frames.clear()
                configuredWidth = 0
                configuredHeight = 0
                resetOutputStateLocked()
                safeStop(failed)
                safeRelease(failed)
                onError("H.264 decoder: ${error.javaClass.simpleName}: ${error.message}")
                return false
            }
        }
    }

    fun release() {
        synchronized(lock) {
            generation++
            running = false
            surface = null
            frames.clear()
            latestKeyframe = null
            codec = null
            codecGeneration = -1
            configuredWidth = 0
            configuredHeight = 0
            selectedDecoderIndex = 0
            resetOutputStateLocked()
            retiringCodec = codec != null || worker?.isAlive == true || retiringCodec
        }
        stopWorker()
    }

    private fun stopWorker() {
        val current = synchronized(lock) { worker }
        current?.interrupt()
        current?.join(STOP_JOIN_MS)
        synchronized(lock) {
            // join 超时时旧 worker 可能仍在 stop/release；此时必须继续阻止创建替代 codec。
            if (current == null || !current.isAlive) retiringCodec = false
        }
    }

    private fun startWorkerLocked() {
        synchronized(lock) {
            if (worker?.isAlive == true || !running || surface == null || codec == null) return
            worker = Thread({ decodeLoop() }, "dfy-h264-decoder").also { it.start() }
        }
    }

    private fun createCodec(targetSurface: Surface, sps: ByteArray, pps: ByteArray): MediaCodec {
        val width = if (desiredWidth > 0) desiredWidth else FALLBACK_WIDTH
        val height = if (desiredHeight > 0) desiredHeight else FALLBACK_HEIGHT
        val format = MediaFormat.createVideoFormat(MIME, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(sps))
            setByteBuffer("csd-1", ByteBuffer.wrap(pps))
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, MAX_FRAME_BYTES)
        }
        var candidateIndex = selectedDecoderIndex
        var failure: Exception? = null
        while (candidateIndex < decoderCandidates.size) {
            val candidate = try {
                MediaCodec.createByCodecName(decoderCandidates[candidateIndex])
            } catch (error: Exception) {
                failure = error
                candidateIndex++
                continue
            }
            try {
                candidate.configure(format, targetSurface, null, 0)
                candidate.start()
                selectedDecoderIndex = candidateIndex
                return candidate
            } catch (error: Exception) {
                failure = error
                Log.w(
                    "DfyH264Decoder",
                    "decoder_configure_failed name=${candidate.name} error=${error.javaClass.simpleName}: ${error.message}",
                )
                runCatching { candidate.stop() }
                runCatching { candidate.release() }
                candidateIndex++
            }
        }
        selectedDecoderIndex = candidateIndex
        throw failure ?: IllegalStateException("没有可用的 H.264 解码器")
    }

    private fun decodeLoop() {
        var active: MediaCodec? = null
        var observedGeneration = generation
        try {
            while (true) {
                var current: MediaCodec? = null
                var retire: MediaCodec? = null
                var terminal: Throwable? = null
                synchronized(lock) {
                    if (!running || surface == null) return
                    if (active != null &&
                        (codec !== active || observedGeneration != generation ||
                            codecGeneration != generation)
                    ) {
                        retire = active
                        active = null
                        if (codec === retire) {
                            codec = null
                            codecGeneration = -1
                        }
                        configuredWidth = 0
                        configuredHeight = 0
                        resetOutputStateLocked()
                        restoreKeyframeLocked()
                    }
                    if (active == null && codec != null && codecGeneration != generation) {
                        retire = codec
                        codec = null
                        codecGeneration = -1
                        configuredWidth = 0
                        configuredHeight = 0
                        resetOutputStateLocked()
                        restoreKeyframeLocked()
                    }
                    if (retire == null && active == null) {
                        active = codec
                        observedGeneration = generation
                    }
                    if (retire == null && active == null && latestKeyframe != null) {
                        try {
                            val keyframe = latestKeyframe ?: return
                            val targetSurface = surface ?: return
                            val csd = ViewerHelpers.AnnexB.parameterSets(keyframe.data)
                            if (csd == null) {
                                running = false
                                codec = null
                                codecGeneration = -1
                                frames.clear()
                                terminal = IllegalStateException("retained keyframe has no SPS/PPS")
                            } else {
                                active = createCodec(targetSurface, csd.first, csd.second)
                                codec = active
                                codecGeneration = generation
                                configuredWidth = if (desiredWidth > 0) desiredWidth else FALLBACK_WIDTH
                                configuredHeight = if (desiredHeight > 0) desiredHeight else FALLBACK_HEIGHT
                                observedGeneration = generation
                                resetOutputStateLocked()
                                restoreKeyframeLocked()
                                Log.i("DfyH264Decoder", "decoder_selected name=${active?.name} index=${selectedDecoderIndex + 1}")
                            }
                        } catch (error: Exception) {
                            running = false
                            codec = null
                            codecGeneration = -1
                            frames.clear()
                            terminal = error
                        }
                    }
                    current = active
                    if (terminal == null && current != null &&
                        outstandingInputs > 0 && SystemClock.elapsedRealtime() >= outputDeadlineMs
                    ) {
                        val failure = IllegalStateException("decoder produced no output")
                        if (codec === current) {
                            codec = null
                            codecGeneration = -1
                        }
                        active = null
                        configuredWidth = 0
                        configuredHeight = 0
                        resetOutputStateLocked()
                        terminal = failureStateLocked(failure)
                        restoreKeyframeLocked()
                        retire = current
                        Log.w("DfyH264Decoder", "decoder_output_timeout outstanding=$outstandingInputs")
                    }
                    retiringCodec = retire != null
                }
                if (terminal != null) {
                    safeStop(retire)
                    safeRelease(retire)
                    synchronized(lock) { retiringCodec = false }
                    onError("H.264 decoder: ${terminal.javaClass.simpleName}: ${terminal.message}")
                    return
                }
                if (retire != null) {
                    safeStop(retire)
                    safeRelease(retire)
                    synchronized(lock) { retiringCodec = false }
                    Log.w("DfyH264Decoder", "decoder_retired next_index=${selectedDecoderIndex + 1}")
                    continue
                }
                val codecToUse = current ?: return
                try {
                    val inputIndex = codecToUse.dequeueInputBuffer(INPUT_POLL_MS)
                    if (inputIndex >= 0) {
                        val frame = synchronized(lock) {
                            if (running && codec === codecToUse && observedGeneration == generation) {
                                frames.pollFirst()
                            } else {
                                null
                            }
                        }
                        if (frame != null) {
                            synchronized(lock) {
                                if (codec === codecToUse) {
                                    outstandingInputs++
                                    if (outstandingInputs == 1) {
                                        outputDeadlineMs = SystemClock.elapsedRealtime() + NO_OUTPUT_TIMEOUT_MS
                                    }
                                }
                            }
                            val input = codecToUse.getInputBuffer(inputIndex) ?: error("Decoder returned no input buffer")
                            input.clear()
                            input.put(frame.data)
                            val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                            codecToUse.queueInputBuffer(inputIndex, 0, frame.data.size, frame.ptsUs, flags)
                        } else {
                            codecToUse.queueInputBuffer(inputIndex, 0, 0, 0, 0)
                        }
                    }
                    val presented = drainOutput(codecToUse)
                    if (presented > 0) {
                        synchronized(lock) {
                            if (codec === codecToUse) {
                                outstandingInputs = maxOf(0, outstandingInputs - presented)
                                outputDeadlineMs = if (outstandingInputs == 0) {
                                    0L
                                } else {
                                    SystemClock.elapsedRealtime() + NO_OUTPUT_TIMEOUT_MS
                                }
                            }
                        }
                    }
                } catch (error: Exception) {
                    val retired = synchronized(lock) {
                        if (!running || codec !== codecToUse || observedGeneration != generation) {
                            false
                        } else {
                            codec = null
                            codecGeneration = -1
                            active = null
                            configuredWidth = 0
                            configuredHeight = 0
                            resetOutputStateLocked()
                            terminal = failureStateLocked(error)
                            restoreKeyframeLocked()
                            retire = codecToUse
                            retiringCodec = true
                            true
                        }
                    }
                    if (retired) {
                        safeStop(codecToUse)
                        safeRelease(codecToUse)
                        synchronized(lock) {
                            retiringCodec = false
                            if (retire == codecToUse) retire = null
                        }
                        Log.w("DfyH264Decoder", "decoder_retired next_index=${selectedDecoderIndex + 1} error=${error.javaClass.simpleName}: ${error.message}")
                        if (terminal != null) {
                            onError("H.264 decoder: ${terminal.javaClass.simpleName}: ${terminal.message}")
                            return
                        }
                    }
                }
            }
        } finally {
            safeStop(active)
            safeRelease(active)
            val restart: Boolean
            synchronized(lock) {
                if (worker === Thread.currentThread()) worker = null
                if (codec === active && (!running || surface == null)) {
                    codec = null
                    codecGeneration = -1
                }
                if (codec == null) retiringCodec = false
                restart = running && surface != null && (codec != null || latestKeyframe != null)
            }
            if (restart) startWorkerLocked()
        }
    }

    private fun failureStateLocked(error: Throwable): Throwable? {
        selectedDecoderIndex = minOf(selectedDecoderIndex + 1, decoderCandidates.size)
        if (selectedDecoderIndex < decoderCandidates.size) return null
        running = false
        frames.clear()
        return error
    }

    private fun restoreKeyframeLocked() {
        frames.clear()
        latestKeyframe?.let { frames.addLast(it) }
    }

    private fun resetOutputStateLocked() {
        outstandingInputs = 0
        outputDeadlineMs = 0L
    }

    private fun drainOutput(codec: MediaCodec): Int {
        var presented = 0
        while (true) {
            val info = MediaCodec.BufferInfo()
            val index = codec.dequeueOutputBuffer(info, 0)
            if (index < 0) break
            if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                val format = codec.outputFormat
                val width = format.getInteger(MediaFormat.KEY_WIDTH)
                val height = format.getInteger(MediaFormat.KEY_HEIGHT)
                if (width > 0 && height > 0) onDimensions(width, height)
            } else if (index >= 0) {
                if ((info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0) {
                    codec.releaseOutputBuffer(index, false)
                } else {
                    codec.releaseOutputBuffer(index, true)
                    presented++
                    onFramePresented()
                }
            }
        }
        return presented
    }

    private fun safeStop(value: MediaCodec?) {
        try { value?.stop() } catch (_: Exception) { }
    }

    private fun safeRelease(value: MediaCodec?) {
        try { value?.release() } catch (_: Exception) { }
    }

    companion object {
        const val MIME = "video/avc"
        private const val FALLBACK_WIDTH = 1920
        private const val FALLBACK_HEIGHT = 1080
        private const val MAX_FRAME_BYTES = 8 * 1024 * 1024
        private const val MAX_QUEUED_FRAMES = 4
        private const val NO_OUTPUT_TIMEOUT_MS = 5_000L
        private const val INPUT_POLL_MS = 20L
        private const val STOP_JOIN_MS = 2_000L
    }
}
