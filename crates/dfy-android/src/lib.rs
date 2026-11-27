// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

//! JNI entry points for the Android controlled end.

mod agent;
mod controller;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent::{AgentCallbacks, AgentHost, AgentSettings, AgentStats, MAX_ENCODED_FRAME_BYTES};
use anyhow::Context;
use controller::{
    button_event, key_event, pointer_event, quality_control as quality_request, run_controller,
    stream_feedback, wheel_event, ControllerAuthorization, ControllerCallbacks, ControllerCommand,
    ControllerSettings, ControllerStats, SystemAction,
};
use dfy_session::{EncodeResolution, EncodedFrame, FrameInput, RuntimeSettings};
use dfy_transport::TlsIdentity;
use jni::objects::{GlobalRef, JByteArray, JClass, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use quinn::Endpoint;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);
static ENDPOINT: Mutex<Option<Arc<Endpoint>>> = Mutex::new(None);
static ACTIVE_FRAMES: Mutex<Option<ActiveFrames>> = Mutex::new(None);
static NEXT_HOST_EPOCH: AtomicU64 = AtomicU64::new(1);
static STATS: OnceLock<Arc<AgentStats>> = OnceLock::new();

// Viewer state is deliberately independent of the existing controlled-end globals above.
static CONTROLLER_CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);
static CONTROLLER_COMMANDS: Mutex<Option<mpsc::Sender<ControllerCommand>>> = Mutex::new(None);
static CONTROLLER_RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);
static CONTROLLER_SHUTDOWN: Mutex<Option<Arc<tokio::sync::Notify>>> = Mutex::new(None);
static CONTROLLER_STATS: OnceLock<Arc<ControllerStats>> = OnceLock::new();
static CONTROLLER_AUTH: OnceLock<Arc<ControllerAuthorization>> = OnceLock::new();

#[derive(Clone)]
struct HostEpoch {
    value: u64,
    active: Arc<AtomicU64>,
}

impl HostEpoch {
    fn allocate() -> Self {
        let value = NEXT_HOST_EPOCH.fetch_add(1, Ordering::SeqCst);
        Self {
            value,
            active: Arc::new(AtomicU64::new(value)),
        }
    }

    fn is_current(&self) -> bool {
        self.value != 0 && self.active.load(Ordering::SeqCst) == self.value
    }

    fn ensure_current(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.is_current(), "Android 主机代际已失效");
        Ok(())
    }

    fn deactivate(&self) {
        self.active.store(0, Ordering::SeqCst);
    }
}

struct ActiveFrames {
    host: HostEpoch,
    input: Arc<FrameInput>,
}

impl ActiveFrames {
    fn input_for(&self, epoch: u64) -> Option<Arc<FrameInput>> {
        if self.host.value == epoch && self.host.is_current() {
            Some(Arc::clone(&self.input))
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct JniCallbacks {
    host: HostEpoch,
}

fn encode_host_input(host: &HostEpoch, event: &dfy_proto::InputEvent) -> Option<(u64, Vec<u8>)> {
    host.ensure_current().ok()?;
    let mut data = Vec::new();
    prost::Message::encode(event, &mut data).ok()?;
    Some((host.value, data))
}

impl AgentCallbacks for JniCallbacks {
    fn on_state(&self, state: &str, detail: &str) {
        if self.host.ensure_current().is_err() {
            return;
        }
        let host = self.host.clone();
        callback(move |env, callback| {
            if host.ensure_current().is_err() {
                return Ok(());
            }
            let state = env.new_string(state)?;
            let detail = env.new_string(detail)?;
            env.call_method(
                callback,
                "onState",
                "(JLjava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Long(host.value as i64),
                    JValue::Object(&state),
                    JValue::Object(&detail),
                ],
            )?;
            Ok(())
        });
    }

    fn on_input(&self, event: &dfy_proto::InputEvent) {
        let Some((epoch, data)) = encode_host_input(&self.host, event) else {
            return;
        };
        let host = self.host.clone();
        callback(move |env, callback| {
            if host.ensure_current().is_err() {
                return Ok(());
            }
            let bytes = env.new_byte_array(data.len() as i32)?;
            let signed: Vec<i8> = data.into_iter().map(|byte| byte as i8).collect();
            env.set_byte_array_region(&bytes, 0, &signed)?;
            env.call_method(
                callback,
                "onInput",
                "(J[B)V",
                &[JValue::Long(epoch as i64), JValue::Object(&bytes)],
            )?;
            Ok(())
        });
    }

    fn on_release_input(&self) {
        let host = self.host.clone();
        callback(move |env, callback| {
            if host.ensure_current().is_err() {
                return Ok(());
            }
            env.call_method(
                callback,
                "onReleaseInput",
                "(J)V",
                &[JValue::Long(host.value as i64)],
            )?;
            Ok(())
        });
    }

    fn on_capture_activity(&self, active: bool) {
        if self.host.ensure_current().is_err() {
            return;
        }
        callback(|env, callback| {
            env.call_method(
                callback,
                "onCaptureActivity",
                "(JZ)V",
                &[
                    JValue::Long(self.host.value as i64),
                    JValue::Bool(jboolean::from(active)),
                ],
            )?;
            Ok(())
        });
        let _ = self.host.ensure_current();
    }

    fn control_allowed(&self) -> bool {
        callback(|env, callback| env.call_method(callback, "isInputAllowed", "()Z", &[])?.z())
    }

    fn apply_runtime_settings(&self, settings: RuntimeSettings) -> anyhow::Result<RuntimeSettings> {
        runtime_settings_callback(&self.host, settings)
    }
}

fn runtime_settings_callback(
    host: &HostEpoch,
    settings: RuntimeSettings,
) -> anyhow::Result<RuntimeSettings> {
    host.ensure_current()?;
    let vm = JAVA_VM.get().context("Android Java VM 尚未初始化")?;
    let callback = CALLBACK
        .lock()
        .map_err(|_| anyhow::anyhow!("Android 回调锁已中毒"))?
        .clone()
        .context("Android 回调尚未注册")?;
    let mut env = vm
        .attach_current_thread()
        .context("附着 Android 回调线程失败")?;
    let result = (|| -> anyhow::Result<RuntimeSettings> {
        let (width, height) = settings.resolution.map_or((-1, -1), |resolution| {
            (resolution.width as i32, resolution.height as i32)
        });
        let applied = match env.call_method(
            callback.as_obj(),
            "applyRuntimeSettings",
            "(JIIII)[I",
            &[
                JValue::Long(host.value as i64),
                JValue::Int(width),
                JValue::Int(height),
                JValue::Int(settings.fps as i32),
                JValue::Int(settings.bitrate_kbps as i32),
            ],
        ) {
            Ok(applied) => applied,
            Err(error) => {
                if let Ok(exception) = env.exception_occurred() {
                    if !exception.is_null() {
                        let message = java_exception_message(&mut env, &exception);
                        anyhow::bail!("Android 运行时设置回调失败：{message}");
                    }
                }
                return Err(error).context("调用 Android 运行时设置回调失败");
            }
        };
        let applied = applied.l()?;
        anyhow::ensure!(!applied.is_null(), "Android 回调返回了空设置数组");
        let applied = JIntArray::from(applied);
        anyhow::ensure!(
            env.get_array_length(&applied)? == 4,
            "Android 回调必须返回 [width, height, fps, bitrate]"
        );
        let mut values = [0; 4];
        env.get_int_array_region(&applied, 0, &mut values)?;
        let to_u32 = |value: i32, name: &str| {
            u32::try_from(value).with_context(|| format!("Android 回调返回无效 {name}：{value}"))
        };
        let resolution = if values[0] == -1 && values[1] == -1 {
            None
        } else {
            Some(EncodeResolution {
                width: to_u32(values[0], "width")?,
                height: to_u32(values[1], "height")?,
            })
        };
        let applied = RuntimeSettings {
            fps: to_u32(values[2], "fps")?,
            bitrate_kbps: to_u32(values[3], "bitrate")?,
            resolution,
        };
        applied
            .validate()
            .context("Android 回调返回的运行时设置无效")?;
        Ok(applied)
    })();
    let still_current = host.ensure_current();
    drop(env);
    // Runtime callbacks only run on Rust worker threads attached above.
    unsafe { vm.detach_current_thread() };
    still_current?;
    result
}

fn java_exception_message(env: &mut JNIEnv, exception: &jni::objects::JThrowable) -> String {
    let _ = env.exception_clear();
    let message = env
        .call_method(exception, "getMessage", "()Ljava/lang/String;", &[])
        .ok()
        .and_then(|value| value.l().ok());
    match message {
        Some(message) => env
            .get_string(&JString::from(message))
            .map(String::from)
            .unwrap_or_else(|_| "Java 回调抛出异常".to_string()),
        None => "Java 回调抛出异常".to_string(),
    }
}

struct ControllerJniCallbacks;

impl ControllerCallbacks for ControllerJniCallbacks {
    fn on_state(&self, state: &str, detail: &str) {
        controller_callback(|env, callback| {
            let state = env.new_string(state)?;
            let detail = env.new_string(detail)?;
            env.call_method(
                callback,
                "onState",
                "(Ljava/lang/String;Ljava/lang/String;)V",
                &[JValue::Object(&state), JValue::Object(&detail)],
            )?;
            Ok(())
        });
    }

    fn on_frame(&self, data: &[u8], pts_us: u32, keyframe: bool) -> bool {
        controller_callback(move |env, callback| {
            let bytes = env.new_byte_array(data.len().min(i32::MAX as usize) as i32)?;
            let signed: Vec<i8> = data.iter().map(|byte| *byte as i8).collect();
            env.set_byte_array_region(&bytes, 0, &signed)?;
            env.call_method(
                callback,
                "onFrame",
                "([BJZ)Z",
                &[
                    JValue::Object(&bytes),
                    JValue::Long(i64::from(pts_us)),
                    JValue::Bool(jboolean::from(keyframe)),
                ],
            )?
            .z()
        })
    }

    fn on_video_packet(
        &self,
        datagram_bytes: usize,
        frames_assembled: u32,
        frames_delivered: u32,
        dropped_frames: u64,
    ) {
        controller_callback(move |env, callback| {
            env.call_method(
                callback,
                "onVideoPacket",
                "(JIIJ)V",
                &[
                    JValue::Long(datagram_bytes as i64),
                    JValue::Int(frames_assembled as i32),
                    JValue::Int(frames_delivered as i32),
                    JValue::Long(dropped_frames.min(i64::MAX as u64) as i64),
                ],
            )?;
            Ok(())
        });
    }

    fn on_video_config(&self, width: u32, height: u32, fps: u32, bitrate_kbps: u32) {
        controller_callback(move |env, callback| {
            env.call_method(
                callback,
                "onVideoConfig",
                "(IIII)V",
                &[
                    JValue::Int(width as i32),
                    JValue::Int(height as i32),
                    JValue::Int(fps as i32),
                    JValue::Int(bitrate_kbps as i32),
                ],
            )?;
            Ok(())
        });
    }

    fn on_session(&self, permissions: u32, capabilities: &[String]) {
        let capabilities = capabilities.join("\n");
        controller_callback(move |env, callback| {
            let capabilities = env.new_string(capabilities)?;
            env.call_method(
                callback,
                "onSession",
                "(ILjava/lang/String;)V",
                &[
                    JValue::Int(permissions as i32),
                    JValue::Object(&capabilities),
                ],
            )?;
            Ok(())
        });
    }

    fn on_control_allowed(&self, allowed: bool) {
        controller_callback(|env, callback| {
            env.call_method(
                callback,
                "onControlAllowed",
                "(Z)V",
                &[JValue::Bool(jboolean::from(allowed))],
            )?;
            Ok(())
        });
    }
}

fn controller_callback<R: Default>(
    action: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>,
) -> R {
    let Some(vm) = JAVA_VM.get() else {
        return R::default();
    };
    let Ok(callback) = CONTROLLER_CALLBACK.lock() else {
        return R::default();
    };
    let Some(callback) = callback.as_ref() else {
        return R::default();
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return R::default();
    };
    let result = match action(&mut env, callback.as_obj()) {
        Ok(value) => value,
        Err(_) => {
            let _ = env.exception_clear();
            R::default()
        }
    };
    drop(env);
    unsafe { vm.detach_current_thread() };
    result
}

fn controller_stats() -> &'static Arc<ControllerStats> {
    CONTROLLER_STATS.get_or_init(|| Arc::new(ControllerStats::default()))
}

fn controller_auth() -> &'static Arc<ControllerAuthorization> {
    CONTROLLER_AUTH.get_or_init(|| Arc::new(ControllerAuthorization::default()))
}

fn stop_controller_native() {
    if let Some(shutdown) = CONTROLLER_SHUTDOWN
        .lock()
        .ok()
        .and_then(|mut value| value.take())
    {
        controller::notify_shutdown(&shutdown);
    }
    CONTROLLER_COMMANDS
        .lock()
        .ok()
        .and_then(|mut value| value.take());
    if let Some(runtime) = CONTROLLER_RUNTIME
        .lock()
        .ok()
        .and_then(|mut value| value.take())
    {
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    }
    controller_auth().clear_session();
    controller_stats().reset();
}

fn start_controller_native(
    host: String,
    port: u16,
    password: String,
    fingerprint: String,
) -> anyhow::Result<String> {
    stop_controller_native();
    let settings = ControllerSettings::new(host, port, password, &fingerprint)?;
    let runtime = Runtime::new().context("创建 Android 查看端 Tokio 运行时失败")?;
    let (commands, receiver) = controller::command_channel();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    controller_stats().reset();
    *CONTROLLER_COMMANDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(commands);
    *CONTROLLER_SHUTDOWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&shutdown));
    runtime.spawn(run_controller(
        settings,
        Arc::new(ControllerJniCallbacks),
        Arc::clone(controller_stats()),
        Arc::clone(controller_auth()),
        receiver,
        shutdown,
    ));
    *CONTROLLER_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime);
    Ok("connecting".to_string())
}

fn enqueue_controller(command: ControllerCommand) -> bool {
    let Some(sender) = CONTROLLER_COMMANDS
        .lock()
        .ok()
        .and_then(|commands| commands.clone())
    else {
        controller_stats()
            .rejected_input
            .fetch_add(1, Ordering::Relaxed);
        return false;
    };
    match sender.try_send(command) {
        Ok(()) => true,
        Err(_) => {
            controller_stats()
                .rejected_input
                .fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

fn callback<R: Default>(action: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>) -> R {
    let Some(vm) = JAVA_VM.get() else {
        return R::default();
    };
    let Ok(callback) = CALLBACK.lock() else {
        return R::default();
    };
    let Some(callback) = callback.as_ref() else {
        return R::default();
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return R::default();
    };
    let result = match action(&mut env, callback.as_obj()) {
        Ok(value) => value,
        Err(_) => {
            let _ = env.exception_clear();
            R::default()
        }
    };
    drop(env);
    // Callbacks only run on Rust worker threads attached above, never on the Java caller thread.
    unsafe { vm.detach_current_thread() };
    result
}

fn stats() -> &'static Arc<AgentStats> {
    STATS.get_or_init(|| Arc::new(AgentStats::default()))
}

fn current_host_epoch() -> u64 {
    ACTIVE_FRAMES
        .lock()
        .ok()
        .and_then(|active| {
            active
                .as_ref()
                .and_then(|active| active.host.is_current().then_some(active.host.value))
        })
        .unwrap_or(0)
}

fn frame_input_for_epoch(epoch: u64) -> anyhow::Result<Option<Arc<FrameInput>>> {
    anyhow::ensure!(epoch > 0, "Android 主机代际无效");
    let active = ACTIVE_FRAMES
        .lock()
        .map_err(|_| anyhow::anyhow!("Android 帧通道锁已中毒"))?;
    Ok(active.as_ref().and_then(|active| active.input_for(epoch)))
}

fn stop_native() {
    if let Some(endpoint) = ENDPOINT.lock().ok().and_then(|mut value| value.take()) {
        endpoint.close(0u32.into(), b"stopped");
    }
    if let Some(active) = ACTIVE_FRAMES.lock().ok().and_then(|mut value| value.take()) {
        active.host.deactivate();
    }
    if let Some(runtime) = RUNTIME.lock().ok().and_then(|mut value| value.take()) {
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    }
}

fn start_native(
    _host: String,
    port: u16,
    password: String,
    identity_key: Vec<u8>,
    allow_input: bool,
    initial_runtime: RuntimeSettings,
    source_resolution: EncodeResolution,
) -> anyhow::Result<String> {
    initial_runtime.validate()?;
    source_resolution.validate()?;
    let identity = TlsIdentity::from_pkcs8(&identity_key, "dfy-agent")
        .context("恢复 Android 持久设备身份失败")?;
    stop_native();
    let host_epoch = HostEpoch::allocate();
    let settings = AgentSettings::new(password, allow_input)?;
    let runtime = Runtime::new().context("创建 Android Tokio 运行时失败")?;
    let addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
    let (host, capture) = {
        let _runtime_guard = runtime.enter();
        AgentHost::bind_with_identity_and_source(
            addr,
            identity,
            settings,
            initial_runtime,
            Some(source_resolution),
            Arc::new(JniCallbacks {
                host: host_epoch.clone(),
            }),
            Arc::clone(stats()),
        )?
    };
    let local = host.local_addr()?;
    let fingerprint = host.fingerprint_hex();
    let endpoint = Arc::clone(&host.endpoint);
    *ENDPOINT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(endpoint);
    *ACTIVE_FRAMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ActiveFrames {
        host: host_epoch.clone(),
        input: Arc::new(host.frames.clone()),
    });
    runtime.spawn(async move {
        if let Err(error) = host.run(capture).await {
            tracing::error!(%error, "Android agent stopped");
        }
    });
    *RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime);
    Ok(format!("{fingerprint}\n{local}"))
}

fn throw_error(env: &mut JNIEnv, message: &str) {
    let _ = env.throw_new("java/lang/IllegalStateException", message);
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_setCallback(
    mut env: JNIEnv,
    _class: JClass,
    callback: JObject,
) {
    let Ok(vm) = env.get_java_vm() else { return };
    let _ = JAVA_VM.set(vm);
    match env.new_global_ref(callback) {
        Ok(callback) => {
            *CALLBACK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(callback);
        }
        Err(error) => throw_error(&mut env, &error.to_string()),
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_start(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    password: JObject,
    identity_key: JObject,
    allow_input: jboolean,
    width: jint,
    height: jint,
    fps: jint,
    bitrate_kbps: jint,
    source_width: jint,
    source_height: jint,
) -> jstring {
    let result = (|| -> anyhow::Result<String> {
        let host: String = env.get_string(&host)?.into();
        let password = env.convert_byte_array(JByteArray::from(password))?;
        let password = String::from_utf8(password).context("会话口令不是有效 UTF-8")?;
        let identity_key = env.convert_byte_array(JByteArray::from(identity_key))?;
        anyhow::ensure!(
            (64..=4096).contains(&(identity_key.len() as i32)),
            "Android 设备身份私钥长度无效"
        );
        anyhow::ensure!(host == "0.0.0.0", "当前被控端仅监听 0.0.0.0");
        let port = u16::try_from(port).context("监听端口必须在 0–65535 之间")?;
        let to_u32 = |value: jint, name: &str| {
            u32::try_from(value).with_context(|| format!("{name} 必须是非负整数"))
        };
        let initial_runtime = RuntimeSettings {
            fps: to_u32(fps, "fps")?,
            bitrate_kbps: to_u32(bitrate_kbps, "bitrate_kbps")?,
            resolution: Some(EncodeResolution {
                width: to_u32(width, "width")?,
                height: to_u32(height, "height")?,
            }),
        };
        initial_runtime.validate()?;
        let source_resolution = EncodeResolution {
            width: to_u32(source_width, "source_width")? & !1,
            height: to_u32(source_height, "source_height")? & !1,
        };
        source_resolution.validate()?;
        start_native(
            host,
            port,
            password,
            identity_key,
            allow_input != 0,
            initial_runtime,
            source_resolution,
        )
    })();
    match result {
        Ok(value) => env
            .new_string(value)
            .map_or(std::ptr::null_mut(), |value| value.into_raw()),
        Err(error) => {
            throw_error(&mut env, &format!("{error:#}"));
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_stop(_env: JNIEnv, _class: JClass) {
    stop_native();
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_hostEpoch(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    current_host_epoch() as i64
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_pushFrame(
    mut env: JNIEnv,
    _class: JClass,
    data: JObject,
    keyframe: jboolean,
    pts_us: jlong,
    host_epoch: jlong,
) {
    let result = (|| -> anyhow::Result<()> {
        let input = frame_input_for_epoch(host_epoch as u64)?;
        let Some(input) = input else { return Ok(()) };
        let bytes = JByteArray::from(data);
        let len = env.get_array_length(&bytes)?;
        anyhow::ensure!(
            len >= 0 && (len as usize) <= MAX_ENCODED_FRAME_BYTES,
            "编码帧超过 8 MiB 上限"
        );
        anyhow::ensure!(pts_us >= 0, "编码帧时间戳不能为负数");
        let bytes = env.convert_byte_array(&bytes)?;
        let accepted = input.push(EncodedFrame {
            data: bytes,
            keyframe: keyframe != 0,
            pts_us: pts_us as u64,
        });
        if !accepted {
            tracing::debug!("Android agent: encoded frame dropped");
        }
        Ok(())
    })();
    if let Err(error) = result {
        throw_error(&mut env, &error.to_string());
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_isKeyframeRequested(
    _env: JNIEnv,
    _class: JClass,
    host_epoch: jlong,
) -> jboolean {
    let requested = frame_input_for_epoch(host_epoch as u64)
        .ok()
        .flatten()
        .map(|input| input.take_keyframe_request())
        .unwrap_or(false);
    jboolean::from(requested)
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyNative_stats(env: JNIEnv, _class: JClass) -> jstring {
    let value = stats().snapshot();
    env.new_string(value)
        .map_or(std::ptr::null_mut(), |value| value.into_raw())
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_setCallback(
    mut env: JNIEnv,
    _class: JClass,
    callback: JObject,
) {
    let Ok(vm) = env.get_java_vm() else { return };
    let _ = JAVA_VM.set(vm);
    match env.new_global_ref(callback) {
        Ok(callback) => {
            *CONTROLLER_CALLBACK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(callback);
        }
        Err(error) => throw_error(&mut env, &error.to_string()),
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_connect(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    password: JObject,
    fingerprint: JString,
) -> jstring {
    let result = (|| -> anyhow::Result<String> {
        let host: String = env.get_string(&host)?.into();
        let fingerprint: String = env.get_string(&fingerprint)?.into();
        let password = Zeroizing::new(env.convert_byte_array(JByteArray::from(password))?);
        let password: Vec<u8> = password.iter().copied().collect();
        let password = String::from_utf8(password).context("会话口令不是有效 UTF-8")?;
        let port = u16::try_from(port).context("端口必须在 1–65535 之间")?;
        start_controller_native(host, port, password, fingerprint)
    })();
    match result {
        Ok(value) => env
            .new_string(value)
            .map_or(std::ptr::null_mut(), |value| value.into_raw()),
        Err(error) => {
            throw_error(&mut env, &format!("{error:#}"));
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_disconnectNative(
    _env: JNIEnv,
    _class: JClass,
) {
    stop_controller_native();
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_setControlAllowed(
    _env: JNIEnv,
    _class: JClass,
    allowed: jboolean,
) {
    enqueue_controller(ControllerCommand::ControlAllowed(allowed != 0));
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_pointer(
    _env: JNIEnv,
    _class: JClass,
    x: jint,
    y: jint,
) {
    enqueue_controller(ControllerCommand::Pointer(pointer_event(
        x as u32, y as u32,
    )));
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_button(
    _env: JNIEnv,
    _class: JClass,
    button: jint,
    down: jboolean,
    x: jint,
    y: jint,
) {
    let event = button_event(button as u32, down != 0, x as u32, y as u32);
    match event {
        Ok(event) => {
            enqueue_controller(ControllerCommand::Button(event));
        }
        Err(_) => {
            controller_stats()
                .rejected_input
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_wheel(
    _env: JNIEnv,
    _class: JClass,
    dx: jint,
    dy: jint,
) {
    enqueue_controller(ControllerCommand::Wheel(wheel_event(dx, dy)));
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_key(
    _env: JNIEnv,
    _class: JClass,
    usage: jint,
    down: jboolean,
    modifiers: jint,
) {
    let event = key_event(usage as u32, down != 0, modifiers as u32);
    match event {
        Ok(event) => {
            enqueue_controller(ControllerCommand::Key(event));
        }
        Err(_) => {
            controller_stats()
                .rejected_input
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_systemAction(
    _env: JNIEnv,
    _class: JClass,
    action: jint,
) {
    let action = match action {
        0 => SystemAction::Back,
        1 => SystemAction::Home,
        _ => return,
    };
    enqueue_controller(ControllerCommand::SystemAction(action));
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_releaseInput(
    _env: JNIEnv,
    _class: JClass,
) {
    enqueue_controller(ControllerCommand::ReleaseInput);
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_requestKeyframe(
    _env: JNIEnv,
    _class: JClass,
) {
    enqueue_controller(ControllerCommand::RequestKeyframe);
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_setQualityControl(
    mut env: JNIEnv,
    _class: JClass,
    width: jlong,
    height: jlong,
    fps: jlong,
    bitrate_kbps: jlong,
) -> jboolean {
    let request = match quality_request(width, height, fps, bitrate_kbps) {
        Ok(request) => request,
        Err(error) => {
            controller_stats()
                .quality_control_errors
                .fetch_add(1, Ordering::Relaxed);
            let _ = env.throw_new("java/lang/IllegalArgumentException", error.to_string());
            return jboolean::from(false);
        }
    };
    if !controller_auth().quality_control_allowed() {
        controller_stats()
            .quality_control_errors
            .fetch_add(1, Ordering::Relaxed);
        return jboolean::from(false);
    }
    jboolean::from(enqueue_controller(ControllerCommand::QualityControl(
        request,
    )))
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_sendStreamFeedback(
    mut env: JNIEnv,
    _class: JClass,
    window_ms: jlong,
    payload_bytes: jlong,
    packets: jlong,
    frames_assembled: jlong,
    frames_delivered: jlong,
    dropped_frames: jlong,
    stall_ms: jlong,
) -> jboolean {
    let result = stream_feedback(
        window_ms,
        payload_bytes,
        packets,
        frames_assembled,
        frames_delivered,
        dropped_frames,
        stall_ms,
    );
    match result {
        Ok(feedback) => {
            if !controller_auth().stream_feedback_allowed() {
                return jboolean::from(false);
            }
            jboolean::from(enqueue_controller(ControllerCommand::StreamFeedback(
                feedback,
            )))
        }
        Err(error) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", error.to_string());
            jboolean::from(false)
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_remote_dafeiyu_DfyControllerNative_stats(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let value = controller_stats().snapshot();
    env.new_string(value)
        .map_or(std::ptr::null_mut(), |value| value.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_channel_is_bounded() {
        let (capture, input) = dfy_session::CaptureLoop::external(1);
        let input = Arc::new(input);
        assert!(input.push(EncodedFrame {
            data: vec![1],
            keyframe: false,
            pts_us: 1,
        }));
        assert!(!input.push(EncodedFrame {
            data: vec![2],
            keyframe: false,
            pts_us: 2,
        }));
        capture.stop();
    }

    #[test]
    fn stale_host_epoch_cannot_access_replacement_frames() {
        let old_host = HostEpoch::allocate();
        let new_host = HostEpoch::allocate();
        let (old_capture, _old_input) = dfy_session::CaptureLoop::external(1);
        let (mut new_capture, new_input) = dfy_session::CaptureLoop::external(1);
        *ACTIVE_FRAMES.lock().unwrap() = Some(ActiveFrames {
            host: new_host.clone(),
            input: Arc::new(new_input),
        });

        assert_eq!(current_host_epoch(), new_host.value);
        assert!(frame_input_for_epoch(old_host.value).unwrap().is_none());
        let current = frame_input_for_epoch(new_host.value).unwrap().unwrap();
        new_capture.request_keyframe();
        let stale_keyframe = frame_input_for_epoch(old_host.value)
            .unwrap()
            .map(|input| input.take_keyframe_request())
            .unwrap_or(false);
        assert!(!stale_keyframe);
        assert!(current.take_keyframe_request());
        assert!(current.push(EncodedFrame {
            data: vec![7],
            keyframe: true,
            pts_us: 9,
        }));
        let received = new_capture.frames.try_recv().unwrap();
        assert_eq!(received.data, vec![7]);

        *ACTIVE_FRAMES.lock().unwrap() = None;
        old_host.deactivate();
        new_host.deactivate();
        old_capture.stop();
        new_capture.stop();
    }

    #[test]
    fn inactive_host_drops_late_input_before_jni_dispatch() {
        let host = HostEpoch::allocate();
        let event = dfy_proto::InputEvent::default();
        let current = encode_host_input(&host, &event).expect("current host input should encode");
        assert_eq!(current.0, host.value);
        host.deactivate();
        assert!(encode_host_input(&host, &event).is_none());
    }

    #[test]
    fn inactive_host_rejects_late_runtime_callback() {
        let host = HostEpoch::allocate();
        host.deactivate();
        let error = runtime_settings_callback(&host, RuntimeSettings::new(30, 4000)).unwrap_err();
        assert!(error.to_string().contains("主机代际已失效"));
    }
}
