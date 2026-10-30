//! mutekey: a global hotkey that mutes every microphone, an always-on-top badge
//! while muted, and a tray dot (green = live, red = muted). Left-click the tray
//! dot to toggle; right-click it for the menu.

#![windows_subsystem = "windows"]

use std::cell::Cell;
use std::ffi::c_void;
use std::{mem, ptr};

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eCapture, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Same shortcut PowerToys' Video Conference Mute used for the microphone.
const HOTKEY_MODIFIERS: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(MOD_WIN.0 | MOD_SHIFT.0);
const HOTKEY_KEY: u32 = b'A' as u32;
const HOTKEY_LABEL: &str = "Win+Shift+A";

// Badge geometry in 96-DPI pixels, centered at the top of the primary screen.
const BADGE_WIDTH: i32 = 220;
const BADGE_HEIGHT: i32 = 44;
const BADGE_TOP_MARGIN: i32 = 16;
const BADGE_OPACITY: u8 = 235;
const BADGE_TEXT: &str = "Microphone muted";
const MIC_OFF_GLYPH: u16 = 0xEC54; // "MicOff" in Segoe Fluent Icons

const RED: COLORREF = rgb(196, 43, 28);
const GREEN: COLORREF = rgb(16, 160, 72);
const GRAY: COLORREF = rgb(128, 128, 128);
const WHITE: COLORREF = rgb(255, 255, 255);

const HOTKEY_ID: i32 = 1;
const POLL_TIMER_ID: usize = 1;
const POLL_INTERVAL_MS: u32 = 500;
const TRAY_ID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const MENU_TOGGLE: usize = 1;
const MENU_EXIT: usize = 2;

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

#[derive(Clone, Copy, PartialEq)]
enum MicState {
    Live,
    Muted,
    NoMicrophone,
}

#[derive(Clone, Copy)]
struct App {
    controller: HWND, // hidden window that owns the hotkey, timer and tray icon
    badge: HWND,
    dpi: i32,
    icon_live: HICON,
    icon_muted: HICON,
    icon_none: HICON,
    text_font: HFONT,
    glyph_font: HFONT,
    taskbar_created: u32,
}

impl App {
    fn px(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }
}

thread_local! {
    static APP: Cell<Option<App>> = const { Cell::new(None) };
    static STATE: Cell<MicState> = const { Cell::new(MicState::NoMicrophone) };
}

fn main() {
    if let Err(e) = run() {
        error_box(&format!("mutekey failed to start: {}", e.message()));
    }
}

fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("mutekey");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let dpi = GetDpiForSystem() as i32;
        let px = |v: i32| v * dpi / 96;

        let controller = CreateWindowExW(
            WS_EX_TOOLWINDOW, class, w!("mutekey"), WS_POPUP,
            0, 0, 0, 0, None, None, instance, None,
        )?;
        // Click-through, never focused, absent from the taskbar and Alt+Tab.
        let badge = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            class, w!("mutekey badge"), WS_POPUP,
            0, 0, px(BADGE_WIDTH), px(BADGE_HEIGHT), controller, None, instance, None,
        )?;
        SetLayeredWindowAttributes(badge, COLORREF(0), BADGE_OPACITY, LWA_ALPHA)?;
        let pill = CreateRoundRectRgn(0, 0, px(BADGE_WIDTH) + 1, px(BADGE_HEIGHT) + 1, px(BADGE_HEIGHT), px(BADGE_HEIGHT));
        SetWindowRgn(badge, pill, TRUE);

        APP.set(Some(App {
            controller,
            badge,
            dpi,
            icon_live: dot_icon(GREEN)?,
            icon_muted: dot_icon(RED)?,
            icon_none: dot_icon(GRAY)?,
            text_font: font("Segoe UI Semibold", px(17)),
            glyph_font: font("Segoe Fluent Icons", px(18)),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        }));

        if RegisterHotKey(controller, HOTKEY_ID, HOTKEY_MODIFIERS | MOD_NOREPEAT, HOTKEY_KEY).is_err() {
            error_box(&format!(
                "{HOTKEY_LABEL} is already taken by another app (or mutekey is already running)."
            ));
            return Ok(());
        }

        let state = current_state();
        STATE.set(state);
        apply_state(state, NIM_ADD);
        // Polling picks up mute changes made elsewhere and microphones being plugged in.
        SetTimer(controller, POLL_TIMER_ID, POLL_INTERVAL_MS, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let Some(app) = APP.get() else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };
        match msg {
            WM_HOTKEY if wparam.0 == HOTKEY_ID as usize => toggle_mute(),
            WM_TIMER if wparam.0 == POLL_TIMER_ID => refresh(),
            WM_TRAY => match lparam.0 as u32 {
                WM_LBUTTONUP => toggle_mute(),
                WM_RBUTTONUP => show_menu(app),
                _ => {}
            },
            WM_PAINT if hwnd == app.badge => paint_badge(app),
            WM_DESTROY if hwnd == app.controller => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(app, MicState::NoMicrophone));
                PostQuitMessage(0);
            }
            // Explorer restarted: the tray icon is gone and has to be re-added.
            m if m == app.taskbar_created => apply_state(STATE.get(), NIM_ADD),
            _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
        }
        LRESULT(0)
    }
}

fn microphones() -> Result<Vec<IAudioEndpointVolume>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        (0..devices.GetCount()?)
            .map(|i| devices.Item(i)?.Activate(CLSCTX_ALL, None))
            .collect()
    }
}

/// Muted only when every active microphone is muted, so a live mic is never hidden.
fn state_of(mics: &[IAudioEndpointVolume]) -> MicState {
    if mics.is_empty() {
        return MicState::NoMicrophone;
    }
    let all_muted = mics
        .iter()
        .all(|m| unsafe { m.GetMute() }.is_ok_and(|muted| muted.as_bool()));
    if all_muted { MicState::Muted } else { MicState::Live }
}

fn current_state() -> MicState {
    state_of(&microphones().unwrap_or_default())
}

fn toggle_mute() {
    let mics = microphones().unwrap_or_default();
    let mute = state_of(&mics) == MicState::Live;
    for mic in &mics {
        unsafe {
            let _ = mic.SetMute(BOOL::from(mute), ptr::null());
        }
    }
    refresh();
}

fn refresh() {
    let state = current_state();
    if state != STATE.get() {
        STATE.set(state);
        apply_state(state, NIM_MODIFY);
    }
}

fn apply_state(state: MicState, tray_action: NOTIFY_ICON_MESSAGE) {
    let Some(app) = APP.get() else { return };
    unsafe {
        if state == MicState::Muted {
            // Recomputed on every show in case the resolution or taskbar changed.
            let mut work = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let width = app.px(BADGE_WIDTH);
            let x = work.left + (work.right - work.left - width) / 2;
            let y = work.top + app.px(BADGE_TOP_MARGIN);
            let _ = SetWindowPos(
                app.badge, HWND_TOPMOST, x, y, width, app.px(BADGE_HEIGHT),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        } else {
            let _ = ShowWindow(app.badge, SW_HIDE);
        }
        let _ = Shell_NotifyIconW(tray_action, &tray_data(app, state));
    }
}

fn tray_data(app: App, state: MicState) -> NOTIFYICONDATAW {
    let (icon, tip) = match state {
        MicState::Live => (app.icon_live, format!("Microphone live ({HOTKEY_LABEL} to mute)")),
        MicState::Muted => (app.icon_muted, format!("Microphone muted ({HOTKEY_LABEL} to unmute)")),
        MicState::NoMicrophone => (app.icon_none, "No microphone found".to_string()),
    };
    let mut data = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: app.controller,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (dst, src) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    data
}

unsafe fn show_menu(app: App) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let toggle_label = HSTRING::from(match STATE.get() {
        MicState::Muted => format!("Unmute\t{HOTKEY_LABEL}"),
        _ => format!("Mute\t{HOTKEY_LABEL}"),
    });
    let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE, &toggle_label);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("Exit"));

    // The foreground dance is what lets the menu close when you click elsewhere.
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let _ = SetForegroundWindow(app.controller);
    let choice = TrackPopupMenu(
        menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
        cursor.x, cursor.y, 0, app.controller, None,
    );
    let _ = PostMessageW(app.controller, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match choice.0 as usize {
        MENU_TOGGLE => toggle_mute(),
        MENU_EXIT => {
            let _ = DestroyWindow(app.controller);
        }
        _ => {}
    }
}

unsafe fn paint_badge(app: App) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(app.badge, &mut ps);
    let mut client = RECT::default();
    let _ = GetClientRect(app.badge, &mut client);

    let background = CreateSolidBrush(RED);
    FillRect(hdc, &client, background);
    let _ = DeleteObject(background);

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, WHITE);
    let flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;

    let mut glyph_rect = RECT {
        left: client.left + app.px(18),
        right: client.left + app.px(18 + 22),
        ..client
    };
    let previous = SelectObject(hdc, HGDIOBJ(app.glyph_font.0));
    DrawTextW(hdc, &mut [MIC_OFF_GLYPH], &mut glyph_rect, flags);

    let mut text_rect = RECT { left: glyph_rect.right + app.px(8), ..client };
    let mut text: Vec<u16> = BADGE_TEXT.encode_utf16().collect();
    SelectObject(hdc, HGDIOBJ(app.text_font.0));
    DrawTextW(hdc, &mut text, &mut text_rect, flags);

    SelectObject(hdc, previous);
    let _ = EndPaint(app.badge, &ps);
}

fn font(face: &str, height: i32) -> HFONT {
    let mut logfont = LOGFONTW {
        lfHeight: -height,
        lfQuality: ANTIALIASED_QUALITY,
        ..Default::default()
    };
    for (dst, src) in logfont.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    unsafe { CreateFontIndirectW(&logfont) }
}

/// A filled, anti-aliased circle sized for the notification area.
fn dot_icon(color: COLORREF) -> Result<HICON> {
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let color_bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);

        let (r, g, b) = (color.0 & 0xFF, (color.0 >> 8) & 0xFF, (color.0 >> 16) & 0xFF);
        let center = (size as f32 - 1.0) / 2.0;
        let radius = size as f32 * 0.4;
        for y in 0..size {
            for x in 0..size {
                let distance = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
                let alpha = ((radius + 0.5 - distance).clamp(0.0, 1.0) * 255.0) as u32;
                pixels[(y * size + x) as usize] = alpha << 24 | r << 16 | g << 8 | b;
            }
        }

        let mask_bits = vec![0u8; (((size + 15) / 16) * 2 * size) as usize];
        let mask_bitmap = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bitmap,
            hbmColor: color_bitmap,
        });
        let _ = DeleteObject(color_bitmap);
        let _ = DeleteObject(mask_bitmap);
        icon
    }
}

fn error_box(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("mutekey"), MB_OK | MB_ICONERROR);
    }
}
