package com.anypage.engine

import android.content.Context
import android.view.Surface

/**
 * The engine, as a library. Implemented in Rust (apps/android/src/host.rs).
 *
 * Call everything on the UI thread, between [create] and [destroy]. Each call
 * only queues work for the engine's render thread and returns at once
 * (except [surfaceDestroyed], which waits until the engine lets go of the
 * surface, and [destroy]).
 */
object NativeBridge {
    init {
        System.loadLibrary("anypage_android")
    }

    /**
     * Starts the engine's render thread. Returns an opaque handle (0 on failure).
     *
     * @param host called back on the render thread (see [NativeHost])
     * @param page a package folder under assets/pages/, or null for the built-in demo
     * @param names file names in that folder, [contents] their bytes (same order)
     */
    @JvmStatic external fun create(
        density: Float,
        host: NativeHost,
        page: String?,
        names: Array<String>,
        contents: Array<ByteArray>,
        hud: Boolean,
    ): Long

    @JvmStatic external fun surfaceCreated(handle: Long, surface: Surface)

    @JvmStatic external fun surfaceChanged(handle: Long, width: Int, height: Int)

    /** Blocks until the engine has let go of the surface. */
    @JvmStatic external fun surfaceDestroyed(handle: Long)

    /** action: 0 down, 1 move, 2 up, 3 cancel, 4 hover (mouse, no button). Physical pixels. */
    @JvmStatic external fun touch(handle: Long, action: Int, id: Int, x: Float, y: Float)

    /** Mouse wheel / trackpad scroll in lines at (x, y); positive dy = wheel up. */
    @JvmStatic external fun wheel(handle: Long, x: Float, y: Float, dx: Float, dy: Float)

    /**
     * A key press. [plain] is the character the key types with Shift but
     * without Ctrl/Alt/Meta; [alt], when right Alt (AltGr) is held without
     * Ctrl or Meta, the character of the layout's Alt level ('@' on a German
     * Q), else 0.
     * Returns whether the engine takes the key.
     */
    @JvmStatic external fun key(handle: Long, keyCode: Int, meta: Int, plain: Int, alt: Int): Boolean

    /** A key by the engine's name ("backspace", "enter", "ctrl+v"), [count] times. */
    @JvmStatic external fun keyName(handle: Long, name: String, count: Int)

    /** Committed text. */
    @JvmStatic external fun text(handle: Long, text: String)

    /** The composition in progress; "" when it ends. */
    @JvmStatic external fun preedit(handle: Long, text: String)

    /** Clipboard text the page asked for (answer to [NativeHost.onPaste]). */
    @JvmStatic external fun paste(handle: Long, text: String)

    /** Answer to [NativeHost.onOpenFile]: a detached descriptor the engine reads and closes. */
    @JvmStatic external fun fileOpened(handle: Long, id: Long, name: String, fd: Int)

    /** Answer to [NativeHost.onSaveFile]: a detached descriptor the engine writes and closes. */
    @JvmStatic external fun fileSaveTo(handle: Long, id: Long, name: String, fd: Int)

    @JvmStatic external fun fileCancelled(handle: Long, id: Long)

    @JvmStatic external fun fileFailed(handle: Long, id: Long, message: String)

    /** A document dropped on the view: a detached descriptor the engine reads and closes. */
    @JvmStatic external fun fileDropped(handle: Long, name: String, fd: Int)

    /** Pixels at the bottom of the view the soft keyboard covers. */
    @JvmStatic external fun imeInset(handle: Long, bottom: Float)

    /**
     * Show another page (same arguments as in [create]). [mode]
     * [LOAD_REPLACE]: the page is replaced, by an error page if it fails;
     * [LOAD_NAVIGATE]: a page that fails to load leaves the current one.
     */
    @JvmStatic external fun loadPage(handle: Long, page: String?, names: Array<String>, contents: Array<ByteArray>, mode: Int)

    /** A semantic-port request for the page (JSON); the answer is logged. */
    @JvmStatic external fun act(handle: Long, request: String)

    @JvmStatic external fun destroy(handle: Long)

    /**
     * Lend the platform to pages, once per process: Bluetooth LE and USB
     * ([devices]), the camera ([camera]) and the secret store ([secrets]).
     * Returns false if something could not be installed (logged).
     */
    @JvmStatic external fun installPlatform(context: Context, devices: DeviceBridge, camera: CameraBridge, secrets: SecretVault): Boolean

    /** Debug tools: the length of secret [name] as page [pageId] gets it through the engine, -1 when unset. */
    @JvmStatic external fun secretProbe(pageId: String, name: String): Int

    const val LOAD_REPLACE = 0
    const val LOAD_NAVIGATE = 1
}
