//! Where Heimdall's voice comes out, for the barge-in (night pass 1022, bug note 21 Oct 17:44:10). Through speakers
//! the microphone hears Heimdall's own voice, so a barge-in asks for more before it stops it (pipeline/barge_in.rs);
//! through headphones it stays as quick as before.
//!
//! Read from Core Audio's default output device each time a barge-in arms (a few property reads, no listener, no
//! state). The built-in output tells its jack from its speakers by data source or UID; Bluetooth output is taken as
//! headphones (AirPods); a USB device that also records (a headset, or the Shure with its headphone jack) is taken as
//! headphones; everything else (built-in speakers, displays, AirPlay, an interface without input) as speakers. When
//! the device cannot be read the answer is headphones: the barge-in behaves as it did before this check.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Speakers,
    Headphones,
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}
const TRANSPORT_BUILT_IN: u32 = fourcc(b"bltn");
const TRANSPORT_BLUETOOTH: u32 = fourcc(b"blue");
const TRANSPORT_BLUETOOTH_LE: u32 = fourcc(b"blea");
const TRANSPORT_USB: u32 = fourcc(b"usb ");
const SOURCE_HEADPHONES: u32 = fourcc(b"hdpn");

/// The route for a device's transport type, its output data source and UID (built-in devices: Apple Silicon shows
/// the jack as its own device, `BuiltInHeadphoneOutputDevice`) and whether it also records.
pub(crate) fn classify(transport: Option<u32>, data_source: Option<u32>, uid: Option<&str>, records: bool) -> Route {
    match transport {
        None => Route::Headphones,
        Some(TRANSPORT_BUILT_IN) => {
            if data_source == Some(SOURCE_HEADPHONES) || uid.is_some_and(|u| u.contains("Headphone")) { Route::Headphones } else { Route::Speakers }
        }
        Some(TRANSPORT_BLUETOOTH | TRANSPORT_BLUETOOTH_LE) => Route::Headphones,
        Some(TRANSPORT_USB) if records => Route::Headphones,
        Some(_) => Route::Speakers,
    }
}

#[cfg(target_os = "macos")]
mod hal {
    use std::ffi::c_void;
    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }
    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyData(object: u32, address: *const Address, qualifier_size: u32, qualifier: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
        fn AudioObjectGetPropertyDataSize(object: u32, address: *const Address, qualifier_size: u32, qualifier: *const c_void, size: *mut u32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetCString(string: *const c_void, buffer: *mut i8, size: isize, encoding: u32) -> bool;
        fn CFRelease(object: *const c_void);
    }
    const SYSTEM_OBJECT: u32 = 1;
    const GLOBAL: u32 = super::fourcc(b"glob");
    const OUTPUT: u32 = super::fourcc(b"outp");
    const INPUT: u32 = super::fourcc(b"inpt");
    const DEFAULT_OUTPUT_DEVICE: u32 = super::fourcc(b"dOut");
    const TRANSPORT_TYPE: u32 = super::fourcc(b"tran");
    const DATA_SOURCE: u32 = super::fourcc(b"ssrc");
    const STREAMS: u32 = super::fourcc(b"stm#");
    const DEVICE_UID: u32 = super::fourcc(b"uid ");
    const UTF8: u32 = 0x0800_0100;

    fn u32_property(object: u32, selector: u32, scope: u32) -> Option<u32> {
        let address = Address { selector, scope, element: 0 };
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: a plain u32 property read into a u32 of the stated size.
        let status = unsafe { AudioObjectGetPropertyData(object, &address, 0, std::ptr::null(), &mut size, (&mut value as *mut u32).cast()) };
        (status == 0 && size as usize == std::mem::size_of::<u32>()).then_some(value)
    }
    fn uid(device: u32) -> Option<String> {
        let address = Address { selector: DEVICE_UID, scope: GLOBAL, element: 0 };
        let mut string: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        // SAFETY: the UID property is a CFStringRef we own (copy rule) and release below.
        let status = unsafe { AudioObjectGetPropertyData(device, &address, 0, std::ptr::null(), &mut size, (&mut string as *mut *const c_void).cast()) };
        if status != 0 || string.is_null() {
            return None;
        }
        let mut buffer = [0u8; 256];
        // SAFETY: a valid CFString and a buffer of the stated size; released once.
        let ok = unsafe {
            let ok = CFStringGetCString(string, buffer.as_mut_ptr().cast(), buffer.len() as isize, UTF8);
            CFRelease(string);
            ok
        };
        if !ok {
            return None;
        }
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
    }
    fn has_input_streams(device: u32) -> bool {
        let address = Address { selector: STREAMS, scope: INPUT, element: 0 };
        let mut size = 0u32;
        // SAFETY: a size query, no data written.
        let status = unsafe { AudioObjectGetPropertyDataSize(device, &address, 0, std::ptr::null(), &mut size) };
        status == 0 && size > 0
    }
    pub(super) fn route() -> super::Route {
        let Some(device) = u32_property(SYSTEM_OBJECT, DEFAULT_OUTPUT_DEVICE, GLOBAL).filter(|d| *d != 0) else {
            return super::Route::Headphones;
        };
        let transport = u32_property(device, TRANSPORT_TYPE, GLOBAL);
        let data_source = u32_property(device, DATA_SOURCE, OUTPUT);
        super::classify(transport, data_source, uid(device).as_deref(), has_input_streams(device))
    }
}

/// The default output's route now.
pub(crate) fn current() -> Route {
    #[cfg(target_os = "macos")]
    return hal::route();
    #[cfg(not(target_os = "macos"))]
    Route::Headphones
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_by_transport_source_and_input() {
        assert_eq!(classify(Some(TRANSPORT_BUILT_IN), Some(fourcc(b"ispk")), Some("BuiltInSpeakerDevice"), false), Route::Speakers, "MacBook speakers");
        assert_eq!(classify(Some(TRANSPORT_BUILT_IN), None, None, false), Route::Speakers);
        assert_eq!(classify(Some(TRANSPORT_BUILT_IN), Some(SOURCE_HEADPHONES), None, false), Route::Headphones, "the jack, by data source");
        assert_eq!(classify(Some(TRANSPORT_BUILT_IN), None, Some("BuiltInHeadphoneOutputDevice"), false), Route::Headphones, "the jack, its own device");
        assert_eq!(classify(Some(TRANSPORT_BLUETOOTH), None, None, true), Route::Headphones, "AirPods");
        assert_eq!(classify(Some(TRANSPORT_BLUETOOTH_LE), None, None, false), Route::Headphones);
        assert_eq!(classify(Some(TRANSPORT_USB), None, None, true), Route::Headphones, "a USB mic with a headphone jack");
        assert_eq!(classify(Some(TRANSPORT_USB), None, None, false), Route::Speakers, "a USB output only");
        assert_eq!(classify(Some(fourcc(b"hdmi")), None, None, false), Route::Speakers, "a display");
        assert_eq!(classify(None, None, None, false), Route::Headphones, "unreadable: as before");
    }
    /// The live probe answers without panicking (read-only Core Audio reads; the answer depends on this Mac).
    #[test]
    fn the_live_probe_answers() {
        let _ = current();
    }
}
