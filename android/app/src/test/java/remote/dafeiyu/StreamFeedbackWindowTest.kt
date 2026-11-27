package remote.dafeiyu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class StreamFeedbackWindowTest {
    @Test fun idleWindowDoesNotStartUntilFirstPacket() {
        val window = StreamFeedbackWindow()
        assertNull(window.take(60_000))
        window.notePacket(10_000, 116)
        val feedback = window.take(10_500)!!
        assertEquals(500, feedback.windowMs)
        assertEquals(100, feedback.payloadBytes)
        assertEquals(1, feedback.packets)
        assertEquals(0, feedback.stallMs)
        assertNull(window.take(60_000))
    }

    @Test fun activeGapsBecomeStallButIdleTimeDoesNot() {
        val window = StreamFeedbackWindow()
        window.notePacket(10_000, 116)
        window.notePacket(10_750, 132)
        window.notePacket(10_800, 16)
        val feedback = window.take(11_000)!!
        assertEquals(1_000, feedback.windowMs)
        assertEquals(750, feedback.stallMs)
        assertEquals(3, feedback.packets)
        assertEquals(216, feedback.payloadBytes)

        window.notePacket(20_000, 16)
        val next = window.take(20_100)!!
        assertEquals(100, next.windowMs)
        assertEquals(0, next.stallMs)
    }

    @Test fun countersIncludeAssemblyDeliveryAndDrops() {
        val window = StreamFeedbackWindow()
        window.notePacket(0, 116, framesAssembledDelta = 1, framesDeliveredDelta = 1)
        window.notePacket(10, 216, droppedFramesDelta = 2)
        window.noteDropped(20, 3)
        val feedback = window.take(500)!!
        assertEquals(300, feedback.payloadBytes)
        assertEquals(2, feedback.packets)
        assertEquals(1, feedback.framesAssembled)
        assertEquals(1, feedback.framesDelivered)
        assertEquals(5, feedback.droppedFrames)
        assertEquals(10, feedback.stallMs)
    }

    @Test fun dropOnlyWindowsAreEmittedAndReset() {
        val window = StreamFeedbackWindow()
        window.noteDropped(1_000, 1)
        val feedback = window.take(1_500)!!
        assertEquals(0, feedback.payloadBytes)
        assertEquals(0, feedback.packets)
        assertEquals(1, feedback.droppedFrames)
        assertEquals(500, feedback.windowMs)
        assertNull(window.take(2_000))
    }

    @Test fun durationsAreClampedAndIntervalHasSafeBounds() {
        val window = StreamFeedbackWindow(Long.MAX_VALUE)
        assertEquals(60_000, window.interval)
        window.notePacket(0, 16)
        window.notePacket(60_001, 16)
        val feedback = window.take(120_000)!!
        assertEquals(60_000, feedback.windowMs)
        assertEquals(60_000, feedback.stallMs)
        assertEquals(1, StreamFeedbackWindow(Long.MIN_VALUE).interval)
        assertEquals(500, StreamFeedbackWindow().interval)
    }

    @Test(expected = IllegalArgumentException::class)
    fun shortDatagramsAreRejected() {
        StreamFeedbackWindow().notePacket(0, 15)
    }

    @Test fun countersSaturateInsteadOfOverflowing() {
        val window = StreamFeedbackWindow()
        window.notePacket(0, Long.MAX_VALUE, droppedFramesDelta = Long.MAX_VALUE)
        window.notePacket(1, 16, droppedFramesDelta = 1)
        val feedback = window.take(2)!!
        assertTrue(feedback.payloadBytes > 0)
        assertEquals(Long.MAX_VALUE, feedback.droppedFrames)
    }
}
