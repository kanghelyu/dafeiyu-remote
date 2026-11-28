package remote.dafeiyu

/** Pure helpers shared by the viewer UI and local JVM tests. */
object ViewerHelpers {
    enum class ViewerLayout { PORTRAIT, WIDE }

    /** Compact windows stack; landscape phones and tablets give the video more room. */
    fun viewerLayoutMode(widthDp: Float, heightDp: Float): ViewerLayout = when {
        widthDp >= 720f -> ViewerLayout.WIDE
        widthDp >= 600f && widthDp > heightDp -> ViewerLayout.WIDE
        else -> ViewerLayout.PORTRAIT
    }

    fun normalizeFingerprint(value: String): String? {
        if (value.any { it !in "0123456789abcdefABCDEF:- \t\r\n" }) return null
        val hex = value.filter { it.isHex() }.lowercase()
        return hex.takeIf { it.length == 64 }
    }

    fun formatFingerprint(value: String): String =
        normalizeFingerprint(value)?.chunked(2)?.joinToString(":") ?: value

    fun systemActionAuthorized(
        permissions: Int,
        capabilities: Set<String>,
        localControl: Boolean,
        requiredCapability: String = "system-action.back-home.v1",
    ): Boolean = localControl && permissions and 0x02 != 0 && permissions and 0x80 != 0 &&
        requiredCapability in capabilities

    private fun Char.isHex() = this in '0'..'9' || this in 'a'..'f' || this in 'A'..'F'

    data class VideoRect(val left: Float, val top: Float, val width: Float, val height: Float)

    fun videoRect(viewWidth: Float, viewHeight: Float, videoWidth: Int, videoHeight: Int): VideoRect {
        val sourceWidth = if (videoWidth > 0) videoWidth.toFloat() else 16f
        val sourceHeight = if (videoHeight > 0) videoHeight.toFloat() else 9f
        if (viewWidth <= 0f || viewHeight <= 0f) return VideoRect(0f, 0f, 0f, 0f)
        val scale = minOf(viewWidth / sourceWidth, viewHeight / sourceHeight)
        val width = sourceWidth * scale
        val height = sourceHeight * scale
        return VideoRect((viewWidth - width) / 2f, (viewHeight - height) / 2f, width, height)
    }

    /** Maps a view point to the protocol's normalized 0..65535 coordinate space. */
    fun normalizedPoint(
        x: Float,
        y: Float,
        viewWidth: Float,
        viewHeight: Float,
        videoWidth: Int,
        videoHeight: Int,
    ): Pair<Int, Int>? {
        val rect = videoRect(viewWidth, viewHeight, videoWidth, videoHeight)
        if (x < rect.left || x >= rect.left + rect.width || y < rect.top || y >= rect.top + rect.height) {
            return null
        }
        val nx = ((x - rect.left) / rect.width * 65_535f).toInt().coerceIn(0, 65_535)
        val ny = ((y - rect.top) / rect.height * 65_535f).toInt().coerceIn(0, 65_535)
        return nx to ny
    }

    sealed interface Input {
        data class Move(val x: Int, val y: Int) : Input
        data class Button(val button: Int, val down: Boolean, val x: Int, val y: Int) : Input
    }

    /** Touch is a left-button pointer; a stationary long press becomes a right click. */
    class TouchGesture {
        private var active = false
        private var moved = false
        private var longPressed = false
        private var x = 0
        private var y = 0
        private var startX = 0f
        private var startY = 0f

        fun begin(px: Float, py: Float, normalized: Pair<Int, Int>): List<Input> {
            active = true
            moved = false
            longPressed = false
            startX = px
            startY = py
            x = normalized.first
            y = normalized.second
            return listOf(Input.Button(1, true, x, y))
        }

        fun move(px: Float, py: Float, normalized: Pair<Int, Int>): List<Input> {
            if (!active) return emptyList()
            if (kotlin.math.hypot(px - startX, py - startY) > 12f) moved = true
            x = normalized.first
            y = normalized.second
            return listOf(Input.Move(x, y))
        }

        fun longPress(): List<Input> {
            if (!active || moved || longPressed) return emptyList()
            longPressed = true
            return listOf(Input.Button(3, true, x, y))
        }

        fun end(): List<Input> {
            if (!active) return emptyList()
            active = false
            val releases = mutableListOf<Input>()
            if (longPressed) releases += Input.Button(3, false, x, y)
            releases += Input.Button(1, false, x, y)
            return releases
        }

        fun cancel(): List<Input> = end()
    }

    /** Extracts H.264 parameter sets without interpreting or transcoding the access unit. */
    object AnnexB {
        fun parameterSets(data: ByteArray): Pair<ByteArray, ByteArray>? {
            val sps = mutableListOf<Byte>()
            val pps = mutableListOf<Byte>()
            var offset = 0
            while (offset < data.size) {
                val start = startCode(data, offset) ?: break
                val nalStart = start.second
                val next = startCode(data, nalStart)?.first ?: data.size
                val end = if (next == data.size) data.size else next
                if (end > nalStart) {
                    val type = data[nalStart].toInt() and 0x1f
                    val target = when (type) {
                        7 -> sps
                        8 -> pps
                        else -> null
                    }
                    if (target != null) {
                        target += 0
                        target += 0
                        target += 0
                        target += 1
                        target += data.copyOfRange(nalStart, end).asList()
                    }
                }
                offset = end
            }
            return if (sps.isNotEmpty() && pps.isNotEmpty()) {
                sps.toByteArray() to pps.toByteArray()
            } else null
        }

        fun hasKeyframe(data: ByteArray): Boolean {
            var offset = 0
            while (offset < data.size) {
                val start = startCode(data, offset) ?: break
                val nalStart = start.second
                if (nalStart < data.size && data[nalStart].toInt() and 0x1f == 5) return true
                offset = nalStart
            }
            return false
        }

        /** Returns the NAL payload offset and the next search offset. */
        private fun startCode(data: ByteArray, from: Int): Pair<Int, Int>? {
            var i = from
            while (i + 2 < data.size) {
                if (data[i] == 0.toByte() && data[i + 1] == 0.toByte()) {
                    if (data[i + 2] == 1.toByte()) return i to i + 3
                    if (i + 3 < data.size && data[i + 2] == 0.toByte() && data[i + 3] == 1.toByte()) {
                        return i to i + 4
                    }
                }
                i++
            }
            return null
        }
    }
}
