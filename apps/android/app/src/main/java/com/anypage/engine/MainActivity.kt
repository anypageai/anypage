package com.anypage.engine

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.ActivityManager
import android.content.ActivityNotFoundException
import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ApplicationInfo
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.provider.OpenableColumns
import android.util.Log
import android.view.DragEvent
import android.webkit.MimeTypeMap
import android.widget.Toast
import java.io.File
import java.io.IOException
import java.util.concurrent.Executors

/**
 * The native host (docs/ARCHITECTURE.md section 14.4): it owns the window and
 * lifecycle and serves what only the platform can (clipboard, document
 * pickers, the soft keyboard, Bluetooth, USB, the camera, secrets); the
 * engine owns everything drawn inside [EngineView].
 *
 * Intent extras:
 * - `page` (String): a page package under assets/pages/ ("pdf-demo");
 *   absent: "home" when the APK has it, else the built-in demo; "" the
 *   built-in demo. A new intent with `page` replaces the page.
 * - `hud` (Boolean, default false): the engine's statistics overlay.
 *
 * Pages open other pages with `page.navigate(name)`; Back returns to the
 * page before (and leaves the app from the first one).
 */
class MainActivity : Activity() {
    private lateinit var view: EngineView
    private val main = Handler(Looper.getMainLooper())
    private val pending = HashMap<Int, PendingFile>()
    private var nextRequest = FIRST_REQUEST
    /** Reads page packages off the UI thread, one at a time. */
    private val io = Executors.newSingleThreadExecutor { r -> Thread(r, "anypage-pages") }

    private class PendingFile(val id: Long, val save: Boolean, val name: String)

    /** Called on the render thread; everything is posted to the UI thread. */
    private val host = object : NativeHost {
        override fun onTextFocus(on: Boolean, show: Boolean, x: Float, y: Float, w: Float, h: Float) {
            main.post { if (::view.isInitialized) view.setTextFocus(on, show, x, y, w, h) }
        }

        override fun onCopy(text: String) {
            main.post { copy(text) }
        }

        override fun onPaste() {
            main.post { paste() }
        }

        override fun onNotice(message: String) {
            main.post { Toast.makeText(this@MainActivity, message, Toast.LENGTH_LONG).show() }
        }

        override fun onOpenFile(id: Long, filters: Array<String>) {
            main.post { openFile(id, filters) }
        }

        override fun onSaveFile(id: Long, name: String, size: Long) {
            main.post { saveFile(id, name, size) }
        }

        override fun onNavigate(name: String) {
            main.post {
                PageHistory.navigating()
                open(name, NativeBridge.LOAD_NAVIGATE)
            }
        }

        override fun onPageShown(name: String?, ok: Boolean, id: String, title: String, secrets: Array<String>) {
            main.post {
                PageHistory.shown(PageHistory.Shown(name, ok, id, title, secrets.toList()))
                Log.i(TAG, "showing ${name ?: "the built-in demo"} (${if (ok) title else "failed"}); ${PageHistory.depth} pages to go back to")
                @Suppress("DEPRECATION") // the (String) constructor is the one API 26 has
                if (ok && title.isNotEmpty()) setTaskDescription(ActivityManager.TaskDescription(title))
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // the engine places the caret above the keyboard itself (IME insets);
        // on Android 15 apps targeting 35 are edge-to-edge anyway
        @Suppress("DEPRECATION")
        if (Build.VERSION.SDK_INT >= 30) window.setDecorFitsSystemWindows(false)
        Permissions.attach(this)
        DeviceBridge.init(this)
        CameraBridge.init(this)
        SecretVault.init(this)
        if (!NativeBridge.installPlatform(applicationContext, DeviceBridge, CameraBridge, SecretVault)) {
            Log.e(TAG, "the platform services (devices, camera, secrets) are not available")
        }
        val pageName = PagePackages.startPage(assets, intent.getStringExtra(EXTRA_PAGE))
        Log.i(TAG, "page packages in this APK: ${PagePackages.available(assets)}; showing ${pageName ?: "the built-in demo"}")
        view = EngineView(this, host, PagePackages.read(assets, pageName), intent.getBooleanExtra(EXTRA_HUD, false))
        setContentView(view)
        view.requestFocus()
        registerDebugReceiver()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (intent.hasExtra(EXTRA_PAGE)) {
            val name = intent.getStringExtra(EXTRA_PAGE)
            Log.i(TAG, "switching to ${name ?: "the built-in demo"}")
            open(name, NativeBridge.LOAD_REPLACE)
        }
    }

    /** Read package [name] (null or "": the built-in demo) off the UI thread and show it. */
    private fun open(name: String?, mode: Int) {
        io.execute {
            val t0 = SystemClock.elapsedRealtime()
            val pkg = PagePackages.read(assets, name)
            Log.i(TAG, "read ${name ?: "the built-in demo"}: ${pkg.names.size} files in ${SystemClock.elapsedRealtime() - t0} ms")
            main.post { if (!isDestroyed) view.loadPage(pkg, mode) }
        }
    }

    @Deprecated("Back without predictive back animations (the app does not opt in)")
    override fun onBackPressed() {
        // pages opened with page.navigate go back to the page that opened them
        if (!PageHistory.back { name -> open(name ?: "", NativeBridge.LOAD_NAVIGATE) }) {
            @Suppress("DEPRECATION")
            super.onBackPressed()
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (!Permissions.onResult(requestCode, permissions, grantResults)) {
            super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        }
    }

    override fun onDestroy() {
        debugReceiver?.let { unregisterReceiver(it) }
        debugReceiver = null
        Permissions.detach(this)
        io.shutdownNow()
        view.release()
        super.onDestroy()
    }

    // ------------------------------------------------------------ clipboard

    private val clipboard: ClipboardManager?
        get() = getSystemService(ClipboardManager::class.java)

    private fun copy(text: String) {
        // the engine refuses clips over 256 KB; the system can still fail
        // (TransactionTooLargeException, a dead clipboard service): never crash
        try {
            clipboard?.setPrimaryClip(ClipData.newPlainText("AnyPage", text))
            Log.i(TAG, "clipboard: copied ${text.length} chars")
        } catch (e: RuntimeException) {
            Log.w(TAG, "clipboard: copying ${text.length} chars failed: $e")
            Toast.makeText(this, "Could not copy: the text is too large for the clipboard", Toast.LENGTH_LONG).show()
        }
    }

    private fun paste() {
        val clip = try {
            clipboard?.primaryClip
        } catch (e: RuntimeException) {
            Log.w(TAG, "clipboard: reading failed: $e")
            null
        }
        var text = try {
            if (clip != null && clip.itemCount > 0) clip.getItemAt(0).coerceToText(this)?.toString() ?: "" else ""
        } catch (e: RuntimeException) {
            // coerceToText may open a content URI from another app
            Log.w(TAG, "clipboard: the clip could not be read as text: $e")
            ""
        }
        if (text.length > MAX_PASTE) {
            Log.w(TAG, "clipboard: ${text.length} chars, passing the first $MAX_PASTE")
            text = text.substring(0, MAX_PASTE)
        }
        Log.i(TAG, "clipboard: pasting ${text.length} chars")
        view.paste(text)
    }

    // ------------------------------------------------------------ documents

    /** Extensions to MIME types; null (any document) when one is unknown. */
    private fun mimeTypes(filters: Array<String>): Array<String>? {
        if (filters.isEmpty()) return null
        val mimes = filters.map { f ->
            if (f.contains('/')) f else MimeTypeMap.getSingleton().getMimeTypeFromExtension(f.trimStart('.').lowercase()) ?: return null
        }
        return mimes.distinct().toTypedArray()
    }

    private fun launch(intent: Intent, p: PendingFile) {
        // the engine allows one dialog at a time; a picker of a page that was
        // replaced may still be on screen, so check here too
        if (pending.isNotEmpty()) {
            Log.w(TAG, "file #${p.id}: refused, a document picker is already open")
            view.fileFailed(p.id, "a file dialog is already open")
            return
        }
        val code = nextRequest
        nextRequest = if (nextRequest >= 0xFFFF) FIRST_REQUEST else nextRequest + 1
        pending[code] = p
        try {
            startActivityForResult(intent, code)
        } catch (e: ActivityNotFoundException) {
            pending.remove(code)
            view.fileFailed(p.id, "this device has no document picker")
        } catch (e: RuntimeException) {
            // e.g. an intent too large for a binder transaction
            pending.remove(code)
            Log.w(TAG, "file #${p.id}: the document picker did not start: $e")
            view.fileFailed(p.id, "the document picker could not be opened")
        }
    }

    private fun openFile(id: Long, filters: Array<String>) {
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
            mimeTypes(filters.take(MAX_FILTERS).toTypedArray())?.let { putExtra(Intent.EXTRA_MIME_TYPES, it) }
        }
        Log.i(TAG, "open #$id: document picker for ${filters.size} filters")
        launch(intent, PendingFile(id, false, ""))
    }

    private fun saveFile(id: Long, name: String, size: Long) {
        // the engine sends at most 255 bytes; never more than that to the picker
        val title = if (name.length > MAX_NAME) name.substring(0, MAX_NAME) else name
        val ext = title.substringAfterLast('.', "").lowercase()
        val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
            putExtra(Intent.EXTRA_TITLE, title)
        }
        Log.i(TAG, "save #$id: document picker for ${title.length} chars of name ($size bytes)")
        launch(intent, PendingFile(id, true, title))
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        val p = pending.remove(requestCode)
        if (p == null) {
            super.onActivityResult(requestCode, resultCode, data)
            return
        }
        val uri = data?.data
        if (resultCode != RESULT_OK || uri == null) {
            Log.i(TAG, "file #${p.id}: cancelled")
            view.fileCancelled(p.id)
            return
        }
        // providers may be slow (cloud documents): open off the UI thread
        Thread {
            try {
                val name = displayName(uri) ?: p.name.ifEmpty { "file" }
                val fd = openFd(uri, p.save)
                Log.i(TAG, "file #${p.id}: ${if (p.save) "saving to" else "opening"} $name")
                main.post { if (p.save) view.fileSaveTo(p.id, name, fd) else view.fileOpened(p.id, name, fd) }
            } catch (e: Exception) {
                Log.w(TAG, "file #${p.id}: $e")
                main.post { view.fileFailed(p.id, e.message ?: e.toString()) }
            }
        }.start()
    }

    private fun displayName(uri: Uri): String? = try {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
            if (it.moveToFirst() && !it.isNull(0)) it.getString(0) else null
        }
    } catch (_: Exception) {
        null
    }

    /** A detached descriptor the engine takes over (it reads/writes and closes it). */
    private fun openFd(uri: Uri, write: Boolean): Int {
        val pfd: ParcelFileDescriptor? = if (write) {
            // "wt" truncates; some providers only know "w"
            try {
                contentResolver.openFileDescriptor(uri, "wt")
            } catch (_: Exception) {
                contentResolver.openFileDescriptor(uri, "w")
            }
        } else {
            contentResolver.openFileDescriptor(uri, "r")
        }
        return (pfd ?: throw IOException("the document could not be opened")).detachFd()
    }

    /** Documents dropped on the view (from another app in split screen or a desktop window). */
    fun dropped(ev: DragEvent) {
        val clip = ev.clipData ?: return
        val perms = requestDragAndDropPermissions(ev)
        val uris = (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri }
        Log.i(TAG, "drop: ${uris.size} documents")
        Thread {
            for (uri in uris) {
                try {
                    val name = displayName(uri) ?: uri.lastPathSegment ?: "file"
                    val fd = openFd(uri, false)
                    main.post { view.fileDropped(name, fd) }
                } catch (e: Exception) {
                    Log.w(TAG, "drop: $e")
                }
            }
            perms?.release()
        }.start()
    }

    // ------------------------------------------------------------ test hooks

    private var debugReceiver: BroadcastReceiver? = null

    /**
     * Debug builds only: on-device tests drive the input connection (the
     * same methods an IME calls), the clipboard and drops with a broadcast
     * that only the shell can send (it needs android.permission.DUMP):
     *
     *     am broadcast -a com.anypage.engine.DEBUG --es ops 'compose:ni|commit:你|delete:1'
     *     am broadcast -a com.anypage.engine.DEBUG --es clip 'text for the clipboard'
     *     am broadcast -a com.anypage.engine.DEBUG --es drop notes.txt --es drop_text 'contents'
     *
     * ops: compose:<text>, commit:<text>, finish, delete:<n>, region:<a>,<b>,
     * menu:<copy|paste|cut|all>, key:<engine key name>; cancel:<token> and
     * opened:<token> (a late file-dialog answer), copychars:<n> (the app's
     * clipboard copy with n characters, bypassing the engine's limit).
     *
     * Pages and platform services:
     *
     *     am broadcast -a com.anypage.engine.DEBUG --es act '{"action":"broadcast","params":{"source":"camera"}}'
     *     am broadcast -a com.anypage.engine.DEBUG --es navigate pdf-demo
     *     am broadcast -a com.anypage.engine.DEBUG --ez back true
     *     am broadcast -a com.anypage.engine.DEBUG --es secret openai=sk-... --es scope '*'
     *
     * act: a semantic-port request (answer in logcat, "act ... -> ...");
     * navigate: as `page.navigate`; back: the Back button; secret: store a
     * secret for scope (a page id, default the current page; '*' all pages);
     * `secret name=` removes it.
     */
    // RECEIVER_EXPORTED (API 33) is a constant compiled in; API 26-32 accept the
    // flags argument and ignore that bit, which is what an exported receiver needs
    @SuppressLint("InlinedApi")
    private fun registerDebugReceiver() {
        if (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE == 0) return
        val r = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) = debugCommand(intent)
        }
        // exported (adb's shell sends it), but only to holders of DUMP; the
        // flags argument exists since API 26, and older releases ignore this flag
        registerReceiver(r, IntentFilter(DEBUG_ACTION), Manifest.permission.DUMP, null, Context.RECEIVER_EXPORTED)
        debugReceiver = r
    }

    private fun debugCommand(intent: Intent) {
        val ic = view.debugConnection()
        intent.getStringExtra("ops")?.split('|')?.forEach { op ->
            val name = op.substringBefore(':')
            val arg = op.substringAfter(':', "")
            val ok: Any = when (name) {
                "compose" -> ic.setComposingText(arg, 1)
                "commit" -> ic.commitText(arg, 1)
                "finish" -> ic.finishComposingText()
                "delete" -> ic.deleteSurroundingText(arg.toIntOrNull() ?: 1, 0)
                "region" -> arg.split(',').mapNotNull { it.trim().toIntOrNull() }.let { it.size == 2 && ic.setComposingRegion(it[0], it[1]) }
                "menu" -> ic.performContextMenuAction(
                    when (arg) {
                        "paste" -> android.R.id.paste
                        "cut" -> android.R.id.cut
                        "all" -> android.R.id.selectAll
                        else -> android.R.id.copy
                    },
                )
                "key" -> view.sendKeyName(arg, 1)
                // a late answer for dialog token <arg>, as if its picker returned now
                "cancel" -> arg.toLongOrNull()?.let { view.fileCancelled(it) } ?: "bad token"
                "opened" -> arg.toLongOrNull()?.let { token ->
                    val f = File(cacheDir, "late-answer.txt").apply { writeText("late") }
                    view.fileOpened(token, f.name, ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY).detachFd())
                } ?: "bad token"
                // put <arg> characters on the clipboard through the app's own copy path
                "copychars" -> copy("z".repeat(arg.toIntOrNull()?.coerceIn(0, 8 * 1024 * 1024) ?: 0))
                else -> "unknown op"
            }
            Log.i(TAG, "debug op $name -> $ok")
        }
        intent.getStringExtra("clip")?.let {
            clipboard?.setPrimaryClip(ClipData.newPlainText("debug", it))
            Log.i(TAG, "debug: clipboard set (${it.length} chars)")
        }
        intent.getStringExtra("drop")?.let { name ->
            val f = File(cacheDir, "drop-" + name.replace(Regex("[^A-Za-z0-9._-]"), "_"))
            f.writeText(intent.getStringExtra("drop_text") ?: "dropped")
            val fd = ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY).detachFd()
            Log.i(TAG, "debug: dropping $name (${f.length()} bytes)")
            view.fileDropped(name, fd)
        }
        intent.getStringExtra("act")?.let {
            Log.i(TAG, "debug: act ${it.take(200)}")
            view.act(it)
        }
        intent.getStringExtra("navigate")?.let { host.onNavigate(it) }
        if (intent.getBooleanExtra("back", false)) {
            @Suppress("DEPRECATION")
            onBackPressed()
        }
        intent.getStringExtra("secret")?.let { s ->
            val name = s.substringBefore('=')
            val value = s.substringAfter('=', "")
            val scope = intent.getStringExtra("scope") ?: PageHistory.current?.id?.takeIf { it.isNotEmpty() } ?: SecretVault.ALL_PAGES
            if (value.isEmpty()) SecretVault.remove(scope, name) else SecretVault.put(scope, name, value)
            Log.i(TAG, "debug: secret \"$name\" for $scope ${if (value.isEmpty()) "removed" else "set (${value.length} chars)"}")
        }
        // what a page gets through the engine's secret store (length only)
        intent.getStringExtra("secret_probe")?.let { name ->
            val page = intent.getStringExtra("scope") ?: PageHistory.current?.id ?: ""
            Log.i(TAG, "debug: secret \"$name\" for page $page: ${NativeBridge.secretProbe(page, name).let { if (it < 0) "not set" else "$it chars" }}")
        }
    }

    companion object {
        private const val TAG = "AnyPageHost"
        const val EXTRA_PAGE = "page"
        const val EXTRA_HUD = "hud"
        private const val DEBUG_ACTION = "com.anypage.engine.DEBUG"
        private const val FIRST_REQUEST = 0x4100
        /** Characters of clipboard text handed to a page at once. */
        private const val MAX_PASTE = 4 * 1024 * 1024
        /** Same limits as the engine's (apps/android/src/requests.rs). */
        private const val MAX_NAME = 255
        private const val MAX_FILTERS = 32
    }
}
