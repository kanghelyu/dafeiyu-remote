package remote.dafeiyu

import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.Typeface
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Gravity
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PixelCopy
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
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
    private lateinit var statsDetailView: TextView
    private lateinit var statsToggle: Button
    private lateinit var surfaceView: SurfaceView
    private lateinit var videoFrame: FrameLayout
    private lateinit var videoOverlay: LinearLayout
    private lateinit var overlayTitle: TextView
    private lateinit var overlayBody: TextView
    private lateinit var stateBadge: TextView
    private lateinit var root: FrameLayout
    private lateinit var portraitRoot: LinearLayout
    private lateinit var wideRoot: LinearLayout
    private lateinit var videoShell: LinearLayout
    private lateinit var settingsScroll: ScrollView
    private lateinit var form: LinearLayout
    private lateinit var decoder: DfyH264Decoder

    private val ui = Handler(Looper.getMainLooper())
    private val gesture = ViewerHelpers.TouchGesture()
    private var gesturePoint: Pair<Int, Int>? = null
    private var videoWidth = 0
    private var videoHeight = 0
    private var permissions = 0
    private var capabilities: Set<String> = emptySet()
    private var connected = false
    private var hasFrame = false
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
        UiTheme.configureWindow(this)
        buildUi()
        onBackPressedDispatcher.addCallback(this, remoteBack)
        decoder = DfyH264Decoder(
            onError = { message -> ui.post { showDecoderError(message) } },
            onDimensions = { width, height -> ui.post { videoWidth = width; videoHeight = height } },
            onFramePresented = {
                inspectDecodedPixels()
                if (!hasFrame) {
                    hasFrame = true
                    ui.post { renderVideoState() }
                }
            },
        )
        DfyControllerNative.setListener(this)
        updateControls()
        updateStats()
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        updateResponsiveLayout()
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
        root = FrameLayout(this).apply { setBackgroundColor(UiTheme.background) }
        UiTheme.applyInsets(root)
        portraitRoot = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        wideRoot = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        videoShell = buildVideoShell()
        settingsScroll = ScrollView(this).apply {
            isFillViewport = true
            clipToPadding = false
            setBackgroundColor(UiTheme.background)
        }
        form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val pad = 16.dp
            setPadding(pad, 4.dp, pad, 28.dp)
        }
        settingsScroll.addView(form, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        addVideoSurface()
        buildConnectionCard()
        buildControlCard()
        buildQualityCard()
        buildStatsCard()
        updateResponsiveLayout()
        setContentView(root)
    }

    private fun buildVideoShell(): LinearLayout = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setBackgroundColor(UiTheme.background)
        val header = LinearLayout(this@ViewerActivity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(16.dp, 8.dp, 16.dp, 10.dp)
        }
        val titles = LinearLayout(this@ViewerActivity).apply { orientation = LinearLayout.VERTICAL }
        titles.addView(UiTheme.title(this@ViewerActivity, getString(R.string.viewer_title)))
        titles.addView(UiTheme.body(this@ViewerActivity, getString(R.string.viewer_subtitle), muted = true).apply {
            textSize = 12f
            setPadding(0, 2.dp, 0, 0)
        })
        header.addView(titles, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        stateBadge = UiTheme.badge(this@ViewerActivity, getString(R.string.state_disconnected))
        header.addView(stateBadge)
        addView(header)
        videoFrame = FrameLayout(this@ViewerActivity).apply {
            setBackgroundColor(UiTheme.video)
            contentDescription = getString(R.string.viewer_title)
        }
        addView(videoFrame, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
        if (BuildConfig.DEBUG && intent.getBooleanExtra("dfy_capture_probe", false)) {
            videoFrame.addView(
                View(this@ViewerActivity).apply {
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
        videoOverlay = LinearLayout(this@ViewerActivity).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            val pad = 28.dp
            setPadding(pad, pad, pad, pad)
            setBackgroundColor(0xE6101923.toInt())
            isClickable = true
            isFocusable = false
        }
        overlayTitle = TextView(this@ViewerActivity).apply {
            textSize = 19f
            setTextColor(UiTheme.videoText)
            typeface = Typeface.create("sans-serif", Typeface.BOLD)
            gravity = Gravity.CENTER
        }
        overlayBody = TextView(this@ViewerActivity).apply {
            textSize = 14f
            setTextColor(UiTheme.videoMuted)
            gravity = Gravity.CENTER
            setPadding(0, 10.dp, 0, 0)
        }
        videoOverlay.addView(overlayTitle)
        videoOverlay.addView(overlayBody)
        videoFrame.addView(videoOverlay, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        stateView = UiTheme.body(this@ViewerActivity).apply {
            textSize = 15f
            setPadding(16.dp, 12.dp, 16.dp, 0)
        }
        addView(stateView)
        detailView = UiTheme.body(this@ViewerActivity, muted = true).apply {
            textSize = 12f
            setPadding(16.dp, 4.dp, 16.dp, 4.dp)
        }
        addView(detailView)
        val actions = LinearLayout(this@ViewerActivity).apply { orientation = LinearLayout.HORIZONTAL }
        back = UiTheme.button(this@ViewerActivity, getString(R.string.viewer_back)).apply {
            setOnClickListener { DfyControllerNative.systemAction(DfyControllerNative.ACTION_BACK) }
        }
        home = UiTheme.button(this@ViewerActivity, getString(R.string.viewer_home)).apply {
            setOnClickListener { DfyControllerNative.systemAction(DfyControllerNative.ACTION_HOME) }
        }
        actions.addView(back, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply { marginEnd = 10.dp })
        actions.addView(home, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        addView(actions, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            leftMargin = 16.dp; rightMargin = 16.dp; topMargin = 4.dp; bottomMargin = 12.dp
        })
    }

    @Suppress("ClickableViewAccessibility") // The remote canvas sends pointer input, not UI clicks.
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

    private fun buildConnectionCard() {
        val card = UiTheme.card(this)
        card.addView(UiTheme.sectionTitle(this, getString(R.string.viewer_connection_title)))
        val addressRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val hostColumn = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        hostColumn.addView(UiTheme.label(this, getString(R.string.viewer_host_label)))
        host = UiTheme.field(this, getString(R.string.viewer_host_hint)).apply {
            contentDescription = getString(R.string.viewer_host_label)
            inputType = EditorInfo.TYPE_CLASS_TEXT or EditorInfo.TYPE_TEXT_VARIATION_URI
        }
        hostColumn.addView(host, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 6.dp })
        addressRow.addView(hostColumn, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply { topMargin = 12.dp; marginEnd = 12.dp })
        val portColumn = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        portColumn.addView(UiTheme.label(this, getString(R.string.viewer_port_label)))
        port = UiTheme.field(this, "1–65535").apply {
            setText(getString(R.string.default_port))
            inputType = EditorInfo.TYPE_CLASS_NUMBER
            contentDescription = getString(R.string.viewer_port_label)
        }
        portColumn.addView(port, LinearLayout.LayoutParams(112.dp, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 6.dp })
        addressRow.addView(portColumn, LinearLayout.LayoutParams(LinearLayout.LayoutParams.WRAP_CONTENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 12.dp })
        card.addView(addressRow)
        addLabeledField(card, getString(R.string.viewer_password_label), getString(R.string.viewer_password_hint)).also { field ->
            password = field
            field.inputType = EditorInfo.TYPE_CLASS_TEXT or EditorInfo.TYPE_TEXT_VARIATION_PASSWORD
            field.isSaveEnabled = false
        }
        addLabeledField(card, getString(R.string.viewer_fingerprint_label), getString(R.string.viewer_fingerprint_hint)).also { field ->
            fingerprint = field
            field.typeface = Typeface.MONOSPACE
            field.textSize = 14f
        }
        val buttons = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        connect = UiTheme.button(this, getString(R.string.connect), filled = true).apply { setOnClickListener { startConnection() } }
        disconnect = UiTheme.button(this, getString(R.string.disconnect)).apply { setOnClickListener { stopConnection() } }
        buttons.addView(connect, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply { marginEnd = 10.dp })
        buttons.addView(disconnect, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        card.addView(buttons, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 14.dp })
        form.addView(card, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { bottomMargin = 12.dp })
    }

    private fun buildControlCard() {
        val card = UiTheme.card(this)
        card.addView(UiTheme.sectionTitle(this, getString(R.string.viewer_control_title)))
        localControl = CheckBox(this).apply {
            text = getString(R.string.viewer_local_control)
            textSize = 15f
            setTextColor(UiTheme.text)
            minHeight = 52.dp
            buttonTintList = android.content.res.ColorStateList.valueOf(UiTheme.primary)
            setOnCheckedChangeListener { _, checked -> DfyControllerNative.setControlAllowed(checked); updateControls() }
        }
        card.addView(localControl, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 6.dp })
        card.addView(UiTheme.body(this, getString(R.string.viewer_control_hint), muted = true))
        pinView = UiTheme.body(this, muted = true).apply { typeface = Typeface.MONOSPACE; textSize = 12f }
        card.addView(pinView, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 12.dp })
        permissionsView = UiTheme.body(this, muted = true).apply { textSize = 12f }
        card.addView(permissionsView)
        form.addView(card, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { bottomMargin = 12.dp })
    }

    private fun buildQualityCard() {
        val card = UiTheme.card(this)
        card.addView(UiTheme.sectionTitle(this, getString(R.string.viewer_quality_title)))
        card.addView(UiTheme.body(this, getString(R.string.viewer_quality_subtitle), muted = true).apply { setPadding(0, 6.dp, 0, 0) })
        qualityWidth = qualityField(getString(R.string.viewer_max_width))
        qualityHeight = qualityField(getString(R.string.viewer_max_height))
        qualityFps = qualityField(getString(R.string.viewer_fps))
        qualityBitrate = qualityField(getString(R.string.viewer_bitrate))
        addSettingRow(card, qualityWidth, qualityHeight)
        addSettingRow(card, qualityFps, qualityBitrate)
        val buttons = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        applyQuality = UiTheme.button(this, getString(R.string.apply), filled = true).apply { setOnClickListener { applyQualityLimit() } }
        resetQuality = UiTheme.button(this, getString(R.string.reset)).apply { setOnClickListener { resetQualityLimit() } }
        buttons.addView(applyQuality, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply { marginEnd = 10.dp })
        buttons.addView(resetQuality, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        card.addView(buttons, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 14.dp })
        qualityView = UiTheme.body(this, getString(R.string.viewer_quality_waiting), muted = true).apply {
            textSize = 12f
            setPadding(0, 12.dp, 0, 0)
        }
        card.addView(qualityView)
        form.addView(card, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { bottomMargin = 12.dp })
    }

    private fun buildStatsCard() {
        val card = UiTheme.card(this)
        card.addView(UiTheme.sectionTitle(this, getString(R.string.statistics)))
        statsView = UiTheme.body(this, muted = true).apply { textSize = 13f; setPadding(0, 8.dp, 0, 0) }
        card.addView(statsView)
        statsDetailView = UiTheme.body(this, muted = true).apply {
            typeface = Typeface.MONOSPACE
            textSize = 11f
            setTextIsSelectable(true)
            visibility = View.GONE
        }
        card.addView(statsDetailView, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 10.dp })
        statsToggle = UiTheme.button(this, getString(R.string.statistics_detail)).apply {
            setOnClickListener {
                val expanded = statsDetailView.visibility == View.VISIBLE
                statsDetailView.visibility = if (expanded) View.GONE else View.VISIBLE
                text = getString(if (expanded) R.string.statistics_detail else R.string.statistics_collapse)
            }
        }
        card.addView(statsToggle, LinearLayout.LayoutParams(LinearLayout.LayoutParams.WRAP_CONTENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 8.dp })
        form.addView(card)
    }

    private fun addLabeledField(parent: LinearLayout, label: String, hint: String): EditText {
        parent.addView(UiTheme.label(this, label), LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 12.dp })
        return UiTheme.field(this, hint).apply {
            contentDescription = label
            parent.addView(this, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 6.dp })
        }
    }

    private fun qualityField(label: String) = UiTheme.field(this, label).apply {
        contentDescription = label
        inputType = EditorInfo.TYPE_CLASS_NUMBER
    }

    private fun addSettingRow(parent: LinearLayout, first: EditText, second: EditText) {
        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        row.addView(first, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        row.addView(second, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply { marginStart = 12.dp })
        parent.addView(row, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply { topMargin = 12.dp })
    }

    private fun updateResponsiveLayout() {
        if (!::root.isInitialized) return
        val metrics = resources.displayMetrics
        val widthDp = metrics.widthPixels / metrics.density
        val heightDp = metrics.heightPixels / metrics.density
        val wide = ViewerHelpers.viewerLayoutMode(widthDp, heightDp) == ViewerHelpers.ViewerLayout.WIDE
        portraitRoot.removeAllViews()
        wideRoot.removeAllViews()
        root.removeAllViews()
        if (wide) {
            val settingsWidth = (widthDp * 0.40f).toInt().coerceIn(300, 440).dp
            wideRoot.addView(videoShell, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
            wideRoot.addView(settingsScroll, LinearLayout.LayoutParams(settingsWidth, LinearLayout.LayoutParams.MATCH_PARENT))
            root.addView(wideRoot, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        } else {
            val videoHeight = (heightDp * 0.47f).toInt().coerceIn(340, 480).dp
            portraitRoot.addView(videoShell, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, videoHeight))
            portraitRoot.addView(settingsScroll, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
            root.addView(portraitRoot, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        }
    }

    private fun startConnection() {
        listOf(host, port, fingerprint, password).forEach { it.error = null }
        val address = host.text.toString().trim()
        val parsedPort = port.text.toString().toIntOrNull()
        if (address.isEmpty() || parsedPort == null || parsedPort !in 1..65535) {
            host.error = getString(R.string.viewer_connection_error)
            host.requestFocus()
            return
        }
        val pin = ViewerHelpers.normalizeFingerprint(fingerprint.text.toString())
        if (pin == null) {
            fingerprint.error = getString(R.string.viewer_fingerprint_error)
            fingerprint.requestFocus()
            return
        }
        val secret = password.text.toString()
        if (secret.toByteArray(Charsets.UTF_8).size !in 16..1024) {
            password.error = getString(R.string.viewer_password_error)
            password.requestFocus()
            return
        }
        password.text.clear()
        stopConnection()
        state = "connecting"
        detail = ""
        pinView.text = getString(R.string.viewer_pin, ViewerHelpers.formatFingerprint(pin))
        try {
            DfyControllerNative.connect(address, parsedPort, secret, pin)
            DfyControllerNative.setControlAllowed(localControl.isChecked)
            updateControls()
        } catch (error: Exception) {
            state = "error"
            detail = error.message ?: getString(R.string.viewer_connect_failed)
            updateControls()
        }
    }

    private fun stopConnection() {
        decodedFrames.set(0)
        hasFrame = false
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
            qualityView.text = getString(R.string.viewer_quality_waiting)
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
            detail = getString(R.string.viewer_recovering, message, surfaceRecoveryCount, MAX_SURFACE_RECOVERIES)
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
        if (!connected) hasFrame = false
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
        qualityView.text = getString(R.string.viewer_quality_value, width, height, fps, bitrateKbps)
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
            qualityWidth.error = getString(R.string.viewer_quality_error)
            qualityWidth.requestFocus()
            return
        }
        listOf(qualityWidth, qualityHeight, qualityFps, qualityBitrate).forEach { it.error = null }
        try {
            if (DfyControllerNative.setQualityControl(width, height, fps, bitrate)) {
                qualityView.text = getString(R.string.viewer_quality_requesting, width, height, fps, bitrate)
            } else {
                Toast.makeText(this, R.string.viewer_quality_denied, Toast.LENGTH_LONG).show()
            }
        } catch (error: IllegalArgumentException) {
            qualityWidth.error = error.message
            qualityWidth.requestFocus()
        }
    }

    private fun resetQualityLimit() {
        try {
            if (DfyControllerNative.setQualityControl(0, 0, 0, 0)) {
                qualityView.text = getString(R.string.viewer_quality_resetting)
            } else {
                Toast.makeText(this, R.string.viewer_quality_denied, Toast.LENGTH_LONG).show()
            }
        } catch (error: IllegalArgumentException) {
            qualityWidth.error = error.message
            qualityWidth.requestFocus()
        }
    }

    private fun hostControl() = connected && permissions and DfyControllerNative.PERM_CONTROL != 0
    private fun canAdjustQuality() = hostControl() && DfyControllerNative.CAPABILITY_QUALITY_CONTROL in capabilities
    private fun canUseSystemActions() = connected &&
        DfyControllerNative.systemActionAuthorized(permissions, capabilities, localControl.isChecked)

    private fun updateControls() {
        if (!::stateView.isInitialized) return
        controlAllowed = connected && localControl.isChecked && hostControl()
        val (label, color) = when (state) {
            "connected" -> R.string.state_connected to UiTheme.success
            "connecting" -> R.string.state_connecting to UiTheme.warning
            "error" -> R.string.state_error to UiTheme.error
            else -> R.string.state_disconnected to UiTheme.muted
        }
        stateBadge.text = getString(label)
        UiTheme.setBadge(stateBadge, color)
        stateView.text = getString(label)
        stateView.setTextColor(color)
        detailView.text = detail
        detailView.setTextColor(if (state == "error") UiTheme.error else UiTheme.muted)
        permissionsView.text = getString(
            R.string.viewer_permissions,
            getString(if (permissions and DfyControllerNative.PERM_VIEW != 0) R.string.enabled else R.string.disabled),
            getString(if (permissions and DfyControllerNative.PERM_CONTROL != 0) R.string.enabled else R.string.disabled),
            getString(if (permissions and DfyControllerNative.PERM_SYSTEM_ACTION != 0) R.string.enabled else R.string.disabled),
        )
        connect.isEnabled = state != "connecting" && state != "connected"
        disconnect.isEnabled = state != "disconnected"
        remoteBack.isEnabled = canUseSystemActions()
        back.isEnabled = canUseSystemActions()
        home.isEnabled = canUseSystemActions()
        applyQuality.isEnabled = canAdjustQuality()
        resetQuality.isEnabled = canAdjustQuality()
        surfaceView.keepScreenOn = connected
        renderVideoState()
    }

    private fun renderVideoState() {
        if (!::videoOverlay.isInitialized) return
        val copy = when {
            state == "connecting" -> R.string.viewer_connecting_title to R.string.viewer_connecting_body
            state == "error" -> R.string.viewer_error_title to R.string.viewer_error_body
            connected && !hasFrame -> R.string.viewer_waiting_title to R.string.viewer_waiting_body
            connected -> null
            state == "disconnected" && detail.isNotBlank() -> R.string.viewer_disconnected_title to R.string.viewer_disconnected_body
            else -> R.string.viewer_empty_title to R.string.viewer_empty_body
        }
        videoOverlay.visibility = if (copy == null) View.GONE else View.VISIBLE
        if (copy != null) {
            overlayTitle.setText(copy.first)
            overlayBody.setText(copy.second)
        }
    }

    private fun updateStats() {
        if (!::statsView.isInitialized) return
        try {
            val stats = JSONObject(DfyControllerNative.stats())
            val packets = stats.optLong("packets_recv")
            val bytes = stats.optLong("bytes_recv")
            val assembled = stats.optLong("frames_assembled")
            val submitted = stats.optLong("frames_submitted")
            val key = stats.optLong("key_packets")
            val malformed = stats.optLong("malformed")
            val dropped = stats.optLong("dropped_frames")
            val moves = stats.optLong("input_moves")
            val inputEvents = stats.optLong("input_events")
            val rejected = stats.optLong("rejected_input")
            val qualitySent = stats.optLong("quality_controls")
            val qualityErrors = stats.optLong("quality_control_errors")
            val rtt = stats.optLong("rtt_ms")
            statsView.text = getString(R.string.viewer_statistics, packets, submitted, rtt, dropped)
            statsDetailView.text = getString(
                R.string.viewer_statistics_detail, packets, bytes, assembled, submitted, key,
                malformed, dropped, rejected, moves + inputEvents, qualitySent, qualityErrors, rtt,
            )
        } catch (_: Exception) {
            statsView.text = getString(R.string.statistics_unavailable)
            statsDetailView.text = ""
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

    override fun onDestroy() {
        ui.removeCallbacks(statsPoll)
        ui.removeCallbacks(longPress)
        ui.removeCallbacks(rebuildSurface)
        DfyControllerNative.setListener(null)
        decoder.release()
        DfyControllerNative.disconnect()
        super.onDestroy()
    }

    private val Int.dp: Int get() = (this * resources.displayMetrics.density).toInt()

    private companion object {
        const val MAX_SURFACE_RECOVERIES = 3
        const val SURFACE_RECOVERY_DELAY_MS = 250L
    }
}
