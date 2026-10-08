//! Windows readings for the owner's policy (ADR-0043, ADR-0045): local time, how long the user has
//! been away, whether the session is locked. Read without privileges, from the user's own session
//! (the worker runs as the user, started at logon).

use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK,
    WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetTickCount};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

/// Local day of the week (0 = Monday) and minutes since midnight, in the machine's time zone.
pub fn local_now() -> (u32, u32) {
    // SAFETY: GetLocalTime fills the SYSTEMTIME it is given and cannot fail.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    // wDayOfWeek: 0 = Sunday.
    (
        (u32::from(t.wDayOfWeek) + 6) % 7,
        u32::from(t.wHour) * 60 + u32::from(t.wMinute),
    )
}

/// Seconds since the last keyboard or mouse input in this session.
pub fn idle_secs() -> Option<u64> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is initialized with its size, as GetLastInputInfo requires.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // SAFETY: GetTickCount has no preconditions. Both counters wrap after 49.7 days; the
    // wrapping difference stays right across a wrap.
    let now = unsafe { GetTickCount() };
    Some(u64::from(now.wrapping_sub(info.dwTime)) / 1000)
}

/// Whether this session is locked (Windows 10 and later report the flags the right way round).
pub fn locked() -> Option<bool> {
    let mut buf: *mut u16 = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: the out pointers are valid; on success the buffer is a WTSINFOEXW that we free with
    // WTSFreeMemory below and never touch afterwards.
    unsafe {
        if WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buf,
            &mut len,
        ) == 0
            || buf.is_null()
        {
            return None;
        }
        let info = &*(buf as *const WTSINFOEXW);
        let flags = if info.Level == 1 && len as usize >= std::mem::size_of::<WTSINFOEXW>() {
            Some(info.Data.WTSInfoExLevel1.SessionFlags)
        } else {
            None
        };
        WTSFreeMemory(buf.cast());
        match flags? {
            f if f == WTS_SESSIONSTATE_LOCK as i32 => Some(true),
            f if f == WTS_SESSIONSTATE_UNLOCK as i32 => Some(false),
            _ => None,
        }
    }
}

/// Whether this process runs elevated (an administrator's full token): the worker refuses, as it
/// refuses root on Linux. A user started from the desktop has a filtered token, even an
/// administrator.
pub fn elevated() -> Option<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: the pseudo-handle of this process; the token handle is checked and closed; the
    // output structure is passed with its size.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut e as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        (ok != 0).then_some(e.TokenIsElevated != 0)
    }
}
