package com.anypage.engine

import android.app.Activity
import android.os.Bundle
import android.text.InputType
import android.util.Log
import android.view.Gravity
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.ExtractedText
import android.view.inputmethod.ExtractedTextRequest
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import android.view.inputmethod.InputMethodManager
import android.widget.EditText

/**
 * Debug builds only: a platform EditText whose input connection logs every
 * call the input method makes (tag AnyPageImeProbe), next to the same log
 * lines from [EngineInputConnection] (tag AnyPageHost). Typing the same
 * keys into both shows where an IME treats the engine's editor differently.
 *
 * Extras: `type` = "plain" makes the field TYPE_CLASS_TEXT only, as the
 * engine's editor declares itself (default: the EditText's own type).
 */
class ImeProbeActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val plain = intent.getStringExtra("type") == "plain"
        val edit = object : EditText(this) {
            override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
                val ic = super.onCreateInputConnection(outAttrs) ?: return null
                Log.i(TAG, "EditorInfo inputType=0x${outAttrs.inputType.toString(16)} imeOptions=0x${outAttrs.imeOptions.toString(16)} sel=${outAttrs.initialSelStart},${outAttrs.initialSelEnd}")
                return Logged(ic)
            }
        }
        if (plain) edit.inputType = InputType.TYPE_CLASS_TEXT
        edit.textSize = 24f
        edit.gravity = Gravity.TOP or Gravity.START
        setContentView(edit)
        edit.requestFocus()
        // ask for the keyboard explicitly, as EngineView does: with a hardware
        // keyboard attached (the emulator) an EditText's implicit request is ignored
        val imm = getSystemService(InputMethodManager::class.java)
        edit.setOnClickListener { Log.i(TAG, "showSoftInput -> ${imm?.showSoftInput(edit, 0)}") }
    }

    private class Logged(target: InputConnection) : InputConnectionWrapper(target, false) {
        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            Log.i(TAG, "setComposingText(${text?.length} chars)")
            return super.setComposingText(text, newCursorPosition)
        }

        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            Log.i(TAG, "commitText(${text?.length} chars)")
            return super.commitText(text, newCursorPosition)
        }

        override fun finishComposingText(): Boolean {
            Log.i(TAG, "finishComposingText")
            return super.finishComposingText()
        }

        override fun setComposingRegion(start: Int, end: Int): Boolean {
            Log.i(TAG, "setComposingRegion($start, $end)")
            return super.setComposingRegion(start, end)
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            Log.i(TAG, "deleteSurroundingText($beforeLength, $afterLength)")
            return super.deleteSurroundingText(beforeLength, afterLength)
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            Log.i(TAG, "sendKeyEvent(${KeyEvent.keyCodeToString(event.keyCode)}, action ${event.action})")
            return super.sendKeyEvent(event)
        }

        override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence? {
            val r = super.getTextBeforeCursor(n, flags)
            Log.d(TAG, "getTextBeforeCursor($n) -> ${r?.length} chars")
            return r
        }

        override fun getExtractedText(request: ExtractedTextRequest?, flags: Int): ExtractedText? {
            Log.d(TAG, "getExtractedText(flags $flags)")
            return super.getExtractedText(request, flags)
        }

        override fun requestCursorUpdates(cursorUpdateMode: Int): Boolean {
            Log.d(TAG, "requestCursorUpdates($cursorUpdateMode)")
            return super.requestCursorUpdates(cursorUpdateMode)
        }

        override fun beginBatchEdit(): Boolean {
            Log.d(TAG, "beginBatchEdit")
            return super.beginBatchEdit()
        }

        override fun endBatchEdit(): Boolean {
            Log.d(TAG, "endBatchEdit")
            return super.endBatchEdit()
        }
    }

    companion object {
        private const val TAG = "AnyPageImeProbe"
    }
}
