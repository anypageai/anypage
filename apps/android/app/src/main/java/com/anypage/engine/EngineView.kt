package com.anypage.engine

import android.content.ClipDescription
import android.graphics.Matrix
import android.graphics.Rect
import android.graphics.RectF
import android.os.Build
import android.os.ParcelFileDescriptor
import android.text.InputType
import android.util.Log
import android.view.DragEvent
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowInsets
import android.view.inputmethod.CursorAnchorInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager

/**
 * A native view that hands its surface to the engine and forwards input:
 * touches, mouse hover and wheel, trackpad scroll, keys, the input method
 * (through [EngineInputConnection]) and dropped documents. The engine owns
 * the frame loop; this view never draws.
 */
class EngineView(
    private val activity: MainActivity,
    host: NativeHost,
    page: PagePackage,
    hud: Boolean,
) : SurfaceView(activity), SurfaceHolder.Callback {
    var handle: Long = NativeBridge.create(resources.displayMetrics.density, host, page.name, page.names, page.contents, hud)
        private set

    private val imm = activity.getSystemService(InputMethodManager::class.java)
    private var textFocused = false
    private val caret = RectF()
    private var connection: EngineInputConnection? = null
    /** Keys the engine took on key down, so their key up is consumed too. */
    private val keysDown = HashSet<Int>()
    private var imeInset = -1
    private val visible = Rect()

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
        if (handle == 0L) Log.e(TAG, "the engine did not start")
        setOnDragListener { _, ev -> onDrag(ev) }
        if (Build.VERSION.SDK_INT >= 30) {
            setOnApplyWindowInsetsListener { _, insets ->
                reportImeInset(insets.getInsets(WindowInsets.Type.ime()).bottom)
                insets
            }
        } else {
            // before Android 11: the keyboard is what the visible frame lost at the bottom
            viewTreeObserver.addOnGlobalLayoutListener {
                getWindowVisibleDisplayFrame(visible)
                val loc = IntArray(2)
                getLocationOnScreen(loc)
                reportImeInset((loc[1] + height - visible.bottom).coerceAtLeast(0))
            }
        }
    }

    private fun reportImeInset(bottom: Int) {
        if (bottom != imeInset && handle != 0L) {
            imeInset = bottom
            Log.i(TAG, "keyboard inset $bottom px")
            NativeBridge.imeInset(handle, bottom.toFloat())
        }
    }

    // ------------------------------------------------------------ surface

    override fun surfaceCreated(holder: SurfaceHolder) {
        if (handle != 0L) NativeBridge.surfaceCreated(handle, holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        if (handle != 0L) NativeBridge.surfaceChanged(handle, width, height)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        if (handle != 0L) NativeBridge.surfaceDestroyed(handle)
    }

    // ------------------------------------------------------------ pointer

    override fun onTouchEvent(e: MotionEvent): Boolean {
        if (handle == 0L) return false
        if (Build.VERSION.SDK_INT >= 34 && e.classification == MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE) {
            // trackpad two-finger scroll: lines, like a wheel
            if (e.actionMasked == MotionEvent.ACTION_MOVE) {
                val line = LINE_DP * resources.displayMetrics.density
                val dx = -e.getAxisValue(MotionEvent.AXIS_GESTURE_SCROLL_X_DISTANCE) / line
                val dy = -e.getAxisValue(MotionEvent.AXIS_GESTURE_SCROLL_Y_DISTANCE) / line
                NativeBridge.wheel(handle, e.x, e.y, dx, dy)
            }
            return true
        }
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                // a tap ends a composition where it is, as in any editor, and
                // may move the caret: the IME starts over
                if (e.actionMasked == MotionEvent.ACTION_DOWN) connection?.let { if (it.hasState) it.restart() }
                val i = e.actionIndex
                NativeBridge.touch(handle, 0, e.getPointerId(i), e.getX(i), e.getY(i))
            }
            MotionEvent.ACTION_MOVE -> {
                for (i in 0 until e.pointerCount) {
                    NativeBridge.touch(handle, 1, e.getPointerId(i), e.getX(i), e.getY(i))
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                val i = e.actionIndex
                NativeBridge.touch(handle, 2, e.getPointerId(i), e.getX(i), e.getY(i))
            }
            MotionEvent.ACTION_CANCEL -> NativeBridge.touch(handle, 3, 0, 0f, 0f)
        }
        return true
    }

    override fun onGenericMotionEvent(e: MotionEvent): Boolean {
        if (handle != 0L && e.isFromSource(InputDevice.SOURCE_CLASS_POINTER)) {
            when (e.actionMasked) {
                MotionEvent.ACTION_SCROLL -> {
                    // AXIS_VSCROLL > 0 is wheel up, as the engine counts; AXIS_HSCROLL > 0 is
                    // right, the engine (like winit) counts content moving right as positive
                    val dx = 0f - e.getAxisValue(MotionEvent.AXIS_HSCROLL) // 0 - 0 is +0, not -0
                    val dy = e.getAxisValue(MotionEvent.AXIS_VSCROLL)
                    Log.i(TAG, "wheel $dx $dy")
                    NativeBridge.wheel(handle, e.x, e.y, dx, dy)
                    return true
                }
                MotionEvent.ACTION_HOVER_MOVE -> {
                    NativeBridge.touch(handle, 4, 0, e.x, e.y)
                    return true
                }
            }
        }
        return super.onGenericMotionEvent(e)
    }

    // ------------------------------------------------------------ keys

    /** Forward a key press; true if the engine takes it. */
    fun sendKey(e: KeyEvent): Boolean {
        if (handle == 0L) return false
        val meta = e.metaState
        val mods = KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK or KeyEvent.META_META_MASK
        val plain = e.getUnicodeChar(meta and mods.inv())
        // AltGr (right Alt) without Ctrl/Meta: the layout may give the key a
        // character of its own ('@' on a German Q); 0 where it has none.
        // Left Alt stays a modifier for page shortcuts.
        val alt = if (meta and KeyEvent.META_ALT_RIGHT_ON != 0 && meta and (KeyEvent.META_CTRL_MASK or KeyEvent.META_META_MASK) == 0) e.getUnicodeChar(meta) else 0
        val taken = NativeBridge.key(handle, e.keyCode, meta, plain, alt)
        if (taken && textFocused) {
            val altTyped = alt > 0 && alt and KEY_COMBINING == 0 && !Character.isISOControl(alt)
            if (altTyped) connection?.keySent(e.keyCode, alt, meta and KeyEvent.META_ALT_MASK.inv())
            else connection?.keySent(e.keyCode, plain, meta)
        }
        return taken
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (sendKey(event)) {
            keysDown.add(keyCode)
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        if (keysDown.remove(keyCode)) return true
        return super.onKeyUp(keyCode, event)
    }

    @Suppress("DEPRECATION") // KeyEvent.characters: how legacy injections deliver text
    override fun onKeyMultiple(keyCode: Int, repeatCount: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_UNKNOWN) {
            val chars = event.characters
            if (!chars.isNullOrEmpty()) {
                sendText(chars)
                return true
            }
        }
        var taken = false
        repeat(repeatCount) { taken = sendKey(event) || taken }
        return taken || super.onKeyMultiple(keyCode, repeatCount, event)
    }

    fun sendKeyName(name: String, count: Int) {
        if (handle != 0L) NativeBridge.keyName(handle, name, count)
    }

    fun sendText(s: String) {
        if (handle != 0L) NativeBridge.text(handle, s)
    }

    fun sendPreedit(s: String) {
        if (handle != 0L) NativeBridge.preedit(handle, s)
    }

    // ------------------------------------------------------------ input method

    override fun onCheckIsTextEditor(): Boolean = textFocused

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        if (!textFocused || handle == 0L) return null
        // plain text with composition and suggestions; the IME is told
        // nothing about the text itself (see EngineInputConnection)
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_ACTION_NONE
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        val ic = EngineInputConnection(this)
        connection = ic
        Log.i(TAG, "input connection opened: inputType=0x${outAttrs.inputType.toString(16)} imeOptions=0x${outAttrs.imeOptions.toString(16)}")
        return ic
    }

    /**
     * From the engine: the page's caret while it wants text ([on]), in view
     * pixels. Shows or hides the soft keyboard and keeps the IME's idea of
     * the caret current.
     */
    fun setTextFocus(on: Boolean, show: Boolean, x: Float, y: Float, w: Float, h: Float) {
        if (handle == 0L) return
        caret.set(x, y, x + w, y + h)
        if (on != textFocused) {
            textFocused = on
            if (on) {
                if (!hasFocus()) requestFocus()
                imm?.restartInput(this)
            } else {
                connection?.abandon()
                connection = null
                imm?.hideSoftInputFromWindow(windowToken, 0)
                imm?.restartInput(this)
                Log.i(TAG, "keyboard: hide")
            }
        }
        val imeVisible = Build.VERSION.SDK_INT >= 30 && rootWindowInsets?.isVisible(WindowInsets.Type.ime()) == true
        if (on && show && !imeVisible) {
            if (!hasFocus()) requestFocus()
            val shown = imm?.showSoftInput(this, 0) ?: false
            Log.i(TAG, "keyboard: show (caret ${x.toInt()},${y.toInt()} ${w.toInt()}x${h.toInt()}) -> $shown")
        }
        if (on) connection?.sendCursor(false)
    }

    fun cursorAnchorInfo(): CursorAnchorInfo {
        val loc = IntArray(2)
        getLocationOnScreen(loc)
        val m = Matrix()
        m.setTranslate(loc[0].toFloat(), loc[1].toFloat())
        return CursorAnchorInfo.Builder()
            .setMatrix(m)
            .setInsertionMarkerLocation(caret.left, caret.top, caret.top + caret.height() * 0.8f, caret.bottom, CursorAnchorInfo.FLAG_HAS_VISIBLE_REGION)
            .build()
    }

    /** The live connection, or a detached one: debug builds drive the page through it. */
    fun debugConnection(): EngineInputConnection = connection ?: EngineInputConnection(this)

    // ------------------------------------------------------------ clipboard and files

    fun paste(text: String) {
        if (handle != 0L) NativeBridge.paste(handle, text)
    }

    fun fileOpened(id: Long, name: String, fd: Int) {
        if (handle != 0L) NativeBridge.fileOpened(handle, id, name, fd) else closeFd(fd)
    }

    fun fileSaveTo(id: Long, name: String, fd: Int) {
        if (handle != 0L) NativeBridge.fileSaveTo(handle, id, name, fd) else closeFd(fd)
    }

    fun fileDropped(name: String, fd: Int) {
        if (handle != 0L) NativeBridge.fileDropped(handle, name, fd) else closeFd(fd)
    }

    fun fileCancelled(id: Long) {
        if (handle != 0L) NativeBridge.fileCancelled(handle, id)
    }

    fun fileFailed(id: Long, message: String) {
        if (handle != 0L) NativeBridge.fileFailed(handle, id, message)
    }

    private fun closeFd(fd: Int) {
        try {
            ParcelFileDescriptor.adoptFd(fd).close()
        } catch (_: Exception) {
        }
    }

    private fun onDrag(ev: DragEvent): Boolean = when (ev.action) {
        // documents (URIs) only; plain dragged text is not a file
        DragEvent.ACTION_DRAG_STARTED -> {
            val d = ev.clipDescription
            d != null && (0 until d.mimeTypeCount).any {
                val m = d.getMimeType(it)
                m != ClipDescription.MIMETYPE_TEXT_PLAIN && m != ClipDescription.MIMETYPE_TEXT_HTML
            }
        }
        DragEvent.ACTION_DROP -> {
            activity.dropped(ev)
            true
        }
        else -> true
    }

    /** Show [page]; [mode] [NativeBridge.LOAD_REPLACE] or [NativeBridge.LOAD_NAVIGATE]. */
    fun loadPage(page: PagePackage, mode: Int) {
        if (handle != 0L) NativeBridge.loadPage(handle, page.name, page.names, page.contents, mode)
    }

    /** A semantic-port request for the page; the answer goes to the log (debug tools). */
    fun act(request: String) {
        if (handle != 0L) NativeBridge.act(handle, request)
    }

    fun release() {
        if (handle != 0L) {
            val h = handle
            handle = 0L
            NativeBridge.destroy(h)
        }
    }

    companion object {
        private const val TAG = "AnyPageHost"
        /** Trackpad scroll distance that counts as one wheel line. */
        private const val LINE_DP = 20f
        /** KeyCharacterMap.COMBINING_ACCENT: a dead key. */
        private const val KEY_COMBINING = 0x80000000.toInt()
    }
}
