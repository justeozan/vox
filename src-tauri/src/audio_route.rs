//! Keep Bluetooth headphones out of call mode.
//!
//! Opening a Bluetooth headset's *input* forces it from the high-quality A2DP
//! profile to the narrow-band HFP "call" profile: everything you hear turns
//! tinny and the sample rate drops to 16 kHz. Vox only needs a microphone, so
//! when the default input is a Bluetooth device we capture from the Mac's
//! built-in mic instead and leave the headset on A2DP for playback. This is the
//! same trick as the system dictation key: the apps' audio dips briefly while
//! the mic opens, nothing more.

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2_foundation::NSString;
    use std::ffi::c_void;

    #[repr(C)]
    struct Addr {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            id: u32,
            addr: *const Addr,
            qualifier_size: u32,
            qualifier: *const c_void,
            out_size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            id: u32,
            addr: *const Addr,
            qualifier_size: u32,
            qualifier: *const c_void,
            io_size: *mut u32,
            out: *mut c_void,
        ) -> i32;
    }

    const fn fourcc(b: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*b)
    }

    const SYSTEM_OBJECT: u32 = 1;
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
    const TRANSPORT_BUILTIN: u32 = fourcc(b"bltn");
    const TRANSPORT_BLUETOOTH: u32 = fourcc(b"blue");
    const TRANSPORT_BLUETOOTH_LE: u32 = fourcc(b"blea");

    fn addr(selector: &[u8; 4], scope: u32) -> Addr {
        Addr { selector: fourcc(selector), scope, element: 0 }
    }

    fn get_u32(id: u32, a: &Addr) -> Option<u32> {
        let mut v: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let rc = unsafe {
            AudioObjectGetPropertyData(id, a, 0, std::ptr::null(), &mut size, &mut v as *mut u32 as *mut c_void)
        };
        (rc == 0).then_some(v)
    }

    fn has_input_streams(id: u32) -> bool {
        let a = addr(b"stm#", SCOPE_INPUT);
        let mut size = 0u32;
        let rc = unsafe { AudioObjectGetPropertyDataSize(id, &a, 0, std::ptr::null(), &mut size) };
        rc == 0 && size > 0
    }

    fn device_name(id: u32) -> Option<String> {
        let a = addr(b"lnam", SCOPE_GLOBAL);
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut size = std::mem::size_of::<*mut c_void>() as u32;
        let rc = unsafe {
            AudioObjectGetPropertyData(id, &a, 0, std::ptr::null(), &mut size, &mut ptr as *mut _ as *mut c_void)
        };
        if rc != 0 || ptr.is_null() {
            return None;
        }
        // CFStringRef is toll-free bridged to NSString and returned retained (+1);
        // `Retained` releases it on drop.
        let s: Retained<NSString> = unsafe { Retained::from_raw(ptr as *mut NSString)? };
        Some(s.to_string())
    }

    fn all_devices() -> Vec<u32> {
        let a = addr(b"dev#", SCOPE_GLOBAL);
        let mut size = 0u32;
        let rc = unsafe { AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &a, 0, std::ptr::null(), &mut size) };
        if rc != 0 || size == 0 {
            return vec![];
        }
        let mut ids = vec![0u32; size as usize / std::mem::size_of::<u32>()];
        let rc = unsafe {
            AudioObjectGetPropertyData(SYSTEM_OBJECT, &a, 0, std::ptr::null(), &mut size, ids.as_mut_ptr() as *mut c_void)
        };
        if rc != 0 {
            return vec![];
        }
        ids.truncate(size as usize / std::mem::size_of::<u32>());
        ids
    }

    pub fn builtin_mic_if_default_is_bluetooth() -> Option<String> {
        let default_in = get_u32(SYSTEM_OBJECT, &addr(b"dIn ", SCOPE_GLOBAL))?;
        let t = get_u32(default_in, &addr(b"tran", SCOPE_GLOBAL))?;
        if t != TRANSPORT_BLUETOOTH && t != TRANSPORT_BLUETOOTH_LE {
            return None;
        }
        all_devices()
            .into_iter()
            .filter(|&id| has_input_streams(id))
            .find(|&id| get_u32(id, &addr(b"tran", SCOPE_GLOBAL)) == Some(TRANSPORT_BUILTIN))
            .and_then(device_name)
    }
}

/// Name of the built-in microphone, but only when the system default input is a
/// Bluetooth headset (the case where capturing would drop it into call mode).
/// `None` means "just use the default input".
#[tauri::command]
pub fn preferred_mic() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        imp::builtin_mic_if_default_is_bluetooth()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Smoke test against the real CoreAudio graph: must not crash, and any
    /// name returned must be non-empty.
    #[test]
    fn preferred_mic_does_not_crash() {
        let m = super::preferred_mic();
        println!("preferred_mic = {m:?}");
        assert!(m.map_or(true, |n| !n.is_empty()));
    }
}
