package com.anypage.engine

import android.app.Activity
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.WindowInsets
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

/**
 * The secrets screen (launcher shortcut "金鑰 Secrets"): enter or clear the
 * secrets the current page declares (`capabilities.secrets`), and remove
 * any stored one. Values are written to [SecretVault] (Android Keystore
 * AES-GCM) and never shown again; the screen only says whether one is set.
 */
class SecretsActivity : Activity() {
    private lateinit var list: LinearLayout
    private val density get() = resources.displayMetrics.density

    private fun dp(v: Int) = (v * density).toInt()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        SecretVault.init(this)
        title = "金鑰 Secrets"
        list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(24), dp(20), dp(32))
        }
        val root = ScrollView(this).apply {
            setBackgroundColor(BG)
            isFillViewport = true
            addView(list, MATCH_PARENT, WRAP_CONTENT)
        }
        // edge to edge (always on Android 15): keep the content clear of the
        // status bar, the navigation bar and the keyboard
        root.setOnApplyWindowInsetsListener { v, insets ->
            val (top, bottom) = if (Build.VERSION.SDK_INT >= 30) {
                val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.ime())
                bars.top to bars.bottom
            } else {
                @Suppress("DEPRECATION")
                insets.systemWindowInsetTop to insets.systemWindowInsetBottom
            }
            v.setPadding(0, top, 0, bottom)
            insets
        }
        setContentView(root)
        render()
    }

    private fun text(s: String, sp: Float = 15f, color: Int = FG, bold: Boolean = false) = TextView(this).apply {
        text = s
        setTextColor(color)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, sp)
        if (bold) typeface = Typeface.DEFAULT_BOLD
        setPadding(0, dp(4), 0, dp(4))
    }

    private fun button(label: String, primary: Boolean, onClick: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        setTextColor(if (primary) Color.WHITE else FG)
        background = GradientDrawable().apply {
            cornerRadius = dp(8).toFloat()
            setColor(if (primary) ACCENT else BUTTON)
        }
        setOnClickListener { onClick() }
    }

    private fun card() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(dp(16), dp(12), dp(16), dp(14))
        background = GradientDrawable().apply {
            cornerRadius = dp(12).toFloat()
            setColor(CARD)
        }
        layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(12) }
    }

    private fun render() {
        list.removeAllViews()
        list.addView(text("金鑰 Secrets", 26f, FG, bold = true))
        val page = PageHistory.current?.takeIf { it.ok && it.id.isNotEmpty() }
        if (page == null) {
            list.addView(text("目前沒有開啟的頁面。開啟需要金鑰的頁面後再來這裡設定。\nNo page is open; open a page that uses secrets, then come back.", 14f, DIM))
        } else {
            list.addView(text("頁面 Page：${page.title}（${page.id}）", 14f, DIM))
            if (page.secrets.isEmpty()) {
                list.addView(text("這個頁面沒有宣告金鑰。\nThis page declares no secrets.", 14f, DIM))
            }
            for (name in page.secrets) list.addView(editor(page.id, name))
        }
        val stored = SecretVault.entries()
        if (stored.isNotEmpty()) {
            list.addView(text("已儲存 Stored", 18f, FG, bold = true).apply { setPadding(0, dp(24), 0, dp(4)) })
            for ((scope, name) in stored) {
                val row = LinearLayout(this).apply {
                    orientation = LinearLayout.HORIZONTAL
                    gravity = Gravity.CENTER_VERTICAL
                    setPadding(0, dp(4), 0, dp(4))
                }
                val where = if (scope == SecretVault.ALL_PAGES) "所有頁面 all pages" else scope
                row.addView(text("$name · $where", 14f, FG), LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
                row.addView(button("刪除 Delete", false) {
                    SecretVault.remove(scope, name)
                    Toast.makeText(this, "已刪除 $name", Toast.LENGTH_SHORT).show()
                    render()
                })
                list.addView(row)
            }
        }
        list.addView(text(
            "金鑰以 Android Keystore（AES-GCM）加密，只存在這台裝置。頁面腳本讀不到金鑰；只有原生模組（例如雲端聊天）會在連線到頁面允許的主機時代為使用。\n" +
                "Secrets are encrypted with an Android Keystore key (AES-GCM) and stay on this device. Page scripts cannot read them; " +
                "native modules use them for the hosts the page allows.",
            12f, DIM,
        ).apply { setPadding(0, dp(24), 0, 0) })
    }

    private fun editor(pageId: String, name: String): View {
        val c = card()
        val set = SecretVault.has(pageId, name)
        val shared = !set && SecretVault.has(SecretVault.ALL_PAGES, name)
        c.addView(text(name, 18f, FG, bold = true))
        c.addView(text(
            when {
                set -> "已設定 Set (encrypted)"
                shared -> "使用所有頁面共用的值 Using the value set for all pages"
                else -> "未設定 Not set"
            },
            13f, if (set || shared) GOOD else WARN,
        ))
        val field = EditText(this).apply {
            hint = if (set) "輸入新值以取代 New value" else "輸入金鑰 Enter the secret"
            setHintTextColor(DIM)
            setTextColor(FG)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            // the password variation switches to a monospace face; keep the hint readable
            typeface = Typeface.DEFAULT
            isSingleLine = true
            importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO
        }
        c.addView(field, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(0, dp(8), 0, 0)
        }
        row.addView(button("儲存 Save", true) {
            val v = field.text.toString().trim()
            when {
                v.isEmpty() -> Toast.makeText(this, "請先輸入金鑰 Enter a value first", Toast.LENGTH_SHORT).show()
                v.length > SecretVault.MAX_VALUE -> Toast.makeText(this, "太長了 Too long", Toast.LENGTH_SHORT).show()
                SecretVault.put(pageId, name, v) -> {
                    field.text.clear()
                    Toast.makeText(this, "已儲存 $name Saved", Toast.LENGTH_SHORT).show()
                    render()
                }
                else -> Toast.makeText(this, "無法加密儲存 Could not store it", Toast.LENGTH_LONG).show()
            }
        })
        if (set) {
            row.addView(View(this), LinearLayout.LayoutParams(dp(12), 1))
            row.addView(button("清除 Clear", false) {
                SecretVault.remove(pageId, name)
                render()
            })
        }
        c.addView(row)
        return c
    }

    private companion object {
        val BG = Color.rgb(18, 20, 26)
        val CARD = Color.rgb(30, 34, 44)
        val BUTTON = Color.rgb(52, 60, 78)
        val FG = Color.rgb(230, 234, 240)
        val DIM = Color.rgb(150, 158, 172)
        val ACCENT = Color.rgb(96, 156, 255)
        val GOOD = Color.rgb(90, 210, 140)
        val WARN = Color.rgb(255, 170, 110)
    }
}
