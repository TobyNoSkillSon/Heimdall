//! What the default output device is, read from CoreAudio without opening or changing anything. Playback logs it when
//! it changes (a reply that played into a device the user could not hear left no trace on 21 Oct), and the wake cue asks
//! whether it is Bluetooth: any sound started on Bluetooth headphones makes macOS claim them back from the user's iPhone
//! (22 Oct: the cue pulled his AirPods off a video 50 ms after each wake).

/// The default output device as playback finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputRoute {
    pub name: String,
    pub transport: Transport,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    BuiltIn,
    Usb,
    Bluetooth,
    Other,
    Unknown,
}
impl Transport {
    /// CoreAudio's `kAudioDevicePropertyTransportType` four-character code.
    pub fn from_code(code: u32) -> Self {
        match &code.to_be_bytes() {
            b"bltn" => Self::BuiltIn,
            b"usb " => Self::Usb,
            b"blue" | b"blea" => Self::Bluetooth,
            [0, 0, 0, 0] => Self::Unknown,
            _ => Self::Other,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Usb => "USB",
            Self::Bluetooth => "Bluetooth",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
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
    unsafe extern "C" {
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(s: *const c_void, buffer: *mut i8, size: isize, encoding: u32) -> u8;
        fn CFRelease(object: *const c_void);
    }
    const fn code(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SYSTEM: u32 = 1;
    const GLOBAL: u32 = code(b"glob");
    const DEFAULT_OUTPUT: u32 = code(b"dOut");
    const TRANSPORT: u32 = code(b"tran");
    const NAME: u32 = code(b"lnam");
    fn read<T: Copy + Default>(object: u32, selector: u32) -> Option<T> {
        let address = Address { selector, scope: GLOBAL, element: 0 };
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(object, &address, 0, std::ptr::null(), &mut size, (&mut value as *mut T).cast())
        };
        (status == 0).then_some(value)
    }
    fn name(device: u32) -> Option<String> {
        let address = Address { selector: NAME, scope: GLOBAL, element: 0 };
        let mut string: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(device, &address, 0, std::ptr::null(), &mut size, (&mut string as *mut *const c_void).cast())
        };
        if status != 0 || string.is_null() {
            return None;
        }
        let mut buffer = [0i8; 256];
        let ok = unsafe { CFStringGetCString(string, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100) };
        unsafe { CFRelease(string) };
        (ok != 0).then(|| unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned())
    }
    pub(super) fn default_output_id() -> Option<u32> {
        read(SYSTEM, DEFAULT_OUTPUT).filter(|d: &u32| *d != 0)
    }
    pub(super) fn default_output() -> Option<super::OutputRoute> {
        let device = default_output_id()?;
        Some(super::OutputRoute {
            name: name(device).unwrap_or_else(|| format!("device {device}")),
            transport: super::Transport::from_code(read::<u32>(device, TRANSPORT).unwrap_or(0)),
        })
    }
}

/// The current default output device, or `None` when there is none or CoreAudio cannot say. Read-only.
pub fn default_output() -> Option<OutputRoute> {
    #[cfg(target_os = "macos")]
    {
        hal::default_output()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
/// CoreAudio's ID of the current default output device, `None` when there is none or CoreAudio cannot say. One
/// property read, cheap enough for playback to poll while a reply plays: a new ID means the route moved (AirPods
/// connected or dropped, the user picked another output), and the reply follows it.
pub fn default_output_id() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        hal::default_output_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
/// The default output is Bluetooth headphones (or a speaker): a sound started there claims them for this Mac.
pub fn default_output_is_bluetooth() -> bool {
    default_output().is_some_and(|route| route.transport == Transport::Bluetooth)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_codes_map_to_kinds() {
        let code = |s: &[u8; 4]| u32::from_be_bytes(*s);
        assert_eq!(Transport::from_code(code(b"blue")), Transport::Bluetooth);
        assert_eq!(Transport::from_code(code(b"blea")), Transport::Bluetooth);
        assert_eq!(Transport::from_code(code(b"usb ")), Transport::Usb);
        assert_eq!(Transport::from_code(code(b"bltn")), Transport::BuiltIn);
        assert_eq!(Transport::from_code(code(b"virt")), Transport::Other);
        assert_eq!(Transport::from_code(0), Transport::Unknown);
    }
}
