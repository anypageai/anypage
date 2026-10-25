package com.anypage.engine

import android.content.Context
import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Secrets pages declare (`capabilities.secrets`: API keys, cloud
 * credentials), kept for native modules: the Kotlin half of
 * `anypage_runtime::SecretStore` (apps/android/src/bridge.rs).
 *
 * Values live in app-private preferences, each encrypted with AES-256-GCM
 * under a key that never leaves the Android Keystore; the entry's name is
 * the authenticated data, so a value cannot be moved to another entry. A
 * secret belongs to one page (by its id) or to every page (scope `*`); a
 * page's own entry wins, as on the desktop (`secrets.json`).
 *
 * Page scripts never see the values: native modules ask for them by name
 * when they send a request (cloud chat, S3, GCS), and only for hosts the
 * page binds the secret to. Nothing here logs a value.
 */
object SecretVault {
    private const val TAG = "AnyPageSecrets"
    private const val PREFS = "anypage-secrets"
    private const val KEY_ALIAS = "anypage-secrets-v1"
    private const val SEP = '\u001f'
    const val ALL_PAGES = "*"
    /** Longest value accepted (a service-account JSON is a few KB). */
    const val MAX_VALUE = 64 * 1024

    private lateinit var app: Context

    fun init(context: Context) {
        if (!::app.isInitialized) app = context.applicationContext
    }

    private fun prefs(): SharedPreferences = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    private fun entry(scope: String, name: String) = "$scope$SEP$name"

    @Synchronized
    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        Log.i(TAG, "created the secrets key in the Android Keystore")
        return gen.generateKey()
    }

    private fun encrypt(entry: String, value: String): String {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        c.updateAAD(entry.toByteArray(Charsets.UTF_8))
        val ct = c.doFinal(value.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(c.iv, Base64.NO_WRAP) + ":" + Base64.encodeToString(ct, Base64.NO_WRAP)
    }

    private fun decrypt(entry: String, stored: String): String? = try {
        val (iv, ct) = stored.split(':', limit = 2).let { Base64.decode(it[0], Base64.NO_WRAP) to Base64.decode(it[1], Base64.NO_WRAP) }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, iv))
        c.updateAAD(entry.toByteArray(Charsets.UTF_8))
        String(c.doFinal(ct), Charsets.UTF_8)
    } catch (e: Exception) {
        // e.g. restored from a backup without the Keystore key: treat as unset
        Log.w(TAG, "an entry could not be decrypted (${e.javaClass.simpleName}); ignoring it")
        null
    }

    /** For [pageId]: its own entry, else the one for all pages. Called from engine threads. */
    fun get(pageId: String, name: String): String? {
        if (!::app.isInitialized) return null
        for (scope in listOf(pageId, ALL_PAGES)) {
            val e = entry(scope, name)
            val stored = prefs().getString(e, null) ?: continue
            decrypt(e, stored)?.let { return it }
        }
        return null
    }

    /** Store [value] for [scope] (a page id or [ALL_PAGES]); false if it could not be encrypted. */
    fun put(scope: String, name: String, value: String): Boolean {
        if (value.length > MAX_VALUE || name.isEmpty() || scope.isEmpty()) return false
        return try {
            val e = entry(scope, name)
            prefs().edit().putString(e, encrypt(e, value)).apply()
            Log.i(TAG, "stored secret \"$name\" for $scope (${value.length} chars)")
            true
        } catch (e: Exception) {
            Log.w(TAG, "storing secret \"$name\" failed: $e")
            false
        }
    }

    fun remove(scope: String, name: String) {
        prefs().edit().remove(entry(scope, name)).apply()
        Log.i(TAG, "removed secret \"$name\" for $scope")
    }

    fun has(scope: String, name: String): Boolean = prefs().contains(entry(scope, name))

    /** Every stored entry as (scope, name), values not read. */
    fun entries(): List<Pair<String, String>> = prefs().all.keys.mapNotNull { k ->
        val i = k.indexOf(SEP)
        if (i <= 0) null else k.substring(0, i) to k.substring(i + 1)
    }.sortedWith(compareBy({ it.first }, { it.second }))
}
