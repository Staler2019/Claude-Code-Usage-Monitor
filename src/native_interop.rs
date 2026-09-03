use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

// Window style constants
pub const WS_POPUP_STYLE: u32 = 0x80000000;
pub const WS_CHILD_STYLE: u32 = 0x40000000;
pub const WS_CLIPSIBLINGS_STYLE: u32 = 0x04000000;

// Win event constants
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// Timer IDs
pub const TIMER_POLL: usize = 1;
pub const TIMER_COUNTDOWN: usize = 2;
pub const TIMER_RESET_POLL: usize = 3;
pub const TIMER_UPDATE_CHECK: usize = 4;

// Custom messages
pub const WM_APP: u32 = 0x8000;
pub const WM_APP_USAGE_UPDATED: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 3;

/// Get the taskbar window handle
pub fn find_taskbar() -> Option<HWND> {
    unsafe {
        let class = wide_str("Shell_TrayWnd");
        match FindWindowW(PCWSTR::from_raw(class.as_ptr()), PCWSTR::null()) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Find a child window by class name
pub fn find_child_window(parent: HWND, class_name: &str) -> Option<HWND> {
    unsafe {
        let class = wide_str(class_name);
        match FindWindowExW(
            parent,
            HWND::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
        ) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Get taskbar position via SHAppBarMessage
pub fn get_taskbar_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: taskbar_hwnd,
            ..Default::default()
        };
        let result = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        if result == 0 {
            return None;
        }
        Some(abd.rc)
    }
}

/// Get the bounding rectangle of a window
pub fn get_window_rect_safe(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

/// Embed our window as a child of the taskbar
pub fn embed_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) {
    unsafe {
        // Preserve existing extended style, add tool window + no activate
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32,
        );

        // Change from popup to child
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP_STYLE) | WS_CHILD_STYLE | WS_CLIPSIBLINGS_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let _ = SetParent(hwnd, taskbar_hwnd);
    }
}

/// Move the window
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

/// Set up a WinEvent hook for tray location changes
pub fn set_tray_event_hook(
    thread_id: u32,
    callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32),
) -> Option<HWINEVENTHOOK> {
    unsafe {
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(callback),
            0,
            thread_id,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.is_invalid() {
            None
        } else {
            Some(hook)
        }
    }
}

/// Get the thread ID that owns a window
pub fn get_window_thread_id(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// Unhook a WinEvent hook
pub fn unhook_win_event(hook: HWINEVENTHOOK) {
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

/// Convert a Rust string to a null-terminated wide string
pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Largest buffer we are willing to allocate for the module path (in UTF-16 units).
const MAX_MODULE_PATH_UNITS: usize = 32_768;

/// NUL-terminated UTF-16 path of the running executable.
///
/// `GetModuleFileNameW` truncates silently and returns the buffer size when the
/// path does not fit (and on some Windows versions leaves the buffer without a
/// terminating NUL). Callers used to pass a fixed 260-unit array and treat any
/// non-zero return as success, which could hand an unterminated buffer to
/// `ExtractIconExW` or read one unit past the end of the array. This helper
/// grows the buffer until the whole path fits and always returns a terminated
/// string (the trailing 0 is included in the returned vector).
pub fn current_module_path_wide() -> Option<Vec<u16>> {
    let mut capacity = 260usize;
    loop {
        let mut buf = vec![0u16; capacity];
        let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
        if len == 0 {
            return None;
        }
        if len < buf.len() {
            buf.truncate(len + 1);
            buf[len] = 0;
            return Some(buf);
        }
        if capacity >= MAX_MODULE_PATH_UNITS {
            return None;
        }
        capacity = (capacity * 2).min(MAX_MODULE_PATH_UNITS);
    }
}

/// Path of the running executable as a Rust string (no trailing NUL).
pub fn current_module_path() -> Option<String> {
    let wide = current_module_path_wide()?;
    let without_nul = wide.strip_suffix(&[0u16]).unwrap_or(&wide);
    Some(String::from_utf16_lossy(without_nul))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_str_is_nul_terminated() {
        let wide = wide_str("ab");
        assert_eq!(wide, vec![b'a' as u16, b'b' as u16, 0]);
        assert_eq!(wide_str(""), vec![0]);
    }

    #[test]
    fn color_from_hex_and_colorref_round_trip() {
        let c = Color::from_hex("#D97757");
        assert_eq!((c.r, c.g, c.b), (0xD9, 0x77, 0x57));
        // COLORREF is 0x00BBGGRR.
        assert_eq!(c.to_colorref(), 0x0057_77D9);
        assert_eq!(colorref(1, 2, 3), 0x0003_0201);
    }

    #[cfg(windows)]
    #[test]
    fn current_module_path_wide_is_nul_terminated_and_matches_current_exe() {
        let wide = current_module_path_wide().expect("module path");
        assert_eq!(wide.last(), Some(&0), "must be NUL-terminated");
        assert!(
            !wide[..wide.len() - 1].contains(&0),
            "no interior NUL units"
        );

        let decoded = current_module_path().expect("module path string");
        let expected = std::env::current_exe()
            .expect("current_exe")
            .to_string_lossy()
            .to_string();
        assert!(
            decoded.eq_ignore_ascii_case(&expected),
            "{decoded} != {expected}"
        );
    }
}

/// COLORREF wrapper (RGB packed into u32)
pub fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Color helper
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    #[allow(dead_code)]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Self { r, g, b }
    }

    pub fn to_colorref(self) -> u32 {
        colorref(self.r, self.g, self.b)
    }
}
