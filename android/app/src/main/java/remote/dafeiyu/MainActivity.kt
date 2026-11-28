package remote.dafeiyu

import android.Manifest
import android.content.Intent
import android.content.res.Configuration
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.activity.ComponentActivity

class MainActivity : ComponentActivity() {
    private lateinit var passwordInput: EditText
    private lateinit var portInput: EditText
    private lateinit var maxWidthInput: EditText
    private lateinit var maxHeightInput: EditText
    private lateinit var fpsInput: EditText
    private lateinit var bitrateInput: EditText
    private lateinit var allowInput: CheckBox
    private lateinit var startButton: Button
    private lateinit var stopButton: Button
    private lateinit var accessibilityButton: Button
    private lateinit var statusBadge: TextView
    private lateinit var statusView: TextView
    private lateinit var endpointView: TextView
    private lateinit var statsView: TextView
    private lateinit var content: LinearLayout
    private val handler = Handler(Looper.getMainLooper())
    private var pendingStart = false
    private var pendingPassword: CharArray? = null
    private var pendingConfiguration: IntArray? = null
    private var displayState = "stopped" to ""

    private val stateListener: (String, String) -> Unit = { state, detail ->
        runOnUiThread {
            displayState = state to detail
            renderState()
        }
    }

    private val projectionLauncher = registerForActivityResult(
        androidx.activity.result.contract.ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == RESULT_OK && result.data != null) {
            if (!pendingStart || pendingPassword == null || pendingConfiguration == null) {
                clearPendingStart()
                val message = getString(R.string.host_start_permission_lost)
                DfyNative.reportLocal("stopped", message)
                Toast.makeText(this, message, Toast.LENGTH_LONG).show()
            } else {
                startService(result.resultCode, result.data!!)
            }
        } else {
            clearPendingStart()
            DfyNative.reportLocal("stopped", getString(R.string.host_projection_denied))
        }
    }

    private val notificationLauncher = registerForActivityResult(
        androidx.activity.result.contract.ActivityResultContracts.RequestPermission()
    ) { beginProjection() }

    private val statsRefresh = object : Runnable {
        override fun run() {
            statsView.text = runCatching { DfyNative.stats() }.getOrDefault(getString(R.string.statistics_unavailable))
            handler.postDelayed(this, 1_000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        UiTheme.configureWindow(this)
        setContentView(buildView())
        DfyNative.addListener(stateListener)
        displayState = RemoteService.currentState
        renderState()
    }

    override fun onResume() {
        super.onResume()
        renderGestureState()
        handler.post(statsRefresh)
    }

    override fun onPause() {
        handler.removeCallbacks(statsRefresh)
        super.onPause()
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        content.post { fitContentWidth() }
    }

    override fun onDestroy() {
        clearPendingStart()
        DfyNative.removeListener(stateListener)
        super.onDestroy()
    }

    private fun buildView(): View {
        val root = FrameLayout(this).apply { setBackgroundColor(UiTheme.background) }
        UiTheme.applyInsets(root)
        content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val pad = 20.dp
            setPadding(pad, 12.dp, pad, 32.dp)
        }
        val scroll = ScrollView(this).apply {
            isFillViewport = true
            clipToPadding = false
            addView(content, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.WRAP_CONTENT))
        }
        root.addView(scroll, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        root.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> fitContentWidth() }

        content.addView(header())
        content.addView(roleCard())
        content.addView(privacyCard())
        content.addView(sessionCard())
        content.addView(qualityCard())
        content.addView(gestureCard())
        content.addView(statusCard())
        content.post { fitContentWidth() }
        return root
    }

    private fun fitContentWidth() {
        if (!::content.isInitialized) return
        val maxWidth = 760.dp
        val available = (content.parent as? ScrollView)?.width ?: return
        val params = content.layoutParams as FrameLayout.LayoutParams
        params.width = minOf(available, maxWidth)
        params.gravity = Gravity.CENTER_HORIZONTAL
        content.layoutParams = params
    }

    private fun header(): LinearLayout = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(4.dp, 12.dp, 4.dp, 20.dp)
        addView(UiTheme.title(this@MainActivity, getString(R.string.host_title), large = true))
        addView(UiTheme.body(this@MainActivity, getString(R.string.host_subtitle), muted = true).apply {
            setPadding(0, 8.dp, 0, 0)
        })
    }

    private fun roleCard(): View = UiTheme.card(this).apply {
        addView(UiTheme.sectionTitle(this@MainActivity, getString(R.string.host_viewer_title)))
        addView(UiTheme.body(this@MainActivity, getString(R.string.host_viewer_body), muted = true).apply {
            setPadding(0, 8.dp, 0, 0)
        })
        addView(UiTheme.button(this@MainActivity, getString(R.string.host_viewer_action), filled = true).apply {
            setOnClickListener { startActivity(Intent(this@MainActivity, ViewerActivity::class.java)) }
        }, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            topMargin = 14.dp
        })
    }

    private fun privacyCard(): View = UiTheme.card(this).apply {
        addView(UiTheme.sectionTitle(this@MainActivity, getString(R.string.host_privacy_title)))
        addView(UiTheme.body(this@MainActivity, getString(R.string.host_privacy_body), muted = true).apply {
            setPadding(0, 8.dp, 0, 0)
        })
        val start = UiTheme.shape(UiTheme.accent, 4.dp.toFloat())
        addView(View(this@MainActivity).apply {
            layoutParams = LinearLayout.LayoutParams(56.dp, 4.dp).apply { topMargin = 14.dp }
            background = start
        })
    }

    private fun sessionCard(): View = UiTheme.card(this).apply {
        addView(UiTheme.sectionTitle(this@MainActivity, getString(R.string.host_session_title)))
        addView(UiTheme.label(this@MainActivity, getString(R.string.host_password_label)).apply {
            setPadding(0, 16.dp, 0, 6.dp)
        })
        passwordInput = UiTheme.field(this@MainActivity, getString(R.string.host_password_hint)).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            contentDescription = getString(R.string.host_password_label)
            isSaveEnabled = false
        }
        addView(passwordInput, fieldParams())
        val portRow = LinearLayout(this@MainActivity).apply { orientation = LinearLayout.HORIZONTAL }
        portRow.addView(
            UiTheme.label(this@MainActivity, getString(R.string.host_port_label)).apply {
                gravity = Gravity.CENTER_VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        portInput = UiTheme.field(this@MainActivity, "1–65535").apply {
            setText(String.format(java.util.Locale.ROOT, "%d", RemoteService.DEFAULT_PORT))
            inputType = InputType.TYPE_CLASS_NUMBER
            contentDescription = getString(R.string.host_port_label)
            gravity = Gravity.END
        }
        portRow.addView(portInput, LinearLayout.LayoutParams(144.dp, LinearLayout.LayoutParams.WRAP_CONTENT))
        addView(portRow, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            topMargin = 14.dp
        })
        val buttons = LinearLayout(this@MainActivity).apply { orientation = LinearLayout.HORIZONTAL }
        startButton = UiTheme.button(this@MainActivity, getString(R.string.start), filled = true).apply {
            setOnClickListener { requestStart() }
        }
        stopButton = UiTheme.button(this@MainActivity, getString(R.string.stop)).apply {
            setOnClickListener { stopRemote() }
        }
        buttons.addView(startButton, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply { marginEnd = 10.dp })
        buttons.addView(stopButton, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        addView(buttons, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            topMargin = 16.dp
        })
    }

    private fun qualityCard(): View = UiTheme.card(this).apply {
        addView(UiTheme.sectionTitle(this@MainActivity, getString(R.string.host_quality_title)))
        addView(UiTheme.body(this@MainActivity, getString(R.string.host_quality_subtitle), muted = true).apply {
            setPadding(0, 6.dp, 0, 4.dp)
        })
        maxWidthInput = settingField(getString(R.string.host_max_width_label), RemoteService.DEFAULT_MAX_WIDTH)
        maxHeightInput = settingField(getString(R.string.host_max_height_label), RemoteService.DEFAULT_MAX_HEIGHT)
        fpsInput = settingField(getString(R.string.host_fps_label), RemoteService.DEFAULT_FPS)
        bitrateInput = settingField(getString(R.string.host_bitrate_label), RemoteService.DEFAULT_BITRATE_KBPS)
        addSettingRow(this, maxWidthInput, maxHeightInput)
        addSettingRow(this, fpsInput, bitrateInput)
    }

    private fun addSettingRow(parent: LinearLayout, first: EditText, second: EditText) {
        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val params = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
        first.layoutParams = LinearLayout.LayoutParams(params)
        second.layoutParams = LinearLayout.LayoutParams(params).apply { marginStart = 12.dp }
        row.addView(first)
        row.addView(second)
        parent.addView(row, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            topMargin = 12.dp
        })
    }

    private fun gestureCard(): View = UiTheme.card(this).apply {
        addView(UiTheme.sectionTitle(this@MainActivity, getString(R.string.host_gesture_title)))
        allowInput = CheckBox(this@MainActivity).apply {
            text = getString(R.string.host_gesture_toggle)
            textSize = 15f
            setTextColor(UiTheme.text)
            minHeight = 52.dp
            buttonTintList = android.content.res.ColorStateList.valueOf(UiTheme.primary)
        }
        addView(allowInput, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            topMargin = 8.dp
        })
        addView(UiTheme.body(this@MainActivity, getString(R.string.host_gesture_body), muted = true))
        accessibilityButton = UiTheme.button(this@MainActivity, getString(R.string.host_gesture_manage)).apply {
            setOnClickListener {
                startActivity(Intent(android.provider.Settings.ACTION_ACCESSIBILITY_SETTINGS))
            }
        }
        addView(accessibilityButton, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            topMargin = 12.dp
        })
    }

    private fun statusCard(): View = UiTheme.card(this).apply {
        val heading = LinearLayout(this@MainActivity).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL }
        heading.addView(UiTheme.sectionTitle(this@MainActivity, getString(R.string.host_status_title)), LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        statusBadge = UiTheme.badge(this@MainActivity, getString(R.string.state_stopped))
        heading.addView(statusBadge)
        addView(heading)
        statusView = UiTheme.body(this@MainActivity).apply {
            textSize = 16f
            setPadding(0, 14.dp, 0, 0)
        }
        addView(statusView)
        endpointView = UiTheme.body(this@MainActivity, muted = true).apply {
            typeface = android.graphics.Typeface.MONOSPACE
            textSize = 12f
            setTextIsSelectable(true)
            setPadding(0, 12.dp, 0, 0)
        }
        addView(endpointView)
        addView(UiTheme.label(this@MainActivity, getString(R.string.statistics)).apply { setPadding(0, 18.dp, 0, 6.dp) })
        statsView = UiTheme.body(this@MainActivity, muted = true).apply {
            typeface = android.graphics.Typeface.MONOSPACE
            textSize = 12f
            setTextIsSelectable(true)
        }
        addView(statsView)
    }

    private fun requestStart() {
        if (RemoteService.isRunning()) return
        clearFieldErrors()
        val password = passwordInput.text.toString()
        if (password.toByteArray(Charsets.UTF_8).size !in 6..1024 || password.isBlank()) {
            passwordInput.error = getString(R.string.host_password_error)
            passwordInput.requestFocus()
            return
        }
        val port = portInput.text.toString().toIntOrNull()
        if (port == null || port !in 1..65_535) {
            portInput.error = getString(R.string.host_port_error)
            portInput.requestFocus()
            return
        }
        val maxWidth = maxWidthInput.text.toString().toIntOrNull()
        val maxHeight = maxHeightInput.text.toString().toIntOrNull()
        val fps = fpsInput.text.toString().toIntOrNull()
        val bitrateKbps = bitrateInput.text.toString().toIntOrNull()
        val qualityWidth = maxWidth ?: return qualityError()
        val qualityHeight = maxHeight ?: return qualityError()
        val qualityFps = fps ?: return qualityError()
        val qualityBitrate = bitrateKbps ?: return qualityError()
        val (sourceWidth, sourceHeight) = sourceDimensions()
        val fitsSource = runCatching {
            EncoderSettings.fitWithin(
                sourceWidth, sourceHeight, qualityWidth, qualityHeight, qualityFps, qualityBitrate
            )
        }.isSuccess
        if (!validateHostQuality(maxWidth, maxHeight, fps, bitrateKbps) || !fitsSource) {
            qualityError()
            return
        }
        if (allowInput.isChecked && !RemoteAccessibilityService.isEnabled(this)) {
            allowInput.isChecked = false
            Toast.makeText(this, R.string.host_gesture_required, Toast.LENGTH_LONG).show()
            return
        }
        pendingPassword = password.toCharArray()
        pendingConfiguration = intArrayOf(
            port, qualityWidth, qualityHeight, qualityFps, qualityBitrate
        )
        pendingStart = true
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            notificationLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            beginProjection()
        }
        passwordInput.text.clear()
    }

    private fun qualityError() {
        val message = getString(R.string.host_quality_error)
        listOf(maxWidthInput, maxHeightInput, fpsInput, bitrateInput).forEach { it.error = message }
        maxWidthInput.requestFocus()
    }

    private fun clearFieldErrors() {
        listOf(passwordInput, portInput, maxWidthInput, maxHeightInput, fpsInput, bitrateInput).forEach { it.error = null }
    }

    private fun sourceDimensions(): Pair<Int, Int> {
        val windows = getSystemService(WindowManager::class.java)
        return if (Build.VERSION.SDK_INT >= 30) {
            val metrics = windows.maximumWindowMetrics
            metrics.bounds.width() to metrics.bounds.height()
        } else {
            val metrics = android.util.DisplayMetrics()
            windows.defaultDisplay.getRealMetrics(metrics)
            metrics.widthPixels to metrics.heightPixels
        }
    }

    private fun beginProjection() {
        if (!pendingStart) return
        val manager = getSystemService(MediaProjectionManager::class.java)
        projectionLauncher.launch(manager.createScreenCaptureIntent())
    }

    private fun startService(resultCode: Int, resultData: Intent) {
        val passwordChars = pendingPassword ?: return
        val configuration = pendingConfiguration ?: return
        val password = String(passwordChars)
        passwordChars.fill('\u0000')
        val intent = Intent(this, RemoteService::class.java).apply {
            action = RemoteService.ACTION_START
            putExtra(RemoteService.EXTRA_RESULT_CODE, resultCode)
            putExtra(RemoteService.EXTRA_RESULT_DATA, resultData)
            putExtra(RemoteService.EXTRA_PASSWORD, password)
            putExtra(RemoteService.EXTRA_PORT, configuration[0])
            putExtra(RemoteService.EXTRA_MAX_WIDTH, configuration[1])
            putExtra(RemoteService.EXTRA_MAX_HEIGHT, configuration[2])
            putExtra(RemoteService.EXTRA_FPS, configuration[3])
            putExtra(RemoteService.EXTRA_BITRATE_KBPS, configuration[4])
            putExtra(RemoteService.EXTRA_ALLOW_INPUT, allowInput.isChecked)
        }
        startForegroundService(intent)
        pendingStart = false
        passwordInput.text.clear()
    }

    private fun clearPendingStart() {
        pendingStart = false
        pendingPassword?.fill('\u0000')
        pendingPassword = null
        pendingConfiguration = null
    }

    private fun stopRemote() {
        clearPendingStart()
        clearFieldErrors()
        passwordInput.text.clear()
        startService(Intent(this, RemoteService::class.java).setAction(RemoteService.ACTION_STOP))
    }

    private fun renderState() {
        if (!::statusView.isInitialized) return
        val running = RemoteService.isRunning()
        statusBadge.text = getString(
            when (displayState.first) {
                "capturing" -> R.string.state_capturing
                "connecting" -> R.string.state_connecting
                "idle" -> R.string.state_idle
                "error" -> R.string.state_error
                else -> R.string.state_stopped
            }
        )
        val badgeColor = when {
            displayState.first == "error" -> UiTheme.error
            displayState.first == "capturing" -> UiTheme.success
            running -> UiTheme.warning
            else -> UiTheme.muted
        }
        UiTheme.setBadge(statusBadge, badgeColor)
        statusView.text = if (running) displayState.second else displayState.second.ifBlank { getString(R.string.state_stopped) }
        statusView.setTextColor(if (displayState.first == "error") UiTheme.error else UiTheme.text)
        endpointView.text = if (RemoteService.currentFingerprint.isEmpty()) {
            getString(R.string.host_fingerprint_idle)
        } else {
            getString(R.string.host_fingerprint_running, RemoteService.currentFingerprint, RemoteService.currentLocalAddress)
        }
        startButton.isEnabled = !running
        stopButton.isEnabled = running
    }

    private fun renderGestureState() {
        if (!::accessibilityButton.isInitialized) return
        accessibilityButton.text = getString(
            if (RemoteAccessibilityService.isEnabled(this)) R.string.host_gesture_enabled else R.string.host_gesture_disabled
        )
    }

    private val Int.dp: Int get() = (this * resources.displayMetrics.density).toInt()
    private fun fieldParams() = LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT)

    private fun settingField(label: String, defaultValue: Int) = UiTheme.field(this, label).apply {
        contentDescription = label
        setText(String.format(java.util.Locale.ROOT, "%d", defaultValue))
        inputType = InputType.TYPE_CLASS_NUMBER
    }
}

internal fun validateHostQuality(
    maxWidth: Int?,
    maxHeight: Int?,
    fps: Int?,
    bitrateKbps: Int?
): Boolean {
    if (maxWidth == null || maxHeight == null || fps == null || bitrateKbps == null) return false
    if (maxWidth !in 2..7680 || maxHeight !in 2..7680) return false
    if (maxWidth % 2 != 0 || maxHeight % 2 != 0) return false
    if (fps !in 1..120 || bitrateKbps !in 100..100_000) return false
    return true
}
