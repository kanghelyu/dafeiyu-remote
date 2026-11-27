package remote.dafeiyu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class EncoderSettingsTest {
    @Test
    fun defaultFitPreservesBehaviorAndNeverUpscales() {
        assertEquals(EncoderSettings(1920, 1080, 30, 4000), EncoderSettings.fit(3840, 2160))
        assertEquals(EncoderSettings(1920, 1080, 30, 4000), EncoderSettings.fit(1920, 1080))
        assertEquals(EncoderSettings(1280, 720, 30, 4000), EncoderSettings.fit(1280, 720))
    }

    @Test
    fun fittingRoundsToEvenDimensionsAndHandlesPortraitAndOddSources() {
        assertEquals(EncoderSettings(1244, 700, 30, 4000),
            EncoderSettings.fitWithin(1920, 1080, 1280, 700))
        assertEquals(EncoderSettings(1080, 1920, 30, 4000), EncoderSettings.fit(2160, 3840))
        assertEquals(EncoderSettings(1920, 1080, 30, 4000), EncoderSettings.fit(3841, 2161))
        assertEquals(EncoderSettings(1920, 1080, 30, 4000), EncoderSettings.fit(1921, 1081))
    }

    @Test
    fun hostQualityControlsAllowHighFrameRateAndBitrate() {
        assertEquals(
            EncoderSettings(1280, 720, 120, 12_000),
            EncoderSettings.fitWithin(1920, 1080, 1280, 720, 120, 12_000)
        )
        assertEquals(
            EncoderSettings(1080, 1920, 120, 12_000),
            EncoderSettings.fitWithin(1080, 1920, 1920, 1920, 120, 12_000)
        )
    }

    @Test
    fun nativeResolutionMeansActualEvenSourceSize() {
        assertEquals(EncoderSettings(1078, 1918, 60, 5000),
            EncoderSettings.fit(1079, 1919, Int.MAX_VALUE - 1, 60, 5000))
    }

    @Test
    fun validationMatchesSessionBounds() {
        assertEquals(EncoderSettings(2, 2, 1, 100), EncoderSettings(2, 2, 1, 100))
        assertEquals(EncoderSettings(3840, 2160, 120, 100_000),
            EncoderSettings(3840, 2160, 120, 100_000))
        for (invalid in listOf(
            Triple(0, 2, 30), Triple(2, 3, 30), Triple(2, 2, 0), Triple(2, 2, 121)
        )) {
            assertThrows(IllegalArgumentException::class.java) {
                EncoderSettings(invalid.first, invalid.second, invalid.third, 4000)
            }
        }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings(2, 2, 30, 99) }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings(2, 2, 30, 100_001) }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings.fit(0, 1080) }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings.fit(1, 1080) }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings.fit(1920, 1080, 1921) }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings.fitWithin(1920, 1080, 1280, 700, 0, 4000) }
        assertThrows(IllegalArgumentException::class.java) { EncoderSettings.fitWithin(1920, 1080, 1280, 700, 30, 99) }
    }

    @Test
    fun restartedHostRejectsOldFramesAndSettingsCallbacks() {
        HostSession.begin(11)
        assertTrue(HostSession.isCurrent(11))
        HostSession.begin(12)
        assertFalse(HostSession.isCurrent(11))
        assertTrue(HostSession.isCurrent(12))
        assertThrows(IllegalStateException::class.java) { HostSession.requireCurrent(11) }
        HostSession.requireCurrent(12)
        HostSession.end(11)
        assertTrue(HostSession.isCurrent(12))
        HostSession.end(12)
        assertFalse(HostSession.isCurrent(12))
        assertThrows(IllegalStateException::class.java) { HostSession.requireCurrent(12) }
    }

    @Test
    fun hostSessionRejectsMissingEpoch() {
        assertThrows(IllegalArgumentException::class.java) { HostSession.begin(0) }
        assertFalse(HostSession.isCurrent(0))
        assertThrows(IllegalStateException::class.java) { HostSession.requireCurrent(0) }
    }
}
