package com.anypage.engine

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.graphics.ImageFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Range
import android.util.Size
import java.util.concurrent.ConcurrentHashMap
import kotlin.math.abs
import kotlin.math.ln

/**
 * Cameras for pages (`media.broadcast { source = "camera" }`): the Kotlin
 * half of `anypage_media::camera::HostCamera` (the Rust half is
 * apps/android/src/camera.rs and bridge.rs).
 *
 * Camera2 straight from the platform (no AndroidX): an [ImageReader] in
 * `YUV_420_888` at the supported size closest to what the page asked for,
 * a repeating request at the closest frame rate. Each image's planes go to
 * Rust as direct buffers (no copy here), where they become I420; the image
 * is closed when that call returns. Everything camera-related runs on one
 * handler thread.
 *
 * Rust calls [cameraOpen] and waits (up to 30 s, the permission dialog
 * included) for [nativeOpened] or [nativeFailed] with the same token.
 */
@SuppressLint("MissingPermission") // openCamera runs only after the CAMERA permission is granted
object CameraBridge {
    private const val TAG = "AnyPageCamera"
    /** Largest picture this host delivers (the encoder caps at 1080p anyway). */
    private const val MAX_AREA = 1920 * 1080

    private lateinit var app: Context
    private lateinit var handler: Handler

    fun init(context: Context) {
        if (::app.isInitialized) return
        app = context.applicationContext
        val t = HandlerThread("anypage-camera").apply { start() }
        handler = Handler(t.looper)
    }

    private fun manager(): CameraManager? = app.getSystemService(CameraManager::class.java)

    /** One open camera; its fields change on the camera thread only, except [closed] checks. */
    private class Cam(val token: Long) {
        var device: CameraDevice? = null
        var session: CameraCaptureSession? = null
        var reader: ImageReader? = null
        @Volatile var opened = false
        @Volatile var closed = false
        var frames = 0L
    }

    private val cams = ConcurrentHashMap<Long, Cam>()

    /** "0: back", "1: front", ... in the order pages count them. */
    fun cameraList(): Array<String> = try {
        val m = manager() ?: return emptyArray()
        m.cameraIdList.map { id ->
            val facing = try {
                when (m.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING)) {
                    CameraCharacteristics.LENS_FACING_BACK -> "back"
                    CameraCharacteristics.LENS_FACING_FRONT -> "front"
                    else -> "external"
                }
            } catch (_: Exception) {
                "camera"
            }
            "$id: $facing"
        }.toTypedArray()
    } catch (e: Exception) {
        Log.w(TAG, "camera list: $e")
        emptyArray()
    }

    /** Start opening camera [index] for [token]: null when started, else why not. */
    fun cameraOpen(token: Long, index: Int, width: Int, height: Int, fps: Int): String? {
        val m = manager() ?: return "this device has no camera service"
        val ids = try {
            m.cameraIdList
        } catch (e: Exception) {
            return "cameras are not available: $e"
        }
        if (ids.isEmpty()) return "this device has no camera (此裝置沒有相機)"
        if (index < 0 || index >= ids.size) return "there is no camera $index (${ids.size} found)"
        val id = ids[index]
        val cam = Cam(token)
        cams[token] = cam
        val perms = arrayOf(Manifest.permission.CAMERA)
        if (Permissions.granted(app, perms)) {
            handler.post { start(cam, m, id, width, height, fps) }
        } else {
            Log.i(TAG, "camera $token: asking for the camera permission")
            Permissions.request(perms) { ok ->
                if (ok) handler.post { start(cam, m, id, width, height, fps) }
                else fail(cam, "camera permission was denied: allow Camera for AnyPage in Settings > Apps (相機權限被拒絕)")
            }
        }
        return null
    }

    fun cameraClose(token: Long) {
        val cam = cams.remove(token) ?: return
        handler.post { release(cam) }
    }

    private fun fail(cam: Cam, message: String) {
        Log.w(TAG, "camera ${cam.token}: $message")
        // only a camera Rust still holds hears about it (not one it closed)
        val alive = cams.remove(cam.token) != null
        handler.post { release(cam) }
        if (alive) nativeFailed(cam.token, message)
    }

    private fun release(cam: Cam) {
        if (cam.closed) return
        cam.closed = true
        try {
            cam.session?.close()
        } catch (_: Exception) {
        }
        try {
            cam.device?.close()
        } catch (_: Exception) {
        }
        try {
            cam.reader?.close()
        } catch (_: Exception) {
        }
        Log.i(TAG, "camera ${cam.token}: closed after ${cam.frames} frames")
    }

    /** The supported size closest to [w] x [h] (area and shape), at most 1080p. */
    private fun choose(sizes: Array<Size>, w: Int, h: Int): Size? {
        val want = (w.coerceAtLeast(16) * h.coerceAtLeast(16)).toDouble()
        val aspect = w.coerceAtLeast(1).toDouble() / h.coerceAtLeast(1)
        return sizes.filter { it.width * it.height <= MAX_AREA }.minByOrNull {
            abs(ln(it.width * it.height / want)) + 2 * abs(ln(it.width.toDouble() / it.height / aspect))
        } ?: sizes.minByOrNull { it.width * it.height }
    }

    /** A range holding [fps], the narrowest (a fixed rate if there is one). */
    private fun fpsRange(ranges: Array<Range<Int>>?, fps: Int): Range<Int>? {
        if (ranges.isNullOrEmpty()) return null
        return ranges.filter { fps in it.lower..it.upper }.minByOrNull { it.upper - it.lower + abs(it.upper - fps) }
            ?: ranges.minByOrNull { abs(it.upper - fps) }
    }

    private fun start(cam: Cam, m: CameraManager, id: String, width: Int, height: Int, fps: Int) {
        if (cam.closed || !cams.containsKey(cam.token)) return
        try {
            val chars = m.getCameraCharacteristics(id)
            val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP) ?: return fail(cam, "camera $id has no stream configurations")
            val size = choose(map.getOutputSizes(ImageFormat.YUV_420_888) ?: emptyArray(), width, height) ?: return fail(cam, "camera $id offers no YUV output")
            val range = fpsRange(chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES), fps)
            Log.i(TAG, "camera ${cam.token}: opening $id at ${size.width}x${size.height}, ${range ?: "default"} fps (asked ${width}x$height at $fps)")
            val reader = ImageReader.newInstance(size.width, size.height, ImageFormat.YUV_420_888, 3)
            cam.reader = reader
            reader.setOnImageAvailableListener({ r -> deliver(cam, r) }, handler)
            m.openCamera(id, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    if (cam.closed) {
                        device.close()
                        return
                    }
                    cam.device = device
                    session(cam, device, reader, size, range)
                }

                override fun onDisconnected(device: CameraDevice) {
                    cam.device = device
                    fail(cam, "the camera was disconnected (another app took it, or the app went to the background)")
                }

                override fun onError(device: CameraDevice, error: Int) {
                    cam.device = device
                    val why = when (error) {
                        ERROR_CAMERA_IN_USE -> "the camera is in use by another app"
                        ERROR_MAX_CAMERAS_IN_USE -> "too many cameras are open"
                        ERROR_CAMERA_DISABLED -> "the camera is disabled by a device policy"
                        ERROR_CAMERA_DEVICE -> "the camera failed"
                        ERROR_CAMERA_SERVICE -> "the camera service failed"
                        else -> "camera error $error"
                    }
                    fail(cam, why)
                }
            }, handler)
        } catch (e: SecurityException) {
            fail(cam, "camera permission missing: ${e.message}")
        } catch (e: Exception) {
            fail(cam, "camera $id: $e")
        }
    }

    @Suppress("DEPRECATION") // createCaptureSession(List, ...) is the API 26 way; SessionConfiguration needs API 28
    private fun session(cam: Cam, device: CameraDevice, reader: ImageReader, size: Size, range: Range<Int>?) {
        try {
            device.createCaptureSession(listOf(reader.surface), object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(s: CameraCaptureSession) {
                    if (cam.closed) {
                        s.close()
                        return
                    }
                    cam.session = s
                    try {
                        val req = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                            addTarget(reader.surface)
                            set(CaptureRequest.CONTROL_MODE, CaptureRequest.CONTROL_MODE_AUTO)
                            if (range != null) set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, range)
                        }
                        s.setRepeatingRequest(req.build(), null, handler)
                        cam.opened = true
                        Log.i(TAG, "camera ${cam.token}: streaming ${size.width}x${size.height}")
                        nativeOpened(cam.token, size.width, size.height)
                    } catch (e: Exception) {
                        fail(cam, "the camera could not start: $e")
                    }
                }

                override fun onConfigureFailed(s: CameraCaptureSession) = fail(cam, "the camera could not be configured for ${size.width}x${size.height}")
            }, handler)
        } catch (e: Exception) {
            fail(cam, "the camera session could not be created: $e")
        }
    }

    private fun deliver(cam: Cam, r: ImageReader) {
        val img = try {
            r.acquireLatestImage()
        } catch (e: IllegalStateException) {
            null
        } ?: return
        try {
            if (!cam.opened || cam.closed) return
            val p = img.planes
            if (p.size < 3) return
            cam.frames++
            val wanted = nativeFrame(
                cam.token, img.width, img.height,
                p[0].buffer, p[0].rowStride, p[1].buffer, p[2].buffer, p[1].rowStride, p[1].pixelStride, img.timestamp,
            )
            if (!wanted) {
                Log.i(TAG, "camera ${cam.token}: nobody reads it any more; closing")
                cams.remove(cam.token)
                release(cam)
            }
        } catch (e: Exception) {
            Log.w(TAG, "camera ${cam.token}: frame: $e")
        } finally {
            img.close()
        }
    }

    // ------------------------------------------------------------ into Rust (bridge.rs)

    @JvmStatic external fun nativeOpened(token: Long, width: Int, height: Int)

    @JvmStatic external fun nativeFailed(token: Long, message: String)

    @JvmStatic external fun nativeFrame(
        token: Long, width: Int, height: Int,
        y: java.nio.ByteBuffer, yRowStride: Int, u: java.nio.ByteBuffer, v: java.nio.ByteBuffer, uvRowStride: Int, uvPixelStride: Int,
        timestampNs: Long,
    ): Boolean
}
