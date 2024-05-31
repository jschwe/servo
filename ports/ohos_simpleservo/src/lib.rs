// /* This Source Code Form is subject to the terms of the Mozilla Public
//  * License, v. 2.0. If a copy of the MPL was not distributed with this
//  * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
#![allow(non_snake_case)]

mod gl_glue;
mod simpleservo;

use std::{collections::HashMap, env, ffi::{CStr, CString}, mem::MaybeUninit, os::raw::{c_char, c_void}, sync::{atomic::AtomicUsize, Once, OnceLock}, time::Duration};
use std::sync::mpsc;
use std::thread;
use core::ptr;
use std::convert::{TryFrom, TryInto};

use ohos_sys::ace::xcomponent::native_interface_xcomponent::{OH_NATIVE_XCOMPONENT_OBJ, OH_NativeXComponent, OH_NativeXComponent_Callback, OH_NativeXComponent_GetTouchEvent, OH_NativeXComponent_TouchEvent, OH_NativeXComponent_TouchEventType};

use servo::embedder_traits::PromptResult;
use simpleservo::{Coordinates, EventLoopWaker, HostTrait, InitOptions, ServoGlue, SERVO};

use ohos_sys::napi::{napi_define_properties, napi_module, napi_status};
use ohos_sys::ace::xcomponent::native_interface_xcomponent::OH_NativeXComponent_GetXComponentOffset;
use ohos_sys::ace::xcomponent::native_interface_xcomponent::OH_NativeXComponent_GetXComponentSize;

use napi_ohos::{bindgen_prelude::Undefined, Env, JsFunction, JsObject, JsString, NapiRaw, sys::{napi_property_descriptor, napi_throw_error}};
use napi_derive_ohos::{module_exports, napi};
use napi_ohos::sys::napi_unwrap;
use servo::config::opts;
use servo::euclid::Point2D;
use servo::style::Zero;

#[macro_use]
extern crate log;
use log::LevelFilter;
use ohos_hilog::{Config, FilterBuilder};

mod backtrace;
//mod touch_gesture;

#[link(name = "ace_napi.z")]
#[link(name = "ace_ndk.z")]
#[link(name = "hilog_ndk.z")]
#[link(name = "native_window")]
#[link(name = "clang_rt.builtins", kind = "static")]
extern "C" {}

#[derive(Debug)]
enum CallError{
    ChannelNotInitialized,
    ChannelDied,
}

fn call(action: ServoAction) -> Result<(), CallError>
{
        let tx = SERVO_CHANNEL.get().ok_or(CallError::ChannelNotInitialized)?;
        tx.send(action).map_err(|_| CallError::ChannelDied)?;
        Ok(())
}
#[repr(C)]
pub struct ServoOptions {
    pub args: *mut c_char,
    pub url: *mut c_char,
    pub coordinates: ServoCoordinates,
    pub density: f32,
    pub enable_subpixel_text_antialiasing: bool,
    pub vr_external_context: u64,
    pub log_str: *mut c_char,
    pub gst_debug_str: *mut c_char,
    pub enable_logs: bool,
}

#[repr(C)]
pub struct ServoCoordinates {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub fb_width: i32,
    pub fb_height: i32,
}

use ohos_sys::napi::{napi_env, napi_value};
use std::sync::mpsc::{Sender, Receiver};
use napi_ohos::bindgen_prelude::{Function, FunctionRef};
use napi_ohos::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode, UnknownReturnValue};

#[repr(transparent)]
struct XComponentWrapper(*mut OH_NativeXComponent);
#[repr(transparent)]
struct WindowWrapper(*mut c_void);
unsafe impl Send for XComponentWrapper {}
unsafe impl Send for WindowWrapper {}

#[derive(Debug, Copy, Clone)]
enum TouchEventType {
    Down,
    Up,
    Move,
    Scroll{dx: f32, dy: f32},
    Cancel,
    Unknown,
}

#[derive(Debug)]
enum ServoAction {
    WakeUp,
    LoadUrl(String),
    TouchEvent{kind: TouchEventType, x: f32, y: f32, pointer_id: i32},
}


#[derive(Debug, Copy, Clone, Default)]
enum Direction2D {
    Horizontal,
    Vertical,
    #[default]
    Free
}
#[derive(Clone, Debug)]
struct TouchTracker {
    last_position: Point2D<f32, f32>,
}

impl TouchTracker {
    fn new(first_point: Point2D<f32, f32>) -> Self {
        TouchTracker {
            last_position: first_point
        }
    }
}

// Todo: Need to check if OnceLock is suitable, or if the TS function can be destroyed, e.g.
// if the activity gets suspended.
static SET_URL_BAR_CB: OnceLock<ThreadsafeFunction<String, ErrorStrategy::Fatal>> = OnceLock::new();

struct TsThreadState {
    // last_touch_event: Option<OH_NativeXComponent_TouchEvent>,
    velocity_tracker: Option<TouchTracker>
}

impl TsThreadState {
    const fn new() -> Self {
        Self {
            velocity_tracker: None,
        }
    }
}

static mut TS_THREAD_STATE: TsThreadState = TsThreadState::new();

impl ServoAction {
    fn dispatch_touch_event(servo: &mut ServoGlue, kind: TouchEventType, x: f32, y: f32, pointer_id: i32) -> Result<(), &'static str> {
        match kind {
            TouchEventType::Down => servo.touch_down(x, y, pointer_id),
            TouchEventType::Up => servo.touch_up(x, y, pointer_id),
            TouchEventType::Scroll{dx, dy} => servo.scroll(dx, dy, x as i32, y as i32),
            TouchEventType::Move => servo.touch_move(x, y, pointer_id),
            TouchEventType::Cancel => servo.touch_cancel(x, y, pointer_id),
            TouchEventType::Unknown => Err("Can't dispatch Unknown Touch Event"),
        }
    }

    fn do_action(&self, servo: &mut ServoGlue) {
        use ServoAction::*;
        let res = match self {
            WakeUp => servo.perform_updates(),
            LoadUrl(url) => servo.load_uri(url.as_str()),
            TouchEvent {kind, x, y, pointer_id} => Self::dispatch_touch_event(servo, *kind, *x, *y, *pointer_id),
        };
        if let Err(e) = res {
            error!("Failed to do {self:?} with error {e}");
        }
    }
}

static SERVO_CHANNEL: OnceLock<Sender<ServoAction>> = OnceLock::new();


#[no_mangle]
pub extern "C" fn  on_surface_created_cb(xcomponent: *mut OH_NativeXComponent, window: *mut c_void) {
    info!("on_surface_created_cb");

    let (tx_done, rx_done): (Sender<Result<(), &'static str>>, Receiver<Result<(), &'static str>>) = mpsc::channel();
    let xc_wrapper = XComponentWrapper(xcomponent);
    let window_wrapper = WindowWrapper(window);

    // Each thread will send its id via the channel
    let main_surface_thread = thread::spawn(move || {

        let (tx, rx): (Sender<ServoAction>, Receiver<ServoAction>) = mpsc::channel();

        SERVO_CHANNEL.set(tx.clone()).expect("Servo channel already initialized");

        let wakeup = Box::new(WakeupCallback::new(tx));
        let callbacks = Box::new(HostCallbacks::new());

        let egl_init = gl_glue::egl::init().expect("egl::init() failed");
        let mut servo = simpleservo::init(window_wrapper.0,
             xc_wrapper.0,
             egl_init.gl_wrapper,
              wakeup,
              callbacks)
              .expect("Servo initialization failed");


        info!("Surface created!");
        tx_done.send(Ok(())).unwrap();
        drop(tx_done);

        while let Ok(action) = rx.recv() {
            info!("Wakeup message received!");
            action.do_action(&mut servo);
        }

        info!("Sender disconnected - Terminating main surface thread");
    });

    match rx_done.recv() {
        Ok(Err(reason)) => error!("Failed to initialize servo with {reason}"),
        Ok(Ok(())) => {},
        Err(e) => error!("Channel failure"),
    }
    info!("Returning from on_surface_created_cb");

}


// Todo: Probably we need to block here, until the main thread has processed the change.
pub extern "C" fn  on_surface_changed_cb(component: *mut OH_NativeXComponent, window: *mut c_void) {
    info!("on_surface_changed_cb");
}

pub extern "C" fn  on_surface_destroyed_cb(component: *mut OH_NativeXComponent, window: *mut c_void) {
    error!("on_surface_destroyed_cb is currently not implemented");
}

pub extern "C" fn  on_dispatch_touch_event_cb(component: *mut OH_NativeXComponent, window: *mut c_void) {
    info!("DispatchTouchEvent");
    let mut touch_event: MaybeUninit<OH_NativeXComponent_TouchEvent> = MaybeUninit::uninit();
    let res = unsafe {
        OH_NativeXComponent_GetTouchEvent(component, window, touch_event.as_mut_ptr())
    };
    if res != 0 {
        error!("OH_NativeXComponent_GetTouchEvent failed with {res}");
        return;
    }
    let touch_event = unsafe { touch_event.assume_init()};
    let kind: TouchEventType = match touch_event.type_ {
        OH_NativeXComponent_TouchEventType::OH_NATIVEXCOMPONENT_DOWN => {
            if touch_event.id == 0 {
                unsafe {
                    let old = TS_THREAD_STATE.velocity_tracker.replace(TouchTracker::new(Point2D::new(touch_event.x, touch_event.y)));
                    assert!(old.is_none());
                }
            }
            TouchEventType::Down

        },
        OH_NativeXComponent_TouchEventType::OH_NATIVEXCOMPONENT_UP => {
            if touch_event.id == 0 {
                unsafe {
                    let old = TS_THREAD_STATE.velocity_tracker.take();
                    assert!(old.is_some());
                }
            }
            TouchEventType::Up
        },
        OH_NativeXComponent_TouchEventType::OH_NATIVEXCOMPONENT_MOVE => {
            // SAFETY: We only access TS_THREAD_STATE from the main TS thread.
            if touch_event.id == 0 {
                let (lastX, lastY) = unsafe {
                    if let Some(last_event) = &mut TS_THREAD_STATE.velocity_tracker {
                        let touch_point = last_event.last_position;
                        last_event.last_position = Point2D::new(touch_event.x, touch_event.y);
                        (touch_point.x, touch_point.y)
                    } else {
                        error!("Move Event received, but no previous touch event was stored!");
                        // todo: handle this error case
                        panic!("Move Event received, but no previous touch event was stored!");
                    }
                };
                let dx = touch_event.x - lastX;
                let dy = touch_event.y - lastY;
                TouchEventType::Scroll {dx, dy}
            } else {
                TouchEventType::Move
            }
        },
        OH_NativeXComponent_TouchEventType::OH_NATIVEXCOMPONENT_CANCEL => {
            if touch_event.id == 0 {
                unsafe {
                    let old = TS_THREAD_STATE.velocity_tracker.take();
                    assert!(old.is_some());
                }
            }
            TouchEventType::Cancel
        },
        _ => {
            error!("Failed to dispatch call for touch Event {:?}", touch_event.type_);
            TouchEventType::Unknown
        },
    };
    if let Err(e) = call(ServoAction::TouchEvent {kind, x: touch_event.x, y: touch_event.y, pointer_id: touch_event.id}) {
        error!("Failed to dispatch call for touch Event {kind:?}");
    }
}

fn initialize_logging_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ohos_hilog::init_once(
            Config::default()
                .with_max_level(LevelFilter::Debug)
                .with_tag("simpleservo"),
        );
        info!("Servo Register callback called!");

        std::panic::set_hook(Box::new(|info| {
            error!("Panic in Rust code");
            error!("PanicInfo: {info}");
            let msg = match info.payload().downcast_ref::<&'static str>() {
                Some(s) => *s,
                None => match info.payload().downcast_ref::<String>() {
                    Some(s) => &**s,
                    None => "Box<Any>",
                },
            };
            let current_thread = thread::current();
            let name = current_thread.name().unwrap_or("<unnamed>");
            let stderr = std::io::stderr();
            let mut stderr = stderr.lock();
            if let Some(location) = info.location() {
                let _ = error!("{} (thread {}, at {}:{})",
                    msg,
                    name,
                    location.file(),
                    location.line()
                );
            } else {
                let _ = error!("{} (thread {})", msg, name);
            }

            let _ = crate::backtrace::print();
            drop(stderr);

            // if opts::get().hard_fail && !opts::get().multiprocess {
            //     std::process::exit(1);
            // }

            error!("{}", msg);
        }));
    })
}

fn register_xcomponent_callbacks(env: &Env, xcomponent: &JsObject) -> napi_ohos::Result<()> {
    info!("napi_get_named_property call successfull");
    let raw = unsafe { xcomponent.raw() };
    let raw_env = env.raw();
    let mut nativeXComponent: *mut OH_NativeXComponent = core::ptr::null_mut();
    unsafe {
        let res = napi_ohos::sys::napi_unwrap(raw_env, raw , &mut nativeXComponent as *mut *mut OH_NativeXComponent as *mut *mut c_void);
        assert!(res.is_zero());
    }
    info!("Got nativeXComponent!");
    let cbs = Box::new(OH_NativeXComponent_Callback {
        OnSurfaceCreated: Some(on_surface_created_cb),
        OnSurfaceChanged: Some(on_surface_changed_cb),
        OnSurfaceDestroyed: Some(on_surface_destroyed_cb),
        DispatchTouchEvent: Some(on_dispatch_touch_event_cb)
    });
    use  ohos_sys::ace::xcomponent::native_interface_xcomponent::OH_NativeXComponent_RegisterCallback;
    let res = unsafe {
        OH_NativeXComponent_RegisterCallback(nativeXComponent, Box::leak(cbs) as *mut _)
    };
    if res != 0 {
        error!("Failed to register callbacks");
    }
    else {
        info!("Registerd callbacks successfully");
    }
    Ok(())
}

#[allow(unused)]
fn debug_jsobject(obj: &JsObject, obj_name: &str) -> napi_ohos::Result<()> {
    let names = obj.get_property_names()?;
    error!("Getting property names of object {obj_name}");
    let len = names.get_array_length()?;
    error!("{obj_name} has {len} elements");
    for i in 0..len {
        let name: JsString = names.get_element(i)?;
        let name = name.into_utf8()?;
        error!("{obj_name} property {i}: {}", name.as_str()?)
    }
    Ok(())
}

#[module_exports]
fn init(mut exports: JsObject, env: Env) -> napi_ohos::Result<()> {
    initialize_logging_once();
    error!("simpleservo init function called");
    if let Ok(xcomponent) = exports.get_named_property::<JsObject>("__NATIVE_XCOMPONENT_OBJ__") {
        register_xcomponent_callbacks(&env, &xcomponent)?;
    }

    info!("Finished init");
    Ok(())
}

#[napi(js_name = "loadURL")]
pub fn load_url(url: String) -> Undefined {
    debug!("load url");
    call(ServoAction::LoadUrl(url)).expect("Failed to load url");
}

#[napi(js_name = "registerURLcallback")]
pub fn register_url_callback(cb: JsFunction) -> napi_ohos::Result<()>{
    info!("register_url_callback called!");
    let tsfn: ThreadsafeFunction<String, ErrorStrategy::Fatal> = cb.create_threadsafe_function(
        1, |ctx| {
            debug!("url callback argument transformer called with arg {}", ctx.value);
            let s = ctx.env.create_string_from_std(ctx.value)
                .inspect_err(|e| error!("Failed to create JsString: {e:?}") )?;
            Ok(vec![s])
        }
    )?;
    // We ignore any error for now - but probably we should propagate it back to the TS layer.
    let _ = SET_URL_BAR_CB.set(tsfn)
        .inspect_err(|_| warn!("Failed to set URL callback - register_url_callback called twice?"));
    Ok(())
}


fn get_options(
    opts: &mut ServoOptions,
    surface: *mut c_void,
) -> Result<(InitOptions, bool, Option<String>, Option<String>), String> {
    let args = get_string_from_c_char(opts.args);
    let url = get_string_from_c_char(opts.url);
    let log_str = get_string_from_c_char(opts.log_str);
    let gst_debug_str = get_string_from_c_char(opts.gst_debug_str);
    let density = opts.density;
    let log = opts.enable_logs;
    let coordinates = c_coords_to_rust_coords(&opts.coordinates);

    let args = match args {
        Some(args) => serde_json::from_str(&args)
            .map_err(|_| "Invalid arguments. Servo arguments must be formatted as a JSON array")?,
        None => None,
    };

    // disable JIT
    let mut prefs = HashMap::new();
    prefs.insert("js.baseline_interpreter.enabled".to_string(), false.into());
    prefs.insert("js.baseline_jit.enabled".to_string(), false.into());
    prefs.insert("js.ion.enabled".to_string(), false.into());

    let opts = InitOptions {
        args: args.unwrap_or(vec![]),
        coordinates,
        density,
        xr_discovery: None,
        surfman_integration: simpleservo::SurfmanIntegration::Widget(surface),
        prefs: Some(prefs),
    };

    Ok((opts, log, log_str, gst_debug_str))
}

fn get_string_from_c_char(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }

    unsafe {
        let cstr = CStr::from_ptr(ptr);
        match cstr.to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => None,
        }
    }
}

fn c_coords_to_rust_coords(c_coords: &ServoCoordinates) -> Coordinates {
    Coordinates::new(
        c_coords.x,
        c_coords.y,
        c_coords.width,
        c_coords.height,
        c_coords.fb_width,
        c_coords.fb_height,
    )
}

#[derive(Clone)]
pub struct WakeupCallback {
    chan: Sender<ServoAction>
}

impl WakeupCallback {
    pub fn new(chan: Sender<ServoAction>) -> Self {
        WakeupCallback {
            chan
        }
    }
}

impl EventLoopWaker for WakeupCallback {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        info!("wake called!");
        self.chan.send(ServoAction::WakeUp).unwrap_or_else(|e| {error!("Failed to send wake message with: {e}");}) ;
    }
}

// TODO HostCallbacks
struct HostCallbacks {}

impl HostCallbacks {
    pub fn new() -> Self {
        HostCallbacks {}
    }
}

#[allow(unused)]
impl HostTrait for HostCallbacks {
    fn prompt_alert(&self, msg: String, trusted: bool) {}

    fn prompt_ok_cancel(&self, msg: String, trusted: bool) -> servo::embedder_traits::PromptResult {
        warn!("Prompt not implemented. Cacelled. {}", msg);
        PromptResult::Secondary
    }

    fn prompt_yes_no(&self, msg: String, trusted: bool) -> servo::embedder_traits::PromptResult {
        warn!("Prompt not implemented. Cacelled. {}", msg);
        PromptResult::Secondary
    }

    fn prompt_input(&self, msg: String, default: String, trusted: bool) -> Option<String> {
        warn!("Input prompt not implemented. Cacelled. {}", msg);
        Some(default)
    }

    fn on_load_started(&self) {
        warn!("on_load_started not implemented")
        // todo: android calls java method, which enables /  disables some buttons etc.
    }

    fn on_load_ended(&self) {
        warn!("on_load_ended not implemented")
    }

    fn on_shutdown_complete(&self) {}

    fn on_title_changed(&self, title: Option<String>) {}

    fn on_allow_navigation(&self, url: String) -> bool {
        false
    }

    fn on_url_changed(&self, url: String) {
        debug!("Hosttrait `on_url_changed` called with new url: {url}");
        if let Some(cb) = SET_URL_BAR_CB.get() {
            cb.call(url, ThreadsafeFunctionCallMode::Blocking);
        } else {
            warn!("`on_url_changed` called without a registered callback")
        }
    }

    fn on_history_changed(&self, can_go_back: bool, can_go_forward: bool) {}

    fn on_animating_changed(&self, animating: bool) {}

    fn on_ime_show(
        &self,
        input_type: servo::embedder_traits::InputMethodType,
        text: Option<(String, i32)>,
        multiline: bool,
        bounds: servo::webrender_api::units::DeviceIntRect,
    ) {
    }

    fn on_ime_hide(&self) {}

    fn get_clipboard_contents(&self) -> Option<String> {
        None
    }

    fn set_clipboard_contents(&self, contents: String) {}

    fn on_media_session_metadata(&self, title: String, artist: String, album: String) {}

    fn on_media_session_playback_state_change(
        &self,
        state: servo::embedder_traits::MediaSessionPlaybackState,
    ) {
    }

    fn on_media_session_set_position_state(
        &self,
        duration: f64,
        position: f64,
        playback_rate: f64,
    ) {
    }

    fn on_devtools_started(&self, port: Result<u16, ()>, token: String) {}

    fn on_panic(&self, reason: String, backtrace: Option<String>) {}

    fn show_context_menu(&self, title: Option<String>, items: Vec<String>) {}
}

