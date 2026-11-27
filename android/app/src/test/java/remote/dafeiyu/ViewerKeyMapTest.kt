package remote.dafeiyu

import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ViewerKeyMapTest {
    @Test fun commonKeysUseHidUsages() {
        assertEquals(0x04, ViewerKeyMap.usage(KeyEvent.KEYCODE_A, 0))
        assertEquals(0x28, ViewerKeyMap.usage(KeyEvent.KEYCODE_ENTER, 0))
        assertEquals(0x29, ViewerKeyMap.usage(KeyEvent.KEYCODE_ESCAPE, 0))
        assertEquals(0x4A, ViewerKeyMap.usage(KeyEvent.KEYCODE_MOVE_HOME, 0))
        assertNull(ViewerKeyMap.usage(KeyEvent.KEYCODE_UNKNOWN, 0))
    }

    @Test fun modifierBitmapMatchesHostProtocol() {
        assertEquals(0x0F, ViewerKeyMap.modifiers(
            KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON or KeyEvent.META_ALT_ON or KeyEvent.META_META_ON,
        ))
        assertEquals(0, ViewerKeyMap.modifiers(0))
    }
}
