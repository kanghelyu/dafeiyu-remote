package remote.dafeiyu

internal data class InputWire(
    val kind: Int,
    val hidUsage: Int,
    val mods: Int,
    val state: Int,
    val x: Int,
    val y: Int,
    val dx: Int,
    val dy: Int,
    val unicode: Int,
    val timestampUs: Long
) {
    companion object {
        fun decode(data: ByteArray): InputWire? {
            return try {
                require(data.isNotEmpty() && data.size <= 1024)
            val reader = Reader(data)
            var kind = 0
            var usage = 0
            var mods = 0
            var state = 0
            var x = 0
            var y = 0
            var dx = 0
            var dy = 0
            var unicode = 0
            var timestamp = 0L
            while (reader.hasMore()) {
                val tag = reader.varint()
                val field = (tag ushr 3).toInt()
                require(field in 1..0x1fffffff)
                when ((tag and 7).toInt()) {
                    0 -> when (field) {
                        1 -> kind = reader.varint().toInt()
                        2 -> usage = reader.varint().toInt()
                        3 -> mods = reader.varint().toInt()
                        4 -> state = reader.varint().toInt()
                        5 -> x = reader.varint().toInt()
                        6 -> y = reader.varint().toInt()
                        7 -> dx = reader.sint32()
                        8 -> dy = reader.sint32()
                        9 -> unicode = reader.varint().toInt()
                        10 -> timestamp = reader.varint()
                        else -> reader.varint()
                    }
                    1 -> reader.skip(8)
                    2 -> reader.skip(reader.length().toInt())
                    5 -> reader.skip(4)
                    else -> return null
                }
            }
            InputWire(kind, usage, mods, state, x, y, dx, dy, unicode, timestamp)
            } catch (_: IllegalArgumentException) {
                null
            }
        }

        fun normalize(value: Int, extent: Int): Int {
            require(extent > 0)
            return (value.toLong() * extent / 65_535L)
                .coerceIn(0, extent - 1L).toInt()
        }

        fun carriesPointerPosition(kind: Int): Boolean = kind == 2 || kind == 3 || kind == 5
    }

    private class Reader(private val data: ByteArray) {
        private var offset = 0
        fun hasMore() = offset < data.size
        fun varint(): Long {
            var result = 0L
            for (index in 0 until 10) {
                require(offset < data.size)
                val byte = data[offset++].toInt() and 0xff
                if (index == 9) require(byte and 0xfe == 0)
                result = result or ((byte and 0x7f).toLong() shl (index * 7))
                if (byte and 0x80 == 0) return result
            }
            throw IllegalArgumentException("overlong varint")
        }
        fun sint32(): Int {
            val value = varint()
            return ((value ushr 1).toInt()) xor (-(value and 1L)).toInt()
        }
        fun length(): Long {
            val length = varint()
            require(length in 0..(data.size - offset).toLong())
            return length
        }
        fun skip(count: Int) {
            require(count in 0..(data.size - offset))
            offset += count
        }
    }
}
