package remote.dafeiyu

/** Pure receive-side accounting, shared by the JNI callback and local unit tests. */
class StreamFeedbackWindow(private val intervalMs: Long = DEFAULT_INTERVAL_MS) {
    data class Feedback(
        val windowMs: Long,
        val payloadBytes: Long,
        val packets: Long,
        val framesAssembled: Long,
        val framesDelivered: Long,
        val droppedFrames: Long,
        val stallMs: Long,
    )

    private var startedAtMs: Long? = null
    private var lastPacketAtMs: Long? = null
    private var payloadBytes = 0L
    private var packets = 0L
    private var framesAssembled = 0L
    private var framesDelivered = 0L
    private var droppedFrames = 0L
    private var stallMs = 0L

    val interval = intervalMs.coerceIn(1L, MAX_WINDOW_MS)

    @Synchronized
    fun reset() {
        startedAtMs = null
        lastPacketAtMs = null
        payloadBytes = 0
        packets = 0
        framesAssembled = 0
        framesDelivered = 0
        droppedFrames = 0
        stallMs = 0
    }

    /** [datagramBytes] includes the 16-byte video header; deltas belong to this packet. */
    @Synchronized
    fun notePacket(
        nowMs: Long,
        datagramBytes: Long,
        framesAssembledDelta: Int = 0,
        framesDeliveredDelta: Int = 0,
        droppedFramesDelta: Long = 0,
    ) {
        require(datagramBytes >= VIDEO_HEADER_BYTES) { "Video datagram is shorter than its header" }
        require(framesAssembledDelta >= 0 && framesDeliveredDelta >= 0 && droppedFramesDelta >= 0) {
            "Feedback counter deltas must be non-negative"
        }
        startIfIdle(nowMs)
        val last = lastPacketAtMs
        if (last != null && nowMs > last) stallMs = maxOf(stallMs, nowMs - last)
        lastPacketAtMs = nowMs
        payloadBytes = saturatingAdd(payloadBytes, datagramBytes - VIDEO_HEADER_BYTES)
        packets = saturatingAdd(packets, 1)
        framesAssembled = saturatingAdd(framesAssembled, framesAssembledDelta.toLong())
        framesDelivered = saturatingAdd(framesDelivered, framesDeliveredDelta.toLong())
        droppedFrames = saturatingAdd(droppedFrames, droppedFramesDelta)
    }

    /** Drop-only activity is reportable, but does not itself create an inter-packet gap. */
    @Synchronized
    fun noteDropped(nowMs: Long, count: Long) {
        require(count >= 0) { "Feedback counter deltas must be non-negative" }
        startIfIdle(nowMs)
        droppedFrames = saturatingAdd(droppedFrames, count)
    }

    /** Returns and clears the active window. Idle windows never produce feedback. */
    @Synchronized
    fun take(nowMs: Long): Feedback? {
        val startedAt = startedAtMs ?: return null
        val result = if (packets > 0 || droppedFrames > 0) {
            Feedback(
                windowMs = elapsedMs(startedAt, nowMs).coerceIn(1L, MAX_WINDOW_MS),
                payloadBytes = payloadBytes,
                packets = packets,
                framesAssembled = framesAssembled,
                framesDelivered = framesDelivered,
                droppedFrames = droppedFrames,
                stallMs = stallMs.coerceIn(0L, MAX_WINDOW_MS),
            )
        } else {
            null
        }
        reset()
        return result
    }

    private fun startIfIdle(nowMs: Long) {
        if (packets == 0L && droppedFrames == 0L) startedAtMs = nowMs
    }

    private fun elapsedMs(start: Long, end: Long): Long =
        if (end > start) end - start else 0

    private fun saturatingAdd(value: Long, delta: Long): Long =
        if (delta > Long.MAX_VALUE - value) Long.MAX_VALUE else value + delta

    companion object {
        const val VIDEO_HEADER_BYTES = 16L
        const val DEFAULT_INTERVAL_MS = 500L
        const val MAX_WINDOW_MS = 60_000L
    }
}
