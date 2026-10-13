//! Keep the computer awake while recording (decision 37). A 1-2 hour tutorial must not be cut short
//! by the display sleeping or the screen locking half-way -- that would capture a black or locked
//! screen, or stop the capture outright. Held for the life of the guard; dropping it releases.

pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    id: u32,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    #[cfg(target_os = "linux")]
    inhibit: Option<(zbus::blocking::Connection, u32)>,
}

// The handle/cookie is only used again in drop.
unsafe impl Send for KeepAwake {}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn bhc_awake_hold(reason: *const std::ffi::c_char) -> u32;
    fn bhc_awake_release(id: u32);
}

impl KeepAwake {
    /// Best effort: if the system refuses, recording still goes ahead (it just may sleep).
    pub fn hold(reason: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            let r = std::ffi::CString::new(reason).unwrap_or_default();
            let id = unsafe { bhc_awake_hold(r.as_ptr()) };
            if id == 0 { eprintln!("[awake] could not keep the Mac awake"); }
            Self { id }
        }
        #[cfg(windows)]
        {
            use windows::core::PWSTR;
            use windows::Win32::System::Power::*;
            use windows::Win32::System::Threading::*;
            let mut text: Vec<u16> = reason.encode_utf16().chain(std::iter::once(0)).collect();
            let ctx = REASON_CONTEXT {
                Version: 0, // POWER_REQUEST_CONTEXT_VERSION
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 { SimpleReasonString: PWSTR(text.as_mut_ptr()) },
            };
            let handle = unsafe { PowerCreateRequest(&ctx) }.unwrap_or_default();
            if !handle.is_invalid() {
                unsafe {
                    let _ = PowerSetRequest(handle, PowerRequestDisplayRequired);
                    let _ = PowerSetRequest(handle, PowerRequestSystemRequired);
                }
            } else { eprintln!("[awake] could not keep the PC awake"); }
            Self { handle }
        }
        #[cfg(target_os = "linux")]
        {
            // org.freedesktop.ScreenSaver.Inhibit: honoured by GNOME, KDE, Xfce and others; it is
            // released when the cookie is returned or the connection closes (even on a crash).
            let inhibit = (|| -> zbus::Result<(zbus::blocking::Connection, u32)> {
                let c = zbus::blocking::Connection::session()?;
                let reply = c.call_method(Some("org.freedesktop.ScreenSaver"), "/org/freedesktop/ScreenSaver",
                    Some("org.freedesktop.ScreenSaver"), "Inhibit", &("BHCapture", reason))?;
                let cookie: u32 = reply.body().deserialize()?;
                Ok((c, cookie))
            })();
            if let Err(e) = &inhibit { eprintln!("[awake] could not keep the computer awake: {e}"); }
            Self { inhibit: inhibit.ok() }
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.id != 0 { unsafe { bhc_awake_release(self.id) } }
        #[cfg(windows)]
        if !self.handle.is_invalid() {
            use windows::Win32::System::Power::*;
            unsafe {
                let _ = PowerClearRequest(self.handle, PowerRequestDisplayRequired);
                let _ = PowerClearRequest(self.handle, PowerRequestSystemRequired);
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
        #[cfg(target_os = "linux")]
        if let Some((c, cookie)) = self.inhibit.take() {
            let _ = c.call_method(Some("org.freedesktop.ScreenSaver"), "/org/freedesktop/ScreenSaver",
                Some("org.freedesktop.ScreenSaver"), "UnInhibit", &(cookie,));
        }
    }
}
