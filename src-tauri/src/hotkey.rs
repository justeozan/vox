//! Dictation key through a CGEventTap — what makes Fn (and right-hand
//! modifiers) usable as a push-to-talk key, which no Carbon/global-shortcut
//! API can do: they only see key *combinations*, never a lone modifier, and
//! never Fn at all.
//!
//! The tap is a session-level, head-insert, *active* tap (it may swallow
//! events). It swallows exactly one thing: Esc while a dictation is recording.
//! Everything else passes through untouched; a key pressed while the dictation
//! key is held means it was a chord (Fn+←, ⌘+Tab with right ⌘…) and the
//! dictation is dropped silently.
//!
//! Needs Accessibility. Without it the tap can't be created, so the thread
//! retries until the permission shows up — no restart needed.
//!
//! `alt_shift_space` is a key *combination*: it stays on the global-shortcut
//! plugin (lib.rs); the tap still provides Esc for it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

/// Marks events Vox posts itself (the ⌘V of a paste) so the tap ignores them.
pub const SELF_TAG: i64 = 0x564F_5844_4943_54; // "VOXDICT"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Key {
    Fn = 0,
    RightOption = 1,
    RightCommand = 2,
    RightShift = 3,
    /// Handled by the global-shortcut plugin, not the tap.
    AltShiftSpace = 4,
}

pub const KEYS: &[&str] = &["fn", "right_option", "right_command", "right_shift", "alt_shift_space"];

impl Key {
    pub fn parse(s: &str) -> Key {
        match s {
            "right_option" => Key::RightOption,
            "right_command" => Key::RightCommand,
            "right_shift" => Key::RightShift,
            "alt_shift_space" => Key::AltShiftSpace,
            _ => Key::Fn,
        }
    }
    fn from_u8(v: u8) -> Key {
        match v {
            1 => Key::RightOption,
            2 => Key::RightCommand,
            3 => Key::RightShift,
            4 => Key::AltShiftSpace,
            _ => Key::Fn,
        }
    }
    /// (virtual keycode, flag bit that is set while it is down)
    fn modifier(self) -> Option<(i64, u64)> {
        match self {
            Key::Fn => Some((63, 0x0080_0000)),         // kCGEventFlagMaskSecondaryFn
            Key::RightOption => Some((61, 0x0000_0040)), // NX_DEVICERALTKEYMASK
            Key::RightCommand => Some((54, 0x0000_0010)), // NX_DEVICERCMDKEYMASK
            Key::RightShift => Some((60, 0x0000_0004)),  // NX_DEVICERSHIFTKEYMASK
            Key::AltShiftSpace => None,
        }
    }
}

static KEY: AtomicU8 = AtomicU8::new(Key::Fn as u8);
static HELD: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn set_key(k: Key) {
    KEY.store(k as u8, Ordering::SeqCst);
    HELD.store(false, Ordering::SeqCst);
}

pub fn key() -> Key {
    Key::from_u8(KEY.load(Ordering::SeqCst))
}

// ── CoreGraphics / CoreFoundation FFI ────────────────────────────────────────

type CFTypeRef = *const c_void;
type CGEventRef = *mut c_void;
type TapCallback = extern "C" fn(proxy: *mut c_void, ty: u32, event: CGEventRef, user: *mut c_void) -> CGEventRef;

const KEY_DOWN: u32 = 10;
const FLAGS_CHANGED: u32 = 12;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER: u32 = 0xFFFF_FFFF;
const FIELD_KEYCODE: u32 = 9;
const FIELD_AUTOREPEAT: u32 = 8;
pub const FIELD_SOURCE_USER_DATA: u32 = 42;
const KEY_ESC: i64 = 53;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, cb: TapCallback, user: *mut c_void) -> CFTypeRef;
    fn CGEventTapEnable(tap: CFTypeRef, enable: bool);
    fn CGEventGetIntegerValueField(ev: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(ev: CGEventRef) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(alloc: CFTypeRef, port: CFTypeRef, order: isize) -> CFTypeRef;
    fn CFRunLoopGetCurrent() -> CFTypeRef;
    fn CFRunLoopAddSource(rl: CFTypeRef, src: CFTypeRef, mode: CFTypeRef);
    fn CFRunLoopRun();
    static kCFRunLoopCommonModes: CFTypeRef;
}

/// The tap's mach port, for re-enabling it from inside the callback.
static mut TAP: CFTypeRef = std::ptr::null();

extern "C" fn callback(_proxy: *mut c_void, ty: u32, event: CGEventRef, _user: *mut c_void) -> CGEventRef {
    if ty == TAP_DISABLED_BY_TIMEOUT || ty == TAP_DISABLED_BY_USER {
        // macOS turns a slow tap off; turn it back on. Any key state we
        // tracked may be stale now, so forget it.
        HELD.store(false, Ordering::SeqCst);
        unsafe {
            if !TAP.is_null() {
                CGEventTapEnable(TAP, true);
            }
        }
        return event;
    }
    unsafe {
        if CGEventGetIntegerValueField(event, FIELD_SOURCE_USER_DATA) == SELF_TAG {
            return event;
        }
        let keycode = CGEventGetIntegerValueField(event, FIELD_KEYCODE);
        let key = key();
        match ty {
            FLAGS_CHANGED => {
                if let Some((code, bit)) = key.modifier() {
                    if keycode == code {
                        let down = CGEventGetFlags(event) & bit != 0;
                        if down != HELD.swap(down, Ordering::SeqCst) {
                            crate::dictation::on_key(down);
                        }
                    }
                }
            }
            KEY_DOWN => {
                if keycode == KEY_ESC && crate::dictation::is_recording() {
                    crate::dictation::cancel();
                    return std::ptr::null_mut(); // swallowed
                }
                let repeat = CGEventGetIntegerValueField(event, FIELD_AUTOREPEAT) != 0;
                if !repeat && HELD.load(Ordering::SeqCst) {
                    crate::dictation::chord();
                }
            }
            _ => {}
        }
    }
    event
}

/// Install the tap on its own run-loop thread. Idempotent.
pub fn start() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("vox-hotkey".into())
        .spawn(|| {
            let mask: u64 = (1 << KEY_DOWN) | (1 << FLAGS_CHANGED);
            let mut warned = false;
            loop {
                // Session tap (1), head insert (0), default = active (0).
                let tap = unsafe { CGEventTapCreate(1, 0, 0, mask, callback, std::ptr::null_mut()) };
                if !tap.is_null() {
                    unsafe {
                        TAP = tap;
                        let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                        CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
                        CGEventTapEnable(tap, true);
                        println!("[vox] dictation key listener installed");
                        CFRunLoopRun();
                    }
                    eprintln!("[vox] dictation key listener stopped");
                    return;
                }
                if !warned {
                    eprintln!("[vox] dictation key needs Accessibility — waiting for it");
                    warned = true;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })
        .expect("hotkey thread");
}

/// What the 🌐/Fn key does on its own in macOS (Keyboard settings). Anything
/// but "do nothing" fires alongside Vox's Fn dictation.
///
/// `None` when macOS hasn't written the preference (common: it's only stored
/// once the user touches the popup). Unknown must not raise a warning — Fn
/// works fine on machines where the key is simply absent.
pub fn globe_does_nothing() -> Option<bool> {
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.HIToolbox", "AppleFnUsageType"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
}
