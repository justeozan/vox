//! Keep Bluetooth headphones out of call mode.
//!
//! Opening a Bluetooth headset's *input* forces it from the high-quality A2DP
//! profile to the narrow-band HFP "call" profile: everything you hear turns
//! tinny and the sample rate drops to 16 kHz. Vox only needs a microphone, so
//! when the default input is a Bluetooth device we capture from the Mac's
//! built-in mic instead and leave the headset on A2DP for playback. This is the
//! same trick as the system dictation key: the apps' audio dips briefly while
//! the mic opens, nothing more.
//!
//! We do it by pointing the system *default input* at the built-in mic for the
//! length of a listening session, then putting it back. (Picking the device from
//! the webview doesn't work: WebKit hides device labels until a mic is already
//! open, and opening the Bluetooth one is exactly what we're avoiding.)

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::sync::Mutex;

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
        /// Undocumented but long-standing SPI (macOS 10.13+; also used by Chromium,
        /// Firefox and WebKit): ramps down everyone else's output on `device`.
        /// The system lifts the duck by itself if this process dies.
        fn AudioDeviceDuck(
            device: u32,
            ducked_level: f32,
            start_time: *const c_void,
            ramp_secs: f32,
        ) -> i32;
        fn AudioObjectSetPropertyData(
            id: u32,
            addr: *const Addr,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: u32,
            data: *const c_void,
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

    #[cfg(test)]
    pub fn default_input_for_test() -> Option<u32> {
        default_input()
    }

    fn default_input() -> Option<u32> {
        get_u32(SYSTEM_OBJECT, &addr(b"dIn ", SCOPE_GLOBAL))
    }

    fn set_default_input(id: u32) -> bool {
        let a = addr(b"dIn ", SCOPE_GLOBAL);
        let rc = unsafe {
            AudioObjectSetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                std::ptr::null(),
                std::mem::size_of::<u32>() as u32,
                &id as *const u32 as *const c_void,
            )
        };
        rc == 0
    }

    fn is_bluetooth(id: u32) -> bool {
        matches!(
            get_u32(id, &addr(b"tran", SCOPE_GLOBAL)),
            Some(TRANSPORT_BLUETOOTH) | Some(TRANSPORT_BLUETOOTH_LE)
        )
    }

    fn builtin_mic() -> Option<u32> {
        all_devices()
            .into_iter()
            .filter(|&id| has_input_streams(id))
            .find(|&id| get_u32(id, &addr(b"tran", SCOPE_GLOBAL)) == Some(TRANSPORT_BUILTIN))
    }

    /// (original default input, built-in mic we switched to) while a switch is live.
    static SWITCHED: Mutex<Option<(u32, u32)>> = Mutex::new(None);

    pub fn route_away_from_bluetooth() -> bool {
        let mut g = SWITCHED.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            return true;
        }
        let Some(cur) = default_input() else { return false };
        if !is_bluetooth(cur) {
            return false;
        }
        let Some(builtin) = builtin_mic() else { return false };
        if !set_default_input(builtin) {
            return false;
        }
        *g = Some((cur, builtin));
        true
    }

    /// Device we ducked, so un-ducking targets the same one even if the default
    /// output changed meanwhile.
    static DUCKED: Mutex<Option<u32>> = Mutex::new(None);

    pub fn duck_others(on: bool) {
        let mut g = DUCKED.lock().unwrap_or_else(|e| e.into_inner());
        if on {
            if g.is_some() {
                return;
            }
            let Some(dev) = get_u32(SYSTEM_OBJECT, &addr(b"dOut", SCOPE_GLOBAL)) else { return };
            // Fully silent, like the system dictation; short ramp so it isn't abrupt.
            if unsafe { AudioDeviceDuck(dev, 0.0, std::ptr::null(), 0.25) } == 0 {
                *g = Some(dev);
            }
        } else if let Some(dev) = g.take() {
            unsafe { AudioDeviceDuck(dev, 1.0, std::ptr::null(), 0.5) };
        }
    }

    pub fn restore() {
        let mut g = SWITCHED.lock().unwrap_or_else(|e| e.into_inner());
        let Some((orig, builtin)) = g.take() else { return };
        // Respect a change made meanwhile (user picked another input) and a
        // headset that has since disconnected.
        if default_input() == Some(builtin) && has_input_streams(orig) {
            set_default_input(orig);
        }
    }
}

/// If the system default input is a Bluetooth headset, point the default input
/// at the built-in mic so opening the mic doesn't drop the headset into call
/// mode. Returns whether a switch is active. Pair with [`restore_mic_route`].
#[tauri::command]
pub fn route_mic_away_from_bluetooth() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::route_away_from_bluetooth()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Silence everything else the Mac is playing (music, videos…) while Vox listens,
/// then bring it back smoothly — what the system dictation does. Idempotent.
#[tauri::command]
pub fn duck_other_audio(on: bool) {
    #[cfg(target_os = "macos")]
    imp::duck_others(on);
    #[cfg(not(target_os = "macos"))]
    let _ = on;
}

/// Put the default input back where the user had it (no-op if nothing was switched).
#[tauri::command]
pub fn restore_mic_route() {
    #[cfg(target_os = "macos")]
    imp::restore();
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Smoke test against the real CoreAudio graph: must not crash, and a
    /// route/restore round-trip must leave the default input unchanged.
    #[test]
    fn route_and_restore_round_trip() {
        let before = super::imp::default_input_for_test();
        let switched = super::route_mic_away_from_bluetooth();
        println!("switched = {switched}");
        super::restore_mic_route();
        assert_eq!(before, super::imp::default_input_for_test());
    }

    /// Ducking is idempotent and un-ducking without a duck is a no-op.
    #[test]
    fn duck_round_trip() {
        super::duck_other_audio(false);
        super::duck_other_audio(true);
        super::duck_other_audio(true);
        std::thread::sleep(std::time::Duration::from_millis(600));
        super::duck_other_audio(false);
        super::duck_other_audio(false);
    }
}
