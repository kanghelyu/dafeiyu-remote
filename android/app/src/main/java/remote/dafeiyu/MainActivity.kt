package remote.dafeiyu

import android.Manifest
import android.content.Intent
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
    private lateinit var statusView: TextView
    private lateinit var endpointView: TextView
    private lateinit var statsView: TextView
    private val handler = Handler(Looper.getMainLooper())
    private var pendingStart = false
    private var pendingPassword: CharArray? = null
    private var pendingConfiguration: IntArray? = null
    private var displayState = "stopped" to "未启动"

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
                DfyNative.reportLocal("stopped", "授权返回时启动状态已丢失，请重新开始")
                Toast.makeText(this, "授权返回时启动状态已丢失，请重新开始", Toast.LENGTH_LONG).show()
            } else {
                startService(result.resultCode, result.data!!)
            }
        } else {
            clearPendingStart()
            DfyNative.reportLocal("stopped", "未授予录屏授权")
        }
    }

    private val notificationLauncher = registerForActivityResult(
        androidx.activity.result.contract.ActivityResultContracts.RequestPermission()
    ) { beginProjection() }

    private val statsRefresh = object : Runnable {
        override fun run() {
            statsView.text = runCatching { DfyNative.stats() }.getOrDefault("原生统计不可用")
            handler.postDelayed(this, 1_000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildView())
        DfyNative.addListener(stateListener)
        displayState = RemoteService.currentState
        renderState()
    }

    override fun onResume() {
        super.onResume()
        accessibilityButton.text = if (RemoteAccessibilityService.isEnabled(this)) {
            "手势服务：已授权"
        } else {
            "手势服务：未授权"
        }
        handler.post(statsRefresh)
    }

    override fun onPause() {
        handler.removeCallbacks(statsRefresh)
        super.onPause()
    }

    override fun onDestroy() {
        clearPendingStart()
        DfyNative.removeListener(stateListener)
        super.onDestroy()
    }

    private fun buildView(): View {
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(40, 48, 40, 48)
        }
        content.addView(title("大肥鱼远程 · Android 被控端"))
        content.addView(note("仅在您明确授权录屏后采集画面。会话口令不会保存。"))
        content.addView(label("会话口令（16–1024 字节）"))
        passwordInput = EditText(this).apply {
            hint = "请输入本次会话口令"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            isSingleLine = true
            setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO)
        }
        content.addView(passwordInput)
        content.addView(label("监听端口"))
        portInput = EditText(this).apply {
            setText(RemoteService.DEFAULT_PORT.toString())
            inputType = InputType.TYPE_CLASS_NUMBER
            isSingleLine = true
        }
        content.addView(portInput)
        content.addView(label("编码画质上限（保持比例，不放大）"))
        maxWidthInput = settingField("最大宽度（偶数）", "最大宽度（偶数）", RemoteService.DEFAULT_MAX_WIDTH)
        maxHeightInput = settingField("最大高度（偶数）", "最大高度（偶数）", RemoteService.DEFAULT_MAX_HEIGHT)
        fpsInput = settingField("帧率（1–120）", "帧率（1–120）", RemoteService.DEFAULT_FPS)
        bitrateInput = settingField("码率 kbps（100–100000）", "码率 kbps（100–100000）", RemoteService.DEFAULT_BITRATE_KBPS)
        content.addView(maxWidthInput)
        content.addView(maxHeightInput)
        content.addView(fpsInput)
        content.addView(bitrateInput)
        allowInput = CheckBox(this).apply { text = "允许经无障碍服务发送已支持的手势" }
        content.addView(allowInput)
        content.addView(note("目前仅支持点击、滚动及有限的全局返回/主页操作；不提供全局键盘、剪贴板、文件传输或拖拽。"))
        val buttons = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        startButton = Button(this).apply {
            text = "开始被控"
            setOnClickListener { requestStart() }
        }
        stopButton = Button(this).apply {
            text = "停止"
            setOnClickListener { stopRemote() }
        }
        buttons.addView(startButton, buttonParams())
        buttons.addView(stopButton, buttonParams())
        content.addView(buttons)
        accessibilityButton = Button(this).apply {
            text = "管理手势服务授权"
            setOnClickListener {
                startActivity(Intent(android.provider.Settings.ACTION_ACCESSIBILITY_SETTINGS))
            }
        }
        content.addView(accessibilityButton)
        content.addView(label("连接状态"))
        statusView = TextView(this).apply { textSize = 16f }
        content.addView(statusView)
        endpointView = TextView(this).apply {
            textSize = 12f
            typeface = android.graphics.Typeface.MONOSPACE
            setTextIsSelectable(true)
        }
        content.addView(endpointView)
        content.addView(label("传输统计"))
        statsView = TextView(this).apply {
            textSize = 12f
            typeface = android.graphics.Typeface.MONOSPACE
            setTextIsSelectable(true)
        }
        content.addView(statsView)
        return ScrollView(this).apply { addView(content) }
    }

    private fun requestStart() {
        if (RemoteService.isRunning()) return
        val password = passwordInput.text.toString()
        if (password.toByteArray(Charsets.UTF_8).size !in 16..1024) {
            Toast.makeText(this, "口令必须为 16–1024 个 UTF-8 字节", Toast.LENGTH_LONG).show()
            return
        }
        val port = portInput.text.toString().toIntOrNull()
        if (port == null || port !in 1..65_535) {
            Toast.makeText(this, "端口必须在 1–65535 之间", Toast.LENGTH_LONG).show()
            return
        }
        val maxWidth = maxWidthInput.text.toString().toIntOrNull()
        val maxHeight = maxHeightInput.text.toString().toIntOrNull()
        val fps = fpsInput.text.toString().toIntOrNull()
        val bitrateKbps = bitrateInput.text.toString().toIntOrNull()
        val validQuality = validateHostQuality(maxWidth, maxHeight, fps, bitrateKbps)
        val qualityWidth = maxWidth ?: return
        val qualityHeight = maxHeight ?: return
        val qualityFps = fps ?: return
        val qualityBitrate = bitrateKbps ?: return
        val (sourceWidth, sourceHeight) = sourceDimensions()
        val fitsSource = runCatching {
            EncoderSettings.fitWithin(
                sourceWidth, sourceHeight, qualityWidth, qualityHeight, qualityFps, qualityBitrate
            )
        }.isSuccess
        if (!validQuality || !fitsSource) {
            Toast.makeText(this, "画质上限：偶数尺寸 2–7680，帧率 1–120，码率 100–100000 kbps", Toast.LENGTH_LONG).show()
            return
        }
        if (allowInput.isChecked && !RemoteAccessibilityService.isEnabled(this)) {
            allowInput.isChecked = false
            Toast.makeText(this, "请先明确启用大肥鱼远程手势服务", Toast.LENGTH_LONG).show()
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
        val port = configuration[0]
        val intent = Intent(this, RemoteService::class.java).apply {
            action = RemoteService.ACTION_START
            putExtra(RemoteService.EXTRA_RESULT_CODE, resultCode)
            putExtra(RemoteService.EXTRA_RESULT_DATA, resultData)
            putExtra(RemoteService.EXTRA_PASSWORD, password)
            putExtra(RemoteService.EXTRA_PORT, port)
            putExtra(RemoteService.EXTRA_MAX_WIDTH, configuration[1])
            putExtra(RemoteService.EXTRA_MAX_HEIGHT, configuration[2])
            putExtra(RemoteService.EXTRA_FPS, configuration[3])
            putExtra(RemoteService.EXTRA_BITRATE_KBPS, configuration[4])
            putExtra(RemoteService.EXTRA_ALLOW_INPUT, allowInput.isChecked)
        }
        if (Build.VERSION.SDK_INT >= 26) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
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
        passwordInput.text.clear()
        startService(Intent(this, RemoteService::class.java).setAction(RemoteService.ACTION_STOP))
    }

    private fun renderState() {
        statusView.text = if (RemoteService.isRunning()) {
            "${displayState.first}：${displayState.second}"
        } else {
            displayState.second
        }
        endpointView.text = if (RemoteService.currentFingerprint.isEmpty()) {
            "会话指纹：未启动"
        } else {
            "会话指纹：${RemoteService.currentFingerprint}\n监听地址：${RemoteService.currentLocalAddress}"
        }
        startButton.isEnabled = !RemoteService.isRunning()
    }

    private fun title(text: String) = TextView(this).apply {
        this.text = text
        textSize = 24f
        setPadding(0, 0, 0, 24)
    }

    private fun label(text: String) = TextView(this).apply {
        this.text = text
        textSize = 14f
        setPadding(0, 24, 0, 8)
    }

    private fun note(text: String) = TextView(this).apply {
        this.text = text
        textSize = 14f
        setPadding(0, 0, 0, 24)
    }

    private fun settingField(hint: String, contentDescription: String, defaultValue: Int) = EditText(this).apply {
        this.hint = hint
        this.contentDescription = contentDescription
        setText(defaultValue.toString())
        inputType = InputType.TYPE_CLASS_NUMBER
        isSingleLine = true
    }

    private fun buttonParams() = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply {
        marginEnd = 16
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
