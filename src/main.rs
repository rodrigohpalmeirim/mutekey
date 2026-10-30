//! mutekey: a global hotkey that mutes every microphone, an always-on-top badge
//! while muted, and a tray icon (green = live, red = muted). Left-click the tray
//! icon to toggle; right-click it to change the shortcut or the badge position.
//! Settings are kept in %APPDATA%\mutekey\settings.ini.

#![windows_subsystem = "windows"]

use std::cell::Cell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::{fs, mem, ptr};

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eCapture, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Badge geometry in 96-DPI pixels; it is rendered for the primary screen's DPI.
const BADGE_HEIGHT: i32 = 40;
const BADGE_PADDING: i32 = 16;
const BADGE_GLYPH_SIZE: i32 = 18;
const BADGE_GAP: i32 = 10;
const BADGE_FONT_SIZE: i32 = 15;
const BADGE_SCREEN_MARGIN: i32 = 24; // from the work area's edges, i.e. above the taskbar
const SHADOW_BLUR: i32 = 12;
const SHADOW_OFFSET: i32 = 2;
const SHADOW_OPACITY: f32 = 0.3;
const BADGE_TEXT: &str = "Microphone muted";
const CAPTURE_TEXT: &str = "Press the new shortcut  \u{00B7}  Esc to cancel";
const PREVIEW_MS: u32 = 2500;
const CAPTURE_TIMEOUT_MS: u32 = 15_000;

// Icon resources (resources.rc, drawn by icons/make_icons.py).
const ICON_LIVE: u16 = 101;
const ICON_MUTED: u16 = 102;
const ICON_NONE: u16 = 103;
const GLYPH_MUTED: u16 = 104;
const GLYPH_KEYBOARD: u16 = 105;

const RED: COLORREF = rgb(196, 43, 28);
const DARK: COLORREF = rgb(44, 44, 44);
const WHITE: COLORREF = rgb(255, 255, 255);

const POLL_TIMER_ID: usize = 1;
const PREVIEW_TIMER_ID: usize = 2;
const CAPTURE_TIMER_ID: usize = 3;
const POLL_INTERVAL_MS: u32 = 500;
const TRAY_ID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const WM_HOTKEY_PRESSED: u32 = WM_APP + 2;
const WM_SHORTCUT_CAPTURED: u32 = WM_APP + 3;
const MENU_TOGGLE: usize = 1;
const MENU_EXIT: usize = 2;
const MENU_SHORTCUT: usize = 3;
const MENU_AUTOSTART: usize = 4;
const MENU_POSITION: usize = 10; // + index into POSITIONS

// Marks keystrokes mutekey injects itself, so its own hook lets them through.
const INJECTED: usize = 0x6D75_7465;

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

#[derive(Clone, Copy, PartialEq)]
enum MicState {
    Live,
    Muted,
    NoMicrophone,
}

#[derive(Clone, Copy, PartialEq)]
struct Hotkey {
    win: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Position {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

// (position, name in settings.ini, menu label)
const POSITIONS: [(Position, &str, &str); 6] = [
    (Position::TopLeft, "top-left", "Top left"),
    (Position::TopCenter, "top-center", "Top center"),
    (Position::TopRight, "top-right", "Top right"),
    (Position::BottomLeft, "bottom-left", "Bottom left"),
    (Position::BottomCenter, "bottom-center", "Bottom center"),
    (Position::BottomRight, "bottom-right", "Bottom right"),
];

#[derive(Clone, Copy)]
struct Settings {
    hotkey: Hotkey,
    position: Position,
}

#[derive(Clone, Copy)]
struct App {
    instance: HINSTANCE,
    controller: HWND, // hidden window that owns the timers and tray icon
    badge: HWND,
    icon_live: HICON,
    icon_muted: HICON,
    icon_none: HICON,
    taskbar_created: u32,
}

thread_local! {
    static APP: Cell<Option<App>> = const { Cell::new(None) };
    static STATE: Cell<MicState> = const { Cell::new(MicState::NoMicrophone) };
    static SETTINGS: Cell<Settings> = const { Cell::new(Settings::DEFAULT) };
    // "Change shortcut…" is waiting for the next key combination.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    // The badge is shown briefly after picking a new position, even when live.
    static PREVIEW: Cell<bool> = const { Cell::new(false) };
    // A key whose key-down was swallowed, so its repeats and key-up must be too.
    static SWALLOWED_KEY: Cell<Option<u32>> = const { Cell::new(None) };
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

        // A second copy would toggle the mute straight back on every press.
        let _single_instance = CreateMutexW(None, true, w!("Local\\mutekey"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            error_box("mutekey is already running.");
            return Ok(());
        }

        let settings = Settings::load();
        SETTINGS.set(settings);
        if Settings::path().is_some_and(|path| !path.exists()) {
            settings.save(); // so there is a file to find and edit
        }

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("mutekey");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let controller = CreateWindowExW(
            WS_EX_TOOLWINDOW, class, w!("mutekey"), WS_POPUP,
            0, 0, 0, 0, None, None, instance, None,
        )?;
        // Click-through, never focused, absent from the taskbar and Alt+Tab. Its pixels come
        // from UpdateLayeredWindow, which gives the pill smooth edges and a soft shadow.
        let badge = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            class, w!("mutekey badge"), WS_POPUP,
            0, 0, 0, 0, controller, None, instance, None,
        )?;

        let tray_size = GetSystemMetrics(SM_CXSMICON);
        APP.set(Some(App {
            instance,
            controller,
            badge,
            icon_live: load_icon(instance, ICON_LIVE, tray_size)?,
            icon_muted: load_icon(instance, ICON_MUTED, tray_size)?,
            icon_none: load_icon(instance, ICON_NONE, tray_size)?,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        }));

        // Windows 11 registers Win+Shift+A itself, so RegisterHotKey can't have it. Like
        // PowerToys, mutekey watches the keyboard with a low-level hook and swallows the keystroke.
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0)?;

        let state = current_state();
        STATE.set(state);
        apply_state(NIM_ADD);
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
            WM_HOTKEY_PRESSED => toggle_mute(),
            WM_SHORTCUT_CAPTURED => finish_capture(app, Hotkey::from_wparam(wparam)),
            WM_TIMER => match wparam.0 {
                POLL_TIMER_ID => refresh(),
                PREVIEW_TIMER_ID => {
                    let _ = KillTimer(app.controller, PREVIEW_TIMER_ID);
                    PREVIEW.set(false);
                    update_badge();
                }
                CAPTURE_TIMER_ID => {
                    CAPTURING.set(false);
                    finish_capture(app, None);
                }
                _ => {}
            },
            WM_TRAY => match lparam.0 as u32 {
                WM_LBUTTONUP => toggle_mute(),
                WM_RBUTTONUP => show_menu(app),
                _ => {}
            },
            // Resolution, scaling or taskbar changes move the work area the badge is placed in.
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE if hwnd == app.controller => update_badge(),
            WM_DESTROY if hwnd == app.controller => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(app, MicState::NoMicrophone));
                PostQuitMessage(0);
            }
            // Explorer restarted: the tray icon is gone and has to be re-added.
            m if m == app.taskbar_created => apply_state(NIM_ADD),
            _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
        }
        LRESULT(0)
    }
}

/// Runs on the UI thread for every keystroke system-wide, so it only posts messages: Windows
/// drops hooks that take too long, and the audio calls can be slow.
extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let event = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            if event.dwExtraInfo != INJECTED && swallow_key(event.vkCode, down) {
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Acts on the shortcut (or on a new one being captured); true means hide the keystroke.
fn swallow_key(key: u32, down: bool) -> bool {
    let swallowed = SWALLOWED_KEY.get() == Some(key);
    if !down {
        if swallowed {
            SWALLOWED_KEY.set(None);
        }
        return swallowed;
    }
    if swallowed || is_modifier(key) {
        return swallowed; // auto-repeat of a swallowed key, or a modifier on its way to a combination
    }
    let pressed = Hotkey::pressed(key);
    let (message, wparam) = if CAPTURING.get() {
        if key == VK_ESCAPE.0 as u32 {
            (WM_SHORTCUT_CAPTURED, WPARAM(0))
        } else if pressed.has_modifier() {
            (WM_SHORTCUT_CAPTURED, pressed.to_wparam())
        } else {
            return false; // plain typing keeps working while the prompt is up
        }
    } else if pressed == SETTINGS.get().hotkey {
        (WM_HOTKEY_PRESSED, WPARAM(0))
    } else {
        return false;
    };
    CAPTURING.set(false);
    SWALLOWED_KEY.set(Some(key));
    if pressed.win || pressed.alt {
        press_dummy_key();
    }
    if let Some(app) = APP.get() {
        unsafe {
            let _ = PostMessageW(app.controller, message, wparam, LPARAM(0));
        }
    }
    true
}

fn is_modifier(key: u32) -> bool {
    [VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
        .iter()
        .any(|vk| vk.0 as u32 == key)
}

/// Releasing Win or Alt with no other key seen since it went down opens the Start menu or the
/// focused app's menu bar. The shortcut's key was hidden, so send one that nothing listens to.
fn press_dummy_key() {
    let input = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xFF), dwFlags: flags, dwExtraInfo: INJECTED, ..Default::default() },
        },
    };
    unsafe {
        SendInput(&[input(KEYBD_EVENT_FLAGS(0)), input(KEYEVENTF_KEYUP)], mem::size_of::<INPUT>() as i32);
    }
}

impl Hotkey {
    // Same shortcut PowerToys' Video Conference Mute used for the microphone.
    const DEFAULT: Hotkey = Hotkey { win: true, ctrl: false, alt: false, shift: true, key: b'A' as u32 };

    /// `key` combined with the modifiers held right now.
    fn pressed(key: u32) -> Hotkey {
        let held = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0;
        Hotkey {
            win: held(VK_LWIN) || held(VK_RWIN),
            ctrl: held(VK_CONTROL),
            alt: held(VK_MENU),
            shift: held(VK_SHIFT),
            key,
        }
    }

    /// Shift alone doesn't count: Shift+key is just typing.
    fn has_modifier(self) -> bool {
        self.win || self.ctrl || self.alt
    }

    fn label(self) -> String {
        let modifiers = [(self.win, "Win"), (self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")];
        let mut parts: Vec<String> = modifiers.iter().filter(|m| m.0).map(|m| m.1.to_string()).collect();
        parts.push(key_name(self.key));
        parts.join("+")
    }

    fn parse(text: &str) -> Option<Hotkey> {
        let mut hotkey = Hotkey { win: false, ctrl: false, alt: false, shift: false, key: 0 };
        for part in text.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "win" => hotkey.win = true,
                "ctrl" | "control" => hotkey.ctrl = true,
                "alt" => hotkey.alt = true,
                "shift" => hotkey.shift = true,
                _ if hotkey.key == 0 => hotkey.key = key_from_name(part)?,
                _ => return None,
            }
        }
        (hotkey.key != 0 && hotkey.has_modifier()).then_some(hotkey)
    }

    fn to_wparam(self) -> WPARAM {
        let flags = (self.win as usize) << 16 | (self.ctrl as usize) << 17 | (self.alt as usize) << 18 | (self.shift as usize) << 19;
        WPARAM(self.key as usize | flags)
    }

    /// None for WPARAM(0): the capture was cancelled.
    fn from_wparam(wparam: WPARAM) -> Option<Hotkey> {
        let bit = |n: u32| wparam.0 >> n & 1 == 1;
        let key = (wparam.0 & 0xFFFF) as u32;
        (key != 0).then_some(Hotkey { win: bit(16), ctrl: bit(17), alt: bit(18), shift: bit(19), key })
    }
}

const NAMED_KEYS: [(VIRTUAL_KEY, &str); 17] = [
    (VK_SPACE, "Space"), (VK_RETURN, "Enter"), (VK_TAB, "Tab"), (VK_BACK, "Backspace"),
    (VK_PAUSE, "Pause"), (VK_SCROLL, "ScrollLock"), (VK_SNAPSHOT, "PrintScreen"),
    (VK_INSERT, "Insert"), (VK_DELETE, "Delete"), (VK_HOME, "Home"), (VK_END, "End"),
    (VK_PRIOR, "PageUp"), (VK_NEXT, "PageDown"), (VK_LEFT, "Left"), (VK_RIGHT, "Right"),
    (VK_UP, "Up"), (VK_DOWN, "Down"),
];

fn key_name(key: u32) -> String {
    if let Some((_, name)) = NAMED_KEYS.iter().find(|(vk, _)| vk.0 as u32 == key) {
        return name.to_string();
    }
    let hex = format!("0x{key:02X}");
    match key {
        0x70..=0x87 => format!("F{}", key - 0x6F),
        0x60..=0x69 => format!("Num{}", key - 0x60),
        // Letters, digits and punctuation: what the key types in the current layout.
        _ => match unsafe { MapVirtualKeyW(key, MAPVK_VK_TO_CHAR) } & 0xFFFF {
            0 => hex,
            ch => char::from_u32(ch).map_or(hex, |c| c.to_uppercase().to_string()),
        },
    }
}

fn key_from_name(name: &str) -> Option<u32> {
    if let Some((vk, _)) = NAMED_KEYS.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
        return Some(vk.0 as u32);
    }
    let lower = name.to_ascii_lowercase();
    let number = |prefix: &str| lower.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok());
    if let Some(n) = number("f").filter(|n| (1..=24).contains(n)) {
        return Some(0x6F + n);
    }
    if let Some(n) = number("num").filter(|n| *n <= 9) {
        return Some(0x60 + n);
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).ok();
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => {
            let scan = unsafe { VkKeyScanW(c as u16) };
            (scan != -1).then_some((scan & 0xFF) as u32)
        }
        _ => None,
    }
}

impl Position {
    fn name(self) -> &'static str {
        POSITIONS.iter().find(|p| p.0 == self).map_or("bottom-center", |p| p.1)
    }

    fn is_top(self) -> bool {
        matches!(self, Position::TopLeft | Position::TopCenter | Position::TopRight)
    }
}

impl Settings {
    const DEFAULT: Settings = Settings { hotkey: Hotkey::DEFAULT, position: Position::BottomCenter };

    fn path() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("APPDATA")?).join("mutekey").join("settings.ini"))
    }

    /// Missing or unrecognized values fall back to the defaults.
    fn load() -> Settings {
        let mut settings = Settings::DEFAULT;
        let text = Settings::path().and_then(|path| fs::read_to_string(path).ok()).unwrap_or_default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            match key.trim() {
                "hotkey" => settings.hotkey = Hotkey::parse(value).unwrap_or(settings.hotkey),
                "position" => {
                    if let Some(p) = POSITIONS.iter().find(|p| p.1 == value.trim()) {
                        settings.position = p.0;
                    }
                }
                _ => {}
            }
        }
        settings
    }

    fn save(self) {
        let Some(path) = Settings::path() else { return };
        let text = format!(
            "; mutekey settings. Also changeable from the tray menu; restart mutekey after editing.\r\n\
             ; position: top-left, top-center, top-right, bottom-left, bottom-center or bottom-right\r\n\
             hotkey={}\r\nposition={}\r\n",
            self.hotkey.label(),
            self.position.name(),
        );
        let written = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::write(&path, text));
        if let Err(e) = written {
            error_box(&format!("Couldn't save settings to {}: {e}", path.display()));
        }
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
    set_mute(current_state() == MicState::Live);
}

fn set_mute(mute: bool) {
    for mic in &microphones().unwrap_or_default() {
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
        apply_state(NIM_MODIFY);
    }
}

fn apply_state(tray_action: NOTIFY_ICON_MESSAGE) {
    let Some(app) = APP.get() else { return };
    unsafe {
        let _ = Shell_NotifyIconW(tray_action, &tray_data(app, STATE.get()));
    }
    update_badge();
}

fn update_badge() {
    let Some(app) = APP.get() else { return };
    unsafe {
        if CAPTURING.get() {
            show_badge(app, DARK, GLYPH_KEYBOARD, CAPTURE_TEXT);
        } else if STATE.get() == MicState::Muted || PREVIEW.get() {
            show_badge(app, RED, GLYPH_MUTED, BADGE_TEXT);
        } else {
            let _ = ShowWindow(app.badge, SW_HIDE);
        }
    }
}

fn tray_data(app: App, state: MicState) -> NOTIFYICONDATAW {
    let shortcut = SETTINGS.get().hotkey.label();
    let (icon, tip) = match state {
        MicState::Live => (app.icon_live, format!("Microphone live ({shortcut} to mute)")),
        MicState::Muted => (app.icon_muted, format!("Microphone muted ({shortcut} to unmute)")),
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
    let settings = SETTINGS.get();
    let (Ok(menu), Ok(positions)) = (CreatePopupMenu(), CreatePopupMenu()) else { return };
    for (i, (_, _, label)) in POSITIONS.iter().enumerate() {
        let _ = AppendMenuW(positions, MF_STRING, MENU_POSITION + i, &HSTRING::from(*label));
    }
    let current = POSITIONS.iter().position(|p| p.0 == settings.position).unwrap_or(0);
    let last = MENU_POSITION + POSITIONS.len() - 1;
    let _ = CheckMenuRadioItem(positions, MENU_POSITION as u32, last as u32, (MENU_POSITION + current) as u32, MF_BYCOMMAND.0);

    let toggle_label = HSTRING::from(match STATE.get() {
        MicState::Muted => format!("Unmute\t{}", settings.hotkey.label()),
        _ => format!("Mute\t{}", settings.hotkey.label()),
    });
    let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE, &toggle_label);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, MENU_SHORTCUT, w!("Change shortcut\u{2026}"));
    let _ = AppendMenuW(menu, MF_POPUP, positions.0 as usize, w!("Badge position"));
    let autostart_flags = if starts_with_windows() { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, autostart_flags, MENU_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    // Exiting unmutes, so mutekey never leaves a muted mic behind with no badge to show it.
    let exit_label = if STATE.get() == MicState::Muted { w!("Unmute and exit") } else { w!("Exit") };
    let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, exit_label);

    // The foreground dance is what lets the menu close when you click elsewhere.
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let _ = SetForegroundWindow(app.controller);
    let choice = TrackPopupMenu(
        menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
        cursor.x, cursor.y, 0, app.controller, None,
    );
    let _ = PostMessageW(app.controller, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu); // and the submenu with it

    match choice.0 as usize {
        MENU_TOGGLE => toggle_mute(),
        MENU_SHORTCUT => {
            CAPTURING.set(true);
            SetTimer(app.controller, CAPTURE_TIMER_ID, CAPTURE_TIMEOUT_MS, None);
            update_badge();
        }
        id if (MENU_POSITION..=last).contains(&id) => {
            SETTINGS.set(Settings { position: POSITIONS[id - MENU_POSITION].0, ..settings });
            SETTINGS.get().save();
            PREVIEW.set(true);
            SetTimer(app.controller, PREVIEW_TIMER_ID, PREVIEW_MS, None);
            update_badge();
        }
        MENU_AUTOSTART => set_start_with_windows(!starts_with_windows()),
        MENU_EXIT => {
            set_mute(false);
            let _ = DestroyWindow(app.controller);
        }
        _ => {}
    }
}

fn finish_capture(app: App, hotkey: Option<Hotkey>) {
    unsafe {
        let _ = KillTimer(app.controller, CAPTURE_TIMER_ID);
    }
    if let Some(hotkey) = hotkey {
        SETTINGS.set(Settings { hotkey, ..SETTINGS.get() });
        SETTINGS.get().save();
    }
    apply_state(NIM_MODIFY); // the tooltip names the shortcut
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
// Where Task Manager's Startup apps page records entries the user turned off.
const STARTUP_APPROVED_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
const RUN_VALUE: PCWSTR = w!("mutekey");

fn run_command() -> Option<HSTRING> {
    Some(HSTRING::from(format!("\"{}\"", std::env::current_exe().ok()?.display())))
}

/// On when the Run entry points at this copy of mutekey and Task Manager hasn't disabled it.
fn starts_with_windows() -> bool {
    let Some(command) = run_command() else { return false };
    unsafe {
        let mut value = [0u16; 1024];
        let mut size = mem::size_of_val(&value) as u32;
        let found = RegGetValueW(
            HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None,
            Some(value.as_mut_ptr() as *mut c_void), Some(&mut size),
        ) == ERROR_SUCCESS;
        let len = (size as usize / 2).saturating_sub(1); // bytes, including the terminating NUL
        if !found || !String::from_utf16_lossy(&value[..len]).eq_ignore_ascii_case(&command.to_string()) {
            return false;
        }
        // The first byte is 2 when enabled and 3 when turned off there.
        let mut approval = [0u8; 12];
        let mut size = approval.len() as u32;
        let recorded = RegGetValueW(
            HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, RUN_VALUE, RRF_RT_REG_BINARY, None,
            Some(approval.as_mut_ptr() as *mut c_void), Some(&mut size),
        ) == ERROR_SUCCESS;
        !recorded || approval[0] & 1 == 0
    }
}

fn set_start_with_windows(enable: bool) {
    unsafe {
        // Clearing Task Manager's record re-enables an entry the user turned off there.
        let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, RUN_VALUE);
        let result = match run_command() {
            Some(command) if enable => RegSetKeyValueW(
                HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0,
                Some(command.as_ptr() as *const c_void), ((command.len() + 1) * 2) as u32,
            ),
            _ => RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE),
        };
        if result != ERROR_SUCCESS && !(result == ERROR_FILE_NOT_FOUND && !enable) {
            error_box(&format!("Couldn't change the startup setting ({}).", result.0));
        }
    }
}

/// Renders the pill (glyph, text, soft shadow) into a per-pixel-alpha bitmap and shows it.
unsafe fn show_badge(app: App, color: COLORREF, glyph: u16, text: &str) {
    let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO { cbSize: mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(monitor, &mut info);
    let (mut dpi, mut dpi_y) = (96, 96);
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
    let px = |v: i32| v * dpi as i32 / 96;

    let glyph_size = px(BADGE_GLYPH_SIZE);
    let Ok(glyph) = load_icon(app.instance, glyph, glyph_size) else { return };
    let dc = CreateCompatibleDC(None);
    let text_font = font("Segoe UI", px(BADGE_FONT_SIZE), FW_SEMIBOLD.0 as i32);
    let mut text: Vec<u16> = text.encode_utf16().collect();
    let mut text_size = SIZE::default();
    let previous_font = SelectObject(dc, text_font);
    let _ = GetTextExtentPoint32W(dc, &text, &mut text_size);

    let pill_width = 2 * px(BADGE_PADDING) + glyph_size + px(BADGE_GAP) + text_size.cx;
    let pill_height = px(BADGE_HEIGHT);
    let margin = px(SHADOW_BLUR) + px(SHADOW_OFFSET); // room for the shadow
    let (width, height) = (pill_width + 2 * margin, pill_height + 2 * margin);
    let Ok((bitmap, pixels)) = dib(width, height) else {
        SelectObject(dc, previous_font);
        let _ = DeleteObject(text_font);
        let _ = DeleteDC(dc);
        let _ = DestroyIcon(glyph);
        return;
    };
    let previous_bitmap = SelectObject(dc, bitmap);

    // Opaque pill color everywhere, then the text in white; alpha is worked out below.
    let (r, g, b) = channels(color);
    pixels.fill(r << 16 | g << 8 | b);
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, WHITE);
    let left = margin + px(BADGE_PADDING);
    let _ = DrawIconEx(dc, left, margin + (pill_height - glyph_size) / 2, glyph, glyph_size, glyph_size, 0, None, DI_NORMAL);
    let mut rect = RECT { left: left + glyph_size + px(BADGE_GAP), top: margin, right: width, bottom: margin + pill_height };
    DrawTextW(dc, &mut text, &mut rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    let _ = GdiFlush();

    // Anti-aliased pill over a blurred shadow, premultiplied as UpdateLayeredWindow wants.
    let (half_w, half_h) = (pill_width as f32 / 2.0, pill_height as f32 / 2.0);
    let (center_x, center_y) = (width as f32 / 2.0, height as f32 / 2.0);
    let (blur, offset) = (px(SHADOW_BLUR) as f32, px(SHADOW_OFFSET) as f32);
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (x as f32 + 0.5 - center_x, y as f32 + 0.5 - center_y);
            let pill = (0.5 - pill_distance(fx, fy, half_w, half_h)).clamp(0.0, 1.0);
            let shadow_distance = pill_distance(fx, fy - offset, half_w, half_h).max(0.0) / blur;
            let shadow = SHADOW_OPACITY * (1.0 - shadow_distance.min(1.0)).powi(2);
            let alpha = pill + shadow * (1.0 - pill);
            let pixel = &mut pixels[(y * width + x) as usize];
            let scale = |c: u32| ((c & 0xFF) as f32 * pill + 0.5) as u32;
            *pixel = ((alpha * 255.0 + 0.5) as u32) << 24 | scale(*pixel >> 16) << 16 | scale(*pixel >> 8) << 8 | scale(*pixel);
        }
    }

    let work = info.rcWork;
    let edge = px(BADGE_SCREEN_MARGIN) - margin; // window edge to work-area edge
    let position = SETTINGS.get().position;
    let x = match position {
        Position::TopLeft | Position::BottomLeft => work.left + edge,
        Position::TopCenter | Position::BottomCenter => work.left + (work.right - work.left - width) / 2,
        Position::TopRight | Position::BottomRight => work.right - edge - width,
    };
    let y = if position.is_top() { work.top + edge } else { work.bottom - edge - height };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        app.badge, None, Some(&POINT { x, y }), Some(&SIZE { cx: width, cy: height }),
        dc, Some(&POINT::default()), COLORREF(0), Some(&blend), ULW_ALPHA,
    );
    let _ = SetWindowPos(app.badge, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);

    SelectObject(dc, previous_bitmap);
    SelectObject(dc, previous_font);
    let _ = DeleteObject(bitmap);
    let _ = DeleteObject(text_font);
    let _ = DeleteDC(dc);
    let _ = DestroyIcon(glyph);
}

/// Signed distance from a point (relative to the center) to a pill with the given half-extents.
fn pill_distance(x: f32, y: f32, half_w: f32, half_h: f32) -> f32 {
    let radius = half_h.min(half_w);
    let qx = x.abs() - (half_w - radius);
    let qy = y.abs() - (half_h - radius);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

/// One of the icons in resources.rc, at the given size (each .ico holds several).
fn load_icon(instance: HINSTANCE, id: u16, size: i32) -> Result<HICON> {
    unsafe {
        let handle = LoadImageW(instance, PCWSTR(id as usize as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)?;
        Ok(HICON(handle.0))
    }
}

/// A top-down 32-bit DIB and its pixels (0xAARRGGBB).
unsafe fn dib(width: i32, height: i32) -> Result<(HBITMAP, &'static mut [u32])> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = ptr::null_mut();
    let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    Ok((bitmap, std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize)))
}

fn channels(color: COLORREF) -> (u32, u32, u32) {
    (color.0 & 0xFF, (color.0 >> 8) & 0xFF, (color.0 >> 16) & 0xFF)
}

fn font(face: &str, height: i32, weight: i32) -> HFONT {
    let mut logfont = LOGFONTW {
        lfHeight: -height,
        lfWeight: weight,
        // ClearType: Segoe UI's hinting turns grayscale smoothing off at badge sizes.
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    for (dst, src) in logfont.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    unsafe { CreateFontIndirectW(&logfont) }
}

fn error_box(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("mutekey"), MB_OK | MB_ICONERROR);
    }
}
