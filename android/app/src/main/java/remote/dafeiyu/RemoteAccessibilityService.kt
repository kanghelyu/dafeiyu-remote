package remote.dafeiyu

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.accessibilityservice.GestureDescription.StrokeDescription
import android.graphics.Path
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES
import android.util.DisplayMetrics
import android.view.accessibility.AccessibilityEvent

class RemoteAccessibilityService : AccessibilityService() {
    private val handler = Handler(Looper.getMainLooper())
    private var lastX = 0
    private var lastY = 0

    @Volatile
    private var active = false

    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        active = true
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) = Unit

    override fun onInterrupt() {
        active = false
        release()
    }

    override fun onDestroy() {
        active = false
        release()
        if (instance === this) instance = null
        super.onDestroy()
    }

    internal fun handle(event: InputWire) {
        if (!active) return
        val size = screenSize()
        if (InputWire.carriesPointerPosition(event.kind)) {
            lastX = InputWire.normalize(event.x.coerceIn(0, 65_535), size.first)
            lastY = InputWire.normalize(event.y.coerceIn(0, 65_535), size.second)
        }
        when (event.kind) {
            2 -> if (event.hidUsage == 1 && event.state == 1) tap(lastX, lastY)
            3 -> Unit
            4 -> wheel(event.dx, event.dy, size.first, size.second)
            1 -> if (event.state == 1) when (event.hidUsage) {
                0x29 -> performGlobalAction(GLOBAL_ACTION_BACK)
                0x4a -> performGlobalAction(GLOBAL_ACTION_HOME)
            }
        }
    }

    fun release() = Unit

    private fun screenSize(): Pair<Int, Int> {
        val windows = getSystemService(android.view.WindowManager::class.java)
        if (Build.VERSION.SDK_INT >= 30) {
            val bounds = windows.maximumWindowMetrics.bounds
            return bounds.width() to bounds.height()
        }
        val metrics = DisplayMetrics()
        windows.defaultDisplay.getRealMetrics(metrics)
        return metrics.widthPixels to metrics.heightPixels
    }

    private fun tap(x: Int, y: Int) {
        val path = Path().apply { moveTo(x.toFloat(), y.toFloat()) }
        val gesture = GestureDescription.Builder()
            .addStroke(StrokeDescription(path, 0, 60))
            .build()
        dispatchGesture(gesture, null, handler)
    }

    private fun wheel(dx: Int, dy: Int, width: Int, height: Int) {
        if (dx == 0 && dy == 0) return
        val distance = minOf(300, maxOf(width, height) / 4)
        val xDirection = if (dx > 0) 1 else -1
        val yDirection = if (dy > 0) 1 else -1
        val endX = (lastX.toLong() + xDirection * distance).coerceIn(0, width - 1L).toInt()
        val endY = (lastY.toLong() - yDirection * distance).coerceIn(0, height - 1L).toInt()
        val path = Path().apply {
            moveTo(lastX.toFloat(), lastY.toFloat())
            lineTo(endX.toFloat(), endY.toFloat())
        }
        val gesture = GestureDescription.Builder()
            .addStroke(StrokeDescription(path, 0, 180))
            .build()
        dispatchGesture(gesture, null, handler)
    }

    companion object {
        @Volatile var instance: RemoteAccessibilityService? = null
            private set

        val isActive: Boolean
            get() = instance?.active == true

        fun isEnabled(context: android.content.Context): Boolean {
            val expected = android.content.ComponentName(context, RemoteAccessibilityService::class.java)
            val enabled = android.provider.Settings.Secure.getString(
                context.contentResolver,
                ENABLED_ACCESSIBILITY_SERVICES
            ).orEmpty()
            return enabled.split(':').any {
                android.content.ComponentName.unflattenFromString(it) == expected
            }
        }
    }
}
