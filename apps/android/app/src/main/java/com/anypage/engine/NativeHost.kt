package com.anypage.engine

/**
 * What the engine asks of the app. Called from native code on the engine's
 * render thread: implementations post to the UI thread and return at once,
 * never blocking (the UI thread may be waiting for the render thread).
 *
 * The method names and signatures are looked up from Rust
 * (apps/android/src/host.rs); keep them in step.
 */
interface NativeHost {
    /**
     * The page's caret, in view pixels, while it wants text input ([on]);
     * [show]: bring up the soft keyboard (focus gained, or the field was
     * tapped again after the keyboard was dismissed).
     */
    fun onTextFocus(on: Boolean, show: Boolean, x: Float, y: Float, w: Float, h: Float)

    /** Put [text] on the clipboard (at most 256 KB; the engine checks). */
    fun onCopy(text: String)

    /** Read the clipboard and answer with [NativeBridge.paste]. */
    fun onPaste()

    /** Tell the user something briefly (a request the engine refused). */
    fun onNotice(message: String)

    /**
     * Ask the user for a document; answer with [NativeBridge.fileOpened],
     * [NativeBridge.fileCancelled] or [NativeBridge.fileFailed], passing
     * [id] back (a dialog token, not the page's request id).
     * [filters] are file extensions ("pdf") or MIME types ("image/png").
     */
    fun onOpenFile(id: Long, filters: Array<String>)

    /**
     * Ask the user where to save [name] ([size] bytes, kept by the engine);
     * answer with [NativeBridge.fileSaveTo], [NativeBridge.fileCancelled] or
     * [NativeBridge.fileFailed], passing [id] back.
     */
    fun onSaveFile(id: Long, name: String, size: Long)

    /**
     * The page asked to open the package [name] (`page.navigate`, a valid
     * folder name): read assets/pages/<name>/ off the UI thread and answer
     * with [NativeBridge.loadPage] in navigate mode.
     */
    fun onNavigate(name: String)

    /**
     * The engine shows a page now: package [name] (null: the built-in demo);
     * [ok] false when it failed and an error page stands in for it. [id],
     * [title] and the [secrets] it declares come from its page.json.
     */
    fun onPageShown(name: String?, ok: Boolean, id: String, title: String, secrets: Array<String>)
}
