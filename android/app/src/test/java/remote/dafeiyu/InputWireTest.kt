package remote.dafeiyu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class InputWireTest {
    @Test fun decodesRelativeMoveAndUnicode() {
        val data = byteArrayOf(
            56, 3,
            72, 1,
            80, 1
        )
        val event = InputWire.decode(data)!!
        assertEquals(-2, event.dx)
        assertEquals(1, event.unicode)
        assertEquals(1L, event.timestampUs)
    }

    @Test fun rejectsTruncatedAndOversizedPackets() {
        assertNull(InputWire.decode(byteArrayOf(8, 0x80.toByte())))
        assertNull(InputWire.decode(ByteArray(1025)))
        assertNull(InputWire.decode(byteArrayOf()))
        assertNull(InputWire.decode(byteArrayOf(0)))
        assertNull(InputWire.decode(byteArrayOf(11)))
        assertNull(InputWire.decode(byteArrayOf(-1, -1, -1, -1, -1, -1, -1, -1, -1, 2)))
        assertNull(InputWire.decode(byteArrayOf(18, 2, 1)))
    }

    @Test fun mapsNormalizedCoordinatesWithoutOverflow() {
        assertEquals(0, InputWire.normalize(0, 1080))
        assertEquals(1079, InputWire.normalize(65_535, 1080))
        assertEquals(1079, InputWire.normalize(Int.MAX_VALUE, 1080))
    }

    @Test fun onlyPointerEventsUpdateTheLastPointerPosition() {
        assertTrue(InputWire.carriesPointerPosition(2))
        assertTrue(InputWire.carriesPointerPosition(3))
        assertTrue(InputWire.carriesPointerPosition(5))
        assertFalse(InputWire.carriesPointerPosition(1))
        assertFalse(InputWire.carriesPointerPosition(4))
    }
}
