// /* This Source Code Form is subject to the terms of the Mozilla Public
//  * License, v. 2.0. If a copy of the MPL was not distributed with this
//  * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
#![allow(non_snake_case)]

mod gl_glue;
mod simpleservo;

use std::{
    collections::HashMap, ffi::{CStr, CString}, mem::MaybeUninit, os::raw::{c_char, c_void}, sync::{atomic::AtomicUsize, Once, OnceLock}, time::Duration
};
use std::sync::mpsc;
use std::thread;
use ctor::ctor;
use core::ptr;

use ohos_sys::ace::xcomponent::native_interface_xcomponent::{OH_NATIVE_XCOMPONENT_OBJ, OH_NativeXComponent, OH_NativeXComponent_Callback, OH_NativeXComponent_GetTouchEvent, OH_NativeXComponent_TouchEvent};

use servo::embedder_traits::PromptResult;
use simpleservo::{Coordinates, EventLoopWaker, HostTrait, InitOptions, ServoGlue, SERVO};

use ohos_sys::napi::napi_module;
use ohos_sys::ace::xcomponent::native_interface_xcomponent::OH_NativeXComponent_GetXComponentOffset;
use ohos_sys::ace::xcomponent::native_interface_xcomponent::OH_NativeXComponent_GetXComponentSize;

#[macro_use]
extern crate log;
use log::LevelFilter;
use ohos_hilog::{Config, FilterBuilder};

#[link(name = "ace_napi.z")]
#[link(name = "ace_ndk.z")]
#[link(name = "hilog_ndk.z")]
#[link(name = "clang_rt.builtins", kind = "static")]
extern "C" {}

fn call(action: ServoAction) -> Result<(), &'static str>
{
        let tx = SERVO_CHANNEL.get().expect("Servo channel not initialized yet");
        tx.send(action).expect("Channel dead...");
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

#[repr(transparent)]
struct XComponentWrapper(*mut OH_NativeXComponent);
#[repr(transparent)]
struct WindowWrapper(*mut c_void);
unsafe impl Send for XComponentWrapper {}
unsafe impl Send for WindowWrapper {}

#[derive(Debug)]
enum ServoAction {
    WakeUp,
    LoadUrl(String),
}

impl ServoAction {
    fn do_action(&self, servo: &mut ServoGlue) {
        use ServoAction::*;
        let res = match self {
            WakeUp => servo.perform_updates(),
            LoadUrl(url) => servo.load_uri(url.as_str()) ,
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
extern "C" fn  on_surface_changed_cb(component: *mut OH_NativeXComponent, window: *mut c_void) {
    info!("on_surface_changed_cb");
}

extern "C" fn  on_surface_destroyed_cb(component: *mut OH_NativeXComponent, window: *mut c_void) {
    error!("on_surface_destroyed_cb is currently not implemented");
}

extern "C" fn  on_dispatch_touch_event_cb(component: *mut OH_NativeXComponent, window: *mut c_void) {
    info!("DispatchTouchEvent");
    let mut touch_event: MaybeUninit<OH_NativeXComponent_TouchEvent> = MaybeUninit::uninit();
    let res = unsafe {
        OH_NativeXComponent_GetTouchEvent(component, window, touch_event.as_mut_ptr())
    };
    // call(|s| s.perform_updates());
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
        }));
    })
}

// TODO: WIP-code, very un-rusty.
extern "C" fn register(env: napi_env, exports: napi_value) -> napi_value
{
    use ohos_sys::napi::{napi_status, napi_get_named_property};
    use ohos_sys::ace::xcomponent::native_interface_xcomponent::{OH_NativeXComponent, OH_NATIVE_XCOMPONENT_OBJ, OH_NativeXComponent_Callback};

    initialize_logging_once();

    // For some reason the register function is called twice, and napi_unwrap fails the first time.
    //if XCOMPONENT_REGISTERED.load(std::sync::atomic::Ordering::Acquire) == 1 {
    let mut exportInstance: napi_value = core::ptr::null_mut();
    let mut nativeXComponent: *mut OH_NativeXComponent = core::ptr::null_mut();

    let status: napi_status = unsafe { napi_get_named_property(env, exports,
        OH_NATIVE_XCOMPONENT_OBJ as *const u8,
        &mut exportInstance as *mut _)
    };
    if status != napi_status::napi_ok {
        error!("napi_get_named_property error: {status:?}");
        unsafe { napi_throw_error(env, core::ptr::null(), "Failed to get JsXcomponent...\0".as_ptr()); }
        // return exports;
    } else {
        info!("napi_get_named_property call successfull");
        let status = unsafe {
            ohos_sys::napi::napi_unwrap(env, exportInstance,
                &mut nativeXComponent as *mut *mut OH_NativeXComponent as *mut _)
        };
        if status == napi_status::napi_ok {
            let mut cbs = OH_NativeXComponent_Callback {
                OnSurfaceCreated: Some(on_surface_created_cb),
                OnSurfaceChanged: Some(on_surface_changed_cb),
                OnSurfaceDestroyed: Some(on_surface_destroyed_cb),
                DispatchTouchEvent: Some(on_dispatch_touch_event_cb)
            };
            use  ohos_sys::ace::xcomponent::native_interface_xcomponent::OH_NativeXComponent_RegisterCallback;
            let res = unsafe {
                OH_NativeXComponent_RegisterCallback(nativeXComponent, &mut cbs as *mut _)
            };
            if res != 0 {
                error!("Failed to register callbacks");
            }
            else {
                info!("Registerd callbacks successfully");
                XCOMPONENT_REGISTERED.fetch_add(1, std::sync::atomic::Ordering::Release);
            }
            }
        else {
            error!("napi_unwrap error on nativeXComponent: {status:?}");
            // return exports;
        }
    }

    let properties: [napi_property_descriptor;1] = [napi_property_descriptor {
        utf8name: "loadURL\0".as_ptr(),
        name: core::ptr::null_mut(),
        method: Some(__napi__load_url),
        getter: None,
        setter: None,
        value: core::ptr::null_mut(),
        attributes: 0, // todo: napi_default binding
        data: core::ptr::null_mut(),
    }];

    let res =unsafe {
        napi_define_properties(env.cast(), exports.cast(),
            properties.len(), properties.as_ptr())
    };
    if res == 0 {
        info!("Registered Node functions successfully");
    }
    else {
        error!("Failed to register Node functions");
    }
    return exports;
}

struct NapiModule(napi_module);
unsafe impl Sync for NapiModule {}

static SERVO_MOD_NAME: &'static str = "simpleservo\0";
static mut DEMO_MODULE: NapiModule = NapiModule(napi_module{
    nm_version: 1,
    nm_flags: 0,
    nm_filename: ptr::null(),
    nm_register_func: Some(register),
    nm_modname: SERVO_MOD_NAME.as_ptr(),
    nm_priv: ptr::null_mut(),
    reserved: [ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()],
});


#[ctor]
fn _init() {
    unsafe {
        // Note: This function seems to require napi_module to live long
        ohos_sys::napi::napi_module_register(&mut DEMO_MODULE.0 as *mut napi_module);
    }
}

// #[no_mangle]
// pub extern "C" fn servo_resize(c_coords: &ServoCoordinates) {
//     let coords = c_coords_to_rust_coords(c_coords);
//     debug!("resize: {:#?}", coords);
//     call(|s| s.resize(coords.clone()));
// }

// #[no_mangle]
// pub extern "C" fn servo_perform_updates() {
//     debug!("perform updates");
//     call(|s| s.perform_updates());
// }

// #[no_mangle]
// pub extern "C" fn servo_set_batch_mode(batch: bool) {
//     debug!("set batch mode");
//     call(|s| s.set_batch_mode(batch));
// }

// #[no_mangle]
// pub extern "C" fn servo_request_shutdown() {
//     debug!("request shutdown");
//     call(|s| s.request_shutdown());
// }

// #[no_mangle]
// pub extern "C" fn servo_deinit() {
//     debug!("deinit");
//     simpleservo::deinit();
// }
use napi_ohos::{bindgen_prelude::Undefined, sys::{napi_define_properties, napi_property_descriptor, napi_throw_error}, JsObject, JsUnknown, NapiRaw, NapiValue};
use napi_derive_ohos::{module_exports, napi};
use napi_ohos::sys::napi_unwrap;

#[napi]
pub fn load_url(url: String) -> Undefined {
    debug!("load url");
    call(ServoAction::LoadUrl(url)).expect("Failed to load url");
}

// #[no_mangle]
// pub extern "C" fn servo_reload() {
//     debug!("reload");
//     call(|s| s.reload());
// }

// #[no_mangle]
// pub extern "C" fn servo_stop() {
//     debug!("stop");
//     call(|s| s.stop());
// }

// #[no_mangle]
// pub extern "C" fn servo_refresh() {
//     debug!("refresh");
//     call(|s| s.refresh());
// }

// #[no_mangle]
// pub extern "C" fn servo_go_back() {
//     debug!("go back");
//     call(|s| s.go_back());
// }

// #[no_mangle]
// pub extern "C" fn servo_go_farward() {
//     debug!("go forward");
//     call(|s| s.go_forward());
// }

// #[no_mangle]
// pub extern "C" fn servo_scroll_start(dx: f32, dy: f32, x: i32, y: i32) {
//     debug!("scroll start");
//     call(|s| s.scroll_start(dx, dy, x, y));
// }

// #[no_mangle]
// pub extern "C" fn servo_scroll_end(dx: f32, dy: f32, x: i32, y: i32) {
//     debug!("scroll end");
//     call(|s| s.scroll_end(dx, dy, x, y));
// }

// #[no_mangle]
// pub extern "C" fn servo_scroll(dx: f32, dy: f32, x: i32, y: i32) {
//     debug!("scroll");
//     call(|s| s.scroll(dx, dy, x, y));
// }

// #[no_mangle]
// pub extern "C" fn servo_touch_down(x: f32, y: f32, pointer_id: i32) {
//     debug!("touch down");
//     call(|s| s.touch_down(x, y, pointer_id));
// }

// #[no_mangle]
// pub extern "C" fn servo_touch_up(x: f32, y: f32, pointer_id: i32) {
//     debug!("touch up");
//     call(|s| s.touch_up(x, y, pointer_id));
// }

// #[no_mangle]
// pub extern "C" fn servo_touch_move(x: f32, y: f32, pointer_id: i32) {
//     debug!("touch move");
//     call(|s| s.touch_move(x, y, pointer_id));
// }

// #[no_mangle]
// pub extern "C" fn servo_touch_cancel(x: f32, y: f32, pointer_id: i32) {
//     debug!("touch cancel");
//     call(|s| s.touch_cancel(x, y, pointer_id));
// }

// #[no_mangle]
// pub extern "C" fn servo_pinch_zoom_start(factor: f32, x: i32, y: i32) {
//     debug!("pinch zoom start");
//     call(|s| s.pinchzoom_start(factor, x as u32, y as u32));
// }

// #[no_mangle]
// pub extern "C" fn servo_pinch_zoom(factor: f32, x: i32, y: i32) {
//     debug!("pinch zoom");
//     call(|s| s.pinchzoom(factor, x as u32, y as u32));
// }

// #[no_mangle]
// pub extern "C" fn servo_pinch_zoom_end(factor: f32, x: i32, y: i32) {
//     debug!("pinch zoom end");
//     call(|s| s.pinchzoom_end(factor, x as u32, y as u32));
// }

// #[no_mangle]
// pub extern "C" fn servo_click(x: f32, y: f32) {
//     debug!("click");
//     call(|s| s.click(x, y));
// }

// #[no_mangle]
// pub extern "C" fn servo_pause_compositor() {
//     debug!("pause compositor");
//     call(|s| s.pause_compositor());
// }

// #[no_mangle]
// pub extern "C" fn servo_resume_compositor(surface: *mut c_void, coordinates: &ServoCoordinates) {
//     debug!("resume compositor");
//     let coords = c_coords_to_rust_coords(coordinates);
//     call(|s| s.resume_compositor(surface, coords.clone()));
// }

// #[no_mangle]
// pub extern "C" fn servo_media_session_action(action: i32) {
//     debug!("media session action");
//     call(|s| s.media_session_action(action.into()));
// }

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

    fn on_url_changed(&self, url: String) {}

    fn on_history_changed(&self, can_go_back: bool, can_go_forward: bool) {}

    fn on_animating_changed(&self, animating: bool) {}

    fn on_ime_show(
        &self,
        input_type: servo::msg::constellation_msg::InputMethodType,
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

