//! The notification area icon: how a Windows user runs the host.
//!
//! The UI thread owns a hidden window, the icon and the menu; the agent runs
//! on a Tokio runtime and reports back by queueing [`Event`]s and posting
//! [`WM_UPDATE`]. Everything the menu changes is saved to `config.json` and
//! sent to the agent, so it holds from the next session on.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT,
    WPARAM,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP,
    NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT,
    NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::system::input_permitted;
use crate::agent::{self, Command, Status};
use crate::config::{Config, DeviceKey};

const WM_TRAY: u32 = WM_APP + 1;
const WM_UPDATE: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;
/// `NIN_SELECT | NINF_KEY`: the icon chosen with the keyboard.
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Waystone");

/// Menu command ids.
mod item {
    pub const END_SESSION: usize = 10;
    pub const PAIR: usize = 11;
    pub const LOCK: usize = 20;
    pub const MUTE: usize = 21;
    pub const AUTOSTART: usize = 22;
    pub const ELEVATE: usize = 30;
    pub const LOG: usize = 31;
    pub const QUIT: usize = 40;
}

/// What other threads tell the UI thread.
enum Event {
    Status(Status),
    Paired(Config),
    Notice {
        title: String,
        text: String,
        warning: bool,
    },
}

/// Hands events to the UI thread from anywhere.
#[derive(Clone)]
struct Mailbox {
    queue: Arc<Mutex<Vec<Event>>>,
    /// The window handle as an integer: `HWND` is not `Send`.
    window: isize,
}

impl Mailbox {
    fn post(&self, event: Event) {
        self.queue.lock().unwrap().push(event);
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut _)),
                WM_UPDATE,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

struct App {
    window: HWND,
    dir: PathBuf,
    runtime: Runtime,
    mailbox: Mailbox,
    config: Option<Config>,
    agent: Option<(JoinHandle<()>, UnboundedSender<Command>)>,
    status: Status,
    icons: Icons,
    taskbar_created: u32,
    single_instance: Option<HANDLE>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub fn run(dir: &Path) -> Result<()> {
    let Some(single_instance) = claim_single_instance() else {
        message_box("Waystone 已经在运行，图标在任务栏右下角的通知区域。");
        return Ok(());
    };
    init_logging(dir);
    // No console to print to, and a panic in the window procedure aborts:
    // the log is the only place that can say what went wrong.
    std::panic::set_hook(Box::new(|panic| tracing::error!("{panic}")));
    info!("tray starting, state in {}", dir.display());

    let window = create_window()?;
    let mailbox = Mailbox {
        queue: Arc::new(Mutex::new(Vec::new())),
        window: window.0 as isize,
    };
    let config = Config::load(&dir.join("config.json")).ok();
    let app = App {
        window,
        dir: dir.to_path_buf(),
        runtime: Runtime::new().context("starting the runtime")?,
        mailbox,
        config,
        agent: None,
        status: Status::Connecting,
        icons: Icons::load(),
        taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        single_instance: Some(single_instance),
    };
    APP.with(|cell| *cell.borrow_mut() = Some(app));
    with_app(|app| {
        app.add_icon();
        if app.config.is_some() {
            app.start_agent();
        } else {
            app.status = Status::Offline {
                reason: "还没有配对".into(),
            };
            app.refresh();
            app.notify(
                "Waystone 还没有配对",
                "在服务端面板里为这台电脑生成配对链接并复制，然后在托盘菜单选「从剪贴板配对」。",
                false,
            );
        }
    });

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    APP.with(|cell| {
        if let Some(app) = cell.borrow_mut().take() {
            app.shutdown();
        }
    });
    Ok(())
}

/// Runs `f` on the app. `None` while the app is already borrowed: menus,
/// UAC and shell calls run nested message loops that re-enter the window
/// procedure, and a panic there would abort the process.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

impl App {
    fn start_agent(&mut self) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let key = match DeviceKey::load_or_create(&self.dir.join("device.key")) {
            Ok(key) => Arc::new(key),
            Err(error) => {
                warn!("device key: {error:#}");
                return;
            }
        };
        if let Some((task, _)) = self.agent.take() {
            task.abort();
        }
        let (commands, receiver) = unbounded_channel();
        let mailbox = self.mailbox.clone();
        let report: agent::Reporter = Arc::new(move |status| mailbox.post(Event::Status(status)));
        let mailbox = self.mailbox.clone();
        let task = self.runtime.spawn(async move {
            if let Err(error) = agent::run(config, key, report, receiver).await {
                warn!("agent stopped: {error:#}");
                mailbox.post(Event::Status(Status::Offline {
                    reason: format!("{error:#}"),
                }));
            }
        });
        self.agent = Some((task, commands));
    }

    fn send(&self, command: Command) {
        if let Some((_, commands)) = &self.agent {
            let _ = commands.send(command);
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Status(status) => {
                if let Status::Streaming { controller } = &status {
                    if !matches!(self.status, Status::Streaming { .. }) {
                        self.notify(
                            "Waystone 串流已开始",
                            &format!("{controller} 正在查看并控制这台电脑。"),
                            false,
                        );
                    }
                }
                self.status = status;
                self.refresh();
            }
            Event::Paired(config) => {
                self.notify(
                    "配对成功",
                    &format!("这台电脑以「{}」加入了设备池。", config.name),
                    false,
                );
                self.config = Some(config);
                self.start_agent();
            }
            Event::Notice {
                title,
                text,
                warning,
            } => self.notify(&title, &text, warning),
        }
    }

    fn command(&mut self, id: usize) {
        match id {
            item::END_SESSION => self.send(Command::EndSession),
            item::PAIR => self.pair_from_clipboard(),
            item::LOCK | item::MUTE => {
                let Some(config) = self.config.as_mut() else {
                    return;
                };
                if id == item::LOCK {
                    config.lock_on_disconnect = !config.lock_on_disconnect;
                } else {
                    config.mute_speakers = !config.mute_speakers;
                }
                if let Err(error) = config.save(&self.dir.join("config.json")) {
                    warn!("saving settings: {error:#}");
                }
                let command = Command::Settings {
                    lock_on_disconnect: config.lock_on_disconnect,
                    mute_speakers: config.mute_speakers,
                };
                self.send(command);
            }
            item::AUTOSTART => {
                if let Err(error) = set_autostart(!autostart_enabled()) {
                    self.notify("无法修改开机自启", &format!("{error:#}"), true);
                }
            }
            item::ELEVATE => self.restart_elevated(),
            item::LOG => {
                let log = HSTRING::from(self.dir.join("host.log").as_os_str());
                unsafe { ShellExecuteW(None, w!("open"), &log, None, None, SW_SHOWNORMAL) };
            }
            item::QUIT => unsafe { PostQuitMessage(0) },
            _ => {}
        }
    }

    fn pair_from_clipboard(&mut self) {
        let link = clipboard_text().unwrap_or_default();
        let link = link.trim().to_string();
        if !link.starts_with("waystone://pair") {
            self.notify(
                "剪贴板里没有配对链接",
                "在服务端面板为这台电脑（被控端）生成配对链接，复制后再试。",
                true,
            );
            return;
        }
        let name = self
            .config
            .as_ref()
            .map(|config| config.name.clone())
            .unwrap_or_else(Config::default_name);
        let (dir, mailbox) = (self.dir.clone(), self.mailbox.clone());
        self.runtime.spawn(async move {
            match agent::pair(&dir, &link, name).await {
                Ok(config) => mailbox.post(Event::Paired(config)),
                Err(error) => mailbox.post(Event::Notice {
                    title: "配对失败".into(),
                    text: format!("{error:#}"),
                    warning: true,
                }),
            }
        });
    }

    /// Hands over to an elevated copy, which can control elevated windows.
    fn restart_elevated(&mut self) {
        // The new instance must be able to claim the single-instance mutex.
        if let Some(mutex) = self.single_instance.take() {
            let _ = unsafe { CloseHandle(mutex) };
        }
        let exe = HSTRING::from(current_exe().as_os_str());
        let result =
            unsafe { ShellExecuteW(None, w!("runas"), &exe, w!("tray"), None, SW_SHOWNORMAL) };
        if result.0 as isize > 32 {
            unsafe { PostQuitMessage(0) };
        } else {
            // Declined at the UAC prompt.
            self.single_instance = claim_single_instance();
        }
    }

    fn shutdown(mut self) {
        // End a running session properly (input released, speakers back)
        // before the runtime goes away.
        if let Some((task, commands)) = self.agent.take() {
            let _ = commands.send(Command::EndSession);
            self.runtime.block_on(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
            });
            task.abort();
        }
        self.remove_icon();
        self.runtime.shutdown_timeout(Duration::from_secs(2));
        if let Some(mutex) = self.single_instance.take() {
            let _ = unsafe { CloseHandle(mutex) };
        }
        info!("tray stopped");
    }

    // MARK: icon and menu

    fn icon_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    fn add_icon(&self) {
        let mut data = self.icon_data();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icons.for_status(&self.status);
        copy_wide(&mut data.szTip, &self.tooltip());
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    fn remove_icon(&self) {
        let data = self.icon_data();
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }

    fn refresh(&self) {
        let mut data = self.icon_data();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.hIcon = self.icons.for_status(&self.status);
        copy_wide(&mut data.szTip, &self.tooltip());
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    fn notify(&self, title: &str, text: &str, warning: bool) {
        let mut data = self.icon_data();
        data.uFlags = NIF_INFO;
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, text);
        data.dwInfoFlags =
            if warning { NIIF_WARNING } else { NIIF_INFO } | NOTIFY_ICON_INFOTIP_FLAGS(0);
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    fn status_line(&self) -> String {
        match &self.status {
            Status::Connecting => "正在连接服务器…".into(),
            Status::Online => "在线，等待手机连接".into(),
            Status::Streaming { controller } => format!("{controller} 正在串流"),
            Status::Offline { reason } => format!("离线：{}", short(reason, 60)),
        }
    }

    fn tooltip(&self) -> String {
        let name = self
            .config
            .as_ref()
            .map(|c| c.name.as_str())
            .unwrap_or("未配对");
        format!("Waystone · {name}\n{}", self.status_line())
    }

    /// The menu for the current state; the caller tracks and destroys it.
    fn build_menu(&self) -> Option<HMENU> {
        let streaming = matches!(self.status, Status::Streaming { .. });
        let paired = self.config.is_some();
        let (lock, mute) = self
            .config
            .as_ref()
            .map(|config| (config.lock_on_disconnect, config.mute_speakers))
            .unwrap_or((true, true));
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let header = HSTRING::from(self.status_line());
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &header);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(
                menu,
                MF_STRING | if streaming { MF_ENABLED } else { MF_GRAYED },
                item::END_SESSION,
                w!("断开当前串流"),
            );
            let pair = if paired {
                w!("重新配对（读取剪贴板里的链接）")
            } else {
                w!("从剪贴板配对")
            };
            let _ = AppendMenuW(menu, MF_STRING, item::PAIR, pair);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let toggle = |on: bool| MF_STRING | if on { MF_CHECKED } else { MF_UNCHECKED };
            let settings = if paired { MF_ENABLED } else { MF_GRAYED };
            let _ = AppendMenuW(
                menu,
                toggle(lock) | settings,
                item::LOCK,
                w!("断开后锁定电脑"),
            );
            let _ = AppendMenuW(
                menu,
                toggle(mute) | settings,
                item::MUTE,
                w!("串流时静音本机扬声器"),
            );
            let _ = AppendMenuW(
                menu,
                toggle(autostart_enabled()),
                item::AUTOSTART,
                w!("登录 Windows 时自动启动"),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            if !input_permitted(false) {
                let _ = AppendMenuW(
                    menu,
                    MF_STRING,
                    item::ELEVATE,
                    w!("以管理员身份重新启动（控制管理员窗口）"),
                );
            }
            let _ = AppendMenuW(menu, MF_STRING, item::LOG, w!("打开日志"));
            let _ = AppendMenuW(menu, MF_STRING, item::QUIT, w!("退出"));
            Some(menu)
        }
    }
}

/// Shows the menu at `x, y` and runs the chosen command. Must not be called
/// with the app borrowed: the menu's own message loop re-enters the window
/// procedure, which needs the app for status updates.
fn show_menu(window: HWND, x: i32, y: i32) {
    let Some(menu) = with_app(|app| app.build_menu()).flatten() else {
        return;
    };
    let chosen = unsafe {
        // Without this the menu does not close when clicking elsewhere.
        let _ = SetForegroundWindow(window);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            x,
            y,
            None,
            window,
            None,
        );
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen.0 as usize
    };
    if chosen != 0 {
        with_app(|app| app.command(chosen));
    }
    // Status changes that arrived while the menu was open.
    let _ = unsafe { PostMessageW(Some(window), WM_UPDATE, WPARAM(0), LPARAM(0)) };
}

extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam,
            // the anchor point in wparam.
            let event = (lparam.0 as u32) & 0xffff;
            if event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT {
                let mut point = POINT {
                    x: (wparam.0 & 0xffff) as i16 as i32,
                    y: ((wparam.0 >> 16) & 0xffff) as i16 as i32,
                };
                if event != WM_CONTEXTMENU {
                    let _ = unsafe { GetCursorPos(&mut point) };
                }
                show_menu(window, point.x, point.y);
            }
            LRESULT(0)
        }
        WM_UPDATE => {
            with_app(|app| {
                let events = std::mem::take(&mut *app.mailbox.queue.lock().unwrap());
                for event in events {
                    app.handle(event);
                }
            });
            LRESULT(0)
        }
        _ => {
            // Explorer restarted: the notification area forgot every icon.
            let restarted = with_app(|app| {
                if message == app.taskbar_created && app.taskbar_created != 0 {
                    app.add_icon();
                    true
                } else {
                    false
                }
            });
            if restarted == Some(true) {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
    }
}

fn create_window() -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("WaystoneTray"),
            ..Default::default()
        };
        RegisterClassW(&class);
        // A hidden top-level window rather than a message-only one: only
        // top-level windows receive the TaskbarCreated broadcast.
        Ok(CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("WaystoneTray"),
            w!("Waystone"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?)
    }
}

/// Holds a named mutex for the life of the tray; `None` if another
/// instance already does.
fn claim_single_instance() -> Option<HANDLE> {
    let mutex = unsafe { CreateMutexW(None, true, w!("Local\\WaystoneHostTray")) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let _ = unsafe { CloseHandle(mutex) };
        return None;
    }
    Some(mutex)
}

/// Logs to `host.log` in the state directory, keeping the previous run's
/// log as `host.old.log`.
fn init_logging(dir: &Path) {
    let path = dir.join("host.log");
    let _ = std::fs::rename(&path, dir.join("host.old.log"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,waystone_rtc=info".into()),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
}

fn message_box(text: &str) {
    let text = HSTRING::from(text);
    unsafe { MessageBoxW(None, &text, w!("Waystone"), MB_OK | MB_ICONINFORMATION) };
}

fn current_exe() -> PathBuf {
    let mut buffer = vec![0u16; 1024];
    let length = unsafe { GetModuleFileNameW(None, &mut buffer) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buffer[..length]))
}

fn clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(u32::from(CF_UNICODETEXT.0))
            .ok()
            .and_then(|handle| {
                let memory = HGLOBAL(handle.0);
                let data = GlobalLock(memory) as *const u16;
                if data.is_null() {
                    return None;
                }
                let mut length = 0;
                while *data.add(length) != 0 {
                    length += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(data, length));
                let _ = GlobalUnlock(memory);
                Some(text)
            });
        let _ = CloseClipboard();
        text
    }
}

fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

fn set_autostart(enabled: bool) -> Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key)
            .ok()
            .context("opening the Run key")?;
        let result = if enabled {
            let command = format!("\"{}\" tray", current_exe().display());
            let mut bytes: Vec<u8> = command
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            bytes.shrink_to_fit();
            RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(&bytes))
        } else {
            RegDeleteValueW(key, RUN_VALUE)
        };
        let _ = RegCloseKey(key);
        result.ok().context("writing the Run key")?;
    }
    info!("start at login {}", if enabled { "on" } else { "off" });
    Ok(())
}

fn copy_wide<const N: usize>(target: &mut [u16; N], text: &str) {
    let units: Vec<u16> = text.encode_utf16().take(N - 1).collect();
    target[..units.len()].copy_from_slice(&units);
    target[units.len()] = 0;
}

fn short(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit - 1).collect();
    cut.push('…');
    cut
}

/// The tray icon in three states, at the size the shell wants.
struct Icons {
    online: HICON,
    streaming: HICON,
    offline: HICON,
}

const SIZES: [(u32, &[u8]); 7] = [
    (16, include_bytes!("icons/tray-16.rgba")),
    (20, include_bytes!("icons/tray-20.rgba")),
    (24, include_bytes!("icons/tray-24.rgba")),
    (32, include_bytes!("icons/tray-32.rgba")),
    (40, include_bytes!("icons/tray-40.rgba")),
    (48, include_bytes!("icons/tray-48.rgba")),
    (64, include_bytes!("icons/tray-64.rgba")),
];

impl Icons {
    fn load() -> Icons {
        let wanted =
            unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16) as u32;
        let (size, pixels) = SIZES
            .iter()
            .find(|(size, _)| *size >= wanted)
            .copied()
            .unwrap_or(SIZES[SIZES.len() - 1]);
        Icons {
            online: make_icon(size, pixels.to_vec()),
            streaming: make_icon(size, with_dot(size, pixels)),
            offline: make_icon(size, dimmed(pixels)),
        }
    }

    fn for_status(&self, status: &Status) -> HICON {
        match status {
            Status::Online => self.online,
            Status::Streaming { .. } => self.streaming,
            Status::Connecting | Status::Offline { .. } => self.offline,
        }
    }
}

/// Grey and half transparent: not reachable right now.
fn dimmed(pixels: &[u8]) -> Vec<u8> {
    pixels
        .chunks_exact(4)
        .flat_map(|p| {
            let grey = ((u32::from(p[0]) * 3 + u32::from(p[1]) * 6 + u32::from(p[2])) / 10) as u8;
            [grey, grey, grey, p[3] / 2]
        })
        .collect()
}

/// A green dot in the lower right corner: a phone is streaming.
fn with_dot(size: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = pixels.to_vec();
    let radius = size as f32 * 0.22;
    let centre = size as f32 - radius - 0.5;
    for y in 0..size {
        for x in 0..size {
            let distance = ((x as f32 - centre).powi(2) + (y as f32 - centre).powi(2)).sqrt();
            let index = ((y * size + x) * 4) as usize;
            if distance <= radius {
                out[index..index + 4].copy_from_slice(&[0x34, 0xC7, 0x59, 0xFF]);
            } else if distance <= radius + 1.0 {
                // A dark ring keeps the dot apart from the stone.
                out[index..index + 4].copy_from_slice(&[0x16, 0x20, 0x2B, 0xFF]);
            }
        }
    }
    out
}

/// An icon from straight RGBA pixels, top row first.
fn make_icon(size: u32, rgba: Vec<u8>) -> HICON {
    let bgra: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    // The AND mask is ignored for 32 bpp icons with alpha, but must exist:
    // one bit per pixel, rows padded to 16 bits.
    let row = (size as usize).div_ceil(16) * 2;
    let mask = vec![0u8; row * size as usize];
    unsafe {
        CreateIcon(
            None,
            size as i32,
            size as i32,
            1,
            32,
            mask.as_ptr(),
            bgra.as_ptr(),
        )
        .unwrap_or_default()
    }
}
