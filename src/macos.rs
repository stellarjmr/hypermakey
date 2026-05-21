use crate::state::{HyperAction, HyperState};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::error::Error as StdError;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fmt;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;

const CAPS_LOCK_HID_USAGE_PAGE: u32 = 0x07;
const CAPS_LOCK_HID_USAGE: u32 = 0x39;

const KEY_CAPS_LOCK: u16 = 57;
const KEY_LEFT_COMMAND: u16 = 55;
const KEY_LEFT_SHIFT: u16 = 56;
const KEY_LEFT_OPTION: u16 = 58;
const KEY_LEFT_CONTROL: u16 = 59;

const EVENT_KEY_DOWN: CGEventType = 10;
const EVENT_KEY_UP: CGEventType = 11;
const EVENT_FLAGS_CHANGED: CGEventType = 12;
const EVENT_TAP_DISABLED_BY_TIMEOUT: CGEventType = 0xFFFF_FFFE;
const EVENT_TAP_DISABLED_BY_USER_INPUT: CGEventType = 0xFFFF_FFFF;

const EVENT_FIELD_KEYBOARD_EVENT_KEYCODE: CGEventField = 9;
const EVENT_FIELD_EVENT_SOURCE_USER_DATA: CGEventField = 42;

const EVENT_FLAG_MASK_SHIFT: CGEventFlags = 1 << 17;
const EVENT_FLAG_MASK_CONTROL: CGEventFlags = 1 << 18;
const EVENT_FLAG_MASK_ALTERNATE: CGEventFlags = 1 << 19;
const EVENT_FLAG_MASK_COMMAND: CGEventFlags = 1 << 20;
const HYPER_FLAGS: CGEventFlags = EVENT_FLAG_MASK_CONTROL
    | EVENT_FLAG_MASK_ALTERNATE
    | EVENT_FLAG_MASK_SHIFT
    | EVENT_FLAG_MASK_COMMAND;

const SYNTHETIC_EVENT_MARKER: i64 = 0x4859_5045_524d_4b59;

const EVENT_TAP_LOCATION_HID: CGEventTapLocation = 0;
const EVENT_TAP_HEAD_INSERT: CGEventTapPlacement = 0;
const EVENT_TAP_OPTION_DEFAULT: CGEventTapOptions = 0;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT32_TYPE: CFNumberType = 3;

const HID_DEVICE_USAGE_PAGE_KEY: &CStr = c"DeviceUsagePage";
const HID_DEVICE_USAGE_KEY: &CStr = c"DeviceUsage";
const HID_USAGE_PAGE_GENERIC_DESKTOP: i32 = 0x01;
const HID_USAGE_KEYBOARD: i32 = 0x06;

const IORETURN_SUCCESS: IOReturn = 0;

const MODIFIERS: [Modifier; 4] = [
    Modifier {
        keycode: KEY_LEFT_CONTROL,
        flag: EVENT_FLAG_MASK_CONTROL,
    },
    Modifier {
        keycode: KEY_LEFT_OPTION,
        flag: EVENT_FLAG_MASK_ALTERNATE,
    },
    Modifier {
        keycode: KEY_LEFT_SHIFT,
        flag: EVENT_FLAG_MASK_SHIFT,
    },
    Modifier {
        keycode: KEY_LEFT_COMMAND,
        flag: EVENT_FLAG_MASK_COMMAND,
    },
];

#[derive(Debug)]
pub enum MacosError {
    RunLoopUnavailable,
    EventTapCreateFailed,
    EventTapSourceCreateFailed,
    HidManagerCreateFailed,
    HidMatchingCreateFailed,
    HidManagerOpenFailed(IOReturn),
    SignalInstall(std::io::Error),
    SignalThread(std::io::Error),
}

impl fmt::Display for MacosError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunLoopUnavailable => write!(formatter, "failed to get the current CFRunLoop"),
            Self::EventTapCreateFailed => write!(
                formatter,
                "failed to create a keyboard event tap. Grant Accessibility permission to hypermakey or the launching terminal, then restart it"
            ),
            Self::EventTapSourceCreateFailed => {
                write!(formatter, "failed to attach the event tap to the run loop")
            }
            Self::HidManagerCreateFailed => write!(formatter, "failed to create IOHIDManager"),
            Self::HidMatchingCreateFailed => {
                write!(formatter, "failed to create IOHID keyboard matching dictionary")
            }
            Self::HidManagerOpenFailed(code) => {
                write!(formatter, "failed to open IOHIDManager: IOReturn {code}")
            }
            Self::SignalInstall(error) => write!(formatter, "failed to install signal handlers: {error}"),
            Self::SignalThread(error) => write!(formatter, "failed to start signal thread: {error}"),
        }
    }
}

impl StdError for MacosError {}

pub fn run() -> Result<(), MacosError> {
    let run_loop = unsafe { cf_run_loop_get_current() };
    if run_loop.is_null() {
        return Err(MacosError::RunLoopUnavailable);
    }

    let mut context = Box::new(AppContext::new());
    let context_ptr = context.as_mut() as *mut AppContext as *mut c_void;

    let event_tap = EventTap::create(run_loop, context_ptr)?;
    context.set_event_tap(event_tap.port());

    let hid_manager = HidManager::create(run_loop, context_ptr)?;
    install_signal_handlers(run_loop)?;

    log_message(format_args!(
        "hypermakey {} started. Hold Caps Lock for Hyper (Control+Option+Shift+Command).",
        env!("CARGO_PKG_VERSION")
    ));

    unsafe {
        cf_run_loop_run();
    }

    context.release_hyper_if_active();
    log_message(format_args!("hypermakey stopped."));

    drop(hid_manager);
    context.clear_event_tap();
    drop(event_tap);
    drop(context);

    Ok(())
}

fn log_message(arguments: fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_fmt(arguments);
    let _ = stderr.write_all(b"\n");
}

fn install_signal_handlers(run_loop: CFRunLoopRef) -> Result<(), MacosError> {
    let mut signals = Signals::new([SIGINT, SIGTERM]).map_err(MacosError::SignalInstall)?;
    let run_loop_address = run_loop as usize;

    thread::Builder::new()
        .name("hypermakey-signal".to_owned())
        .spawn(move || {
            if let Some(signal) = signals.forever().next() {
                log_message(format_args!(
                    "hypermakey: received signal {signal}, stopping."
                ));
                let run_loop = run_loop_address as CFRunLoopRef;
                unsafe {
                    cf_run_loop_stop(run_loop);
                    cf_run_loop_wake_up(run_loop);
                }
            }
        })
        .map(|_handle| ())
        .map_err(MacosError::SignalThread)
}

struct AppContext {
    state: Mutex<HyperState>,
    hyper_active: AtomicBool,
    event_tap: AtomicPtr<c_void>,
}

impl AppContext {
    fn new() -> Self {
        Self {
            state: Mutex::new(HyperState::new()),
            hyper_active: AtomicBool::new(false),
            event_tap: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn set_event_tap(&self, event_tap: CFMachPortRef) {
        self.event_tap.store(event_tap, Ordering::Release);
    }

    fn clear_event_tap(&self) {
        self.event_tap.store(ptr::null_mut(), Ordering::Release);
    }

    fn handle_caps_value(&self, pressed: bool) {
        self.perform_action(|state| state.handle_caps_value(pressed));
    }

    fn release_hyper_if_active(&self) {
        self.perform_action(HyperState::reset);
    }

    fn is_hyper_active(&self) -> bool {
        self.hyper_active.load(Ordering::Acquire)
    }

    fn recover_disabled_tap(&self, reason: &str) {
        self.release_hyper_if_active();

        let event_tap = self.event_tap.load(Ordering::Acquire);
        if event_tap.is_null() {
            log_message(format_args!(
                "hypermakey: event tap was disabled ({reason}) and cannot be re-enabled."
            ));
            return;
        }

        unsafe {
            cg_event_tap_enable(event_tap, true);
        }
        log_message(format_args!(
            "hypermakey: event tap was disabled ({reason}); released modifiers and re-enabled it."
        ));
    }

    fn perform_action(&self, action: impl FnOnce(&mut HyperState) -> HyperAction) {
        let hyper_action = match self.state.lock() {
            Ok(mut state) => action(&mut state),
            Err(error) => recover_poisoned_state(error, action),
        };

        match hyper_action {
            HyperAction::None => {}
            HyperAction::Press => {
                self.hyper_active.store(true, Ordering::Release);
                post_hyper_down();
            }
            HyperAction::Release => {
                self.hyper_active.store(false, Ordering::Release);
                post_hyper_up();
            }
        }
    }
}

fn recover_poisoned_state(
    error: PoisonError<std::sync::MutexGuard<'_, HyperState>>,
    action: impl FnOnce(&mut HyperState) -> HyperAction,
) -> HyperAction {
    log_message(format_args!(
        "hypermakey: internal state lock was poisoned; attempting to recover."
    ));
    let mut state = error.into_inner();
    action(&mut state)
}

struct EventTap {
    run_loop: CFRunLoopRef,
    port: CFMachPortRef,
    source: CFRunLoopSourceRef,
}

impl EventTap {
    fn create(run_loop: CFRunLoopRef, context: *mut c_void) -> Result<Self, MacosError> {
        let event_mask =
            event_mask(EVENT_KEY_DOWN) | event_mask(EVENT_KEY_UP) | event_mask(EVENT_FLAGS_CHANGED);

        let port = unsafe {
            cg_event_tap_create(
                EVENT_TAP_LOCATION_HID,
                EVENT_TAP_HEAD_INSERT,
                EVENT_TAP_OPTION_DEFAULT,
                event_mask,
                Some(event_tap_callback),
                context,
            )
        };

        if port.is_null() {
            return Err(MacosError::EventTapCreateFailed);
        }

        let source = unsafe { cf_mach_port_create_run_loop_source(ptr::null(), port, 0) };
        if source.is_null() {
            unsafe {
                cf_release(port as CFTypeRef);
            }
            return Err(MacosError::EventTapSourceCreateFailed);
        }

        unsafe {
            cf_run_loop_add_source(run_loop, source, K_CF_RUN_LOOP_COMMON_MODES);
            cg_event_tap_enable(port, true);
        }

        Ok(Self {
            run_loop,
            port,
            source,
        })
    }

    fn port(&self) -> CFMachPortRef {
        self.port
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        unsafe {
            cg_event_tap_enable(self.port, false);
            cf_run_loop_remove_source(self.run_loop, self.source, K_CF_RUN_LOOP_COMMON_MODES);
            cf_release(self.source as CFTypeRef);
            cf_release(self.port as CFTypeRef);
        }
    }
}

struct HidManager {
    run_loop: CFRunLoopRef,
    manager: IOHIDManagerRef,
}

impl HidManager {
    fn create(run_loop: CFRunLoopRef, context: *mut c_void) -> Result<Self, MacosError> {
        let manager = unsafe { io_hid_manager_create(ptr::null(), 0) };
        if manager.is_null() {
            return Err(MacosError::HidManagerCreateFailed);
        }

        let matching = create_keyboard_matching_dictionary().ok_or_else(|| {
            unsafe {
                cf_release(manager as CFTypeRef);
            }
            MacosError::HidMatchingCreateFailed
        })?;

        unsafe {
            io_hid_manager_set_device_matching(manager, matching);
            cf_release(matching as CFTypeRef);
            io_hid_manager_register_input_value_callback(
                manager,
                Some(hid_input_value_callback),
                context,
            );
            io_hid_manager_schedule_with_run_loop(manager, run_loop, K_CF_RUN_LOOP_COMMON_MODES);
        }

        let result = unsafe { io_hid_manager_open(manager, 0) };
        if result != IORETURN_SUCCESS {
            unsafe {
                io_hid_manager_unschedule_from_run_loop(
                    manager,
                    run_loop,
                    K_CF_RUN_LOOP_COMMON_MODES,
                );
                cf_release(manager as CFTypeRef);
            }
            return Err(MacosError::HidManagerOpenFailed(result));
        }

        Ok(Self { run_loop, manager })
    }
}

impl Drop for HidManager {
    fn drop(&mut self) {
        unsafe {
            io_hid_manager_unschedule_from_run_loop(
                self.manager,
                self.run_loop,
                K_CF_RUN_LOOP_COMMON_MODES,
            );
            io_hid_manager_close(self.manager, 0);
            cf_release(self.manager as CFTypeRef);
        }
    }
}

fn event_mask(event_type: CGEventType) -> CGEventMask {
    1_u64 << event_type
}

extern "C" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: CGEventRef,
    context: *mut c_void,
) -> CGEventRef {
    match panic::catch_unwind(AssertUnwindSafe(|| {
        event_tap_callback_inner(event_type, event, context)
    })) {
        Ok(event) => event,
        Err(_) => {
            log_message(format_args!(
                "hypermakey: recovered from an internal panic in the event tap callback."
            ));
            event
        }
    }
}

fn event_tap_callback_inner(
    event_type: CGEventType,
    event: CGEventRef,
    context: *mut c_void,
) -> CGEventRef {
    if context.is_null() {
        return event;
    }

    let app = unsafe { &*(context as *const AppContext) };

    match event_type {
        EVENT_TAP_DISABLED_BY_TIMEOUT => {
            app.recover_disabled_tap("timeout");
            return ptr::null_mut();
        }
        EVENT_TAP_DISABLED_BY_USER_INPUT => {
            app.recover_disabled_tap("user input");
            return ptr::null_mut();
        }
        _ => {}
    }

    if event.is_null() || is_synthetic_event(event) {
        return event;
    }

    if matches!(
        event_type,
        EVENT_KEY_DOWN | EVENT_KEY_UP | EVENT_FLAGS_CHANGED
    ) && event_keycode(event) == KEY_CAPS_LOCK
    {
        return ptr::null_mut();
    }

    if matches!(
        event_type,
        EVENT_KEY_DOWN | EVENT_KEY_UP | EVENT_FLAGS_CHANGED
    ) && app.is_hyper_active()
    {
        add_hyper_flags(event);
    }

    event
}

extern "C" fn hid_input_value_callback(
    context: *mut c_void,
    result: IOReturn,
    _sender: *mut c_void,
    value: IOHIDValueRef,
) {
    if panic::catch_unwind(AssertUnwindSafe(|| {
        hid_input_value_callback_inner(context, result, value);
    }))
    .is_err()
    {
        log_message(format_args!(
            "hypermakey: recovered from an internal panic in the HID callback."
        ));
    }
}

fn hid_input_value_callback_inner(context: *mut c_void, result: IOReturn, value: IOHIDValueRef) {
    if result != IORETURN_SUCCESS || value.is_null() {
        return;
    }

    if context.is_null() {
        return;
    }

    let element = unsafe { io_hid_value_get_element(value) };
    if element.is_null() {
        return;
    }

    let usage_page = unsafe { io_hid_element_get_usage_page(element) };
    let usage = unsafe { io_hid_element_get_usage(element) };

    if usage_page == CAPS_LOCK_HID_USAGE_PAGE && usage == CAPS_LOCK_HID_USAGE {
        let pressed = unsafe { io_hid_value_get_integer_value(value) } != 0;
        let app = unsafe { &*(context as *const AppContext) };
        app.handle_caps_value(pressed);
    }
}

fn is_synthetic_event(event: CGEventRef) -> bool {
    (unsafe { cg_event_get_integer_value_field(event, EVENT_FIELD_EVENT_SOURCE_USER_DATA) })
        == SYNTHETIC_EVENT_MARKER
}

fn event_keycode(event: CGEventRef) -> u16 {
    unsafe { cg_event_get_integer_value_field(event, EVENT_FIELD_KEYBOARD_EVENT_KEYCODE) as u16 }
}

fn add_hyper_flags(event: CGEventRef) {
    let flags = unsafe { cg_event_get_flags(event) };
    unsafe {
        cg_event_set_flags(event, flags | HYPER_FLAGS);
    }
}

fn post_hyper_down() {
    let mut flags = 0;

    for modifier in MODIFIERS {
        flags |= modifier.flag;
        post_modifier_event(modifier.keycode, true, flags);
    }
}

fn post_hyper_up() {
    let mut flags = HYPER_FLAGS;

    for modifier in MODIFIERS.into_iter().rev() {
        flags &= !modifier.flag;
        post_modifier_event(modifier.keycode, false, flags);
    }
}

fn post_modifier_event(keycode: u16, key_down: bool, flags: CGEventFlags) {
    let event = unsafe { cg_event_create_keyboard_event(ptr::null_mut(), keycode, key_down) };
    if event.is_null() {
        log_message(format_args!(
            "hypermakey: failed to create synthetic modifier event for keycode {keycode}."
        ));
        return;
    }

    unsafe {
        cg_event_set_flags(event, flags);
        cg_event_set_integer_value_field(
            event,
            EVENT_FIELD_EVENT_SOURCE_USER_DATA,
            SYNTHETIC_EVENT_MARKER,
        );
        cg_event_post(EVENT_TAP_LOCATION_HID, event);
        cf_release(event as CFTypeRef);
    }
}

fn create_keyboard_matching_dictionary() -> Option<CFMutableDictionaryRef> {
    let dictionary = unsafe {
        cf_dictionary_create_mutable(
            ptr::null(),
            0,
            &K_CF_TYPE_DICTIONARY_KEY_CALLBACKS,
            &K_CF_TYPE_DICTIONARY_VALUE_CALLBACKS,
        )
    };
    if dictionary.is_null() {
        return None;
    }

    let usage_page_key = create_cf_string(HID_DEVICE_USAGE_PAGE_KEY)?;
    let usage_key = create_cf_string(HID_DEVICE_USAGE_KEY)?;
    let usage_page = create_cf_i32(HID_USAGE_PAGE_GENERIC_DESKTOP)?;
    let usage = create_cf_i32(HID_USAGE_KEYBOARD)?;

    unsafe {
        cf_dictionary_set_value(
            dictionary,
            usage_page_key as CFTypeRef,
            usage_page as CFTypeRef,
        );
        cf_dictionary_set_value(dictionary, usage_key as CFTypeRef, usage as CFTypeRef);
        cf_release(usage_page_key as CFTypeRef);
        cf_release(usage_key as CFTypeRef);
        cf_release(usage_page as CFTypeRef);
        cf_release(usage as CFTypeRef);
    }

    Some(dictionary)
}

fn create_cf_string(value: &CStr) -> Option<CFStringRef> {
    let string = unsafe {
        cf_string_create_with_c_string(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8)
    };

    if string.is_null() {
        None
    } else {
        Some(string)
    }
}

fn create_cf_i32(value: i32) -> Option<CFNumberRef> {
    let number = unsafe {
        cf_number_create(
            ptr::null(),
            CF_NUMBER_SINT32_TYPE,
            &value as *const i32 as *const c_void,
        )
    };

    if number.is_null() {
        None
    } else {
        Some(number)
    }
}

#[derive(Clone, Copy)]
struct Modifier {
    keycode: u16,
    flag: CGEventFlags,
}

type Boolean = bool;
type CFIndex = isize;
type CFAllocatorRef = *const c_void;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFDictionaryRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CGEventType = u32;
type CGEventMask = u64;
type CGEventField = u32;
type CGEventFlags = u64;
type CGEventTapLocation = u32;
type CGEventTapPlacement = u32;
type CGEventTapOptions = u32;
type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, CGEventType, CGEventRef, *mut c_void) -> CGEventRef;
type CGKeyCode = u16;
type CFNumberType = c_int;
type IOReturn = i32;
type IOOptionBits = u32;
type IOHIDManagerRef = *mut c_void;
type IOHIDValueRef = *mut c_void;
type IOHIDElementRef = *mut c_void;
type IOHIDValueCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, IOHIDValueRef);

#[repr(C)]
struct CFDictionaryKeyCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
}

#[repr(C)]
struct CFDictionaryValueCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    #[link_name = "kCFRunLoopCommonModes"]
    static K_CF_RUN_LOOP_COMMON_MODES: CFStringRef;

    #[link_name = "kCFTypeDictionaryKeyCallBacks"]
    static K_CF_TYPE_DICTIONARY_KEY_CALLBACKS: CFDictionaryKeyCallBacks;

    #[link_name = "kCFTypeDictionaryValueCallBacks"]
    static K_CF_TYPE_DICTIONARY_VALUE_CALLBACKS: CFDictionaryValueCallBacks;

    #[link_name = "CFRelease"]
    fn cf_release(cf: CFTypeRef);

    #[link_name = "CFRunLoopGetCurrent"]
    fn cf_run_loop_get_current() -> CFRunLoopRef;

    #[link_name = "CFRunLoopRun"]
    fn cf_run_loop_run();

    #[link_name = "CFRunLoopStop"]
    fn cf_run_loop_stop(run_loop: CFRunLoopRef);

    #[link_name = "CFRunLoopWakeUp"]
    fn cf_run_loop_wake_up(run_loop: CFRunLoopRef);

    #[link_name = "CFRunLoopAddSource"]
    fn cf_run_loop_add_source(
        run_loop: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: CFStringRef,
    );

    #[link_name = "CFRunLoopRemoveSource"]
    fn cf_run_loop_remove_source(
        run_loop: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: CFStringRef,
    );

    #[link_name = "CFMachPortCreateRunLoopSource"]
    fn cf_mach_port_create_run_loop_source(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;

    #[link_name = "CFStringCreateWithCString"]
    fn cf_string_create_with_c_string(
        allocator: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;

    #[link_name = "CFNumberCreate"]
    fn cf_number_create(
        allocator: CFAllocatorRef,
        number_type: CFNumberType,
        value_ptr: *const c_void,
    ) -> CFNumberRef;

    #[link_name = "CFDictionaryCreateMutable"]
    fn cf_dictionary_create_mutable(
        allocator: CFAllocatorRef,
        capacity: CFIndex,
        key_callbacks: *const CFDictionaryKeyCallBacks,
        value_callbacks: *const CFDictionaryValueCallBacks,
    ) -> CFMutableDictionaryRef;

    #[link_name = "CFDictionarySetValue"]
    fn cf_dictionary_set_value(
        dictionary: CFMutableDictionaryRef,
        key: CFTypeRef,
        value: CFTypeRef,
    );
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    #[link_name = "CGEventTapCreate"]
    fn cg_event_tap_create(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        events_of_interest: CGEventMask,
        callback: Option<CGEventTapCallBack>,
        user_info: *mut c_void,
    ) -> CFMachPortRef;

    #[link_name = "CGEventTapEnable"]
    fn cg_event_tap_enable(tap: CFMachPortRef, enable: Boolean);

    #[link_name = "CGEventGetIntegerValueField"]
    fn cg_event_get_integer_value_field(event: CGEventRef, field: CGEventField) -> i64;

    #[link_name = "CGEventSetIntegerValueField"]
    fn cg_event_set_integer_value_field(event: CGEventRef, field: CGEventField, value: i64);

    #[link_name = "CGEventGetFlags"]
    fn cg_event_get_flags(event: CGEventRef) -> CGEventFlags;

    #[link_name = "CGEventCreateKeyboardEvent"]
    fn cg_event_create_keyboard_event(
        source: CGEventSourceRef,
        virtual_key: CGKeyCode,
        key_down: Boolean,
    ) -> CGEventRef;

    #[link_name = "CGEventSetFlags"]
    fn cg_event_set_flags(event: CGEventRef, flags: CGEventFlags);

    #[link_name = "CGEventPost"]
    fn cg_event_post(tap: CGEventTapLocation, event: CGEventRef);
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    #[link_name = "IOHIDManagerCreate"]
    fn io_hid_manager_create(allocator: CFAllocatorRef, options: IOOptionBits) -> IOHIDManagerRef;

    #[link_name = "IOHIDManagerSetDeviceMatching"]
    fn io_hid_manager_set_device_matching(manager: IOHIDManagerRef, matching: CFDictionaryRef);

    #[link_name = "IOHIDManagerRegisterInputValueCallback"]
    fn io_hid_manager_register_input_value_callback(
        manager: IOHIDManagerRef,
        callback: Option<IOHIDValueCallback>,
        context: *mut c_void,
    );

    #[link_name = "IOHIDManagerScheduleWithRunLoop"]
    fn io_hid_manager_schedule_with_run_loop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );

    #[link_name = "IOHIDManagerUnscheduleFromRunLoop"]
    fn io_hid_manager_unschedule_from_run_loop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );

    #[link_name = "IOHIDManagerOpen"]
    fn io_hid_manager_open(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;

    #[link_name = "IOHIDManagerClose"]
    fn io_hid_manager_close(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;

    #[link_name = "IOHIDValueGetElement"]
    fn io_hid_value_get_element(value: IOHIDValueRef) -> IOHIDElementRef;

    #[link_name = "IOHIDElementGetUsagePage"]
    fn io_hid_element_get_usage_page(element: IOHIDElementRef) -> u32;

    #[link_name = "IOHIDElementGetUsage"]
    fn io_hid_element_get_usage(element: IOHIDElementRef) -> u32;

    #[link_name = "IOHIDValueGetIntegerValue"]
    fn io_hid_value_get_integer_value(value: IOHIDValueRef) -> CFIndex;
}
