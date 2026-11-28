package remote.dafeiyu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewerHelpersTest {
    @Test fun responsiveLayoutStacksPhonesAndSplitsWideWindows() {
        assertEquals(ViewerHelpers.ViewerLayout.PORTRAIT, ViewerHelpers.viewerLayoutMode(360f, 800f))
        assertEquals(ViewerHelpers.ViewerLayout.PORTRAIT, ViewerHelpers.viewerLayoutMode(599f, 599f))
        assertEquals(ViewerHelpers.ViewerLayout.WIDE, ViewerHelpers.viewerLayoutMode(640f, 360f))
        assertEquals(ViewerHelpers.ViewerLayout.WIDE, ViewerHelpers.viewerLayoutMode(800f, 1280f))
        assertEquals(ViewerHelpers.ViewerLayout.PORTRAIT, ViewerHelpers.viewerLayoutMode(600f, 600f))
    }

    @Test fun fingerprintRequiresExactlySixtyFourHexDigits() {
        val pin = "AB".repeat(32)
        assertEquals(pin.lowercase(), ViewerHelpers.normalizeFingerprint("AB:".repeat(31) + "AB"))
        assertNull(ViewerHelpers.normalizeFingerprint("11".repeat(31)))
        assertNull(ViewerHelpers.normalizeFingerprint("g".repeat(64)))
        assertNull(ViewerHelpers.normalizeFingerprint("11".repeat(32) + "junk"))
    }

    @Test fun coordinatesRespectLetterboxAndClampToProtocolRange() {
        val rect = ViewerHelpers.videoRect(1000f, 500f, 1920, 1080)
        assertEquals(55.56f, rect.left, 0.01f)
        assertEquals(0f, rect.top, 0.01f)
        assertEquals(888.89f, rect.width, 0.01f)
        assertEquals(500f, rect.height, 0.01f)
        assertEquals(0 to 0, ViewerHelpers.normalizedPoint(rect.left, rect.top, 1000f, 500f, 1920, 1080))
        val far = ViewerHelpers.normalizedPoint(rect.left + rect.width - 0.01f, rect.top + rect.height - 0.01f, 1000f, 500f, 1920, 1080)!!
        assertTrue(far.first in 65_530..65_535 && far.second in 65_530..65_535)
        assertNull(ViewerHelpers.normalizedPoint(10f, 250f, 1000f, 500f, 1920, 1080))
    }

    @Test fun stationaryTouchBecomesRightClickAndAlwaysReleasesButtons() {
        val gesture = ViewerHelpers.TouchGesture()
        val down = gesture.begin(10f, 10f, 100 to 200)
        assertTrue(down.single() is ViewerHelpers.Input.Button)
        assertTrue(gesture.move(30f, 10f, 120 to 200).single() is ViewerHelpers.Input.Move)
        assertTrue(gesture.longPress().isEmpty())
        assertEquals(1, gesture.end().size)

        gesture.begin(10f, 10f, 100 to 200)
        val right = gesture.longPress().single() as ViewerHelpers.Input.Button
        assertEquals(3, right.button)
        assertTrue(right.down)
        val releases = gesture.end()
        assertEquals(listOf(3, 1), releases.map { (it as ViewerHelpers.Input.Button).button })
        assertTrue(releases.all { !(it as ViewerHelpers.Input.Button).down })
        assertTrue(gesture.end().isEmpty())
    }

    @Test fun systemActionsRequireLocalHostPermissionAndCapability() {
        val capability = "system-action.back-home.v1"
        assertFalse(ViewerHelpers.systemActionAuthorized(0x02, setOf(capability), true))
        assertFalse(ViewerHelpers.systemActionAuthorized(0x82, emptySet(), true))
        assertFalse(ViewerHelpers.systemActionAuthorized(0x82, setOf(capability), false))
        assertTrue(ViewerHelpers.systemActionAuthorized(0x82, setOf(capability), true))
    }

    @Test fun annexBExtractsBothParameterSetsAndFindsIdr() {
        val data = byteArrayOf(0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE.toByte(), 0, 0, 0, 1, 0x41)
        val csd = ViewerHelpers.AnnexB.parameterSets(data)
        assertNotNull(csd)
        assertTrue(csd!!.first.contains(0x67.toByte()))
        assertTrue(csd.second.contains(0x68.toByte()))
        assertFalse(ViewerHelpers.AnnexB.hasKeyframe(data))
        assertTrue(ViewerHelpers.AnnexB.hasKeyframe(data + byteArrayOf(0, 0, 1, 0x65)))
        assertNull(ViewerHelpers.AnnexB.parameterSets(byteArrayOf(0, 0, 1, 0x41)))
    }
}
