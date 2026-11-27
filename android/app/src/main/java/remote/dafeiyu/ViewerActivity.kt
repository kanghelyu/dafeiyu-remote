package remote.dafeiyu

import android.content.pm.ActivityInfo
import android.graphics.Bitmap
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Gravity
import android.view.PixelCopy
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.inputmethod.EditorInfo
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicInteger

/** Remote viewer entry point. The controlled-end MainActivity remains independent. */
class ViewerActivity : ComponentActivity(), DfyControllerNative.Listener, SurfaceHolder.Callback {
    private lateinit var host: EditText
    private lateinit var port: EditText
    private lateinit var password: EditText
    private lateinit var fingerprint: EditText
    private lateinit var connect: Button
    private lateinit var disconnect: Button
    private lateinit var localControl: CheckBox
    private lateinit var back: Button
    private lateinit var home: Button
    private lateinit var qualityWidth: EditText
    private lateinit var qualityHeight: EditText
    private lateinit var qualityFps: EditText
    private lateinit var qualityBitrate: EditText
    private lateinit var applyQuality: Button
    private lateinit var resetQuality: Button
    private lateinit var qualityView: TextView
    private lateinit var stateView: TextView
    private lateinit var detailView: TextView
    private lateinit var pinView: TextView
    private lateinit var permissionsView: TextView
    private lateinit var statsView: TextView
    private lateinit var surfaceView: SurfaceView
    private lateinit var videoFrame: FrameLayout
    private lateinit var decoder: DfyH264Decoder

    private val ui = Handler(Looper.getMainLooper())
    private val gesture = ViewerHelpers.TouchGesture()
    private var gesturePoint: Pair<Int, Int>? = null
    private var videoWidth = 0
    private var videoHeight = 0
    private var permissions = 0
    private var capabilities: Set<String> = emptySet()
    private var connected = false
    private var state = "disconnected"
    private var detail = ""
    private var controlAllowed = false
    private var lastKeyframeRequest = 0L
    private val decodedFrames = AtomicInteger()
    private var surfaceRecoveryCount = 0

    private val longPress = Runnable {
        if (gesturePoint == null) return@Runnable
        dispatch(gesture.longPress())
    }

    private val remoteBack = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() {
            DfyControllerNative.systemAction(DfyControllerNative.ACTION_BACK)
        }
    }

    private val rebuildSurface = Runnable { rebuildVideoSurface() }

    private val statsPoll = object : Runnable {
        override fun run() {
            updateStats()
            ui.postDelayed(this, 1_000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        buildUi()
        onBackPressedDispatcher.addCallback(this, remoteBack)
        decoder = DfyH264Decoder(
            onError = { message -> ui.post { showDecoderError(message) } },
            onDimensions = { width, height -> ui.post { videoWidth = width; videoHeight = height } },
            onFramePresented = {
                inspectDecodedPixels()
            },
        )
        DfyControllerNative.setListener(this)
        updateControls()
        updateStats()
    }

    private fun inspectDecodedPixels() {
        if (!BuildConfig.DEBUG) return
        val count = decodedFrames.incrementAndGet()
        if (count !in setOf(1, 15, 30, 60)) return
        ui.postDelayed({
            val width = surfaceView.width
            val height = surfaceView.height
            val source = surfaceView.holder.surface
            if (width <= 0 || height <= 0 || !source.isValid) return@postDelayed
            val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
            PixelCopy.request(source, bitmap, { result ->
                if (result != PixelCopy.SUCCESS) {
                    Log.w("DfyViewerPixels", "pixel_copy_failed frame=$count result=$result size=${width}x$height")
                    bitmap.recycle()
                    return@request
                }
                var min = 255
                var max = 0
                var sum = 0L
                var samples = 0
                var nonBlack = 0
                val step = 16
                var y = 0
                while (y < height) {
                    var x = 0
                    while (x < width) {
                        val pixel = bitmap.getPixel(x, y)
                        val luma = ((pixel ushr 16 and 255) * 299 + (pixel ushr 8 and 255) * 587 + (pixel and 255) * 114) / 1000
                        min = minOf(min, luma)
                        max = maxOf(max, luma)
                        sum += luma
                        samples++
                        if (luma > 8) nonBlack++
                        x += step
                    }
                    y += step
                }
                Log.i(
                    "DfyViewerPixels",
                    "decoded_pixels frame=$count size=${width}x$height min=$min max=$max mean=${sum / samples} non_black_ratio=$nonBlack/$samples",
                )
                bitmap.recycle()
            }, ui)
        }, 250)
    }

    private fun buildUi() {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(12, 12, 12, 12)
        }
        val form = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        host = field("Agent host")
        port = field("Port").apply { setText("24443"); inputType = EditorInfo.TYPE_CLASS_NUMBER }
        password = field("Password").apply {
            inputType = EditorInfo.TYPE_CLASS_TEXT or EditorInfo.TYPE_TEXT_VARIATION_PASSWORD
            isSaveEnabled = false
        }
        fingerprint = field("TLS SPKI SHA-256 (64 hex)")
        form.addView(host)
        form.addView(port)
        form.addView(password)
        form.addView(fingerprint)
        val buttons = LinearLayout(this)
        connect = Button(this).apply { text = "Connect"; setOnClickListener { startConnection() } }
        disconnect = Button(this).apply { text = "Disconnect"; setOnClickListener { stopConnection() } }
        buttons.addView(connect, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        buttons.addView(disconnect, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        form.addView(buttons)
        localControl = CheckBox(this).apply {
            text = "Authorize local pointer/keyboard input"
            setOnCheckedChangeListener { _, checked -> DfyControllerNative.setControlAllowed(checked); updateControls() }
        }
        form.addView(localControl)
        qualityWidth = field("Max width (even)").apply { inputType = EditorInfo.TYPE_CLASS_NUMBER }
        qualityHeight = field("Max height (even)").apply { inputType = EditorInfo.TYPE_CLASS_NUMBER }
        qualityFps = field("FPS (1–120)").apply { inputType = EditorInfo.TYPE_CLASS_NUMBER }
        qualityBitrate = field("Bitrate kbps (100–100000)").apply { inputType = EditorInfo.TYPE_CLASS_NUMBER }
        form.addView(qualityWidth)
        form.addView(qualityHeight)
        form.addView(qualityFps)
        form.addView(qualityBitrate)
        val qualityButtons = LinearLayout(this)
        applyQuality = Button(this).apply { text = "Apply quality"; setOnClickListener { applyQualityLimit() } }
        resetQuality = Button(this).apply { text = "Reset to host"; setOnClickListener { resetQualityLimit() } }
        qualityButtons.addView(applyQuality, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        qualityButtons.addView(resetQuality, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        form.addView(qualityButtons)
        qualityView = textView().apply { text = "Video quality: waiting for Agent" }
        form.addView(qualityView)
        pinView = textView()
        permissionsView = textView()
        form.addView(pinView)
        form.addView(permissionsView)
        root.addView(ScrollView(this).apply { addView(form) }, LinearLayout.LayoutParams(310.dp, LinearLayout.LayoutParams.MATCH_PARENT))

        val viewer = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        videoFrame = FrameLayout(this)
        addVideoSurface()
        if (BuildConfig.DEBUG && intent.getBooleanExtra("dfy_capture_probe", false)) {
            videoFrame.addView(
                View(this).apply {
                    setBackgroundColor(0xFF00FF00.toInt())
                    elevation = 8f
                },
                FrameLayout.LayoutParams(160.dp, 120.dp).apply {
                    gravity = Gravity.TOP or Gravity.START
                    topMargin = 24.dp
                    leftMargin = 24.dp
                },
            )
        }
        viewer.addView(videoFrame, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
        stateView = textView().apply {
            textSize = 16f
            maxLines = 1
            ellipsize = android.text.TextUtils.TruncateAt.END
        }
        detailView = textView().apply {
            maxLines = 2
            ellipsize = android.text.TextUtils.TruncateAt.END
        }
        statsView = textView().apply {
            textSize = 12f
            maxLines = 2
            ellipsize = android.text.TextUtils.TruncateAt.END
        }
        viewer.addView(stateView)
        viewer.addView(detailView)
        val systemButtons = LinearLayout(this)
        back = Button(this).apply {
            text = "Back"
            maxLines = 1
            textSize = 12f
            setOnClickListener { DfyControllerNative.systemAction(DfyControllerNative.ACTION_BACK) }
        }
        home = Button(this).apply {
            text = "Home"
            maxLines = 1
            textSize = 12f
            setOnClickListener { DfyControllerNative.systemAction(DfyControllerNative.ACTION_HOME) }
        }
        systemButtons.addView(back, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        systemButtons.addView(home, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        viewer.addView(systemButtons)
        viewer.addView(statsView)
        root.addView(viewer, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        setContentView(root)
    }

    private fun addVideoSurface() {
        surfaceView = SurfaceView(this).apply {
            setOnTouchListener { _, event -> onTouch(event) }
            setOnGenericMotionListener { _, event -> onMotion(event) }
            setOnKeyListener { _, keyCode, event -> onKey(keyCode, event) }
            isFocusable = true
            isFocusableInTouchMode = true
        }
        videoFrame.addView(
            surfaceView,
            0,
            FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT),
        )
        surfaceView.holder.addCallback(this)
    }

    private fun field(label: String) = EditText(this).apply {
        hint = label
        isSingleLine = true
        importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO
    }

    private fun textView() = TextView(this).apply { setPadding(0, 6, 0, 6) }

    private val Int.dp: Int get() = (this * resources.displayMetrics.density).toInt()

    private fun startConnection() {
        val address = host.text.toString().trim()
        val parsedPort = port.text.toString().toIntOrNull()
        if (address.isEmpty() || parsedPort == null || parsedPort !in 1..65535) {
            Toast.makeText(this, "Enter a host and a valid port", Toast.LENGTH_SHORT).show()
            return
        }
        val pin = ViewerHelpers.normalizeFingerprint(fingerprint.text.toString())
        if (pin == null) {
            Toast.makeText(this, "Enter the agent's 64-character SPKI fingerprint", Toast.LENGTH_SHORT).show()
            return
        }
        val secret = password.text.toString()
        password.text.clear()
        if (secret.toByteArray(Charsets.UTF_8).size !in 16..1024) {
            Toast.makeText(this, "Password must be 16–1024 UTF-8 bytes", Toast.LENGTH_SHORT).show()
            return
        }
        stopConnection()
        state = "connecting"
        detail = address
        pinView.text = "Pin: ${ViewerHelpers.formatFingerprint(pin)}"
        try {
            DfyControllerNative.connect(address, parsedPort, secret, pin)
            DfyControllerNative.setControlAllowed(localControl.isChecked)
            updateControls()
        } catch (error: Exception) {
            state = "error"
            detail = error.message ?: "Connection failed"
            updateControls()
        }
    }

    private fun stopConnection() {
        decodedFrames.set(0)
        surfaceRecoveryCount = 0
        ui.removeCallbacks(longPress)
        ui.removeCallbacks(rebuildSurface)
        gesture.cancel().forEach { dispatch(listOf(it)) }
        gesturePoint = null
        DfyControllerNative.releaseInput()
        DfyControllerNative.disconnect()
        decoder.release()
        holderSurface()?.let { decoder.setSurface(it) }
        connected = false
        controlAllowed = false
        permissions = 0
        capabilities = emptySet()
        if (::stateView.isInitialized) {
            state = "disconnected"
            detail = ""
            updateControls()
        }
    }

    private fun holderSurface(): android.view.Surface? =
        if (::surfaceView.isInitialized && surfaceView.holder.surface?.isValid == true) surfaceView.holder.surface else null

    private fun onTouch(event: MotionEvent): Boolean {
        val action = event.actionMasked
        if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
            ui.removeCallbacks(longPress)
            val hadGesture = gesturePoint != null
            gesturePoint = null
            if (hadGesture) dispatch(gesture.end())
            return hadGesture || controlAllowed
        }
        if (!controlAllowed) return false
        val point = ViewerHelpers.normalizedPoint(
            event.x, event.y, surfaceView.width.toFloat(), surfaceView.height.toFloat(), videoWidth, videoHeight,
        )
        if (point == null) {
            if (action == MotionEvent.ACTION_MOVE && gesturePoint != null) {
                ui.removeCallbacks(longPress)
                gesturePoint = null
                dispatch(gesture.cancel())
                return true
            }
            return false
        }
        when (action) {
            MotionEvent.ACTION_DOWN -> {
                surfaceView.requestFocus()
                ui.removeCallbacks(longPress)
                gesturePoint = point
                dispatch(gesture.begin(event.x, event.y, point))
                ui.postDelayed(longPress, 600)
            }
            MotionEvent.ACTION_MOVE -> {
                if (gesturePoint == null) return false
                gesturePoint = point
                dispatch(gesture.move(event.x, event.y, point))
            }
        }
        return true
    }

    private fun onMotion(event: MotionEvent): Boolean {
        if (!controlAllowed || event.actionMasked != MotionEvent.ACTION_SCROLL) return false
        val dx = (-event.getAxisValue(MotionEvent.AXIS_HSCROLL)).toInt()
        val dy = (-event.getAxisValue(MotionEvent.AXIS_VSCROLL)).toInt()
        if (dx != 0 || dy != 0) DfyControllerNative.wheel(dx, dy)
        return true
    }

    private fun onKey(keyCode: Int, event: KeyEvent): Boolean {
        val usage = ViewerKeyMap.usage(keyCode, event.unicodeChar) ?: return false
        if (event.action == KeyEvent.ACTION_DOWN && controlAllowed) {
            DfyControllerNative.key(usage, true, ViewerKeyMap.modifiers(event.metaState))
        } else if (event.action == KeyEvent.ACTION_UP) {
            DfyControllerNative.key(usage, false, ViewerKeyMap.modifiers(event.metaState))
        }
        return controlAllowed || event.action == KeyEvent.ACTION_UP
    }

    private fun dispatch(actions: List<ViewerHelpers.Input>) {
        for (action in actions) when (action) {
            is ViewerHelpers.Input.Move -> DfyControllerNative.pointer(action.x, action.y)
            is ViewerHelpers.Input.Button -> DfyControllerNative.button(action.button, action.down, action.x, action.y)
        }
    }

    private fun showDecoderError(message: String) {
        detail = message
        if (connected && surfaceRecoveryCount < MAX_SURFACE_RECOVERIES) {
            surfaceRecoveryCount++
            detail = "$message; rebuilding video surface ($surfaceRecoveryCount/$MAX_SURFACE_RECOVERIES)"
            ui.postDelayed(rebuildSurface, SURFACE_RECOVERY_DELAY_MS)
        }
        detailView.text = detail
    }

    private fun rebuildVideoSurface() {
        if (!connected || !::videoFrame.isInitialized || isDestroyed) return
        Log.w("DfyH264Decoder", "rebuilding_output_surface attempt=$surfaceRecoveryCount")
        decoder.setSurface(null)
        videoFrame.removeView(surfaceView)
        surfaceView.holder.removeCallback(this)
        addVideoSurface()
        DfyControllerNative.requestKeyframe()
    }

    override fun onState(state: String, detail: String) {
        this.state = state
        this.detail = detail
        connected = state == "connected"
        updateControls()
    }

    override fun onFrame(data: ByteArray, ptsUs: Long, keyframe: Boolean): Boolean {
        val submitted = decoder.submit(data, ptsUs, keyframe)
        if (!submitted) {
            val now = System.currentTimeMillis()
            if (keyframe && now - lastKeyframeRequest > 1_000) {
                lastKeyframeRequest = now
                DfyControllerNative.requestKeyframe()
            }
        }
        return submitted
    }

    override fun onVideoConfig(width: Int, height: Int, fps: Int, bitrateKbps: Int) {
        videoWidth = width
        videoHeight = height
        decoder.setVideoConfig(width, height)
        qualityView.text = "Video quality: ${width}x${height}, $fps fps, $bitrateKbps kbps"
    }

    override fun onSession(permissions: Int, capabilities: Set<String>) {
        this.permissions = permissions
        this.capabilities = capabilities
        updateControls()
    }

    override fun onControlAllowed(allowed: Boolean) {
        controlAllowed = allowed
        updateControls()
    }

    private fun applyQualityLimit() {
        val width = qualityWidth.text.toString().toIntOrNull()
        val height = qualityHeight.text.toString().toIntOrNull()
        val fps = qualityFps.text.toString().toIntOrNull()
        val bitrate = qualityBitrate.text.toString().toIntOrNull()
        if (width == null || height == null || fps == null || bitrate == null ||
            width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 ||
            fps !in 1..120 || bitrate !in 100..100_000
        ) {
            Toast.makeText(this, "Use positive even dimensions, FPS 1–120 and bitrate 100–100000 kbps", Toast.LENGTH_LONG).show()
            return
        }
        try {
            if (DfyControllerNative.setQualityControl(width, height, fps, bitrate)) {
                qualityView.text = "Video quality: requesting ${width}x${height}, $fps fps, $bitrate kbps"
            } else {
                Toast.makeText(this, "Agent did not authorize this quality request", Toast.LENGTH_LONG).show()
            }
        } catch (error: IllegalArgumentException) {
            Toast.makeText(this, error.message, Toast.LENGTH_LONG).show()
        }
    }

    private fun resetQualityLimit() {
        try {
            if (DfyControllerNative.setQualityControl(0, 0, 0, 0)) {
                qualityView.text = "Video quality: restoring Agent startup ceiling"
            } else {
                Toast.makeText(this, "Agent did not authorize quality reset", Toast.LENGTH_LONG).show()
            }
        } catch (error: IllegalArgumentException) {
            Toast.makeText(this, error.message, Toast.LENGTH_LONG).show()
        }
    }

    private fun hostControl() = connected && permissions and DfyControllerNative.PERM_CONTROL != 0
    private fun canAdjustQuality() = hostControl() && DfyControllerNative.CAPABILITY_QUALITY_CONTROL in capabilities
    private fun canUseSystemActions() = connected &&
        DfyControllerNative.systemActionAuthorized(permissions, capabilities, localControl.isChecked)

    private fun updateControls() {
        if (!::stateView.isInitialized) return
        controlAllowed = connected && localControl.isChecked && hostControl()
        stateView.text = "State: $state"
        detailView.text = detail
        permissionsView.text = "Permissions: ${permissionNames()}\nCapabilities: ${if (capabilities.isEmpty()) "none" else capabilities.joinToString()}"
        connect.isEnabled = state != "connecting" && state != "connected"
        disconnect.isEnabled = state != "disconnected"
        remoteBack.isEnabled = canUseSystemActions()
        back.isEnabled = canUseSystemActions()
        home.isEnabled = canUseSystemActions()
        applyQuality.isEnabled = canAdjustQuality()
        resetQuality.isEnabled = canAdjustQuality()
        surfaceView.keepScreenOn = connected
    }

    private fun permissionNames(): String {
        val names = mutableListOf<String>()
        if (permissions and DfyControllerNative.PERM_VIEW != 0) names += "view"
        if (permissions and DfyControllerNative.PERM_CONTROL != 0) names += "control"
        if (permissions and DfyControllerNative.PERM_SYSTEM_ACTION != 0) names += "system-action"
        return if (names.isEmpty()) "none" else names.joinToString()
    }

    private fun updateStats() {
        if (!::statsView.isInitialized) return
        try {
            val stats = JSONObject(DfyControllerNative.stats())
            statsView.text = "RX ${stats.optLong("packets_recv")} packets / ${stats.optLong("bytes_recv")} bytes | " +
                "frames ${stats.optLong("frames_assembled")} assembled, ${stats.optLong("frames_submitted")} submitted | " +
                "key ${stats.optLong("key_packets")} | malformed ${stats.optLong("malformed")} | " +
                "dropped ${stats.optLong("dropped_frames")} | input ${stats.optLong("input_moves") + stats.optLong("input_events")} | " +
                "rejected ${stats.optLong("rejected_input")} | quality ${stats.optLong("quality_controls")} sent, ${stats.optLong("quality_control_errors")} errors | RTT ${stats.optLong("rtt_ms")} ms"
        } catch (_: Exception) {
            statsView.text = "Statistics unavailable"
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        Log.i("DfyViewerSurface", "surface_created valid=${holder.surface.isValid} same=${surfaceView.holder === holder}")
        if (::surfaceView.isInitialized && surfaceView.holder === holder) decoder.setSurface(holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        Log.i("DfyViewerSurface", "surface_changed size=${width}x$height valid=${holder.surface.isValid}")
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        Log.i("DfyViewerSurface", "surface_destroyed same=${surfaceView.holder === holder}")
        if (::surfaceView.isInitialized && surfaceView.holder === holder) decoder.setSurface(null)
    }

    override fun onResume() {
        super.onResume()
        ui.post(statsPoll)
    }

    override fun onPause() {
        ui.removeCallbacks(statsPoll)
        ui.removeCallbacks(longPress)
        ui.removeCallbacks(rebuildSurface)
        DfyControllerNative.releaseInput()
        super.onPause()
    }

    override fun onStop() {
        stopConnection()
        super.onStop()
    }

    override fun onDestroy() {
        ui.removeCallbacks(statsPoll)
        ui.removeCallbacks(longPress)
        ui.removeCallbacks(rebuildSurface)
        DfyControllerNative.setListener(null)
        decoder.release()
        DfyControllerNative.disconnect()
        super.onDestroy()
    }

    private companion object {
        const val MAX_SURFACE_RECOVERIES = 3
        const val SURFACE_RECOVERY_DELAY_MS = 250L
    }
}
