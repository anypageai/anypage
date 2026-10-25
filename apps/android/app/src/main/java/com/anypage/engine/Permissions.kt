package com.anypage.engine

import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log

/**
 * Runtime permission requests for the platform bridges (Bluetooth, camera),
 * which ask from engine threads. Android shows one permission dialog at a
 * time and delivers the answer to the activity, so requests are queued here
 * and answered on the UI thread.
 *
 * A group the user denied is not asked again for [DENIED_QUIET_MS] (the
 * Bluetooth scan retries every few seconds; it must not bring the dialog
 * back each time). Android itself stops showing the dialog after a second
 * denial; the bridges then say where to allow it in Settings.
 */
object Permissions {
    private const val TAG = "AnyPagePerm"
    const val REQUEST_CODE = 0x5100
    private const val DENIED_QUIET_MS = 60_000L

    private val main = Handler(Looper.getMainLooper())
    private class Request(val perms: Array<String>, val done: (Boolean) -> Unit)

    private val queue = ArrayDeque<Request>()
    private var asking: Request? = null
    /** When each permission was last denied (elapsedRealtime). */
    private val deniedAt = HashMap<String, Long>()

    /** The activity that shows the dialogs (set while it exists). */
    @Volatile
    private var activity: Activity? = null

    fun attach(a: Activity) {
        main.post {
            activity = a
            next()
        }
    }

    fun detach(a: Activity) {
        main.post {
            if (activity !== a) return@post
            activity = null
            // the dialog went with the activity: nobody will answer
            asking?.done?.invoke(false)
            asking = null
            while (queue.isNotEmpty()) queue.removeFirst().done(false)
        }
    }

    fun granted(context: Context, perms: Array<String>): Boolean =
        perms.all { context.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

    /** True when one of [perms] was denied less than a minute ago. */
    @Synchronized
    fun recentlyDenied(perms: Array<String>): Boolean {
        val now = SystemClock.elapsedRealtime()
        return perms.any { p -> deniedAt[p]?.let { now - it < DENIED_QUIET_MS } ?: false }
    }

    /** Ask for [perms] (from any thread); [done] runs on the UI thread with the answer. */
    fun request(perms: Array<String>, done: (Boolean) -> Unit) {
        main.post {
            queue.addLast(Request(perms, done))
            next()
        }
    }

    private fun next() {
        if (asking != null) return
        while (queue.isNotEmpty()) {
            val r = queue.removeFirst()
            val a = activity
            if (a == null) {
                Log.w(TAG, "no activity to ask for ${r.perms.joinToString()}")
                r.done(false)
                continue
            }
            if (granted(a, r.perms)) {
                r.done(true)
                continue
            }
            asking = r
            Log.i(TAG, "asking for ${r.perms.joinToString()}")
            a.requestPermissions(r.perms, REQUEST_CODE)
            return
        }
    }

    /** From the activity's onRequestPermissionsResult; true if it was ours. */
    fun onResult(code: Int, perms: Array<out String>, results: IntArray): Boolean {
        if (code != REQUEST_CODE) return false
        val r = asking ?: return true
        asking = null
        // an empty answer means the request was interrupted
        val ok = results.isNotEmpty() && results.all { it == PackageManager.PERMISSION_GRANTED }
        if (!ok) {
            synchronized(this) {
                val now = SystemClock.elapsedRealtime()
                for (p in r.perms) deniedAt[p] = now
            }
        }
        Log.i(TAG, "${perms.joinToString()}: ${if (ok) "granted" else "denied"}")
        r.done(ok)
        next()
        return true
    }
}
