package com.anypage.engine

import android.content.res.AssetManager
import android.util.Log
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream

/** A page package's files as read from the APK: [name] null means the built-in demo. */
class PagePackage(val name: String?, val names: Array<String>, val contents: Array<ByteArray>)

/**
 * Page packages shipped in the APK under assets/pages/<name>/ (Gradle's
 * `syncPages` copies the repository's pages/ folder there). The files are
 * read here and validated by the engine (apps/android/src/packages.rs):
 * page.json, .luau, .wgsl, .txt, .md at the top (UTF-8, at most 2 MB each)
 * and binary samples under assets/ (passed as "assets/<file>", at most
 * 64 MB each and 256 MB in all, read by pages with `page.asset`).
 *
 * When a package cannot be read (too large for memory, an I/O error) the
 * files carry one entry named "!error" whose bytes say why; the engine
 * shows or reports that instead of loading.
 */
object PagePackages {
    private const val TAG = "AnyPageHost"
    private const val DIR = "pages"
    private const val ASSETS = "assets"
    /** The page the app opens when the intent names none, if the APK has it. */
    const val START = "home"
    const val READ_ERROR = "!error"
    private const val MAX_FILE = 2 * 1024 * 1024
    private const val MAX_ASSET = 64 * 1024 * 1024
    private const val MAX_ASSETS = 256L * 1024 * 1024
    private val NAME = Regex("[A-Za-z0-9_][A-Za-z0-9._-]{0,63}")

    fun builtin() = PagePackage(null, emptyArray(), emptyArray())

    /** The package [name], or the built-in demo when [name] is null or empty. */
    fun read(assets: AssetManager, name: String?): PagePackage {
        if (name.isNullOrEmpty()) return builtin()
        // the engine reports a bad or missing name on the page it shows
        if (!name.matches(NAME)) return PagePackage(name, emptyArray(), emptyArray())
        val names = ArrayList<String>()
        val contents = ArrayList<ByteArray>()
        try {
            for (f in list(assets, "$DIR/$name")) {
                if (f == ASSETS) continue
                try {
                    contents.add(assets.open("$DIR/$name/$f").use { readUpTo(it, MAX_FILE + 1) })
                    names.add(f)
                } catch (_: IOException) {
                    // a sub-folder or an unreadable entry: packages are flat
                }
            }
            var total = 0L
            for (f in list(assets, "$DIR/$name/$ASSETS")) {
                val bytes = try {
                    assets.open("$DIR/$name/$ASSETS/$f").use { readUpTo(it, MAX_ASSET + 1) }
                } catch (_: IOException) {
                    continue
                }
                // over the limit: the engine refuses it by size, with the name
                total += bytes.size
                if (total > MAX_ASSETS) return failed(name, "assets larger than ${MAX_ASSETS shr 20} MB in total")
                names.add("$ASSETS/$f")
                contents.add(bytes)
            }
        } catch (e: OutOfMemoryError) {
            Log.e(TAG, "$name: out of memory reading the package")
            return failed(name, "the package's files do not fit in memory on this device")
        }
        return PagePackage(name, names.toTypedArray(), contents.toTypedArray())
    }

    private fun failed(name: String, why: String) = PagePackage(name, arrayOf(READ_ERROR), arrayOf(why.toByteArray()))

    private fun list(assets: AssetManager, path: String): Array<String> = try {
        assets.list(path) ?: emptyArray()
    } catch (_: IOException) {
        emptyArray()
    }

    /** Names of the packages in the APK. */
    fun available(assets: AssetManager): List<String> = list(assets, DIR).sorted()

    /** The page to start with: the intent's choice, else [START] when the APK has it, else the built-in demo (null). */
    fun startPage(assets: AssetManager, requested: String?): String? = requested ?: START.takeIf { it in available(assets) }

    /** At most [limit] bytes; a longer file arrives cut and the engine refuses it by size. */
    private fun readUpTo(input: InputStream, limit: Int): ByteArray {
        val out = ByteArrayOutputStream()
        val buf = ByteArray(64 * 1024)
        while (out.size() < limit) {
            val n = input.read(buf, 0, minOf(buf.size, limit - out.size()))
            if (n < 0) break
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }
}
