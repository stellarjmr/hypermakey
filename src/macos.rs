use crate::logging::log_message;
use crate::state::{HyperAction, HyperState};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::error::Error as StdError;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::ptr::{self, NonNull};
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

// Device matching keys: kIOHIDDeviceUsagePageKey and kIOHIDDeviceUsageKey.
const HID_DEVICE_USAGE_PAGE_KEY: &CStr = c"DeviceUsagePage";
const HID_DEVICE_USAGE_KEY: &CStr = c"DeviceUsage";
// Input value (element) matching keys: kIOHIDElementUsagePageKey and kIOHIDElementUsageKey.
const HID_ELEMENT_USAGE_PAGE_KEY: &CStr = c"UsagePage";
const HID_ELEMENT_USAGE_KEY: &CStr = c"Usage";
const HID_USAGE_PAGE_GENERIC_DESKTOP: u32 = 0x01;
const HID_USAGE_KEYBOARD: u32 = 0x06;

const IORETURN_SUCCESS: IOReturn = 0;
// kIOReturnNotPrivileged and kIOReturnNotPermitted: iokit_common_err(0x2c1) and (0x2e2).
const IORETURN_NOT_PRIVILEGED: IOReturn = 0xE000_02C1_u32 as IOReturn;
const IORETURN_NOT_PERMITTED: IOReturn = 0xE000_02E2_u32 as IOReturn;

const QOS_CLASS_USER_INTERACTIVE: QosClass = 0x21;

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
    AccessibilityRevoked,
}

impl fmt::Display for MacosError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunLoopUnavailable => write!(formatter, "failed to get the current CFRunLoop"),
            Self::AccessibilityRevoked => {
                write!(
                    formatter,
                    "Accessibility permission was revoked while hypermakey was running. "
                )?;
                write_permission_hint(formatter, "Accessibility")
            }
            Self::EventTapCreateFailed => {
                write!(formatter, "failed to create a keyboard event tap. ")?;
                write_permission_hint(formatter, "Accessibility")
            }
            Self::EventTapSourceCreateFailed => {
                write!(formatter, "failed to attach the event tap to the run loop")
            }
            Self::HidManagerCreateFailed => write!(formatter, "failed to create IOHIDManager"),
            Self::HidMatchingCreateFailed => {
                write!(formatter, "failed to create IOHID matching dictionaries")
            }
            Self::HidManagerOpenFailed(
                code @ (IORETURN_NOT_PERMITTED | IORETURN_NOT_PRIVILEGED),
            ) => {
                write!(
                    formatter,
                    "not permitted to read keyboard input (IOReturn {:#010x}). ",
                    *code as u32
                )?;
                write_permission_hint(formatter, "Input Monitoring")
            }
            Self::HidManagerOpenFailed(code) => {
                write!(
                    formatter,
                    "failed to open IOHIDManager: IOReturn {:#010x}",
                    *code as u32
                )
            }
            Self::SignalInstall(error) => {
                write!(formatter, "failed to install signal handlers: {error}")
            }
            Self::SignalThread(error) => {
                write!(formatter, "failed to start signal thread: {error}")
            }
        }
    }
}

impl StdError for MacosError {}

/// Names the permission and the binary to grant it to, and warns against granting it to a
/// terminal, which would extend it to every program started from that terminal.
fn write_permission_hint(formatter: &mut fmt::Formatter<'_>, permission: &str) -> fmt::Result {
    let executable = std::env::current_exe().map_or_else(
        |_| "hypermakey".to_owned(),
        |path| path.display().to_string(),
    );

    write!(
        formatter,
        "Grant {permission} permission to {executable} in System Settings → Privacy & Security → \
         {permission}, then restart hypermakey. Run it as a service (brew services) rather than \
         from a terminal: started from a terminal, macOS checks the terminal app instead, and \
         granting the terminal extends the permission to every program run from it."
    )
}

pub fn run() -> Result<(), MacosError> {
    raise_thread_qos();

    // SAFETY: CFRunLoopGetCurrent has no preconditions. The returned run loop belongs to this
    // thread and lives until the process exits.
    let run_loop = unsafe { cf_run_loop_get_current() };
    if run_loop.is_null() {
        return Err(MacosError::RunLoopUnavailable);
    }

    let context = Box::new(AppContext::new());
    // Both callbacks only read through this pointer (AppContext uses interior mutability), and
    // `context` outlives the event tap and the HID manager created below.
    let context_ptr = ptr::from_ref(&*context).cast_mut().cast::<c_void>();

    let event_tap = EventTap::create(run_loop, context_ptr)?;
    context.set_event_tap(event_tap.port());

    let hid_manager = HidManager::create(run_loop, context_ptr)?;
    install_signal_handlers(run_loop)?;

    log_message(format_args!(
        "hypermakey {} started. Hold Caps Lock for Hyper (Control+Option+Shift+Command).",
        env!("CARGO_PKG_VERSION")
    ));

    // SAFETY: runs this thread's run loop until the signal thread stops it.
    unsafe {
        cf_run_loop_run();
    }

    // Stop intercepting input before releasing Hyper: the release events are posted at the HID
    // level and would otherwise be routed through this tap, which nothing services any more.
    drop(hid_manager);
    context.clear_event_tap();
    drop(event_tap);
    context.release_hyper_if_active();
    log_message(format_args!("hypermakey stopped."));

    if context.is_permission_revoked() {
        // Exit with an error so that launchd starts hypermakey again, which then reports the
        // missing permission until it is granted.
        return Err(MacosError::AccessibilityRevoked);
    }
    Ok(())
}

/// Every keystroke in the system waits for this thread's event tap callback, so it should not
/// be scheduled like background work. launchd still caps the effective priority unless the job's
/// ProcessType is Interactive.
fn raise_thread_qos() {
    // SAFETY: only changes the scheduling class of the calling thread.
    let result = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if result != 0 {
        log_message(format_args!(
            "hypermakey: could not raise the run loop thread to user-interactive QoS (error {result})."
        ));
    }
}

/// Whether this process currently holds Accessibility permission.
fn is_accessibility_trusted() -> bool {
    // SAFETY: AXIsProcessTrusted has no preconditions and never prompts the user.
    unsafe { ax_is_process_trusted() != 0 }
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
                // SAFETY: CFRunLoopStop and CFRunLoopWakeUp may be called from any thread, and
                // the main thread's run loop exists until the process exits.
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
    permission_revoked: AtomicBool,
}

impl AppContext {
    fn new() -> Self {
        Self {
            state: Mutex::new(HyperState::new()),
            hyper_active: AtomicBool::new(false),
            event_tap: AtomicPtr::new(ptr::null_mut()),
            permission_revoked: AtomicBool::new(false),
        }
    }

    fn is_permission_revoked(&self) -> bool {
        self.permission_revoked.load(Ordering::Acquire)
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

        // After Accessibility is revoked, a re-enabled tap makes every keystroke in the system
        // wait for it to time out again. Stop instead of re-enabling it.
        if !is_accessibility_trusted() {
            self.permission_revoked.store(true, Ordering::Release);
            log_message(format_args!(
                "hypermakey: event tap was disabled ({reason}) and Accessibility permission was revoked; stopping."
            ));
            // SAFETY: tap callbacks run on the thread that owns this run loop.
            unsafe {
                cf_run_loop_stop(cf_run_loop_get_current());
            }
            return;
        }

        let event_tap = self.event_tap.load(Ordering::Acquire);
        if event_tap.is_null() {
            log_message(format_args!(
                "hypermakey: event tap was disabled ({reason}) and cannot be re-enabled."
            ));
            return;
        }

        // SAFETY: the pointer is the tap's port, which `run` clears before dropping the tap.
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

        // SAFETY: the callback has the CGEventTapCallBack signature, and `context` points to an
        // AppContext that outlives the tap (see `run`).
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

        // SAFETY: `port` is the valid mach port created above; a null allocator selects the
        // default allocator.
        let source = unsafe { cf_mach_port_create_run_loop_source(ptr::null(), port, 0) };
        if source.is_null() {
            // SAFETY: we own the only reference to `port`; invalidating it removes the tap.
            unsafe {
                cf_mach_port_invalidate(port);
                cf_release(port as CFTypeRef);
            }
            return Err(MacosError::EventTapSourceCreateFailed);
        }

        // SAFETY: `run_loop` is this thread's run loop, and `source` and `port` are valid.
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
        // SAFETY: `port` and `source` are the objects created in `create` and attached to
        // `run_loop`; each reference we own is released exactly once here. Invalidating the port
        // destroys its receive right, so the tap is gone even if CoreGraphics still holds a
        // reference to the CFMachPort.
        unsafe {
            cg_event_tap_enable(self.port, false);
            cf_run_loop_remove_source(self.run_loop, self.source, K_CF_RUN_LOOP_COMMON_MODES);
            cf_mach_port_invalidate(self.port);
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
        let device_matching = create_usage_matching(
            HID_DEVICE_USAGE_PAGE_KEY,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            HID_DEVICE_USAGE_KEY,
            HID_USAGE_KEYBOARD,
        )
        .ok_or(MacosError::HidMatchingCreateFailed)?;
        // Without input value matching, IOKit delivers every key of every keyboard to this
        // process, although only Caps Lock is needed.
        let value_matching = create_usage_matching(
            HID_ELEMENT_USAGE_PAGE_KEY,
            CAPS_LOCK_HID_USAGE_PAGE,
            HID_ELEMENT_USAGE_KEY,
            CAPS_LOCK_HID_USAGE,
        )
        .ok_or(MacosError::HidMatchingCreateFailed)?;

        // SAFETY: a null allocator selects the default allocator; 0 is kIOHIDOptionsTypeNone.
        let manager = unsafe { io_hid_manager_create(ptr::null(), 0) };
        if manager.is_null() {
            return Err(MacosError::HidManagerCreateFailed);
        }

        // SAFETY: `manager` is valid, and both dictionaries are valid CFDictionaries that the
        // manager keeps its own references to. The callback has the IOHIDValueCallback
        // signature, and `context` points to an AppContext that outlives the manager (see `run`).
        unsafe {
            io_hid_manager_set_device_matching(manager, device_matching.as_ptr());
            io_hid_manager_set_input_value_matching(manager, value_matching.as_ptr());
            io_hid_manager_register_input_value_callback(
                manager,
                Some(hid_input_value_callback),
                context,
            );
            io_hid_manager_schedule_with_run_loop(manager, run_loop, K_CF_RUN_LOOP_COMMON_MODES);
        }

        // SAFETY: `manager` is valid; 0 is kIOHIDOptionsTypeNone (no exclusive device access).
        let result = unsafe { io_hid_manager_open(manager, 0) };
        if result != IORETURN_SUCCESS {
            // SAFETY: undoes the scheduling above and releases our only reference to `manager`.
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
        // SAFETY: `manager` was scheduled on `run_loop` and opened in `create`; each step is
        // undone once here before our only reference is released.
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

    // SAFETY: `context` is the AppContext registered in `run`, which outlives the event tap,
    // and AppContext is only ever accessed through shared references.
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
    ) && event_keycode(event) == i64::from(KEY_CAPS_LOCK)
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

    // SAFETY: `value` is the non-null IOHIDValue passed to this callback.
    let element = unsafe { io_hid_value_get_element(value) };
    if element.is_null() {
        return;
    }

    // SAFETY: `element` is the non-null element of `value`.
    let (usage_page, usage) = unsafe {
        (
            io_hid_element_get_usage_page(element),
            io_hid_element_get_usage(element),
        )
    };

    // Input value matching already limits callbacks to Caps Lock; this check is a guard.
    if usage_page == CAPS_LOCK_HID_USAGE_PAGE && usage == CAPS_LOCK_HID_USAGE {
        // SAFETY: `value` is the non-null IOHIDValue passed to this callback.
        let pressed = unsafe { io_hid_value_get_integer_value(value) } != 0;
        // SAFETY: `context` is the AppContext registered in `run`, which outlives the HID
        // manager, and AppContext is only ever accessed through shared references.
        let app = unsafe { &*(context as *const AppContext) };
        app.handle_caps_value(pressed);
    }
}

fn is_synthetic_event(event: CGEventRef) -> bool {
    // SAFETY: `event` is the non-null CGEvent passed to the tap callback.
    let user_data =
        unsafe { cg_event_get_integer_value_field(event, EVENT_FIELD_EVENT_SOURCE_USER_DATA) };
    user_data == SYNTHETIC_EVENT_MARKER
}

/// Returns the keycode field unnarrowed, so that no out-of-range value can alias a real key.
fn event_keycode(event: CGEventRef) -> i64 {
    // SAFETY: `event` is the non-null CGEvent passed to the tap callback.
    unsafe { cg_event_get_integer_value_field(event, EVENT_FIELD_KEYBOARD_EVENT_KEYCODE) }
}

fn add_hyper_flags(event: CGEventRef) {
    // SAFETY: `event` is the non-null CGEvent passed to the tap callback, which may modify it.
    unsafe {
        let flags = cg_event_get_flags(event);
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
    // SAFETY: a null source selects the default event source; the result is a new event we own.
    let event = unsafe { cg_event_create_keyboard_event(ptr::null_mut(), keycode, key_down) };
    if event.is_null() {
        log_message(format_args!(
            "hypermakey: failed to create synthetic modifier event for keycode {keycode}."
        ));
        return;
    }

    // SAFETY: `event` is the valid event created above. CGEventPost does not take ownership, so
    // our reference is released after posting.
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

/// An owned (+1) reference to a CoreFoundation object, released on drop.
struct CfOwned(NonNull<c_void>);

impl CfOwned {
    /// Takes ownership of the reference returned by a CoreFoundation `Create` function.
    ///
    /// # Safety
    ///
    /// `object` must be null or a CoreFoundation object whose reference the caller owns.
    unsafe fn from_create(object: *const c_void) -> Option<Self> {
        NonNull::new(object.cast_mut()).map(Self)
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: `self` owns exactly one reference to a live CoreFoundation object.
        unsafe {
            cf_release(self.as_ptr());
        }
    }
}

/// Builds `{page_key: page, usage_key: usage}`, the shape of both IOHID device matching and
/// input value (element) matching dictionaries.
fn create_usage_matching(
    page_key: &CStr,
    page: u32,
    usage_key: &CStr,
    usage: u32,
) -> Option<CfOwned> {
    // SAFETY: the callbacks are CoreFoundation's own kCFType callbacks, and the result is a new
    // dictionary that we own.
    let dictionary = unsafe {
        CfOwned::from_create(cf_dictionary_create_mutable(
            ptr::null(),
            0,
            &K_CF_TYPE_DICTIONARY_KEY_CALLBACKS,
            &K_CF_TYPE_DICTIONARY_VALUE_CALLBACKS,
        ))
    }?;

    let entries = [
        (create_cf_string(page_key)?, create_cf_number(page)?),
        (create_cf_string(usage_key)?, create_cf_number(usage)?),
    ];
    for (key, value) in &entries {
        // SAFETY: all three objects are valid. The dictionary retains the key and the value
        // (kCFType callbacks), so our own references can be released afterwards.
        unsafe {
            cf_dictionary_set_value(dictionary.as_ptr(), key.as_ptr(), value.as_ptr());
        }
    }

    Some(dictionary)
}

fn create_cf_string(value: &CStr) -> Option<CfOwned> {
    // SAFETY: `value` is a NUL-terminated ASCII (hence UTF-8) string; the result is a new string
    // that we own.
    unsafe {
        CfOwned::from_create(cf_string_create_with_c_string(
            ptr::null(),
            value.as_ptr(),
            CF_STRING_ENCODING_UTF8,
        ))
    }
}

fn create_cf_number(value: u32) -> Option<CfOwned> {
    let value = i32::try_from(value).ok()?;

    // SAFETY: `value` is a live i32, matching kCFNumberSInt32Type; the result is a new number
    // that we own.
    unsafe {
        CfOwned::from_create(cf_number_create(
            ptr::null(),
            CF_NUMBER_SINT32_TYPE,
            ptr::from_ref(&value).cast(),
        ))
    }
}

#[derive(Clone, Copy)]
struct Modifier {
    keycode: u16,
    flag: CGEventFlags,
}

/// C99 `bool`, as taken by the CoreGraphics event functions.
type Boolean = bool;
/// MacTypes `Boolean` (`unsigned char`), as returned by CoreFoundation and HIServices.
type CFBoolean = u8;
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
type QosClass = u32;

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

    #[link_name = "CFMachPortInvalidate"]
    fn cf_mach_port_invalidate(port: CFMachPortRef);

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
    #[link_name = "AXIsProcessTrusted"]
    fn ax_is_process_trusted() -> CFBoolean;

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

    #[link_name = "IOHIDManagerSetInputValueMatching"]
    fn io_hid_manager_set_input_value_matching(manager: IOHIDManagerRef, matching: CFDictionaryRef);

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

// libSystem is linked by default.
extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: QosClass, relative_priority: c_int) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        #[link_name = "CFDictionaryGetCount"]
        fn cf_dictionary_get_count(dictionary: CFDictionaryRef) -> CFIndex;

        #[link_name = "CFDictionaryGetValue"]
        fn cf_dictionary_get_value(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;

        #[link_name = "CFNumberGetValue"]
        fn cf_number_get_value(
            number: CFNumberRef,
            number_type: CFNumberType,
            value_ptr: *mut c_void,
        ) -> CFBoolean;
    }

    fn entry_count(dictionary: &CfOwned) -> CFIndex {
        // SAFETY: `dictionary` is a valid CFDictionary.
        unsafe { cf_dictionary_get_count(dictionary.as_ptr()) }
    }

    fn entry_i32(dictionary: &CfOwned, key: &CStr) -> Option<i32> {
        let key = create_cf_string(key)?;
        // SAFETY: both objects are valid; the returned value is borrowed from the dictionary.
        let number = unsafe { cf_dictionary_get_value(dictionary.as_ptr(), key.as_ptr()) };
        if number.is_null() {
            return None;
        }

        let mut value = 0_i32;
        // SAFETY: `number` is a CFNumber owned by the dictionary, which is still alive, and
        // `value` is writable storage for kCFNumberSInt32Type.
        let converted = unsafe {
            cf_number_get_value(
                number,
                CF_NUMBER_SINT32_TYPE,
                ptr::from_mut(&mut value).cast(),
            )
        };
        (converted != 0).then_some(value)
    }

    #[test]
    fn caps_lock_value_matching_selects_only_the_caps_lock_element() {
        let matching = create_usage_matching(
            HID_ELEMENT_USAGE_PAGE_KEY,
            CAPS_LOCK_HID_USAGE_PAGE,
            HID_ELEMENT_USAGE_KEY,
            CAPS_LOCK_HID_USAGE,
        )
        .expect("value matching dictionary");

        assert_eq!(entry_count(&matching), 2);
        assert_eq!(entry_i32(&matching, c"UsagePage"), Some(0x07));
        assert_eq!(entry_i32(&matching, c"Usage"), Some(0x39));
    }

    #[test]
    fn keyboard_device_matching_uses_device_keys() {
        let matching = create_usage_matching(
            HID_DEVICE_USAGE_PAGE_KEY,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            HID_DEVICE_USAGE_KEY,
            HID_USAGE_KEYBOARD,
        )
        .expect("device matching dictionary");

        assert_eq!(entry_count(&matching), 2);
        assert_eq!(entry_i32(&matching, c"DeviceUsagePage"), Some(0x01));
        assert_eq!(entry_i32(&matching, c"DeviceUsage"), Some(0x06));
    }

    #[test]
    fn cf_numbers_reject_values_outside_sint32() {
        assert!(create_cf_number(u32::MAX).is_none());
    }

    #[test]
    fn permission_errors_name_the_permission_and_ask_for_a_restart() {
        let tap = MacosError::EventTapCreateFailed.to_string();
        assert!(tap.contains("Grant Accessibility permission to "));
        assert!(tap.contains("then restart hypermakey"));

        let hid = MacosError::HidManagerOpenFailed(IORETURN_NOT_PERMITTED).to_string();
        assert!(hid.contains("IOReturn 0xe00002e2"));
        assert!(hid.contains("Grant Input Monitoring permission to "));

        let revoked = MacosError::AccessibilityRevoked.to_string();
        assert!(revoked.starts_with("Accessibility permission was revoked while hypermakey"));
        assert!(revoked.contains("Grant Accessibility permission to "));
        assert!(revoked.contains("then restart hypermakey"));
    }

    #[test]
    fn other_hid_open_errors_show_the_raw_code() {
        assert_eq!(
            MacosError::HidManagerOpenFailed(-1).to_string(),
            "failed to open IOHIDManager: IOReturn 0xffffffff"
        );
    }
}
