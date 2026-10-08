//! Windows system calls of the desktop application.

use std::time::Duration;

use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    TranslateMessage,
};

/// The user's locale name, such as `fr-FR`.
pub fn user_locale() -> Option<String> {
    // LOCALE_NAME_MAX_LENGTH is 85 UTF-16 units, the terminating null included.
    let mut buf = [0u16; 85];
    // SAFETY: the buffer and its length in UTF-16 units are given together.
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    (n > 1).then(|| String::from_utf16_lossy(&buf[..n as usize - 1]))
}

/// Wait up to `timeout` for a message of this thread's windows, then dispatch every pending one
/// (the tray icon's window receives its clicks this way).
pub fn pump_messages(timeout: Duration) {
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: no handles are waited on (count 0, null array); the MSG is written by PeekMessageW
    // before it is read, and only dispatched on the thread that received it.
    unsafe {
        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, ms, QS_ALLINPUT);
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The standard handles this program received (a log file the installer's output was redirected
/// to, say) are not handed down to the programs it starts: the application and the workers outlive
/// it, and kept that file open for as long as they ran (measured). Rust's `Command` passes every
/// inheritable handle, not only the three it is given.
fn keep_std_handles_to_ourselves() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle has no preconditions; SetHandleInformation on a null or invalid
        // handle only fails.
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Write into the terminal that started the application, if any (a window application has no
/// console of its own); started from the Start menu or at logon, there is none and nothing happens.
pub fn attach_console() {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    keep_std_handles_to_ourselves();
    // SAFETY: GetStdHandle has no preconditions; AttachConsole only fails (harmlessly) when there
    // is no parent console.
    unsafe {
        // Output redirected to a file or a pipe: keep it (attaching would replace it).
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if !out.is_null() && out != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return;
        }
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

// --- The installer's traces in the user's registry (HKEY_CURRENT_USER: no administrator) ---------

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\GPU Goblin";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A key of HKEY_CURRENT_USER open for writing, closed when dropped.
struct Key(HKEY);

impl Key {
    fn create(path: &str) -> Result<Key, String> {
        let mut h: HKEY = std::ptr::null_mut();
        let p = wide(path);
        // SAFETY: valid null-terminated path and out pointer; the key is closed in Drop.
        let r = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                p.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut h,
                std::ptr::null_mut(),
            )
        };
        if r == 0 {
            Ok(Key(h))
        } else {
            Err(format!("registry {path}: error {r}"))
        }
    }

    fn set_str(&self, name: &str, value: &str) -> Result<(), String> {
        let n = wide(name);
        let v = wide(value);
        // SAFETY: the data pointer and its size in bytes (terminating null included) match.
        let r = unsafe {
            RegSetValueExW(
                self.0,
                n.as_ptr(),
                0,
                REG_SZ,
                v.as_ptr().cast(),
                (v.len() * 2) as u32,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(format!("registry value {name}: error {r}"))
        }
    }

    fn set_dword(&self, name: &str, value: u32) -> Result<(), String> {
        let n = wide(name);
        // SAFETY: a DWORD value of 4 bytes.
        let r = unsafe {
            RegSetValueExW(
                self.0,
                n.as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(format!("registry value {name}: error {r}"))
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegCreateKeyExW and is closed once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

/// Start `command` with the user's session.
pub fn set_run_at_logon(name: &str, command: &str) -> Result<(), String> {
    Key::create(RUN_KEY)?.set_str(name, command)
}

/// No longer start `name` with the session.
pub fn remove_run_at_logon(name: &str) {
    let (k, n) = (wide(RUN_KEY), wide(name));
    // SAFETY: valid null-terminated strings; a missing value is not an error worth reporting.
    unsafe {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr());
    }
}

/// The entry in Windows' list of installed apps, whose removal runs `gg-desk uninstall`.
pub fn register_uninstall(desk: &std::path::Path, version: &str) -> Result<(), String> {
    let k = Key::create(UNINSTALL_KEY)?;
    let exe = desk.display().to_string();
    k.set_str("DisplayName", "GPU Goblin")?;
    k.set_str("DisplayVersion", version)?;
    k.set_str("Publisher", "Nicolas Michel (LAAS-CNRS)")?;
    k.set_str("URLInfoAbout", "https://gpugoblin.com")?;
    k.set_str("DisplayIcon", &exe)?;
    k.set_str(
        "InstallLocation",
        &desk
            .parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
    )?;
    k.set_str("UninstallString", &format!("\"{exe}\" uninstall"))?;
    k.set_dword("NoModify", 1)?;
    k.set_dword("NoRepair", 1)
}

/// Remove that entry.
pub fn unregister_uninstall() {
    let k = wide(UNINSTALL_KEY);
    // SAFETY: a valid null-terminated path under HKEY_CURRENT_USER.
    unsafe {
        RegDeleteTreeW(HKEY_CURRENT_USER, k.as_ptr());
    }
}

/// Stop every process of this user whose program lies under `dir` (the workers and executors of
/// a version being replaced or removed), except this one, and wait until they have ended: their
/// files stay locked until then.
pub fn kill_processes_under(dir: &std::path::Path) {
    use windows_sys::Win32::System::Threading::{PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // With the separator: `…\0.1.0` must not match `…\0.1.0-test`.
    let mut prefix = dir.display().to_string().to_lowercase();
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }
    // SAFETY: the snapshot handle is checked and closed; each process handle is checked and
    // closed; the entry and name buffers are sized as the calls require.
    unsafe {
        let me = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more {
            let pid = e.th32ProcessID;
            if pid != me {
                let h = OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                );
                if !h.is_null() {
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
                        != 0
                    {
                        let path = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
                        if path.starts_with(&prefix) {
                            TerminateProcess(h, 1);
                            WaitForSingleObject(h, 5000);
                        }
                    }
                    CloseHandle(h);
                }
            }
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
}

use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO, MessageBoxW,
};

/// A question in a message box; `true` for Yes.
pub fn ask(title: &str, text: &str) -> bool {
    let (t, x) = (wide(title), wide(text));
    // SAFETY: null-terminated strings; no owner window.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            x.as_ptr(),
            t.as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

/// A message in a message box; `warning` for a failure.
pub fn tell(title: &str, text: &str, warning: bool) {
    let (t, x) = (wide(title), wide(text));
    let icon = if warning {
        MB_ICONWARNING
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: null-terminated strings; no owner window.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), x.as_ptr(), t.as_ptr(), MB_OK | icon);
    }
}

/// Give the window the program's own icon (resource 1, embedded by build.rs), large and small:
/// the taskbar and Alt+Tab show the large one, which the window library leaves unset.
pub fn set_window_icons(hwnd: isize) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON,
        SM_CXSMICON, SendMessageW, WM_SETICON,
    };
    // SAFETY: the module is this program (null name); resource 1 is its icon group; the window
    // handle comes from the window library and is valid while the application runs; icons loaded
    // without LR_SHARED belong to the window for its whole life.
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
            let size = GetSystemMetrics(metric);
            let icon = LoadImageW(
                module,
                // MAKEINTRESOURCE(1): a resource number in place of a name.
                std::ptr::without_provenance::<u16>(1),
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            );
            if !icon.is_null() {
                SendMessageW(hwnd as _, WM_SETICON, which as usize, icon as isize);
            }
        }
    }
}

/// Free space for this user on the volume of `dir`, in bytes.
pub fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut path: Vec<u16> = dir.as_os_str().to_string_lossy().encode_utf16().collect();
    path.push(0);
    let mut free = 0u64;
    // SAFETY: a null-terminated path and one out value; the totals are not asked for.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

/// Run `program` with `args` as an administrator: Windows asks the user's approval (UAC). The exit
/// code once it has ended; an error if the approval was refused or it could not start.
pub fn run_elevated(program: &std::path::Path, args: &str) -> Result<u32, String> {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide("runas");
    let file: Vec<u16> = program
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let params = wide(args);
    // SAFETY: the structure is zeroed then filled with null-terminated strings that outlive the
    // call; the process handle it returns is waited on and closed.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
            return Err(format!(
                "not started or not approved: {}",
                std::io::Error::last_os_error()
            ));
        }
        windows_sys::Win32::System::Threading::WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(code)
    }
}
