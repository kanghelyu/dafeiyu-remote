package remote.dafeiyu

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RemoteServiceTest {
    @Test
    fun sequenceHeadersUseAnnexBStartCodes() {
        val sps = byteArrayOf(0x67, 0x42, 0x00)
        val pps = byteArrayOf(0x68, 0x01)
        assertArrayEquals(byteArrayOf(0, 0, 0, 1, 0x67, 0x42, 0), annexB(sps))
        assertArrayEquals(byteArrayOf(0, 0, 1, 0x68, 0x01), annexB(byteArrayOf(0, 0, 1, 0x68, 0x01)))
        assertArrayEquals(byteArrayOf(0, 0, 0, 1, 0x68, 0x01), annexB(pps))
    }

    @Test
    fun onlyKeyframesReceiveTheSequenceHeader() {
        val header = byteArrayOf(0, 0, 0, 1, 0x67)
        val frame = byteArrayOf(0x65, 0x01)
        assertArrayEquals(header + frame, withSequenceHeader(header, frame, true))
        assertArrayEquals(frame, withSequenceHeader(header, frame, false))
        assertArrayEquals(frame, withSequenceHeader(null, frame, true))
        assertEquals(0, annexB(byteArrayOf()).size)
    }

    @Test
    fun staleProjectionCallbackCannotStopRestartedHost() {
        val oldProjection = Any()
        val newProjection = Any()
        HostSession.begin(21)
        assertTrue(isCurrentProjectionToken(oldProjection, oldProjection, 21, 21))
        HostSession.begin(22)
        assertFalse(isCurrentProjectionToken(newProjection, oldProjection, 22, 21))
        assertFalse(isCurrentProjectionToken(newProjection, newProjection, 22, 21))
        assertTrue(isCurrentProjectionToken(newProjection, newProjection, 22, 22))
        HostSession.end(22)
        assertFalse(isCurrentProjectionToken(newProjection, newProjection, 0, 22))
    }
}
