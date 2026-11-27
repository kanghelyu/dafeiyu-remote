package remote.dafeiyu

internal class CaptureActivityGate {
    private val activeByEpoch = mutableMapOf<Long, Boolean>()

    @Synchronized
    fun accept(epoch: Long, active: Boolean, currentEpoch: Long): Boolean {
        if (epoch <= 0L || (currentEpoch != 0L && currentEpoch != epoch)) return false
        if (currentEpoch != 0L) activeByEpoch.clear()
        activeByEpoch[epoch] = active
        return true
    }

    @Synchronized
    fun reset() {
        activeByEpoch.clear()
    }

    @Synchronized
    fun shouldRun(currentEpoch: Long): Boolean =
        currentEpoch > 0L && activeByEpoch[currentEpoch] == true
}
