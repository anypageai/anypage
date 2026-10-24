package com.anypage.engine

import android.content.Context
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView

/**
 * A native view that hands its surface to the engine and forwards touches.
 * The engine owns the frame loop; this view never draws.
 */
class EngineView(context: Context) : SurfaceView(context), SurfaceHolder.Callback {
    private var handle: Long = NativeBridge.create(resources.displayMetrics.density)

    init {
        holder.addCallback(this)
        isFocusable = true
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        if (handle != 0L) NativeBridge.surfaceCreated(handle, holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        if (handle != 0L) NativeBridge.surfaceChanged(handle, width, height)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        if (handle != 0L) NativeBridge.surfaceDestroyed(handle)
    }

    override fun onTouchEvent(e: MotionEvent): Boolean {
        if (handle == 0L) return false
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
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

    fun release() {
        if (handle != 0L) {
            NativeBridge.destroy(handle)
            handle = 0L
        }
    }
}
