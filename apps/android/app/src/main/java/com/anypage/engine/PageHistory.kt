package com.anypage.engine

/**
 * The page on screen and the pages before it, for Back. The engine reports
 * every page it shows ([shown], from `NativeHost.onPageShown`); a page that
 * opened another one (`page.navigate`) goes on the back stack. UI thread only,
 * except [current], which the secrets screen reads.
 */
object PageHistory {
    /** What the engine shows: [name] null is the built-in demo; [ok] false: an error page for it. */
    class Shown(val name: String?, val ok: Boolean, val id: String, val title: String, val secrets: List<String>)

    private const val MAX = 32

    @Volatile
    var current: Shown? = null
        private set

    /** Package names (null: the built-in demo). */
    private val back = ArrayList<String?>()
    private var goingBack = false

    fun shown(s: Shown) {
        val prev = current
        current = s
        if (goingBack) {
            goingBack = false
            return
        }
        if (prev != null && prev.name != s.name) {
            back.add(prev.name)
            if (back.size > MAX) back.removeAt(0)
        }
    }

    /** A page asked to open another one: whatever Back was doing is over. */
    fun navigating() {
        goingBack = false
    }

    /** True when there is a page to go back to; [open] gets its name (null: the built-in demo). */
    fun back(open: (String?) -> Unit): Boolean {
        if (back.isEmpty()) return false
        val name = back.removeAt(back.size - 1)
        goingBack = true
        open(name)
        return true
    }

    val depth: Int
        get() = back.size
}
