package remote.dafeiyu

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class HostQualityValidationTest {
    @Test
    fun qualityLimitsAreValidatedWithoutAssumingSourceAspectRatio() {
        assertTrue(validateHostQuality(7680, 2, 120, 100_000))
        assertTrue(validateHostQuality(2, 7680, 1, 100))
        assertFalse(validateHostQuality(null, 1080, 30, 4000))
        assertFalse(validateHostQuality(1921, 1080, 30, 4000))
        assertFalse(validateHostQuality(1920, 1081, 30, 4000))
        assertFalse(validateHostQuality(1920, 1080, 121, 4000))
        assertFalse(validateHostQuality(1920, 1080, 30, 99))
        assertFalse(validateHostQuality(1920, 1080, 30, 100_001))
    }
}
