package com.anypage.engine

import android.Manifest
import android.annotation.SuppressLint
import android.app.PendingIntent
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.BluetoothStatusCodes
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.location.LocationManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.ParcelUuid
import android.util.Log
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Bluetooth LE and USB for pages: the Kotlin half of
 * `anypage_devices::AndroidDevices` (crates/anypage-devices/src/android.rs;
 * the Rust half is apps/android/src/bridge.rs).
 *
 * Rust calls the public methods from its own threads. Each returns at once:
 * null when the operation started (its result comes back through a
 * `native*` callback) or a message saying why it could not start. The
 * device module allows one GATT operation per connection at a time, so one
 * pending operation id per link is enough here.
 *
 * Permissions: "Nearby devices" (BLUETOOTH_SCAN + BLUETOOTH_CONNECT) on
 * Android 12+, fine location before (for scanning), asked at the first scan
 * or connect; the call fails with a message meanwhile and the page's scan
 * retries on its own. USB needs no runtime permission, only the system's
 * per-device dialog, which `usbRequestPermission` brings up.
 *
 * Lists cross to Rust as text (apps/android/src/devices.rs describes the
 * format): lines of tab-separated fields, errors as a line starting with '!'.
 */
@SuppressLint("MissingPermission") // every Bluetooth call is behind ensureBle and catches SecurityException
object DeviceBridge {
    private const val TAG = "AnyPageDevices"
    private const val ACTION_USB_PERMISSION = "com.anypage.engine.USB_PERMISSION"
    private const val MTU = 517
    private const val MTU_WAIT_MS = 2000L
    private val CCCD: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

    private lateinit var app: Context
    private val main = Handler(Looper.getMainLooper())
    private var started = false

    fun init(context: Context) {
        if (started) return
        started = true
        app = context.applicationContext
        val filter = IntentFilter().apply {
            addAction(ACTION_USB_PERMISSION)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
        }
        // not exported: our own permission answers and the system's detach broadcasts only
        app.registerReceiver(usbReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
    }

    // ------------------------------------------------------------ Bluetooth LE

    private fun adapter(): BluetoothAdapter? = app.getSystemService(BluetoothManager::class.java)?.adapter

    /** A Bluetooth permission dialog is on its way or on screen. */
    private val asking = AtomicBoolean(false)

    private fun blePermissions(scan: Boolean): Array<String> = when {
        Build.VERSION.SDK_INT >= 31 && scan -> arrayOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT)
        Build.VERSION.SDK_INT >= 31 -> arrayOf(Manifest.permission.BLUETOOTH_CONNECT)
        scan -> arrayOf(Manifest.permission.ACCESS_FINE_LOCATION)
        else -> emptyArray()
    }

    private fun locationOn(): Boolean = try {
        app.getSystemService(LocationManager::class.java)?.let {
            if (Build.VERSION.SDK_INT >= 28) it.isLocationEnabled else it.isProviderEnabled(LocationManager.NETWORK_PROVIDER) || it.isProviderEnabled(LocationManager.GPS_PROVIDER)
        } ?: false
    } catch (_: Exception) {
        true
    }

    /** Null when Bluetooth LE can be used now; else why not (and the permission dialog is on its way). */
    private fun ensureBle(scan: Boolean): String? {
        if (!app.packageManager.hasSystemFeature(PackageManager.FEATURE_BLUETOOTH_LE)) return "this device has no Bluetooth LE (此裝置沒有藍牙 LE)"
        val adapter = adapter() ?: return "this device has no Bluetooth adapter (此裝置沒有藍牙)"
        val perms = blePermissions(scan)
        if (!Permissions.granted(app, perms)) {
            if (Permissions.recentlyDenied(perms)) {
                return if (Build.VERSION.SDK_INT >= 31) "Bluetooth permission was denied: allow \"Nearby devices\" for AnyPage in Settings > Apps (藍牙權限被拒絕)"
                else "location permission was denied; Android needs it to scan for Bluetooth devices (定位權限被拒絕)"
            }
            // one dialog at a time: the page's scan retries every few seconds meanwhile
            if (!asking.getAndSet(true)) {
                Permissions.request(perms) { ok ->
                    asking.set(false)
                    Log.i(TAG, "Bluetooth permission ${if (ok) "granted" else "denied"}")
                }
            }
            return "waiting for Bluetooth permission: allow it in the dialog (請在對話框中允許藍牙權限)"
        }
        if (!adapter.isEnabled) return "Bluetooth is off: turn it on in Settings (藍牙已關閉)"
        if (scan && Build.VERSION.SDK_INT < 31 && !locationOn()) return "turn Location on: this Android version finds Bluetooth devices only with it (請開啟定位)"
        return null
    }

    private fun failure(what: String, e: Exception): String {
        Log.w(TAG, "$what: $e")
        return if (e is SecurityException) "$what: permission missing (${e.message})" else "$what: $e"
    }

    @Volatile
    private var scanning = false

    private val scanCallback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult) = report(result)

        override fun onBatchScanResults(results: MutableList<ScanResult>) = results.forEach { report(it) }

        override fun onScanFailed(errorCode: Int) {
            scanning = false
            Log.w(TAG, "scan failed: $errorCode")
            nativeScanFailed(errorCode)
        }
    }

    private fun report(r: ScanResult) {
        try {
            val rec = r.scanRecord
            val name = rec?.deviceName ?: try {
                r.device.name
            } catch (_: SecurityException) {
                null
            }
            val services = rec?.serviceUuids?.map { it.uuid.toString() }?.toTypedArray() ?: emptyArray()
            val mfg = rec?.manufacturerSpecificData
            val n = mfg?.size() ?: 0
            val ids = IntArray(n) { mfg!!.keyAt(it) }
            val data = Array(n) { mfg!!.valueAt(it) ?: ByteArray(0) }
            val sd = rec?.serviceData?.entries?.toList() ?: emptyList()
            nativeScanResult(
                r.device.address, name, r.rssi, rec?.txPowerLevel ?: Int.MIN_VALUE, if (r.isConnectable) 1 else 0,
                services, ids, data, sd.map { it.key.uuid.toString() }.toTypedArray(), sd.map { it.value ?: ByteArray(0) }.toTypedArray(),
            )
        } catch (e: Exception) {
            Log.w(TAG, "scan result: $e")
        }
    }

    /** Start the one scan (services: full UUIDs; empty: every device). */
    fun bleStartScan(services: Array<String>): String? {
        ensureBle(true)?.let { return it }
        return try {
            val scanner = adapter()?.bluetoothLeScanner ?: return "Bluetooth LE scanning is not available (is Bluetooth on?)"
            val filters = services.mapNotNull { s ->
                try {
                    ScanFilter.Builder().setServiceUuid(ParcelUuid.fromString(s)).build()
                } catch (_: IllegalArgumentException) {
                    null
                }
            }
            val settings = ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY).build()
            synchronized(this) {
                if (scanning) scanner.stopScan(scanCallback)
                scanner.startScan(filters, settings, scanCallback)
                scanning = true
            }
            Log.i(TAG, "scanning for ${if (filters.isEmpty()) "all devices" else "${filters.size} services"}")
            null
        } catch (e: Exception) {
            failure("scan", e)
        }
    }

    fun bleStopScan() {
        try {
            synchronized(this) {
                if (scanning) adapter()?.bluetoothLeScanner?.stopScan(scanCallback)
                scanning = false
            }
        } catch (e: Exception) {
            Log.w(TAG, "stop scan: $e")
        }
    }

    private val links = ConcurrentHashMap<Long, Link>()

    /** One GATT connection, its callbacks and the operation in flight. */
    private class Link(val id: Long) : BluetoothGattCallback() {
        @Volatile var gatt: BluetoothGatt? = null
        @Volatile var op = 0L
        @Volatile var reported = false
        @Volatile var closed = false

        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            if (closed) return
            if (newState == BluetoothProfile.STATE_CONNECTED && status == BluetoothGatt.GATT_SUCCESS) {
                // a larger MTU first (one GATT operation at a time), then report the link
                val asked = try {
                    g.requestMtu(MTU)
                } catch (_: Exception) {
                    false
                }
                if (asked) main.postDelayed({ reportConnected() }, MTU_WAIT_MS) else reportConnected()
            } else if (newState == BluetoothProfile.STATE_DISCONNECTED || status != BluetoothGatt.GATT_SUCCESS) {
                Log.i(TAG, "link $id: disconnected (status $status)")
                complete(GATT_FAILURE, null)
                nativeConnectionState(id, false, status)
                close()
            }
        }

        override fun onMtuChanged(g: BluetoothGatt, mtu: Int, status: Int) {
            Log.i(TAG, "link $id: MTU $mtu (status $status)")
            reportConnected()
        }

        private fun reportConnected() {
            if (!reported && !closed) {
                reported = true
                Log.i(TAG, "link $id: connected")
                nativeConnectionState(id, true, 0)
            }
        }

        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            val sb = StringBuilder()
            try {
                for (s in g.services) {
                    sb.append("S\t").append(s.uuid).append('\n')
                    for (c in s.characteristics) sb.append("C\t").append(c.uuid).append('\t').append(c.properties).append('\n')
                }
            } catch (e: Exception) {
                Log.w(TAG, "link $id: services: $e")
            }
            nativeServicesDiscovered(id, status, sb.toString())
        }

        @Deprecated("Android 12 and older")
        override fun onCharacteristicRead(g: BluetoothGatt, c: BluetoothGattCharacteristic, status: Int) {
            @Suppress("DEPRECATION")
            complete(status, c.value)
        }

        override fun onCharacteristicRead(g: BluetoothGatt, c: BluetoothGattCharacteristic, value: ByteArray, status: Int) = complete(status, value)

        override fun onCharacteristicWrite(g: BluetoothGatt, c: BluetoothGattCharacteristic, status: Int) = complete(status, null)

        override fun onDescriptorWrite(g: BluetoothGatt, d: BluetoothGattDescriptor, status: Int) = complete(status, null)

        @Deprecated("Android 12 and older")
        override fun onCharacteristicChanged(g: BluetoothGatt, c: BluetoothGattCharacteristic) {
            @Suppress("DEPRECATION")
            nativeNotification(id, c.service.uuid.toString(), c.uuid.toString(), c.value)
        }

        override fun onCharacteristicChanged(g: BluetoothGatt, c: BluetoothGattCharacteristic, value: ByteArray) =
            nativeNotification(id, c.service.uuid.toString(), c.uuid.toString(), value)

        fun complete(status: Int, value: ByteArray?) {
            val o = op
            op = 0L
            if (o != 0L) nativeOpComplete(o, status, value)
        }

        fun close() {
            if (closed) return
            closed = true
            links.remove(id)
            try {
                gatt?.close()
            } catch (_: Exception) {
            }
        }
    }

    private const val GATT_FAILURE = 257

    fun bleConnect(link: Long, address: String): String? {
        ensureBle(false)?.let { return it }
        if (!BluetoothAdapter.checkBluetoothAddress(address)) return "\"$address\" is not a Bluetooth address"
        return try {
            val a = adapter() ?: return "this device has no Bluetooth adapter"
            val l = Link(link)
            links[link] = l
            val g = a.getRemoteDevice(address).connectGatt(app, false, l, BluetoothDevice.TRANSPORT_LE)
            if (g == null) {
                links.remove(link)
                return "connecting to $address did not start"
            }
            l.gatt = g
            Log.i(TAG, "link $link: connecting to $address")
            null
        } catch (e: Exception) {
            links.remove(link)
            failure("connect to $address", e)
        }
    }

    fun bleDiscover(link: Long): String? {
        val g = links[link]?.gatt ?: return "not connected"
        return try {
            if (g.discoverServices()) null else "service discovery did not start"
        } catch (e: Exception) {
            failure("discover", e)
        }
    }

    private fun characteristic(l: Link, service: String, characteristic: String): BluetoothGattCharacteristic? = try {
        l.gatt?.getService(UUID.fromString(service))?.getCharacteristic(UUID.fromString(characteristic))
    } catch (_: IllegalArgumentException) {
        null
    }

    /** Run a GATT operation for [op]; it completes through the link's callbacks. */
    private fun start(link: Long, op: Long, service: String, characteristic: String, what: String, run: (BluetoothGatt, BluetoothGattCharacteristic) -> String?): String? {
        val l = links[link] ?: return "not connected"
        val g = l.gatt ?: return "not connected"
        val c = characteristic(l, service, characteristic) ?: return "the device has no characteristic $characteristic in service $service"
        l.op = op
        val err = try {
            run(g, c)
        } catch (e: Exception) {
            failure(what, e)
        }
        if (err != null) l.op = 0L
        return err
    }

    fun bleRead(link: Long, op: Long, service: String, characteristic: String): String? =
        start(link, op, service, characteristic, "read") { g, c ->
            if (g.readCharacteristic(c)) null else "the read did not start (another Bluetooth operation in progress?)"
        }

    fun bleWrite(link: Long, op: Long, service: String, characteristic: String, data: ByteArray, withResponse: Boolean): String? =
        start(link, op, service, characteristic, "write") { g, c ->
            val type = if (withResponse) BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT else BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
            if (Build.VERSION.SDK_INT >= 33) {
                val r = g.writeCharacteristic(c, data, type)
                if (r == BluetoothStatusCodes.SUCCESS) null else "the write did not start (status $r)"
            } else {
                @Suppress("DEPRECATION")
                c.writeType = type
                @Suppress("DEPRECATION")
                c.value = data
                @Suppress("DEPRECATION")
                if (g.writeCharacteristic(c)) null else "the write did not start (another Bluetooth operation in progress?)"
            }
        }

    fun bleSetNotify(link: Long, op: Long, service: String, characteristic: String, enable: Boolean): String? =
        start(link, op, service, characteristic, "notify") { g, c ->
            val props = c.properties
            val value = when {
                !enable -> BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE
                props and BluetoothGattCharacteristic.PROPERTY_NOTIFY != 0 -> BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
                props and BluetoothGattCharacteristic.PROPERTY_INDICATE != 0 -> BluetoothGattDescriptor.ENABLE_INDICATION_VALUE
                else -> return@start "characteristic $characteristic can neither notify nor indicate"
            }
            val d = c.getDescriptor(CCCD) ?: return@start "characteristic $characteristic has no notification descriptor (0x2902)"
            if (!g.setCharacteristicNotification(c, enable)) return@start "notifications could not be turned ${if (enable) "on" else "off"}"
            if (Build.VERSION.SDK_INT >= 33) {
                val r = g.writeDescriptor(d, value)
                if (r == BluetoothStatusCodes.SUCCESS) null else "the descriptor write did not start (status $r)"
            } else {
                @Suppress("DEPRECATION")
                d.value = value
                @Suppress("DEPRECATION")
                if (g.writeDescriptor(d)) null else "the descriptor write did not start"
            }
        }

    fun bleDisconnect(link: Long) {
        val l = links[link] ?: return
        Log.i(TAG, "link $link: disconnecting")
        try {
            l.gatt?.disconnect()
        } catch (e: Exception) {
            Log.w(TAG, "disconnect: $e")
        }
        l.close()
    }

    // ------------------------------------------------------------ USB

    private fun usb(): UsbManager? = app.getSystemService(UsbManager::class.java)

    private val connections = ConcurrentHashMap<Int, UsbDeviceConnection>()

    private fun clean(s: String?): String = s?.replace('\t', ' ')?.replace('\n', ' ')?.replace('\r', ' ') ?: ""

    /** The devices the system sees, one `D` line each with `I` and `E` lines. */
    fun usbDevices(): String {
        if (!app.packageManager.hasSystemFeature(PackageManager.FEATURE_USB_HOST)) return "!\tthis device has no USB host support (此裝置不支援 USB 主機模式)"
        val m = usb() ?: return "!\tthe USB service is not available"
        return try {
            val sb = StringBuilder()
            for (d in m.deviceList.values) {
                // the serial number needs the device's permission since Android 10
                val serial = try {
                    if (m.hasPermission(d)) d.serialNumber else null
                } catch (_: SecurityException) {
                    null
                }
                sb.append("D\t").append(clean(d.deviceName)).append('\t').append(d.vendorId).append('\t').append(d.productId)
                    .append('\t').append(d.deviceClass).append('\t').append(d.deviceSubclass).append('\t').append(d.deviceProtocol)
                    .append('\t').append(clean(d.manufacturerName)).append('\t').append(clean(d.productName)).append('\t').append(clean(serial)).append('\n')
                for (i in 0 until d.interfaceCount) {
                    val f = d.getInterface(i)
                    sb.append("I\t").append(f.id).append('\t').append(f.alternateSetting).append('\t').append(f.interfaceClass)
                        .append('\t').append(f.interfaceSubclass).append('\t').append(f.interfaceProtocol).append('\t').append(clean(f.name)).append('\n')
                    for (e in 0 until f.endpointCount) {
                        val ep = f.getEndpoint(e)
                        sb.append("E\t").append(ep.address).append('\t').append(ep.type).append('\t').append(ep.maxPacketSize).append('\n')
                    }
                }
            }
            sb.toString()
        } catch (e: Exception) {
            "!\t" + clean(failure("USB devices", e))
        }
    }

    private fun device(id: String): UsbDevice? = try {
        usb()?.deviceList?.get(id)
    } catch (_: Exception) {
        null
    }

    fun usbHasPermission(id: String): Boolean = device(id)?.let { usb()?.hasPermission(it) } ?: false

    @SuppressLint("UnspecifiedImmutableFlag")
    fun usbRequestPermission(id: String): String? {
        val d = device(id) ?: return "no USB device $id (unplugged?)"
        return try {
            val intent = Intent(ACTION_USB_PERMISSION).setPackage(app.packageName)
            // mutable: the system adds the device and the answer to the intent
            val flags = PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0)
            val pi = PendingIntent.getBroadcast(app, d.deviceId, intent, flags)
            Log.i(TAG, "asking for permission to use ${d.deviceName}")
            usb()?.requestPermission(d, pi) ?: return "the USB service is not available"
            null
        } catch (e: Exception) {
            failure("USB permission", e)
        }
    }

    /** A file descriptor for the device (as text), or "!\t<message>". */
    fun usbOpen(id: String): String {
        val m = usb() ?: return "!\tthe USB service is not available"
        val d = device(id) ?: return "!\tno USB device $id (unplugged?)"
        return try {
            val c = m.openDevice(d) ?: return "!\t$id could not be opened (permission missing or the device is busy)"
            val fd = c.fileDescriptor
            if (fd < 0) {
                c.close()
                return "!\t$id has no file descriptor"
            }
            connections[fd] = c
            Log.i(TAG, "opened ${d.deviceName} as fd $fd")
            fd.toString()
        } catch (e: Exception) {
            "!\t" + clean(failure("open $id", e))
        }
    }

    fun usbClose(fd: Int) {
        connections.remove(fd)?.let {
            Log.i(TAG, "closing USB fd $fd")
            try {
                it.close()
            } catch (_: Exception) {
            }
        }
    }

    private val usbReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val d: UsbDevice? = if (Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
            }
            val name = d?.deviceName ?: return
            when (intent.action) {
                ACTION_USB_PERMISSION -> nativeUsbPermission(name, intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false))
                UsbManager.ACTION_USB_DEVICE_DETACHED -> nativeUsbDetached(name)
            }
        }
    }

    // ------------------------------------------------------------ into Rust (bridge.rs)

    @JvmStatic external fun nativeScanResult(
        address: String, name: String?, rssi: Int, txPower: Int, connectable: Int, services: Array<String>,
        mfgIds: IntArray, mfgData: Array<ByteArray>, serviceDataUuids: Array<String>, serviceData: Array<ByteArray>,
    )

    @JvmStatic external fun nativeScanFailed(code: Int)

    @JvmStatic external fun nativeConnectionState(link: Long, connected: Boolean, status: Int)

    @JvmStatic external fun nativeServicesDiscovered(link: Long, status: Int, table: String)

    @JvmStatic external fun nativeOpComplete(op: Long, status: Int, value: ByteArray?)

    @JvmStatic external fun nativeNotification(link: Long, service: String, characteristic: String, value: ByteArray?)

    @JvmStatic external fun nativeUsbPermission(device: String, granted: Boolean)

    @JvmStatic external fun nativeUsbDetached(device: String)
}
