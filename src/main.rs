//! mutekey: a global hotkey that mutes every microphone, an always-on-top badge
//! while muted, and a tray icon (red mic-off = muted). Left-click the tray icon to
//! toggle; right-click it for the shortcut, the badge's position and size, and startup.
//! Settings are kept in %APPDATA%\mutekey\settings.ini. Badge and tray follow the
//! Windows light/dark mode, styled after Fluent 2 (WinUI's InfoBar and flyouts).

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
const BADGE_RADIUS: i32 = 8; // Fluent's OverlayCornerRadius
const BADGE_PADDING: i32 = 14;
const BADGE_GAP: i32 = 10;
const GLYPH_SIZE: i32 = 16;
const TEXT_SIZE: i32 = 14; // Fluent "Body Strong": 14 px semibold
const CHIP_TEXT_SIZE: i32 = 12; // Fluent "Caption"
const CHIP_HEIGHT: i32 = 22;
const CHIP_PADDING: i32 = 7;
const CHIP_RADIUS: i32 = 4; // Fluent's ControlCornerRadius
const BADGE_SCREEN_MARGIN: i32 = 24; // from the work area's edges, i.e. above the taskbar
const SHADOW_BLUR: i32 = 16;
const SHADOW_OFFSET: i32 = 4;
const BADGE_TEXT: &str = "Muted";
const CAPTURE_TEXT: &str = "Press the new shortcut";
const CAPTURE_CHIP: &str = "Esc to cancel";
const PREVIEW_MS: u32 = 2500;
const CAPTURE_TIMEOUT_MS: u32 = 15_000;

// Segoe Fluent Icons glyphs (Segoe MDL2 Assets has the same ones on Windows 10).
const MIC_OFF_GLYPH: u16 = 0xEC54;
const KEYBOARD_GLYPH: u16 = 0xE765;

// Tray icon resources (resources.rc, drawn by icons/make_icons.py): live, muted, none.
const TRAY_ICONS_LIGHT: [u16; 3] = [101, 102, 103];
const TRAY_ICONS_DARK: [u16; 3] = [111, 112, 113];

/// Fluent 2 colors for one theme, from WinUI's theme resources. Translucent tokens are
/// (color, opacity) and get flattened onto whatever is under them.
struct Palette {
    critical_background: u32, // SystemFillColorCriticalBackground (InfoBar, error severity)
    neutral_background: u32,  // AcrylicBackgroundFillColorDefault, as a solid color
    critical: u32,            // SystemFillColorCritical
    text: (u32, f32),         // TextFillColorPrimary
    text_secondary: (u32, f32),
    stroke: (u32, f32),       // SurfaceStrokeColorFlyout
    chip_fill: (u32, f32),    // SubtleFillColorSecondary: a label, not a control
    chip_stroke: (u32, f32),  // ControlStrokeColorDefault
    shadow: f32,              // flyout shadow opacity
}

const LIGHT: Palette = Palette {
    critical_background: 0xFDE7E9,
    neutral_background: 0xF9F9F9,
    critical: 0xC42B1C,
    text: (0x000000, 0.894),
    text_secondary: (0x000000, 0.62),
    stroke: (0x000000, 0.058),
    chip_fill: (0x000000, 0.037),
    chip_stroke: (0x000000, 0.058),
    shadow: 0.14,
};

const DARK: Palette = Palette {
    critical_background: 0x442726,
    neutral_background: 0x2C2C2C,
    critical: 0xFF99A4,
    text: (0xFFFFFF, 1.0),
    text_secondary: (0xFFFFFF, 0.786),
    stroke: (0x000000, 0.2),
    chip_fill: (0xFFFFFF, 0.06),
    chip_stroke: (0xFFFFFF, 0.07),
    shadow: 0.3,
};

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
const MENU_COMPACT: usize = 5;
const MENU_POSITION: usize = 10; // + index into POSITIONS

// Marks keystrokes mutekey injects itself, so its own hook lets them through.
const INJECTED: usize = 0x6D75_7465;

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
    compact: bool, // the badge shows just the icon
    // Kept here (not only in the Run key) so it can default to on, also for settings files
    // written before the option existed.
    start_with_windows: bool,
}

#[derive(Clone, Copy)]
struct App {
    instance: HINSTANCE,
    controller: HWND, // hidden window that owns the timers and tray icon
    badge: HWND,
    light: bool,            // Windows is in light mode
    tray_icons: [HICON; 3], // live, muted, none, for that mode's taskbar
    text_face: &'static str,
    // GDI sees each weight of the variable font as its own family; asking "Segoe UI
    // Variable Text" for semibold would get a synthesized bold instead.
    strong_face: &'static str,
    icon_face: &'static str,
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
        apply_start_with_windows(settings.start_with_windows);

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

        let light = light_mode();
        APP.set(Some(App {
            instance,
            controller,
            badge,
            light,
            tray_icons: tray_icons(instance, light)?,
            text_face: installed_face("Segoe UI Variable Text", "Segoe UI"),
            strong_face: installed_face("Segoe UI Variable Text Semibold", "Segoe UI"),
            icon_face: installed_face("Segoe Fluent Icons", "Segoe MDL2 Assets"),
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
            // Resolution, scaling or taskbar changes move the work area the badge is placed in;
            // a light/dark switch arrives as WM_SETTINGCHANGE too.
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE if hwnd == app.controller => {
                if light_mode() != app.light {
                    if let Ok(icons) = tray_icons(app.instance, !app.light) {
                        for icon in app.tray_icons {
                            let _ = DestroyIcon(icon);
                        }
                        APP.set(Some(App { light: !app.light, tray_icons: icons, ..app }));
                        let _ = Shell_NotifyIconW(NIM_MODIFY, &tray_data(APP.get().unwrap(), STATE.get()));
                    }
                }
                update_badge();
            }
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
    const DEFAULT: Settings = Settings { hotkey: Hotkey::DEFAULT, position: Position::BottomCenter, compact: false, start_with_windows: true };

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
                "badge" => settings.compact = value.trim().eq_ignore_ascii_case("compact"),
                "start_with_windows" => settings.start_with_windows = !value.trim().eq_ignore_ascii_case("false"),
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
             ; badge: full (icon, text and shortcut) or compact (icon only)\r\n\
             hotkey={}\r\nposition={}\r\nbadge={}\r\nstart_with_windows={}\r\n",
            self.hotkey.label(),
            self.position.name(),
            if self.compact { "compact" } else { "full" },
            self.start_with_windows,
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
    let palette = if app.light { &LIGHT } else { &DARK };
    let settings = SETTINGS.get();
    unsafe {
        if CAPTURING.get() {
            let background = rgb(palette.neutral_background);
            let look = Look {
                background,
                glyph: KEYBOARD_GLYPH,
                glyph_color: over(background, palette.text),
                text: Some(CAPTURE_TEXT.to_string()),
                chip: Some(CAPTURE_CHIP.to_string()),
            };
            show_badge(app, palette, &look);
        } else if STATE.get() == MicState::Muted || PREVIEW.get() {
            let look = Look {
                background: rgb(palette.critical_background),
                glyph: MIC_OFF_GLYPH,
                glyph_color: rgb(palette.critical),
                text: (!settings.compact).then(|| BADGE_TEXT.to_string()),
                chip: (!settings.compact).then(|| settings.hotkey.label()),
            };
            show_badge(app, palette, &look);
        } else {
            let _ = ShowWindow(app.badge, SW_HIDE);
        }
    }
}

fn tray_data(app: App, state: MicState) -> NOTIFYICONDATAW {
    let shortcut = SETTINGS.get().hotkey.label();
    let [live, muted, none] = app.tray_icons;
    let (icon, tip) = match state {
        MicState::Live => (live, format!("Microphone live ({shortcut} to mute)")),
        MicState::Muted => (muted, format!("Microphone muted ({shortcut} to unmute)")),
        MicState::NoMicrophone => (none, "No microphone found".to_string()),
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
    let compact_flags = if settings.compact { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, compact_flags, MENU_COMPACT, w!("Compact badge"));
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
            change_badge(app, Settings { position: POSITIONS[id - MENU_POSITION].0, ..settings })
        }
        MENU_COMPACT => change_badge(app, Settings { compact: !settings.compact, ..settings }),
        MENU_AUTOSTART => {
            let enable = !starts_with_windows();
            set_start_with_windows(enable);
            SETTINGS.set(Settings { start_with_windows: enable, ..settings });
            SETTINGS.get().save();
        }
        MENU_EXIT => {
            set_mute(false);
            let _ = DestroyWindow(app.controller);
        }
        _ => {}
    }
}

/// Saves a new badge position or size and shows the badge there for a moment.
fn change_badge(app: App, settings: Settings) {
    SETTINGS.set(settings);
    settings.save();
    PREVIEW.set(true);
    unsafe {
        SetTimer(app.controller, PREVIEW_TIMER_ID, PREVIEW_MS, None);
    }
    update_badge();
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
    run_entry_is_current() && !disabled_in_task_manager()
}

fn run_entry_is_current() -> bool {
    let Some(command) = run_command() else { return false };
    let mut value = [0u16; 1024];
    let mut size = mem::size_of_val(&value) as u32;
    let found = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None,
            Some(value.as_mut_ptr() as *mut c_void), Some(&mut size),
        )
    } == ERROR_SUCCESS;
    let len = (size as usize / 2).saturating_sub(1); // bytes, including the terminating NUL
    found && String::from_utf16_lossy(&value[..len]).eq_ignore_ascii_case(&command.to_string())
}

/// Task Manager's Startup apps page keeps the Run entry and records the choice here: the
/// first byte is 2 when enabled and 3 when turned off.
fn disabled_in_task_manager() -> bool {
    let mut approval = [0u8; 12];
    let mut size = approval.len() as u32;
    let recorded = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, RUN_VALUE, RRF_RT_REG_BINARY, None,
            Some(approval.as_mut_ptr() as *mut c_void), Some(&mut size),
        )
    } == ERROR_SUCCESS;
    recorded && approval[0] & 1 == 1
}

/// At startup: make the Run entry match the setting (and this exe's current path), but leave
/// a Task Manager "disabled" alone; that was the user's choice too.
fn apply_start_with_windows(enable: bool) {
    if enable != run_entry_is_current() {
        write_run_entry(enable);
    }
}

/// From the menu: also clears Task Manager's record, which re-enables an entry turned off there.
fn set_start_with_windows(enable: bool) {
    unsafe {
        let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, RUN_VALUE);
    }
    write_run_entry(enable);
}

fn write_run_entry(enable: bool) {
    let result = unsafe {
        match run_command() {
            Some(command) if enable => RegSetKeyValueW(
                HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0,
                Some(command.as_ptr() as *const c_void), ((command.len() + 1) * 2) as u32,
            ),
            _ => RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE),
        }
    };
    if result != ERROR_SUCCESS && !(result == ERROR_FILE_NOT_FOUND && !enable) {
        error_box(&format!("Couldn't change the startup setting ({}).", result.0));
    }
}

/// What the badge shows; colors are already flattened onto `background`.
struct Look {
    background: [f32; 3],
    glyph: u16,
    glyph_color: [f32; 3],
    text: Option<String>, // None: compact, just the glyph
    chip: Option<String>, // a keyboard-key chip after the text, e.g. the shortcut
}

/// Renders the badge (Fluent InfoBar-like surface, glyph, text, chip, stroke and shadow)
/// into a per-pixel-alpha bitmap and shows it.
unsafe fn show_badge(app: App, palette: &Palette, look: &Look) {
    let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO { cbSize: mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(monitor, &mut info);
    let (mut dpi, mut dpi_y) = (96, 96);
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
    let px = |v: i32| v * dpi as i32 / 96;

    let dc = CreateCompatibleDC(None);
    let glyph_font = font(app.icon_face, px(GLYPH_SIZE), FW_NORMAL.0 as i32);
    let text_font = font(app.strong_face, px(TEXT_SIZE), FW_SEMIBOLD.0 as i32);
    let chip_font = font(app.text_face, px(CHIP_TEXT_SIZE), FW_NORMAL.0 as i32);
    let previous_font = SelectObject(dc, text_font);
    let measure = |font: HFONT, text: &str| {
        let text: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        SelectObject(dc, font);
        let _ = GetTextExtentPoint32W(dc, &text, &mut size);
        (text, size.cx)
    };
    let text = look.text.as_deref().map(|t| measure(text_font, t));
    let chip = look.chip.as_deref().map(|t| measure(chip_font, t));

    // Left to right: padding, glyph, [gap, text], [gap, chip], padding. A chip sits as far
    // from the right edge as from the top and bottom.
    let height = px(BADGE_HEIGHT);
    let glyph_box = px(GLYPH_SIZE + 4);
    let chip_width = chip.as_ref().map(|c| c.1 + 2 * px(CHIP_PADDING));
    let chip_inset = (height - px(CHIP_HEIGHT)) / 2;
    let mut width = match &text {
        None => height,
        Some((_, text_width)) => px(BADGE_PADDING) + glyph_box + px(BADGE_GAP) + text_width + px(BADGE_PADDING),
    };
    if let (Some(_), Some(chip_width)) = (&text, chip_width) {
        width += px(BADGE_GAP) + chip_width - px(BADGE_PADDING) + chip_inset;
    }
    let margin = px(SHADOW_BLUR) + px(SHADOW_OFFSET); // room for the shadow
    let (bitmap_width, bitmap_height) = (width + 2 * margin, height + 2 * margin);
    let Ok((bitmap, pixels)) = dib(bitmap_width, bitmap_height) else {
        SelectObject(dc, previous_font);
        for font in [glyph_font, text_font, chip_font] {
            let _ = DeleteObject(font);
        }
        let _ = DeleteDC(dc);
        return;
    };
    let previous_bitmap = SelectObject(dc, bitmap);
    let at = |x: i32, y: i32| (y * bitmap_width + x) as usize;

    // 1. The surface color everywhere (alpha comes last), with the chip painted on.
    pixels.fill(pack(look.background));
    let chip_rect = chip_width.map(|chip_width| {
        let right = margin + width - chip_inset;
        RECT { left: right - chip_width, top: margin + chip_inset, right, bottom: margin + height - chip_inset }
    });
    let chip_fill = over(look.background, palette.chip_fill);
    if let Some(r) = chip_rect.filter(|_| text.is_some()) {
        let (half_w, half_h) = ((r.right - r.left) as f32 / 2.0, (r.bottom - r.top) as f32 / 2.0);
        let (cx, cy) = (r.left as f32 + half_w, r.top as f32 + half_h);
        let radius = px(CHIP_RADIUS) as f32;
        for y in r.top - 1..r.bottom + 1 {
            for x in r.left - 1..r.right + 1 {
                let d = rounded_rect_distance(x as f32 + 0.5 - cx, y as f32 + 0.5 - cy, half_w, half_h, radius);
                let fill = (0.5 - d).clamp(0.0, 1.0);
                let color = mix(look.background, chip_fill, fill);
                let color = mix(color, rgb(palette.chip_stroke.0), palette.chip_stroke.1 * edge(d, px(1) as f32));
                pixels[at(x, y)] = pack(color);
            }
        }
    }

    // 2. Glyph, text and chip text, ClearType against the colors they sit on.
    SetBkMode(dc, TRANSPARENT);
    let flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    let glyph_left = if text.is_some() { margin + px(BADGE_PADDING) } else { margin + (height - glyph_box) / 2 };
    let mut rect = RECT { left: glyph_left, top: margin, right: glyph_left + glyph_box, bottom: margin + height };
    SelectObject(dc, glyph_font);
    SetTextColor(dc, colorref(look.glyph_color));
    DrawTextW(dc, &mut [look.glyph], &mut rect, flags | DT_CENTER);
    if let Some((mut text, _)) = text {
        let mut rect = RECT { left: rect.right + px(BADGE_GAP), top: margin, right: margin + width, bottom: margin + height };
        SelectObject(dc, text_font);
        SetTextColor(dc, colorref(over(look.background, palette.text)));
        DrawTextW(dc, &mut text, &mut rect, flags | DT_LEFT);
        if let (Some((mut chip, _)), Some(mut rect)) = (chip, chip_rect) {
            SelectObject(dc, chip_font);
            SetTextColor(dc, colorref(over(chip_fill, palette.text_secondary)));
            DrawTextW(dc, &mut chip, &mut rect, flags | DT_CENTER);
        }
    }
    let _ = GdiFlush();

    // 3. Anti-aliased shape with a hairline stroke, over a soft shadow, premultiplied as
    // UpdateLayeredWindow wants.
    let (half_w, half_h) = (width as f32 / 2.0, height as f32 / 2.0);
    let (center_x, center_y) = (bitmap_width as f32 / 2.0, bitmap_height as f32 / 2.0);
    let (blur, offset) = (px(SHADOW_BLUR) as f32, px(SHADOW_OFFSET) as f32);
    let radius = px(BADGE_RADIUS) as f32;
    let stroke = rgb(palette.stroke.0);
    for y in 0..bitmap_height {
        for x in 0..bitmap_width {
            let (fx, fy) = (x as f32 + 0.5 - center_x, y as f32 + 0.5 - center_y);
            let d = rounded_rect_distance(fx, fy, half_w, half_h, radius);
            let shape = (0.5 - d).clamp(0.0, 1.0);
            let shadow_distance = rounded_rect_distance(fx, fy - offset, half_w, half_h, radius).max(0.0) / blur;
            let shadow = palette.shadow * (1.0 - shadow_distance.min(1.0)).powi(2);
            let alpha = shape + shadow * (1.0 - shape);
            let pixel = &mut pixels[at(x, y)];
            let color = mix(unpack(*pixel), stroke, palette.stroke.1 * edge(d, px(1) as f32));
            *pixel = ((alpha * 255.0 + 0.5) as u32) << 24 | pack(color.map(|c| c * shape));
        }
    }

    let work = info.rcWork;
    let gap = px(BADGE_SCREEN_MARGIN) - margin; // window edge to work-area edge
    let position = SETTINGS.get().position;
    let x = match position {
        Position::TopLeft | Position::BottomLeft => work.left + gap,
        Position::TopCenter | Position::BottomCenter => work.left + (work.right - work.left - bitmap_width) / 2,
        Position::TopRight | Position::BottomRight => work.right - gap - bitmap_width,
    };
    let y = if position.is_top() { work.top + gap } else { work.bottom - gap - bitmap_height };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        app.badge, None, Some(&POINT { x, y }), Some(&SIZE { cx: bitmap_width, cy: bitmap_height }),
        dc, Some(&POINT::default()), COLORREF(0), Some(&blend), ULW_ALPHA,
    );
    let _ = SetWindowPos(app.badge, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);

    SelectObject(dc, previous_bitmap);
    SelectObject(dc, previous_font);
    let _ = DeleteObject(bitmap);
    for font in [glyph_font, text_font, chip_font] {
        let _ = DeleteObject(font);
    }
    let _ = DeleteDC(dc);
}

/// Signed distance from a point (relative to the center) to a rounded rectangle.
fn rounded_rect_distance(x: f32, y: f32, half_w: f32, half_h: f32, radius: f32) -> f32 {
    let qx = x.abs() - (half_w - radius);
    let qy = y.abs() - (half_h - radius);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

/// Coverage of a `width`-pixel line just inside a shape's edge, from its signed distance.
fn edge(distance: f32, width: f32) -> f32 {
    (0.5 - ((distance + width / 2.0).abs() - width / 2.0)).clamp(0.0, 1.0)
}

/// SystemUsesLightTheme: the mode of the taskbar and system flyouts, which the badge and
/// tray icon sit next to. Windows 11 defaults to light.
fn light_mode() -> bool {
    let mut value = 1u32;
    let mut size = mem::size_of::<u32>() as u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"), RRF_RT_REG_DWORD, None,
            Some(&mut value as *mut u32 as *mut c_void), Some(&mut size),
        );
    }
    value != 0
}

fn tray_icons(instance: HINSTANCE, light: bool) -> Result<[HICON; 3]> {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let [live, muted, none] = if light { TRAY_ICONS_LIGHT } else { TRAY_ICONS_DARK };
    Ok([load_icon(instance, live, size)?, load_icon(instance, muted, size)?, load_icon(instance, none, size)?])
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

// Colors as [r, g, b] in 0..=255, so they can be mixed.
fn rgb(hex: u32) -> [f32; 3] {
    unpack(hex)
}

fn unpack(pixel: u32) -> [f32; 3] {
    [(pixel >> 16 & 0xFF) as f32, (pixel >> 8 & 0xFF) as f32, (pixel & 0xFF) as f32]
}

fn pack(color: [f32; 3]) -> u32 {
    let [r, g, b] = color.map(|c| (c + 0.5).clamp(0.0, 255.0) as u32);
    r << 16 | g << 8 | b
}

fn colorref(color: [f32; 3]) -> COLORREF {
    let [r, g, b] = color.map(|c| (c + 0.5).clamp(0.0, 255.0) as u32);
    COLORREF(r | g << 8 | b << 16)
}

fn mix(base: [f32; 3], top: [f32; 3], amount: f32) -> [f32; 3] {
    [0, 1, 2].map(|i| base[i] + (top[i] - base[i]) * amount)
}

/// A translucent Fluent token over a solid color.
fn over(base: [f32; 3], token: (u32, f32)) -> [f32; 3] {
    mix(base, rgb(token.0), token.1)
}

/// `preferred` if it is installed (GDI substitutes silently otherwise), else `fallback`.
fn installed_face(preferred: &'static str, fallback: &'static str) -> &'static str {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let probe = font(preferred, 16, FW_NORMAL.0 as i32);
        let previous = SelectObject(dc, probe);
        let mut name = [0u16; 32];
        let len = GetTextFaceW(dc, Some(&mut name)).max(0) as usize;
        SelectObject(dc, previous);
        let _ = DeleteObject(probe);
        let _ = DeleteDC(dc);
        let found = String::from_utf16_lossy(&name[..len.min(32)]);
        if found.trim_end_matches('\0').eq_ignore_ascii_case(preferred) { preferred } else { fallback }
    }
}

fn font(face: &str, height: i32, weight: i32) -> HFONT {
    let mut logfont = LOGFONTW {
        lfHeight: -height,
        lfWeight: weight,
        // ClearType: Segoe UI's hinting turns grayscale smoothing off at badge sizes.
        // The badge is opaque wherever text is drawn, so it has a solid color to blend with.
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
