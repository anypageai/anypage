package com.anypage.engine

import android.view.Surface

/** The engine, as a library. Implemented in Rust (apps/android/src/lib.rs). */
object NativeBridge {
    init {
        System.loadLibrary("anypage_android")
    }

    /** Starts the engine's render thread. Returns an opaque handle. */
    @JvmStatic external fun create(density: Float): Long

    @JvmStatic external fun surfaceCreated(handle: Long, surface: Surface)

    @JvmStatic external fun surfaceChanged(handle: Long, width: Int, height: Int)

    /** Blocks until the engine has let go of the surface. */
    @JvmStatic external fun surfaceDestroyed(handle: Long)

    /** action: 0 down, 1 move, 2 up, 3 cancel. Physical pixels. */
    @JvmStatic external fun touch(handle: Long, action: Int, id: Int, x: Float, y: Float)

    @JvmStatic external fun destroy(handle: Long)
}
