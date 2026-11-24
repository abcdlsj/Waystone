//! Exercises the C ABI the Swift apps call: start a host and a controller engine
//! through the FFI, relay the signaling through a channel and check that media
//! frames arrive with a decodable header.

use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use waystone_rtc::ffi::{
    ws_engine_accept_answer, ws_engine_accept_offer, ws_engine_add_candidate, ws_engine_offer,
    ws_engine_send_input, ws_engine_send_video, ws_engine_start, ws_engine_stop, WsEventCallback,
    WS_EVENT_CONTROL, WS_EVENT_INPUT, WS_EVENT_VIDEO,
};

/// Collects the bytes handed to the callback.
#[derive(Default)]
struct Collected {
    video: Vec<Vec<u8>>,
    input: Vec<Vec<u8>>,
    control: Vec<String>,
}

extern "C" fn on_event(kind: i32, data: *const u8, len: usize, ctx: *mut c_void) {
    let collected = unsafe { &*(ctx as *const Mutex<Collected>) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let mut guard = collected.lock().unwrap();
    match kind {
        WS_EVENT_VIDEO => guard.video.push(bytes),
        WS_EVENT_INPUT => guard.input.push(bytes),
        WS_EVENT_CONTROL => guard
            .control
            .push(String::from_utf8_lossy(&bytes).into_owned()),
        _ => {}
    }
}

struct Harness {
    handle: *mut waystone_rtc::ffi::WsEngine,
    events: Arc<Mutex<Collected>>,
}

impl Harness {
    fn start(config: &str) -> Harness {
        let events = Arc::new(Mutex::new(Collected::default()));
        let ctx = Arc::as_ptr(&events) as *mut c_void;
        let callback: WsEventCallback = on_event;
        let c_config = CString::new(config).unwrap();
        let mut error = [0i8; 256];
        let handle = unsafe {
            ws_engine_start(
                c_config.as_ptr(),
                callback,
                ctx,
                error.as_mut_ptr() as *mut std::ffi::c_char,
                error.len(),
            )
        };
        assert!(!handle.is_null(), "engine failed to start: {:?}", error);
        Harness { handle, events }
    }

    fn take_control(&self) -> Vec<String> {
        std::mem::take(&mut self.events.lock().unwrap().control)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        unsafe { ws_engine_stop(self.handle) };
    }
}

/// Waits for a control event whose JSON contains `needle`.
fn wait_control(harness: &Harness, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a control event containing {needle}"
        );
        for event in harness.take_control() {
            if event.contains(needle) {
                return event;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Extracts a JSON string field from a control event.
fn field(json: &str, name: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    value[name].as_str().unwrap_or_default().to_string()
}

#[test]
fn ffi_session_delivers_media_and_input() {
    let host = Harness::start(r#"{"role":"host","bind":"127.0.0.1:0"}"#);
    let controller = Harness::start(
        r#"{"role":"controller","bind":"127.0.0.1:0","audio":true,
            "viewport":{"width":1280,"height":720,"dpi":300,"fps":60}}"#,
    );

    unsafe { ws_engine_offer(controller.handle) };
    let offer_json = wait_control(&controller, "local_offer");
    let offer = field(&offer_json, "sdp");
    let c_offer = CString::new(offer).unwrap();
    unsafe { ws_engine_accept_offer(host.handle, c_offer.as_ptr()) };

    let answer_json = wait_control(&host, "local_answer");
    let answer = field(&answer_json, "sdp");
    let c_answer = CString::new(answer).unwrap();
    unsafe { ws_engine_accept_answer(controller.handle, c_answer.as_ptr()) };

    // Trickle both sides until they report a connection.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut controller_connected = false;
    let mut host_connected = false;
    while !(controller_connected && host_connected) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the connection"
        );
        for event in controller.take_control() {
            if event.contains("local_candidate") {
                let candidate = CString::new(field(&event, "candidate")).unwrap();
                unsafe { ws_engine_add_candidate(host.handle, candidate.as_ptr()) };
            } else if event.contains("connected") {
                controller_connected = true;
            }
        }
        for event in host.take_control() {
            if event.contains("local_candidate") {
                let candidate = CString::new(field(&event, "candidate")).unwrap();
                unsafe { ws_engine_add_candidate(controller.handle, candidate.as_ptr()) };
            } else if event.contains("connected") {
                host_connected = true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Host writes a keyframe; the controller must receive a video event.
    let mut frame = Vec::new();
    for (nal_type, payload) in [
        (32u8, &[0x0c, 0x01, 0xff][..]),
        (33u8, &[0x0c, 0x01, 0xff][..]),
        (34u8, &[0x0c, 0x01, 0xff][..]),
        (19u8, &[0xaa, 0xbb, 0xcc, 0xdd][..]),
    ] {
        frame.extend_from_slice(&[0, 0, 0, 1]);
        frame.extend_from_slice(&[nal_type << 1, 0x01]);
        frame.extend_from_slice(payload);
    }
    for index in 0..10 {
        unsafe { ws_engine_send_video(host.handle, frame.as_ptr(), frame.len(), index * 3000) };
        std::thread::sleep(Duration::from_millis(30));
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a video frame"
        );
        if let Some(raw) = controller.events.lock().unwrap().video.first().cloned() {
            let (header, payload) = waystone_rtc::ffi::MediaHeader::parse(&raw)
                .expect("media header must be decodable");
            assert_eq!(header.kind, WS_EVENT_VIDEO as u8);
            assert!(!payload.is_empty());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Input travels the other way.
    let input = vec![0x06u8];
    unsafe { ws_engine_send_input(controller.handle, true, input.as_ptr(), input.len()) };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for an input frame"
        );
        if let Some(raw) = host.events.lock().unwrap().input.first().cloned() {
            let (header, payload) = waystone_rtc::ffi::MediaHeader::parse(&raw).unwrap();
            assert_eq!(header.kind, WS_EVENT_INPUT as u8);
            assert!(header.is_reliable());
            assert_eq!(payload, input.as_slice());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn ffi_rejects_a_bad_role() {
    let config = CString::new(r#"{"role":"router"}"#).unwrap();
    let mut error = [0i8; 256];
    let handle = unsafe {
        ws_engine_start(
            config.as_ptr(),
            on_event,
            std::ptr::null_mut(),
            error.as_mut_ptr() as *mut std::ffi::c_char,
            error.len(),
        )
    };
    assert!(handle.is_null());
    let message = unsafe { std::ffi::CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert!(message.contains("unknown role"), "unexpected: {message}");
}

#[test]
fn ffi_reports_invalid_json() {
    let config = CString::new("{").unwrap();
    let mut error = [0i8; 256];
    let handle = unsafe {
        ws_engine_start(
            config.as_ptr(),
            on_event,
            std::ptr::null_mut(),
            error.as_mut_ptr() as *mut std::ffi::c_char,
            error.len(),
        )
    };
    assert!(handle.is_null());
    let message = unsafe { std::ffi::CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    assert!(message.contains("JSON"), "unexpected: {message}");
}

/// State shared with [`stop_from_callback`].
struct StopFromCallback {
    armed: bool,
    published: std::sync::atomic::AtomicBool,
    handle: std::sync::atomic::AtomicPtr<waystone_rtc::ffi::WsEngine>,
    calls: std::sync::atomic::AtomicUsize,
}

extern "C" fn stop_from_callback(_kind: i32, _data: *const u8, _len: usize, ctx: *mut c_void) {
    use std::sync::atomic::Ordering;
    let state = unsafe { &*(ctx as *const StopFromCallback) };
    state.calls.fetch_add(1, Ordering::SeqCst);
    if state.armed {
        // Block the engine until the test has published the handle, so the
        // first event is the one that stops it.
        while !state.published.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    // Stop on the first event, from the engine's own worker thread.
    let handle = state.handle.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !handle.is_null() {
        unsafe { ws_engine_stop(handle) };
    }
}

#[test]
fn ffi_stop_from_inside_the_callback_is_safe() {
    use std::sync::atomic::Ordering;
    let state = Box::new(StopFromCallback {
        armed: true,
        published: std::sync::atomic::AtomicBool::new(false),
        handle: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = &*state as *const StopFromCallback as *mut c_void;
    let config = CString::new(
        r#"{"role":"controller","bind":"127.0.0.1:0",
            "viewport":{"width":640,"height":480,"dpi":100,"fps":30}}"#,
    )
    .unwrap();
    let mut error = [0i8; 256];
    // The controller emits its offer on its own; the callback waits for the
    // handle before acting on it.
    let handle = unsafe {
        ws_engine_start(
            config.as_ptr(),
            stop_from_callback,
            ctx,
            error.as_mut_ptr() as *mut std::ffi::c_char,
            error.len(),
        )
    };
    assert!(!handle.is_null());
    state.handle.store(handle, Ordering::SeqCst);
    state.published.store(true, Ordering::SeqCst);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !state.handle.load(Ordering::SeqCst).is_null() {
        assert!(std::time::Instant::now() < deadline, "callback never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The stop happened inside the callback; nothing may follow it.
    let calls = state.calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(state.calls.load(Ordering::SeqCst), calls);
}

#[test]
fn ffi_no_callback_after_stop_returns() {
    use std::sync::atomic::Ordering;
    let state = Box::new(StopFromCallback {
        armed: false,
        published: std::sync::atomic::AtomicBool::new(false),
        handle: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = &*state as *const StopFromCallback as *mut c_void;
    let config = CString::new(
        r#"{"role":"controller","bind":"127.0.0.1:0",
            "viewport":{"width":640,"height":480,"dpi":100,"fps":30}}"#,
    )
    .unwrap();
    let mut error = [0i8; 256];
    let handle = unsafe {
        ws_engine_start(
            config.as_ptr(),
            stop_from_callback,
            ctx,
            error.as_mut_ptr() as *mut std::ffi::c_char,
            error.len(),
        )
    };
    assert!(!handle.is_null());
    unsafe { ws_engine_offer(handle) };
    unsafe { ws_engine_stop(handle) };
    let calls = state.calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(state.calls.load(Ordering::SeqCst), calls);
    // Freeing the context now is what Swift does in deinit.
    drop(state);
}
