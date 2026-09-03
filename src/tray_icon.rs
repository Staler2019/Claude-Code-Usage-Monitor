use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop::{self, Color, WM_APP_TRAY};
use crate::poll_schedule::{mono_mask_bytes, pixel_count};

const CLAUDE_TRAY_ICON_ID: u32 = 1;
const CODEX_TRAY_ICON_ID: u32 = 2;

/// Menu item ID for toggling widget visibility (used by window.rs context menu).
pub const IDM_TOGGLE_WIDGET: u16 = 50;

/// Actions the tray message handler can request from the main window.
pub enum TrayAction {
    None,
    ToggleWidget,
    ShowContextMenu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayIconKind {
    Claude,
    Codex,
}

pub struct TrayIconData {
    pub kind: TrayIconKind,
    pub percent: Option<f64>,
    pub tooltip: String,
}

impl TrayIconKind {
    fn id(self) -> u32 {
        match self {
            Self::Claude => CLAUDE_TRAY_ICON_ID,
            Self::Codex => CODEX_TRAY_ICON_ID,
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
        }
    }
}

fn lerp_channel(start: u8, end: u8, t: f64) -> u8 {
    (start as f64 + (end as f64 - start as f64) * t.clamp(0.0, 1.0)).round() as u8
}

fn lerp_color(start: Color, end: Color, t: f64) -> Color {
    Color::new(
        lerp_channel(start.r, end.r, t),
        lerp_channel(start.g, end.g, t),
        lerp_channel(start.b, end.b, t),
    )
}

fn interpolated_fill(percent: f64) -> Color {
    if percent <= 50.0 {
        return Color::from_hex("#D97757");
    }

    let stops = [
        (50.0, Color::from_hex("#D97757")),
        (70.0, Color::from_hex("#D08540")),
        (85.0, Color::from_hex("#CC8C20")),
        (95.0, Color::from_hex("#C45020")),
        (100.0, Color::from_hex("#B82020")),
    ];

    for pair in stops.windows(2) {
        let (start_pct, start_color) = pair[0];
        let (end_pct, end_color) = pair[1];
        if percent <= end_pct {
            let span = (end_pct - start_pct).max(f64::EPSILON);
            let t = (percent - start_pct) / span;
            return lerp_color(start_color, end_color, t);
        }
    }

    stops[stops.len() - 1].1
}

fn codex_fill(percent: f64) -> Color {
    if percent >= 90.0 {
        Color::from_hex("#FFFFFF")
    } else {
        Color::from_hex("#111111")
    }
}

/// The part of a usage value that affects how the badge is drawn: the rounded
/// percentage clamped to the displayable range. Two inputs with the same key
/// produce the same badge text, so a poll that returns an unchanged rounded
/// percentage does not need a new icon at all.
pub(crate) fn icon_render_key(percent: Option<f64>) -> Option<u32> {
    percent.map(|p| {
        if p.is_nan() {
            0
        } else {
            p.round().clamp(0.0, 999.0) as u32
        }
    })
}

/// Everything needed to draw a badge, resolved from the kind/percent inputs.
struct BadgeSpec {
    size: i32,
    radius: i32,
    outline: i32,
    fill: Color,
    outline_color: Color,
    text_color: Color,
    text: String,
    font_height: i32,
}

/// Create a rounded-rectangle tray icon badge showing the usage percentage.
/// For Claude, `percent` = None uses the embedded app icon as the loading state.
/// For Codex, `percent` = None uses a black/white Codex placeholder badge.
///
/// The returned `HICON` is owned by the caller and must be released with
/// `DestroyIcon`. Every GDI object created while drawing is released before
/// this function returns, on every path.
pub fn create_icon(kind: TrayIconKind, percent: Option<f64>) -> HICON {
    if matches!(kind, TrayIconKind::Claude) && percent.is_none() {
        let app_icon = load_embedded_app_icon();
        if !app_icon.is_invalid() {
            return app_icon;
        }
    }

    let raw_percent = percent.unwrap_or(0.0);
    let fill = match kind {
        TrayIconKind::Claude => interpolated_fill(raw_percent),
        TrayIconKind::Codex => codex_fill(raw_percent),
    };
    let text_color = match kind {
        TrayIconKind::Claude => Color::from_hex("#FFFFFF"),
        TrayIconKind::Codex if raw_percent >= 90.0 => Color::from_hex("#111111"),
        TrayIconKind::Codex => Color::from_hex("#FFFFFF"),
    };
    let outline_color = match kind {
        TrayIconKind::Claude => fill,
        TrayIconKind::Codex if raw_percent >= 90.0 => Color::from_hex("#111111"),
        TrayIconKind::Codex => Color::from_hex("#FFFFFF"),
    };

    let text = match icon_render_key(percent) {
        Some(key) => key.to_string(),
        None => match kind {
            TrayIconKind::Claude => String::new(),
            TrayIconKind::Codex => "C".to_string(),
        },
    };

    let font_height = match text.len() {
        1 => -50,
        2 => -42,
        _ => -30,
    };

    let spec = BadgeSpec {
        size: 64,
        radius: 2,
        outline: if matches!(kind, TrayIconKind::Codex) {
            3
        } else {
            0
        },
        fill,
        outline_color,
        text_color,
        text,
        font_height,
    };

    unsafe { draw_badge_icon(&spec) }
}

/// A GDI object handle that is deleted when dropped.
struct OwnedGdiObject(HGDIOBJ);

impl OwnedGdiObject {
    unsafe fn solid_brush(color: &Color) -> Option<Self> {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        if brush.is_invalid() {
            None
        } else {
            Some(Self(HGDIOBJ(brush.0)))
        }
    }

    unsafe fn bold_font(height: i32) -> Option<Self> {
        let font_name = native_interop::wide_str("Arial Bold");
        let font = CreateFontW(
            height,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            ANTIALIASED_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        if font.is_invalid() {
            None
        } else {
            Some(Self(HGDIOBJ(font.0)))
        }
    }

    fn handle(&self) -> HGDIOBJ {
        self.0
    }
}

impl Drop for OwnedGdiObject {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0);
        }
    }
}

/// The DC + DIB scratch surface used to draw a badge. Dropping it restores the
/// DC's original bitmap, deletes the DIB and the memory DC and releases the
/// screen DC, so early returns cannot leak any of them.
struct BadgeSurface {
    screen_dc: HDC,
    mem_dc: HDC,
    dib: HBITMAP,
    previous_bitmap: Option<HGDIOBJ>,
}

impl BadgeSurface {
    /// Restore the DC's original bitmap so `dib` is no longer selected into
    /// any DC. GDI requires this before the bitmap is handed to another API
    /// such as `CreateIconIndirect`.
    unsafe fn deselect_dib(&mut self) {
        if let Some(previous) = self.previous_bitmap.take() {
            SelectObject(self.mem_dc, previous);
        }
    }
}

impl Drop for BadgeSurface {
    fn drop(&mut self) {
        unsafe {
            self.deselect_dib();
            if !self.dib.is_invalid() {
                let _ = DeleteObject(self.dib);
            }
            if !self.mem_dc.is_invalid() {
                let _ = DeleteDC(self.mem_dc);
            }
            if !self.screen_dc.is_invalid() {
                ReleaseDC(HWND::default(), self.screen_dc);
            }
        }
    }
}

unsafe fn draw_badge_icon(spec: &BadgeSpec) -> HICON {
    let size = spec.size;
    let Some(px_count) = pixel_count(size, size) else {
        return HICON::default();
    };

    let screen_dc = GetDC(HWND::default());
    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_invalid() {
        if !screen_dc.is_invalid() {
            ReleaseDC(HWND::default(), screen_dc);
        }
        return HICON::default();
    }
    let mut surface = BadgeSurface {
        screen_dc,
        mem_dc,
        dib: HBITMAP::default(),
        previous_bitmap: None,
    };

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib =
        CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
    if dib.is_invalid() || bits.is_null() {
        if !dib.is_invalid() {
            let _ = DeleteObject(dib);
        }
        return HICON::default();
    }
    surface.dib = dib;
    surface.previous_bitmap = Some(SelectObject(mem_dc, dib));

    // Zero-fill (transparent background). `px_count` was validated above and
    // matches the DIB layout exactly (32 bpp, no row padding).
    let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, px_count);
    pixel_data.fill(0);

    // Draw rounded rectangle badge
    let null_pen = GetStockObject(NULL_PEN);
    let old_pen = SelectObject(mem_dc, null_pen);

    let margin = 0;
    if spec.outline > 0 {
        if let Some(brush) = OwnedGdiObject::solid_brush(&spec.outline_color) {
            let old_brush = SelectObject(mem_dc, brush.handle());
            let _ = RoundRect(
                mem_dc,
                margin,
                margin,
                size - margin + 1,
                size - margin + 1,
                (spec.radius + 1) * 2,
                (spec.radius + 1) * 2,
            );
            SelectObject(mem_dc, old_brush);
        }
    }

    if let Some(brush) = OwnedGdiObject::solid_brush(&spec.fill) {
        let old_brush = SelectObject(mem_dc, brush.handle());
        let _ = RoundRect(
            mem_dc,
            margin + spec.outline,
            margin + spec.outline,
            size - margin - spec.outline + 1,
            size - margin - spec.outline + 1,
            (spec.radius - 1) * 2,
            (spec.radius - 1) * 2,
        );
        SelectObject(mem_dc, old_brush);
    }
    SelectObject(mem_dc, old_pen);

    // Draw centered percentage text. DrawTextW must not be given an empty
    // buffer (its pointer would be dangling), so skip it for empty text.
    if !spec.text.is_empty() {
        if let Some(font) = OwnedGdiObject::bold_font(spec.font_height) {
            let old_font = SelectObject(mem_dc, font.handle());
            let _ = SetBkMode(mem_dc, TRANSPARENT);
            let _ = SetTextColor(mem_dc, COLORREF(spec.text_color.to_colorref()));

            let mut text_rect = RECT {
                left: margin,
                top: margin,
                right: size - margin,
                bottom: size - margin,
            };
            let mut text_wide: Vec<u16> = spec.text.encode_utf16().collect();
            let _ = DrawTextW(
                mem_dc,
                &mut text_wide,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            SelectObject(mem_dc, old_font);
        }
    }

    // GDI batches drawing calls; make sure they have landed in the DIB before
    // the CPU reads and rewrites the pixels.
    let _ = GdiFlush();

    // Set alpha: non-zero BGR pixel -> fully opaque; background stays transparent
    for px in pixel_data.iter_mut() {
        if *px != 0 {
            *px = (*px & 0x00FF_FFFF) | 0xFF00_0000;
        }
    }

    // The colour bitmap must not be selected into a DC when it is handed to
    // CreateIconIndirect.
    surface.deselect_dib();

    // Monochrome mask (per-pixel alpha from the colour bitmap). Rows of a 1 bpp
    // DDB are WORD-aligned; mono_mask_bytes computes the exact size GDI reads.
    let mask_bytes = vec![0u8; mono_mask_bytes(size, size)];
    let mask_bmp = CreateBitmap(
        size,
        size,
        1,
        1,
        Some(mask_bytes.as_ptr() as *const std::ffi::c_void),
    );
    if mask_bmp.is_invalid() {
        return HICON::default();
    }

    let icon_info = ICONINFO {
        fIcon: TRUE,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask_bmp,
        hbmColor: dib,
    };
    let hicon = CreateIconIndirect(&icon_info).unwrap_or_default();
    let _ = DeleteObject(mask_bmp);

    // `surface` is dropped here: DIB deleted, memory DC deleted, screen DC released.
    hicon
}

/// Extract the first small icon from the executable at `path`, or the first
/// large icon if no small one is available.
///
/// `ExtractIconExW` creates one `HICON` per non-null output slot, and the
/// caller owns all of them. The previous implementation requested both sizes,
/// returned one and never destroyed the other, leaking a USER handle (plus two
/// kernel bitmaps) on every call. Requesting a single slot at a time means
/// exactly one icon is created and returned.
pub(crate) fn extract_first_small_icon(path: &[u16]) -> HICON {
    if path.last() != Some(&0) {
        return HICON::default();
    }

    unsafe {
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(path.as_ptr()),
            0,
            None,
            Some(&mut small_icon),
            1,
        );
        if extracted != 0 && !small_icon.is_invalid() {
            return small_icon;
        }

        let mut large_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(path.as_ptr()),
            0,
            Some(&mut large_icon),
            None,
            1,
        );
        if extracted != 0 && !large_icon.is_invalid() {
            return large_icon;
        }
    }

    HICON::default()
}

fn load_embedded_app_icon() -> HICON {
    match native_interop::current_module_path_wide() {
        Some(path) => extract_first_small_icon(&path),
        None => HICON::default(),
    }
}

/// Show a Windows balloon notification from the tray icon.
/// Used to alert the user when re-authentication is required.
pub fn notify_balloon(hwnd: HWND, kind: TrayIconKind, title: &str, message: &str) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = kind.id();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(title, &mut nid.szInfoTitle);
        copy_wide_256(message, &mut nid.szInfo);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Copy a string into a fixed-size wide buffer (truncates to fit).
fn copy_wide<const N: usize>(s: &str, buf: &mut [u16; N]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut len = wide.len().min(N - 1);
    // Don't leave a lone high surrogate at the truncation point
    if len > 0 && (0xD800..=0xDBFF).contains(&wide[len - 1]) {
        len -= 1;
    }
    buf[..len].copy_from_slice(&wide[..len]);
    buf[len] = 0;
}

/// Copy a string into a 256-wide buffer.
fn copy_wide_256(s: &str, buf: &mut [u16; 256]) {
    copy_wide(s, buf)
}

/// What the shell currently shows for one icon slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShownIcon {
    key: Option<u32>,
    tooltip: String,
}

impl ShownIcon {
    fn from_data(data: &TrayIconData) -> Self {
        Self {
            key: icon_render_key(data.percent),
            tooltip: data.tooltip.clone(),
        }
    }
}

/// Shell operation needed to move an icon slot from `shown` to `wanted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncOp {
    Add,
    Modify,
    Skip,
    Remove,
}

/// Decide what to do with one icon slot. Unchanged icons are skipped entirely,
/// so steady-state polls build no icons and make no shell calls.
pub(crate) fn plan_sync(shown: Option<&ShownIcon>, wanted: Option<&ShownIcon>) -> SyncOp {
    match (shown, wanted) {
        (None, None) => SyncOp::Skip,
        (None, Some(_)) => SyncOp::Add,
        (Some(_), None) => SyncOp::Remove,
        (Some(current), Some(next)) if current == next => SyncOp::Skip,
        (Some(_), Some(_)) => SyncOp::Modify,
    }
}

/// Per-kind record of what has been registered with the shell.
static SHOWN: Mutex<[Option<ShownIcon>; 2]> = Mutex::new([None, None]);

fn shown_icon(kind: TrayIconKind) -> Option<ShownIcon> {
    let shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    shown[kind.slot()].clone()
}

fn set_shown_icon(kind: TrayIconKind, value: Option<ShownIcon>) {
    let mut shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    shown[kind.slot()] = value;
}

/// Send one NIM_ADD / NIM_MODIFY with a freshly built icon. The shell copies
/// the icon during the call, so the local handle is destroyed immediately
/// afterwards. Returns whether the shell accepted the request.
fn notify_icon(hwnd: HWND, data: &TrayIconData, message: NOTIFY_ICON_MESSAGE) -> bool {
    let hicon = create_icon(data.kind, data.percent);
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = data.kind.id();
        nid.uFlags = NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        if !hicon.is_invalid() {
            // If the icon could not be built, keep whatever the shell shows now
            // rather than replacing it with a null icon.
            nid.uFlags |= NIF_ICON;
            nid.hIcon = hicon;
        }
        copy_to_tip(&data.tooltip, &mut nid.szTip);
        let accepted = Shell_NotifyIconW(message, &nid).as_bool();
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
        accepted
    }
}

/// Remove the tray icon from the shell.
pub fn remove(hwnd: HWND, kind: TrayIconKind) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = kind.id();
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
    set_shown_icon(kind, None);
}

/// Bring the shell's tray icons in line with `icons`.
///
/// Only slots whose rendered percentage or tooltip changed are touched: an
/// unchanged icon costs no GDI work and no shell round-trip. A slot that was
/// never registered gets NIM_ADD; a registered one gets NIM_MODIFY, falling
/// back to NIM_ADD if the shell no longer knows the icon (explorer restarted).
pub fn sync(hwnd: HWND, icons: &[TrayIconData]) {
    for kind in [TrayIconKind::Claude, TrayIconKind::Codex] {
        let wanted_data = icons.iter().find(|icon| icon.kind == kind);
        let wanted = wanted_data.map(ShownIcon::from_data);
        let shown = shown_icon(kind);

        match plan_sync(shown.as_ref(), wanted.as_ref()) {
            SyncOp::Skip => {}
            SyncOp::Remove => remove(hwnd, kind),
            SyncOp::Add | SyncOp::Modify => {
                let Some(data) = wanted_data else {
                    continue;
                };
                let (first, fallback) = if shown.is_some() {
                    (NIM_MODIFY, NIM_ADD)
                } else {
                    (NIM_ADD, NIM_MODIFY)
                };
                let accepted = notify_icon(hwnd, data, first) || notify_icon(hwnd, data, fallback);
                set_shown_icon(kind, if accepted { wanted } else { None });
            }
        }
    }
}

pub fn remove_all(hwnd: HWND) {
    remove(hwnd, TrayIconKind::Claude);
    remove(hwnd, TrayIconKind::Codex);
}

/// Interpret a tray callback message and return the action to take.
pub fn handle_message(lparam: LPARAM) -> TrayAction {
    let mouse_msg = lparam.0 as u32;
    match mouse_msg {
        WM_LBUTTONUP => TrayAction::ToggleWidget,
        WM_RBUTTONUP => TrayAction::ShowContextMenu,
        _ => TrayAction::None,
    }
}

/// Copy a string into the fixed-size szTip field (max 127 chars + null).
fn copy_to_tip(s: &str, tip: &mut [u16; 128]) {
    copy_wide(s, tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS,
    };

    /// The GDI/USER counters are per process, so the leak tests must not
    /// interleave with each other.
    static GDI_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Slack for lazily-initialised GDI/USER objects (font cache, etc.).
    /// The bugs these tests guard against leak one object per iteration, so
    /// with 200 iterations the signal is ~25x the slack.
    const LEAK_SLACK: i64 = 8;
    const LEAK_ITERATIONS: usize = 200;

    fn gui_counts() -> (i64, i64) {
        unsafe {
            let process = GetCurrentProcess();
            (
                i64::from(GetGuiResources(process, GR_GDIOBJECTS)),
                i64::from(GetGuiResources(process, GR_USEROBJECTS)),
            )
        }
    }

    fn destroy(icon: HICON) {
        if !icon.is_invalid() {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }

    fn shown(key: Option<u32>, tooltip: &str) -> ShownIcon {
        ShownIcon {
            key,
            tooltip: tooltip.to_string(),
        }
    }

    // -- pure helpers --

    #[test]
    fn icon_render_key_rounds_and_clamps() {
        assert_eq!(icon_render_key(None), None);
        assert_eq!(icon_render_key(Some(49.4)), Some(49));
        assert_eq!(icon_render_key(Some(49.6)), Some(50));
        assert_eq!(icon_render_key(Some(-5.0)), Some(0));
        assert_eq!(icon_render_key(Some(1e9)), Some(999));
        assert_eq!(icon_render_key(Some(f64::INFINITY)), Some(999));
        assert_eq!(icon_render_key(Some(f64::NEG_INFINITY)), Some(0));
        assert_eq!(icon_render_key(Some(f64::NAN)), Some(0));
    }

    #[test]
    fn plan_sync_only_touches_the_shell_when_something_changed() {
        let a = shown(Some(50), "Claude 5h: 50%");
        let b = shown(Some(51), "Claude 5h: 51%");
        let a_other_tip = shown(Some(50), "Claude 5h: 50% | 7d: 1%");

        assert_eq!(plan_sync(None, None), SyncOp::Skip);
        assert_eq!(plan_sync(None, Some(&a)), SyncOp::Add);
        assert_eq!(plan_sync(Some(&a), None), SyncOp::Remove);
        assert_eq!(plan_sync(Some(&a), Some(&a)), SyncOp::Skip);
        assert_eq!(plan_sync(Some(&a), Some(&b)), SyncOp::Modify);
        assert_eq!(plan_sync(Some(&a), Some(&a_other_tip)), SyncOp::Modify);
        // Loading state (None key) to a real value is a modify, not a skip.
        assert_eq!(
            plan_sync(Some(&shown(None, "t")), Some(&shown(Some(0), "t"))),
            SyncOp::Modify
        );
    }

    #[test]
    fn shown_icon_from_data_uses_the_render_key() {
        let data = TrayIconData {
            kind: TrayIconKind::Claude,
            percent: Some(49.6),
            tooltip: "tip".to_string(),
        };
        assert_eq!(ShownIcon::from_data(&data), shown(Some(50), "tip"));
    }

    #[test]
    fn mask_buffer_covers_the_badge_bitmap() {
        // 64 px wide -> 8 bytes per row, 64 rows.
        assert_eq!(mono_mask_bytes(64, 64), 512);
        assert_eq!(pixel_count(64, 64), Some(4096));
    }

    #[test]
    fn fill_colours_never_panic_across_the_percent_range() {
        for tenth in 0..=1_200 {
            let percent = f64::from(tenth) / 10.0;
            let _ = interpolated_fill(percent);
            let _ = codex_fill(percent);
        }
        let _ = interpolated_fill(f64::NAN);
        let _ = interpolated_fill(f64::INFINITY);
        let _ = interpolated_fill(-1.0);
    }

    #[test]
    fn copy_to_tip_truncates_without_splitting_a_surrogate_pair() {
        // U+1F600 is two UTF-16 units; 200 of them is far more than 127 units.
        let text: String = std::iter::repeat_n('\u{1F600}', 200).collect();
        let mut tip = [0xFFFFu16; 128];
        copy_to_tip(&text, &mut tip);

        let len = tip.iter().position(|&u| u == 0).expect("NUL terminator");
        assert!(len <= 127);
        assert_eq!(
            len, 126,
            "odd cut point must drop the dangling high surrogate"
        );
        assert!(
            !(0xD800..=0xDBFF).contains(&tip[len - 1]),
            "must not end with a high surrogate"
        );
        assert!((0xDC00..=0xDFFF).contains(&tip[len - 1]));
    }

    #[test]
    fn copy_wide_fits_short_strings_exactly() {
        let mut buf = [0xFFFFu16; 8];
        copy_wide("abc", &mut buf);
        assert_eq!(&buf[..4], &[b'a' as u16, b'b' as u16, b'c' as u16, 0]);

        let mut small = [0xFFFFu16; 3];
        copy_wide("abcdef", &mut small);
        assert_eq!(small, [b'a' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn tray_icon_kinds_map_to_distinct_ids_and_slots() {
        assert_ne!(TrayIconKind::Claude.id(), TrayIconKind::Codex.id());
        assert_ne!(TrayIconKind::Claude.slot(), TrayIconKind::Codex.slot());
        assert!(TrayIconKind::Claude.slot() < 2);
        assert!(TrayIconKind::Codex.slot() < 2);
    }

    // -- GDI / USER handle balance (Windows runner) --

    /// Regression test for handle leaks in badge creation: every GDI object
    /// used to draw the icon must be released, and the icon itself must be the
    /// only object left for the caller.
    #[cfg(windows)]
    #[test]
    fn create_icon_does_not_leak_gdi_or_user_objects() {
        let _serial = GDI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let cases: [(TrayIconKind, Option<f64>); 5] = [
            (TrayIconKind::Claude, Some(50.0)),
            (TrayIconKind::Codex, Some(95.0)),
            (TrayIconKind::Claude, None),
            (TrayIconKind::Codex, None),
            (TrayIconKind::Claude, Some(f64::NAN)),
        ];

        // Warm up lazily-created GDI state before taking the baseline.
        for _ in 0..5 {
            for (kind, percent) in cases {
                destroy(create_icon(kind, percent));
            }
        }
        let probe = create_icon(TrayIconKind::Claude, Some(50.0));
        if probe.is_invalid() {
            eprintln!("skipping: GDI icon creation unavailable in this session");
            return;
        }
        destroy(probe);

        let (gdi_before, user_before) = gui_counts();
        for _ in 0..LEAK_ITERATIONS {
            for (kind, percent) in cases {
                destroy(create_icon(kind, percent));
            }
        }
        let (gdi_after, user_after) = gui_counts();

        assert!(
            gdi_after - gdi_before <= LEAK_SLACK,
            "GDI objects leaked: {gdi_before} -> {gdi_after}"
        );
        assert!(
            user_after - user_before <= LEAK_SLACK,
            "USER objects leaked: {user_before} -> {user_after}"
        );
    }

    /// Regression test for the `ExtractIconExW` sibling-icon leak. Uses
    /// shell32.dll, which always carries icons, rather than the test binary.
    #[cfg(windows)]
    #[test]
    fn extract_first_small_icon_does_not_leak_user_objects() {
        let _serial = GDI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let path = native_interop::wide_str(&format!(r"{system_root}\System32\shell32.dll"));

        for _ in 0..5 {
            destroy(extract_first_small_icon(&path));
        }
        let probe = extract_first_small_icon(&path);
        if probe.is_invalid() {
            eprintln!("skipping: no icon could be extracted from shell32.dll");
            return;
        }
        destroy(probe);

        let (gdi_before, user_before) = gui_counts();
        for _ in 0..LEAK_ITERATIONS {
            let icon = extract_first_small_icon(&path);
            assert!(!icon.is_invalid());
            destroy(icon);
        }
        let (gdi_after, user_after) = gui_counts();

        assert!(
            user_after - user_before <= LEAK_SLACK,
            "USER objects leaked: {user_before} -> {user_after}"
        );
        assert!(
            gdi_after - gdi_before <= LEAK_SLACK,
            "GDI objects leaked: {gdi_before} -> {gdi_after}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_first_small_icon_rejects_unterminated_paths() {
        assert!(extract_first_small_icon(&[]).is_invalid());
        assert!(extract_first_small_icon(&[b'C' as u16, b':' as u16]).is_invalid());
    }

    #[cfg(windows)]
    #[test]
    fn create_icon_handles_extreme_percentages_without_panicking() {
        let _serial = GDI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for percent in [
            None,
            Some(0.0),
            Some(100.0),
            Some(-1.0),
            Some(1e12),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ] {
            destroy(create_icon(TrayIconKind::Claude, percent));
            destroy(create_icon(TrayIconKind::Codex, percent));
        }
    }
}
