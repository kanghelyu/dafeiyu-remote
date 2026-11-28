package remote.dafeiyu

import android.app.Activity
import android.content.res.ColorStateList
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.graphics.drawable.StateListDrawable
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat

/** Shared native visual language for the controlled and controlling Android roles. */
internal object UiTheme {
    val background = 0xFFF3F6FA.toInt()
    val surface = 0xFFFFFFFF.toInt()
    val surfaceMuted = 0xFFEAF0F6.toInt()
    val primary = 0xFF245D88.toInt()
    val primaryDark = 0xFF194665.toInt()
    val accent = 0xFF0B7A69.toInt()
    val text = 0xFF172536.toInt()
    val muted = 0xFF5C6D80.toInt()
    val border = 0xFFD9E2EB.toInt()
    val success = 0xFF176B55.toInt()
    val warning = 0xFF8A5A08.toInt()
    val error = 0xFFB3261E.toInt()
    val video = 0xFF101923.toInt()
    val videoText = 0xFFF4F8FC.toInt()
    val videoMuted = 0xFFAFBECC.toInt()

    fun Int.dp(context: android.content.Context): Int = (this * context.resources.displayMetrics.density).toInt()

    fun configureWindow(activity: Activity) {
        activity.window.setStatusBarColor(background)
        activity.window.setNavigationBarColor(background)
        WindowCompat.getInsetsController(activity.window, activity.window.decorView).apply {
            isAppearanceLightStatusBars = true
            isAppearanceLightNavigationBars = true
        }
    }

    fun applyInsets(view: View) {
        ViewCompat.setOnApplyWindowInsetsListener(view) { target, insets ->
            val bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
            )
            target.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            insets
        }
    }

    fun shape(fill: Int, radius: Float, stroke: Int = 0, strokeWidth: Int = 0) = GradientDrawable().apply {
        setColor(fill)
        cornerRadius = radius
        if (strokeWidth > 0) setStroke(strokeWidth, stroke)
    }

    fun card(context: android.content.Context): LinearLayout = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        val pad = 16.dp(context)
        setPadding(pad, pad, pad, pad)
        background = shape(surface, 18.dp(context).toFloat(), border, 1.dp(context))
        elevation = 1.dp(context).toFloat()
    }

    fun title(context: android.content.Context, value: CharSequence, large: Boolean = false): TextView =
        TextView(context).apply {
            this.text = value
            textSize = if (large) 28f else 20f
            setTextColor(UiTheme.text)
            typeface = Typeface.create("sans-serif", Typeface.BOLD)
        }

    fun sectionTitle(context: android.content.Context, value: CharSequence): TextView =
        TextView(context).apply {
            this.text = value
            textSize = 16f
            setTextColor(UiTheme.text)
            typeface = Typeface.create("sans-serif", Typeface.BOLD)
        }

    fun body(context: android.content.Context, value: CharSequence = "", muted: Boolean = false): TextView =
        TextView(context).apply {
            this.text = value
            textSize = 14f
            setLineSpacing(3.dp(context).toFloat(), 1f)
            setTextColor(if (muted) UiTheme.muted else UiTheme.text)
        }

    fun label(context: android.content.Context, value: CharSequence): TextView =
        TextView(context).apply {
            this.text = value
            textSize = 13f
            setTextColor(muted)
            typeface = Typeface.create("sans-serif", Typeface.NORMAL)
        }

    fun field(context: android.content.Context, hint: CharSequence): EditText = EditText(context).apply {
        this.hint = hint
        textSize = 16f
        setTextColor(UiTheme.text)
        setHintTextColor(muted)
        isSingleLine = true
        minHeight = 52.dp(context)
        val pad = 14.dp(context)
        setPadding(pad, 12.dp(context), pad, 12.dp(context))
        background = shape(UiTheme.background, 12.dp(context).toFloat(), border, 1.dp(context))
        importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO
        layoutDirection = View.LAYOUT_DIRECTION_LOCALE
    }

    fun button(context: android.content.Context, value: CharSequence, filled: Boolean = false): Button =
        Button(context, null, android.R.attr.borderlessButtonStyle).apply {
            this.text = value
            isAllCaps = false
            textSize = 15f
            typeface = Typeface.create("sans-serif", Typeface.BOLD)
            minHeight = 50.dp(context)
            minimumHeight = 50.dp(context)
            val pad = 14.dp(context)
            setPadding(pad, 0, pad, 0)
            setTextColor(
                ColorStateList(
                    arrayOf(intArrayOf(-android.R.attr.state_enabled), intArrayOf()),
                    intArrayOf(muted, if (filled) surface else primary),
                )
            )
            stateListAnimator = null
            elevation = 0f
            val radius = 14.dp(context).toFloat()
            background = RippleDrawable(
                ColorStateList.valueOf(if (filled) 0x33FFFFFF else 0x18245D88),
                buttonBackground(if (filled) primary else surface, if (filled) primary else border, radius),
                null,
            )
        }

    private fun buttonBackground(fill: Int, stroke: Int, radius: Float): StateListDrawable = StateListDrawable().apply {
        addState(intArrayOf(-android.R.attr.state_enabled), shape(surfaceMuted, radius, stroke, 0))
        addState(intArrayOf(android.R.attr.state_pressed), shape(if (fill == UiTheme.primary) primaryDark else surfaceMuted, radius, stroke, 1))
        addState(intArrayOf(), shape(fill, radius, stroke, 1))
    }

    fun badge(context: android.content.Context, value: CharSequence): TextView = TextView(context).apply {
        this.text = value
        textSize = 12f
        typeface = Typeface.create("sans-serif", Typeface.BOLD)
        val horizontal = 10.dp(context)
        val vertical = 5.dp(context)
        setPadding(horizontal, vertical, horizontal, vertical)
        setTextColor(primary)
        background = shape(surfaceMuted, 100f)
    }

    fun setBadge(view: TextView, fill: Int, textColor: Int = surface) {
        view.background = shape(fill, 100f)
        view.setTextColor(textColor)
    }
}
