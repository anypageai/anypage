package com.anypage.engine

import android.icu.text.BreakIterator
import android.os.Bundle
import android.text.Editable
import android.text.Selection
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.TextUtils
import android.text.style.UnderlineSpan
import android.util.Log
import android.view.KeyEvent
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.CompletionInfo
import android.view.inputmethod.CorrectionInfo
import android.view.inputmethod.ExtractedText
import android.view.inputmethod.ExtractedTextRequest
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.SurroundingText
import android.view.inputmethod.TextAttribute
import android.annotation.TargetApi

/**
 * The input method's view of a page's text field.
 *
 * The page owns its text and the engine cannot read it back yet
 * (apps/android/CORE_REQUESTS.md, item 1). So the IME is shown a shadow: the
 * text it typed itself since the field got focus or the caret last moved
 * elsewhere, which is the end of the page's text before the caret. That is
 * enough for suggestions, autocorrect and re-composing the word just typed.
 * Whenever the caret moves in a way the IME cannot see (a tap, arrow keys,
 * Enter, shortcuts) the shadow is dropped and the input restarted.
 *
 * Edits reach the page as engine events:
 *
 * - commitText -> `text` (ending any composition first)
 * - setComposingText -> `ime` (preedit), "" when composition ends
 * - finishComposingText -> the composition is committed as `text`
 * - setComposingRegion over the word before the caret -> that word is
 *   deleted (backspace per grapheme) and shown as the preedit again
 * - deleteSurroundingText -> `backspace` per grapheme of the shadow text it
 *   removes (one per character beyond the shadow), `delete` per character after
 * - sendKeyEvent -> keys by engine name, like a hardware keyboard
 * - performEditorAction -> `enter`; context menu -> ctrl+a / x / c / v
 *
 * Every method runs on the UI thread.
 */
class EngineInputConnection(private val view: EngineView) : BaseInputConnection(view, true) {
    private val committed = StringBuilder()
    private var composing = ""
    private var batch = 0
    private var monitorCursor = false
    private val imm: InputMethodManager? = view.context.getSystemService(InputMethodManager::class.java)

    val isComposing: Boolean
        get() = composing.isNotEmpty()

    /** Anything the IME could be holding on to (typed text or a composition). */
    val hasState: Boolean
        get() = committed.isNotEmpty() || composing.isNotEmpty()

    private fun cursor() = committed.length + composing.length

    /** Tell the IME where the caret and the composition are now. */
    private fun changed() {
        if (batch > 0) return
        val c = cursor()
        if (composing.isEmpty()) imm?.updateSelection(view, c, c, -1, -1)
        else imm?.updateSelection(view, c, c, committed.length, c)
    }

    private fun append(s: String) {
        committed.append(s)
        if (committed.length > MAX_SHADOW) restart()
    }

    override fun beginBatchEdit(): Boolean {
        batch++
        return true
    }

    override fun endBatchEdit(): Boolean {
        if (batch > 0) batch--
        if (batch == 0) changed()
        return batch > 0
    }

    // ------------------------------------------------------------ composition

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val s = text?.toString() ?: ""
        Log.i(TAG, "setComposingText(${s.length} chars)")
        if (s != composing) {
            composing = s
            view.sendPreedit(s)
        }
        changed()
        return true
    }

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val s = text?.toString() ?: ""
        Log.i(TAG, "commitText(${s.length} chars)")
        endComposition()
        if (s.isNotEmpty()) {
            view.sendText(s)
            append(s)
        }
        changed()
        return true
    }

    override fun finishComposingText(): Boolean {
        if (composing.isNotEmpty()) {
            Log.i(TAG, "finishComposingText: committing ${composing.length} chars")
            val s = composing
            endComposition()
            view.sendText(s)
            append(s)
            changed()
        }
        return true
    }

    /** Drop the composition without committing it (the page lost focus). */
    fun abandon() {
        endComposition()
        committed.setLength(0)
    }

    private fun endComposition() {
        if (composing.isNotEmpty()) {
            composing = ""
            view.sendPreedit("")
        }
    }

    override fun setComposingRegion(start: Int, end: Int): Boolean {
        val a = minOf(start, end)
        val b = maxOf(start, end)
        Log.i(TAG, "setComposingRegion($a, $b)")
        finishComposingText()
        if (a == b) return true
        // only the text just typed before the caret can be taken back
        if (a < 0 || b != committed.length || Character.isLowSurrogate(committed[a])) return false
        val word = committed.substring(a)
        view.sendKeyName("backspace", graphemes(word))
        committed.setLength(a)
        composing = word
        view.sendPreedit(word)
        changed()
        return true
    }

    // ------------------------------------------------------------ deleting

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        Log.i(TAG, "deleteSurroundingText($beforeLength, $afterLength)")
        finishComposingText()
        val before = beforeLength.coerceAtLeast(0)
        var k = minOf(before, committed.length)
        // never split a surrogate pair
        if (k in 1 until committed.length && Character.isLowSurrogate(committed[committed.length - k])) k++
        val removed = committed.substring(committed.length - k)
        committed.setLength(committed.length - k)
        // the shadow's text by grapheme; beyond it the page's text is unknown: one key per character
        val keys = graphemes(removed) + (before - k).coerceAtLeast(0)
        if (keys > 0) view.sendKeyName("backspace", keys)
        if (afterLength > 0) view.sendKeyName("delete", afterLength)
        changed()
        return true
    }

    override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
        // code points to UTF-16 units, as far as the shadow knows them
        var units = 0
        var i = committed.length
        repeat(beforeLength.coerceAtLeast(0)) {
            if (i > 0) {
                val cp = Character.codePointBefore(committed, i)
                i -= Character.charCount(cp)
                units += Character.charCount(cp)
            } else {
                units++
            }
        }
        return deleteSurroundingText(units, afterLength)
    }

    // ------------------------------------------------------------ keys and actions

    @Suppress("DEPRECATION") // ACTION_MULTIPLE with characters: old IMEs still send it
    override fun sendKeyEvent(event: KeyEvent): Boolean {
        Log.i(TAG, "sendKeyEvent(${KeyEvent.keyCodeToString(event.keyCode)}, action ${event.action})")
        when {
            event.action == KeyEvent.ACTION_DOWN -> view.sendKey(event)
            event.action == KeyEvent.ACTION_MULTIPLE && event.keyCode == KeyEvent.KEYCODE_UNKNOWN ->
                event.characters?.let { if (it.isNotEmpty()) commitText(it, 1) }
        }
        return true
    }

    /**
     * A key reached the page (hardware keyboard or the IME's key events):
     * keep the shadow in step, or drop it when the caret moved.
     */
    fun keySent(keyCode: Int, typed: Int, meta: Int) {
        val chord = meta and (KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK or KeyEvent.META_META_MASK) != 0
        val ch = if (typed > 0 && typed and KEY_COMBINING == 0) typed else 0
        when {
            chord -> restart()
            keyCode == KeyEvent.KEYCODE_DEL -> {
                finishComposingText()
                if (committed.isNotEmpty()) {
                    val it = BreakIterator.getCharacterInstance()
                    it.setText(committed.toString())
                    committed.setLength(it.preceding(committed.length).coerceAtLeast(0))
                }
                changed()
            }
            keyCode == KeyEvent.KEYCODE_FORWARD_DEL -> Unit
            ch != 0 && !Character.isISOControl(ch) -> {
                finishComposingText()
                append(String(Character.toChars(ch)))
                changed()
            }
            else -> restart()
        }
    }

    /** The caret moved where the IME cannot follow: start over with an empty shadow. */
    fun restart() {
        finishComposingText()
        committed.setLength(0)
        view.post { imm?.restartInput(view) }
    }

    override fun performEditorAction(editorAction: Int): Boolean {
        Log.i(TAG, "performEditorAction($editorAction)")
        finishComposingText()
        view.sendKeyName("enter", 1)
        restart()
        return true
    }

    override fun performContextMenuAction(id: Int): Boolean {
        val key = when (id) {
            android.R.id.selectAll -> "ctrl+a"
            android.R.id.cut -> "ctrl+x"
            android.R.id.copy -> "ctrl+c"
            android.R.id.paste, android.R.id.pasteAsPlainText -> "ctrl+v"
            else -> return false
        }
        finishComposingText()
        view.sendKeyName(key, 1)
        restart()
        return true
    }

    override fun commitCompletion(text: CompletionInfo?): Boolean {
        text?.text?.let { commitText(it, 1) }
        return true
    }

    override fun commitCorrection(correctionInfo: CorrectionInfo?): Boolean = true

    // ------------------------------------------------------------ what the IME can read

    /**
     * A copy of the shadow with the caret at its end and the composition
     * marked: what the base class's own readers (getSurroundingText,
     * takeSnapshot, ...) see. Edits never go through it.
     */
    override fun getEditable(): Editable {
        val e = SpannableStringBuilder(committed).append(composing)
        Selection.setSelection(e, e.length)
        // the composing region is any span flagged SPAN_COMPOSING
        if (composing.isNotEmpty()) e.setSpan(UnderlineSpan(), committed.length, e.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE or Spanned.SPAN_COMPOSING)
        return e
    }

    override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence {
        val s = committed.toString() + composing
        var from = (s.length - n.coerceAtLeast(0)).coerceAtLeast(0)
        if (from in 1 until s.length && Character.isLowSurrogate(s[from])) from++
        Log.d(TAG, "getTextBeforeCursor($n) -> ${s.length - from} chars")
        return s.substring(from)
    }

    @TargetApi(31)
    override fun getSurroundingText(beforeLength: Int, afterLength: Int, flags: Int): SurroundingText {
        val before = getTextBeforeCursor(beforeLength, flags)
        Log.d(TAG, "getSurroundingText($beforeLength, $afterLength)")
        return SurroundingText(before, before.length, before.length, cursor() - before.length)
    }

    /** Android 14: replace a range; only the text just before the caret can be replaced. */
    @TargetApi(34)
    override fun replaceText(start: Int, end: Int, text: CharSequence, newCursorPosition: Int, textAttribute: TextAttribute?): Boolean {
        val a = minOf(start, end)
        val b = maxOf(start, end)
        Log.i(TAG, "replaceText($a, $b, ${text.length} chars)")
        finishComposingText()
        if (b != cursor() || a < 0) return false
        deleteSurroundingText(b - a, 0)
        return commitText(text, newCursorPosition)
    }

    // the page's text after the caret is unknown
    override fun getTextAfterCursor(n: Int, flags: Int): CharSequence = ""

    override fun getSelectedText(flags: Int): CharSequence? = null

    override fun getExtractedText(request: ExtractedTextRequest?, flags: Int): ExtractedText {
        Log.d(TAG, "getExtractedText(flags $flags)")
        val s = committed.toString() + composing
        return ExtractedText().apply {
            text = s
            startOffset = 0
            partialStartOffset = -1
            partialEndOffset = -1
            selectionStart = s.length
            selectionEnd = s.length
        }
    }

    override fun getCursorCapsMode(reqModes: Int): Int {
        Log.d(TAG, "getCursorCapsMode")
        return if (committed.isEmpty()) 0 else TextUtils.getCapsMode(committed.toString() + composing, cursor(), reqModes)
    }

    override fun setSelection(start: Int, end: Int): Boolean {
        Log.i(TAG, "setSelection($start, $end)")
        // moving the caret within what was typed is not possible yet: start over
        if (start != cursor() || end != cursor()) restart()
        return true
    }

    override fun performPrivateCommand(action: String?, data: Bundle?): Boolean {
        Log.d(TAG, "performPrivateCommand($action)")
        return false
    }

    override fun requestCursorUpdates(cursorUpdateMode: Int): Boolean {
        Log.d(TAG, "requestCursorUpdates($cursorUpdateMode)")
        monitorCursor = cursorUpdateMode and InputConnection.CURSOR_UPDATE_MONITOR != 0
        if (cursorUpdateMode and InputConnection.CURSOR_UPDATE_IMMEDIATE != 0) sendCursor(true)
        return true
    }

    /** Tell the IME where the caret is (it places candidate windows there). */
    fun sendCursor(immediate: Boolean) {
        if (!immediate && !monitorCursor) return
        imm?.updateCursorAnchorInfo(view, view.cursorAnchorInfo())
    }

    override fun closeConnection() {
        // the base class finishes (commits) the composition, as an editor does
        super.closeConnection()
        composing = ""
        committed.setLength(0)
    }

    private fun graphemes(s: String): Int {
        if (s.isEmpty()) return 0
        val it = BreakIterator.getCharacterInstance()
        it.setText(s)
        var n = 0
        while (it.next() != BreakIterator.DONE) n++
        return n
    }

    companion object {
        private const val TAG = "AnyPageHost"
        /** Longest shadow kept; beyond it the input restarts. */
        private const val MAX_SHADOW = 4096
        /** KeyCharacterMap.COMBINING_ACCENT */
        private const val KEY_COMBINING = 0x80000000.toInt()
    }
}
