package remote.dafeiyu

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class CaptureActivityGateTest {
    @Test
    fun authenticatedActivationCanArriveDuringHostStartup() {
        val gate = CaptureActivityGate()
        assertTrue(gate.accept(21, true, 0))
        assertTrue(gate.shouldRun(21))
        assertFalse(gate.shouldRun(22))
    }

    @Test
    fun staleHostActivityIsIgnored() {
        val gate = CaptureActivityGate()
        gate.accept(21, true, 21)
        assertFalse(gate.accept(20, false, 21))
        assertTrue(gate.shouldRun(21))
    }

    @Test
    fun staleStartupDisconnectCannotSuppressNewHostActivation() {
        val gate = CaptureActivityGate()
        gate.accept(20, false, 0)
        gate.accept(21, true, 0)
        gate.accept(20, false, 0)
        assertFalse(gate.shouldRun(20))
        assertTrue(gate.shouldRun(21))
    }

    @Test
    fun queuedDisconnectAndReconnectCoalesceToLatestState() {
        val gate = CaptureActivityGate()
        gate.accept(21, true, 21)
        gate.accept(21, false, 21)
        assertFalse(gate.shouldRun(21))
        gate.accept(21, true, 21)
        assertTrue(gate.shouldRun(21))
    }

    @Test
    fun resetDropsDesiredStateBeforeANewHostStarts() {
        val gate = CaptureActivityGate()
        gate.accept(21, true, 21)
        gate.reset()
        assertFalse(gate.shouldRun(21))
        assertFalse(gate.shouldRun(22))
        assertTrue(gate.accept(22, true, 0))
        assertTrue(gate.shouldRun(22))
    }
}
