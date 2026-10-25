//! Platform services the Kotlin app lends to pages, installed once per
//! process by `NativeBridge.installPlatform` (before the engine starts):
//!
//! | for | Rust side | Kotlin side |
//! |---|---|---|
//! | `ble`, `usb`, `serial` (anypage-devices) | [`JniDevices`] implements `anypage_devices::AndroidDevices`, installed with `set_android_bridge` | `DeviceBridge.kt`: `BluetoothLeScanner`, `BluetoothGatt`, `UsbManager`, runtime permissions |
//! | `media.broadcast { source = "camera" }`, `media.cameras()` | [`crate::camera::CameraHub`] behind `anypage_media::camera::set_host_camera` | `CameraBridge.kt`: Camera2, `ImageReader` in `YUV_420_888`, the CAMERA permission |
//! | `grants.secret(name)` (cloud keys) | [`JniSecrets`] implements `anypage_runtime::SecretStore`, passed to every page load | `SecretVault.kt`: app-private preferences, values encrypted with an Android Keystore AES-GCM key |
//! | crates that find the JVM through `ndk-context` (cpal's AAudio output) | `ndk_context::initialize_android_context` | the application `Context` |
//!
//! Rust calls Kotlin from any thread (page job workers, stream threads):
//! [`Kotlin::call`] attaches the thread (jni detaches it when the thread
//! ends), runs the call inside a local frame (attached native threads never
//! return to Java, so local references must not pile up) and turns a Java
//! exception into an error string. Kotlin calls back through the
//! `DeviceBridge.native*` and `CameraBridge.native*` functions below, on its own
//! threads (binder threads for Bluetooth and USB, the camera's handler
//! thread). Every entry point catches panics.

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anypage_devices::android::{self as dev, AndroidDevices, AndroidScanResult};
use anypage_devices::usb::UsbDeviceInfo;
use anypage_runtime::SecretStore;
use jni::objects::{GlobalRef, JByteArray, JByteBuffer, JClass, JIntArray, JObject, JObjectArray, JString, JValue};
use jni::sys::{jboolean, jint, jlong, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};

use crate::camera::{yuv_420_888_to_i420, CameraHost, CameraHub, HubCamera, Plane};
use crate::devices;
use crate::host::{guard, init_logging};

/// How long opening a camera may take, the permission dialog included.
const CAMERA_OPEN: Duration = Duration::from_secs(30);

struct CallError(String);

impl From<jni::errors::Error> for CallError {
    fn from(e: jni::errors::Error) -> Self {
        CallError(e.to_string())
    }
}

/// A Kotlin object Rust calls from any thread.
pub(crate) struct Kotlin {
    vm: JavaVM,
    obj: GlobalRef,
    name: &'static str,
}

impl Kotlin {
    fn new(env: &JNIEnv, obj: &JObject, name: &'static str) -> Result<Self, String> {
        if obj.is_null() {
            return Err(format!("{name}: no object"));
        }
        Ok(Kotlin { vm: env.get_java_vm().map_err(|e| e.to_string())?, obj: env.new_global_ref(obj).map_err(|e| e.to_string())?, name })
    }

    /// Call `method` through `f`; a Java exception becomes its message.
    fn call<T>(&self, method: &str, f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Result<T, String> {
        let mut env = self.vm.attach_current_thread_permanently().map_err(|e| format!("{}.{method}: cannot attach to the JVM: {e}", self.name))?;
        let r = env.with_local_frame(32, |env| -> Result<T, CallError> {
            match f(env, self.obj.as_obj()) {
                Ok(v) => Ok(v),
                Err(e) => Err(CallError(java_exception(env).unwrap_or_else(|| e.to_string()))),
            }
        });
        r.map_err(|CallError(e)| {
            log::warn!("{}.{method}: {e}", self.name);
            format!("{}.{method}: {e}", self.name)
        })
    }
}

/// The pending Java exception as text, cleared (inside a local frame).
fn java_exception(env: &mut JNIEnv) -> Option<String> {
    if !env.exception_check().unwrap_or(false) {
        return None;
    }
    let ex = env.exception_occurred().ok()?;
    env.exception_clear().ok()?;
    let s = env.call_method(&ex, "toString", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
    let text: String = env.get_string(&JString::from(s)).ok()?.into();
    Some(text)
}

fn string_array<'a>(env: &mut JNIEnv<'a>, items: &[String]) -> jni::errors::Result<JObjectArray<'a>> {
    let arr = env.new_object_array(items.len() as i32, "java/lang/String", JObject::null())?;
    for (i, s) in items.iter().enumerate() {
        let js = env.new_string(s)?;
        env.set_object_array_element(&arr, i as i32, &js)?;
        env.delete_local_ref(js)?;
    }
    Ok(arr)
}

fn opt_string(env: &mut JNIEnv, o: JObject) -> jni::errors::Result<Option<String>> {
    if o.is_null() {
        return Ok(None);
    }
    Ok(Some(env.get_string(&JString::from(o))?.into()))
}

fn strings_of(env: &mut JNIEnv, arr: &JObjectArray) -> jni::errors::Result<Vec<String>> {
    if arr.is_null() {
        return Ok(Vec::new());
    }
    let n = env.get_array_length(arr)?.clamp(0, 4096);
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let o = env.get_object_array_element(arr, i)?;
        if let Some(s) = opt_string(env, o)? {
            out.push(s);
        }
    }
    Ok(out)
}

fn bytes_of(env: &mut JNIEnv, arr: &JObjectArray) -> jni::errors::Result<Vec<Vec<u8>>> {
    if arr.is_null() {
        return Ok(Vec::new());
    }
    let n = env.get_array_length(arr)?.clamp(0, 4096);
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let o = env.get_object_array_element(arr, i)?;
        out.push(if o.is_null() { Vec::new() } else { env.convert_byte_array(JByteArray::from(o))? });
    }
    Ok(out)
}

fn byte_arg(env: &mut JNIEnv, a: &JByteArray) -> Vec<u8> {
    if a.is_null() {
        return Vec::new();
    }
    env.convert_byte_array(a).unwrap_or_default()
}

fn string_arg(env: &mut JNIEnv, s: &JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    env.get_string(s).ok().map(Into::into)
}

// ---------------------------------------------------------------- devices

/// `anypage_devices::AndroidDevices` over Kotlin's `DeviceBridge`. Methods
/// that start something answer null (started) or a message (could not start).
struct JniDevices(Kotlin);

impl AndroidDevices for JniDevices {
    fn ble_start_scan(&self, services: &[String]) -> Result<(), String> {
        let r = self.0.call("bleStartScan", |env, o| {
            let arr = string_array(env, services)?;
            let r = env.call_method(o, "bleStartScan", "([Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&arr)])?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn ble_stop_scan(&self) {
        let _ = self.0.call("bleStopScan", |env, o| env.call_method(o, "bleStopScan", "()V", &[]).map(drop));
    }

    fn ble_connect(&self, link: u64, address: &str) -> Result<(), String> {
        let r = self.0.call("bleConnect", |env, o| {
            let a = env.new_string(address)?;
            let r = env.call_method(o, "bleConnect", "(JLjava/lang/String;)Ljava/lang/String;", &[JValue::Long(link as i64), JValue::Object(&a)])?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn ble_discover_services(&self, link: u64) -> Result<(), String> {
        let r = self.0.call("bleDiscover", |env, o| {
            let r = env.call_method(o, "bleDiscover", "(J)Ljava/lang/String;", &[JValue::Long(link as i64)])?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn ble_read(&self, link: u64, op: u64, service: &str, characteristic: &str) -> Result<(), String> {
        let r = self.0.call("bleRead", |env, o| {
            let (s, c) = (env.new_string(service)?, env.new_string(characteristic)?);
            let args = [JValue::Long(link as i64), JValue::Long(op as i64), JValue::Object(&s), JValue::Object(&c)];
            let r = env.call_method(o, "bleRead", "(JJLjava/lang/String;Ljava/lang/String;)Ljava/lang/String;", &args)?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn ble_write(&self, link: u64, op: u64, service: &str, characteristic: &str, data: &[u8], with_response: bool) -> Result<(), String> {
        let r = self.0.call("bleWrite", |env, o| {
            let (s, c) = (env.new_string(service)?, env.new_string(characteristic)?);
            let d = env.byte_array_from_slice(data)?;
            let args = [JValue::Long(link as i64), JValue::Long(op as i64), JValue::Object(&s), JValue::Object(&c), JValue::Object(&d), JValue::Bool(with_response as jboolean)];
            let r = env.call_method(o, "bleWrite", "(JJLjava/lang/String;Ljava/lang/String;[BZ)Ljava/lang/String;", &args)?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn ble_set_notify(&self, link: u64, op: u64, service: &str, characteristic: &str, enable: bool) -> Result<(), String> {
        let r = self.0.call("bleSetNotify", |env, o| {
            let (s, c) = (env.new_string(service)?, env.new_string(characteristic)?);
            let args = [JValue::Long(link as i64), JValue::Long(op as i64), JValue::Object(&s), JValue::Object(&c), JValue::Bool(enable as jboolean)];
            let r = env.call_method(o, "bleSetNotify", "(JJLjava/lang/String;Ljava/lang/String;Z)Ljava/lang/String;", &args)?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn ble_disconnect(&self, link: u64) {
        let _ = self.0.call("bleDisconnect", |env, o| env.call_method(o, "bleDisconnect", "(J)V", &[JValue::Long(link as i64)]).map(drop));
    }

    fn usb_devices(&self) -> Result<Vec<UsbDeviceInfo>, String> {
        let text = self.0.call("usbDevices", |env, o| {
            let r = env.call_method(o, "usbDevices", "()Ljava/lang/String;", &[])?.l()?;
            opt_string(env, r)
        })?;
        devices::parse_usb_list(&text.unwrap_or_default())
    }

    fn usb_has_permission(&self, device: &str) -> bool {
        self.0
            .call("usbHasPermission", |env, o| {
                let d = env.new_string(device)?;
                env.call_method(o, "usbHasPermission", "(Ljava/lang/String;)Z", &[JValue::Object(&d)])?.z()
            })
            .unwrap_or(false)
    }

    fn usb_request_permission(&self, device: &str) -> Result<(), String> {
        let r = self.0.call("usbRequestPermission", |env, o| {
            let d = env.new_string(device)?;
            let r = env.call_method(o, "usbRequestPermission", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&d)])?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn usb_open(&self, device: &str) -> Result<i32, String> {
        let r = self.0.call("usbOpen", |env, o| {
            let d = env.new_string(device)?;
            let r = env.call_method(o, "usbOpen", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&d)])?.l()?;
            opt_string(env, r)
        })?;
        devices::parse_fd(&r.unwrap_or_default())
    }

    fn usb_close(&self, fd: i32) {
        let _ = self.0.call("usbClose", |env, o| env.call_method(o, "usbClose", "(I)V", &[JValue::Int(fd)]).map(drop));
    }
}

// ---------------------------------------------------------------- camera

/// What the camera hub asks of Kotlin's `CameraBridge`.
struct JniCamera(Kotlin);

impl CameraHost for JniCamera {
    fn list(&self) -> Vec<String> {
        self.0
            .call("cameraList", |env, o| {
                let r = env.call_method(o, "cameraList", "()[Ljava/lang/String;", &[])?.l()?;
                strings_of(env, &JObjectArray::from(r))
            })
            .unwrap_or_default()
    }

    fn open(&self, token: u64, index: usize, width: usize, height: usize, fps: f64) -> Result<(), String> {
        let clamp = |v: usize| v.min(i32::MAX as usize) as i32;
        let r = self.0.call("cameraOpen", |env, o| {
            let args = [JValue::Long(token as i64), JValue::Int(clamp(index)), JValue::Int(clamp(width)), JValue::Int(clamp(height)), JValue::Int(fps.round().clamp(1.0, 240.0) as i32)];
            let r = env.call_method(o, "cameraOpen", "(JIIII)Ljava/lang/String;", &args)?.l()?;
            opt_string(env, r)
        })?;
        devices::started(r)
    }

    fn close(&self, token: u64) {
        let _ = self.0.call("cameraClose", |env, o| env.call_method(o, "cameraClose", "(J)V", &[JValue::Long(token as i64)]).map(drop));
    }
}

// ---------------------------------------------------------------- secrets

/// Secrets from Kotlin's `SecretVault` (decrypted there with the Keystore key).
struct JniSecrets(Kotlin);

impl SecretStore for JniSecrets {
    fn get(&self, page_id: &str, name: &str) -> Option<String> {
        // values are never logged; failures only by name
        self.0
            .call("get", |env, o| {
                let (p, n) = (env.new_string(page_id)?, env.new_string(name)?);
                let r = env.call_method(o, "get", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&p), JValue::Object(&n)])?.l()?;
                opt_string(env, r)
            })
            .unwrap_or_else(|_| {
                log::warn!("secret \"{name}\" for {page_id}: the vault could not be read");
                None
            })
            .filter(|s| !s.is_empty())
    }
}

// ---------------------------------------------------------------- installation

struct Platform {
    camera: Arc<CameraHub>,
    secrets: Arc<dyn SecretStore>,
    /// Keeps the application context alive for ndk-context users.
    _context: GlobalRef,
}

static PLATFORM: OnceLock<Platform> = OnceLock::new();

/// The secret store pages load with (None before `installPlatform`).
pub(crate) fn secrets() -> Option<Arc<dyn SecretStore>> {
    PLATFORM.get().map(|p| p.secrets.clone())
}

fn install(env: &mut JNIEnv, context: &JObject, devices: &JObject, camera: &JObject, secrets: &JObject) -> Result<(), String> {
    if PLATFORM.get().is_some() {
        return Ok(());
    }
    if context.is_null() {
        return Err("no application context".into());
    }
    // everything that can fail first, so a failed call can be repeated
    let ctx = env.new_global_ref(context).map_err(|e| format!("context: {e}"))?;
    let vm = env.get_java_vm().map_err(|e| e.to_string())?;
    let devices = JniDevices(Kotlin::new(env, devices, "DeviceBridge")?);
    let camera = JniCamera(Kotlin::new(env, camera, "CameraBridge")?);
    let vault: Arc<dyn SecretStore> = Arc::new(JniSecrets(Kotlin::new(env, secrets, "SecretVault")?));
    static NDK_CONTEXT: std::sync::Once = std::sync::Once::new();
    NDK_CONTEXT.call_once(|| {
        // SAFETY: a valid JavaVM pointer and a global reference to the
        // application context, kept alive in PLATFORM for the process
        // lifetime; ndk-context takes it exactly once (this Once).
        unsafe { ndk_context::initialize_android_context(vm.get_java_vm_pointer() as *mut c_void, ctx.as_obj().as_raw() as *mut c_void) };
    });
    drop(dev::set_android_bridge(Arc::new(devices)));
    let hub = CameraHub::new(Arc::new(camera), CAMERA_OPEN);
    anypage_media::camera::set_host_camera(Some(Arc::new(HubCamera(hub.clone()))));
    let _ = PLATFORM.set(Platform { camera: hub, secrets: vault, _context: ctx });
    log::info!("platform services installed: Bluetooth LE and USB bridge, Camera2, Keystore secrets");
    Ok(())
}

/// Install the device bridge, the camera and the secret store (once per
/// process; later calls do nothing). Returns false when something could not
/// be installed (logged).
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_installPlatform<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    context: JObject<'l>,
    devices: JObject<'l>,
    camera: JObject<'l>,
    secrets: JObject<'l>,
) -> jboolean {
    init_logging();
    guard("installPlatform", JNI_FALSE, || match install(&mut env, &context, &devices, &camera, &secrets) {
        Ok(()) => JNI_TRUE,
        Err(e) => {
            log::error!("platform services: {e}");
            JNI_FALSE
        }
    })
}

/// Debug tools: the length of the secret `name` the store gives page
/// `page_id` (through the same path native modules use), -1 when unset.
/// Never the value.
#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_NativeBridge_secretProbe(mut env: JNIEnv, _class: JClass, page_id: JString, name: JString) -> jint {
    guard("secretProbe", -1, || {
        let (Some(p), Some(n)) = (string_arg(&mut env, &page_id), string_arg(&mut env, &name)) else { return -1 };
        match secrets().and_then(|s| s.get(&p, &n)) {
            Some(v) => v.chars().count().min(i32::MAX as usize) as jint,
            None => -1,
        }
    })
}

// ---------------------------------------------------------------- DeviceBridge callbacks

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeScanResult<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    address: JString<'l>,
    name: JString<'l>,
    rssi: jint,
    tx_power: jint,
    connectable: jint,
    services: JObjectArray<'l>,
    mfg_ids: JIntArray<'l>,
    mfg_data: JObjectArray<'l>,
    sd_uuids: JObjectArray<'l>,
    sd_data: JObjectArray<'l>,
) {
    guard("onScanResult", (), || {
        let r = (|| -> jni::errors::Result<AndroidScanResult> {
            let address = string_arg(&mut env, &address).unwrap_or_default();
            let name = string_arg(&mut env, &name);
            let service_uuids = strings_of(&mut env, &services)?;
            let ids = if mfg_ids.is_null() {
                Vec::new()
            } else {
                let n = env.get_array_length(&mfg_ids)?.clamp(0, 64) as usize;
                let mut v = vec![0i32; n];
                env.get_int_array_region(&mfg_ids, 0, &mut v)?;
                v
            };
            let data = bytes_of(&mut env, &mfg_data)?;
            let sd_u = strings_of(&mut env, &sd_uuids)?;
            let sd_d = bytes_of(&mut env, &sd_data)?;
            Ok(AndroidScanResult {
                address,
                name,
                rssi,
                service_uuids,
                manufacturer_data: ids.into_iter().zip(data).map(|(i, d)| (i.clamp(0, 0xFFFF) as u16, d)).collect(),
                service_data: sd_u.into_iter().zip(sd_d).collect(),
                tx_power: (tx_power != i32::MIN).then_some(tx_power),
                connectable: match connectable {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                },
            })
        })();
        match r {
            Ok(r) if !r.address.is_empty() => dev::on_scan_result(r),
            Ok(_) => {}
            Err(e) => log::warn!("scan result: {e}"),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeScanFailed(_env: JNIEnv, _class: JClass, code: jint) {
    guard("onScanFailed", (), || dev::on_scan_failed(code));
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeConnectionState(_env: JNIEnv, _class: JClass, link: jlong, connected: jboolean, status: jint) {
    guard("onConnectionState", (), || dev::on_connection_state(link as u64, connected != JNI_FALSE, status));
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeServicesDiscovered(mut env: JNIEnv, _class: JClass, link: jlong, status: jint, table: JString) {
    guard("onServicesDiscovered", (), || {
        let text = string_arg(&mut env, &table).unwrap_or_default();
        dev::on_services_discovered(link as u64, status, devices::parse_services(&text));
    });
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeOpComplete(mut env: JNIEnv, _class: JClass, op: jlong, status: jint, value: JByteArray) {
    guard("onOpComplete", (), || {
        let v = byte_arg(&mut env, &value);
        dev::on_op_complete(op as u64, status, v);
    });
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeNotification(mut env: JNIEnv, _class: JClass, link: jlong, service: JString, characteristic: JString, value: JByteArray) {
    guard("onNotification", (), || {
        let (Some(s), Some(c)) = (string_arg(&mut env, &service), string_arg(&mut env, &characteristic)) else { return };
        let v = byte_arg(&mut env, &value);
        dev::on_notification(link as u64, &s, &c, v);
    });
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeUsbPermission(mut env: JNIEnv, _class: JClass, device: JString, granted: jboolean) {
    guard("onUsbPermission", (), || {
        if let Some(d) = string_arg(&mut env, &device) {
            log::info!("USB permission for {d}: {}", if granted != JNI_FALSE { "granted" } else { "denied" });
            dev::on_usb_permission(&d, granted != JNI_FALSE);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_DeviceBridge_nativeUsbDetached(mut env: JNIEnv, _class: JClass, device: JString) {
    guard("onUsbDetached", (), || {
        if let Some(d) = string_arg(&mut env, &device) {
            log::info!("USB device detached: {d}");
            dev::on_usb_detached(&d);
        }
    });
}

// ---------------------------------------------------------------- CameraBridge callbacks

fn hub() -> Option<&'static Arc<CameraHub>> {
    PLATFORM.get().map(|p| &p.camera)
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_CameraBridge_nativeOpened(_env: JNIEnv, _class: JClass, token: jlong, width: jint, height: jint) {
    guard("cameraOpened", (), || {
        if let Some(h) = hub() {
            h.opened(token as u64, width.max(0) as usize, height.max(0) as usize);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_com_anypage_engine_CameraBridge_nativeFailed(mut env: JNIEnv, _class: JClass, token: jlong, message: JString) {
    guard("cameraFailed", (), || {
        let m = string_arg(&mut env, &message).unwrap_or_else(|| "the camera failed".into());
        log::warn!("camera {token}: {m}");
        if let Some(h) = hub() {
            h.failed(token as u64, &m);
        }
    });
}

/// The bytes of a direct `ByteBuffer` (an `Image` plane), valid while Kotlin
/// keeps the image open, which it does until this call returns.
fn direct<'b>(env: &JNIEnv, buf: &JByteBuffer) -> Result<&'b [u8], String> {
    if buf.is_null() {
        return Err("missing plane".into());
    }
    let ptr = env.get_direct_buffer_address(buf).map_err(|e| format!("plane: {e}"))?;
    let len = env.get_direct_buffer_capacity(buf).map_err(|e| format!("plane: {e}"))?;
    if ptr.is_null() {
        return Err("plane without memory".into());
    }
    // SAFETY: a direct buffer's memory of `len` bytes, alive for this call
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// One camera picture; false when no stream wants frames from `token` (Kotlin closes it).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_anypage_engine_CameraBridge_nativeFrame<'l>(
    env: JNIEnv<'l>,
    _class: JClass<'l>,
    token: jlong,
    width: jint,
    height: jint,
    y: JByteBuffer<'l>,
    y_row: jint,
    u: JByteBuffer<'l>,
    v: JByteBuffer<'l>,
    uv_row: jint,
    uv_pixel: jint,
    timestamp_ns: jlong,
) -> jboolean {
    guard("cameraFrame", JNI_FALSE, || {
        let Some(h) = hub() else { return JNI_FALSE };
        let wanted = h.frame(token as u64, timestamp_ns as f64 / 1e9, || {
            let (yb, ub, vb) = (direct(&env, &y)?, direct(&env, &u)?, direct(&env, &v)?);
            let st = |s: jint| usize::try_from(s).map_err(|_| format!("negative stride {s}"));
            let (w, hh) = (st(width)?, st(height)?);
            let yp = Plane { data: yb, row_stride: st(y_row)?, pixel_stride: 1 };
            let (row, px) = (st(uv_row)?, st(uv_pixel)?);
            yuv_420_888_to_i420(w, hh, yp, Plane { data: ub, row_stride: row, pixel_stride: px }, Plane { data: vb, row_stride: row, pixel_stride: px })
        });
        if wanted {
            JNI_TRUE
        } else {
            JNI_FALSE
        }
    })
}
