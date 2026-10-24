package com.anypage.engine

import android.app.Activity
import android.os.Bundle

/**
 * The native host (docs/ARCHITECTURE.md section 14.4): it owns the window and
 * lifecycle, the engine owns everything drawn inside [EngineView]. Text input
 * will be native overlays added here (spike 4).
 */
class MainActivity : Activity() {
    private lateinit var view: EngineView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        view = EngineView(this)
        setContentView(view)
    }

    override fun onDestroy() {
        view.release()
        super.onDestroy()
    }
}
