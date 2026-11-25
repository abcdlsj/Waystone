//! Native Win32 Setup Wizard UI (`ComCtl32 v6`, Per-Monitor DPI scaled).
//!
//! Presents a classic, crisp 3-step Windows installer flow:
//! 1. **Configure** (or **Uninstall Confirm** when launched with `--uninstall`)
//! 2. **Progress** (background worker thread streaming step updates + log)
//! 3. **Finish** (summary + optional one-click launch of the tray icon)

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetStockObject,
    InvalidateRect, SetBkColor, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_BOLD, FW_NORMAL, HBRUSH, HDC, HFONT,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    PBM_SETPOS, PBM_SETRANGE32, PROGRESS_CLASSW,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::model::{
    validate_pairing_uri, InstallOptions, InstallSummary, LaunchArgs, HOST_EXE_NAME, VERSION,
};
use crate::ops::{self, ExistingState, Progress};

const WM_WORKER: u32 = WM_APP + 10;
const BASE_W: i32 = 540;
const BASE_H: i32 = 430;
const HEADER_H: i32 = 74;
const FOOTER_H: i32 = 56;

mod id {
    pub const EDIT_DIR: usize = 101;
    pub const BTN_BROWSE: usize = 102;
    pub const CHK_SERVICE: usize = 103;
    pub const CHK_AUTOSTART: usize = 104;
    pub const CHK_SHORTCUTS: usize = 105;
    pub const EDIT_PAIR: usize = 106;
    pub const CHK_REMOVE_DATA: usize = 107;
    pub const CHK_LAUNCH_TRAY: usize = 108;

    pub const BTN_PRIMARY: usize = 201;
    pub const BTN_SECONDARY: usize = 202;
    pub const BTN_UNINSTALL_SWITCH: usize = 203;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Configure,
    UninstallConfirm,
    Working { uninstall: bool },
    Done { success: bool, uninstall: bool },
}

enum WorkerEvent {
    Progress(Progress),
    InstallDone(Result<InstallSummary>),
    UninstallDone(Result<()>),
}

#[derive(Clone)]
struct Mailbox {
    queue: Arc<Mutex<Vec<WorkerEvent>>>,
    hwnd: isize,
}

impl Mailbox {
    fn push(&self, event: WorkerEvent) {
        self.queue.lock().unwrap().push(event);
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_WORKER,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

struct Wizard {
    hwnd: HWND,
    instance: HINSTANCE,
    dpi: i32,
    font_title: HFONT,
    font_body: HFONT,
    font_mono: HFONT,
    bg_brush: HBRUSH,
    white_brush: HBRUSH,
    icon_big: Option<HICON>,
    mailbox: Mailbox,
    existing: ExistingState,
    page: Page,
    install_dir: PathBuf,
    install_service: bool,
    autostart_tray: bool,
    create_shortcuts: bool,
    pair_text: String,
    remove_data: bool,
    launch_tray: bool,
    // Active page controls
    controls: Vec<HWND>,
    header_title: Option<HWND>,
    header_sub: Option<HWND>,
    progress_bar: Option<HWND>,
    progress_label: Option<HWND>,
    log_edit: Option<HWND>,
    log_buffer: String,
    summary_text: String,
}

thread_local! {
    static WIZARD: RefCell<Option<Wizard>> = const { RefCell::new(None) };
}

pub fn run(args: LaunchArgs) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);
    }

    let existing = ops::detect_existing();
    let default_dir = args
        .install_dir
        .clone()
        .or_else(|| existing.installed_dir.clone())
        .unwrap_or_else(ops::default_install_dir);
    let initial_pair = args
        .pair_uri
        .clone()
        .or_else(|| {
            if existing.already_paired {
                None
            } else {
                ops::clipboard_pairing_uri()
            }
        })
        .unwrap_or_default();

    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;

    let icon_big = unsafe {
        LoadImageW(
            Some(instance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            scale(48, dpi),
            scale(48, dpi),
            LR_DEFAULTCOLOR,
        )
        .ok()
        .map(|h| HICON(h.0))
    };
    let icon_small = unsafe {
        LoadImageW(
            Some(instance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            scale(16, dpi),
            scale(16, dpi),
            LR_DEFAULTCOLOR,
        )
        .ok()
        .map(|h| HICON(h.0))
    };

    let bg_brush = unsafe { CreateSolidBrush(COLORREF(0x00F3F3F3)) };
    let white_brush = unsafe { HBRUSH(GetStockObject(WHITE_BRUSH).0) };

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: icon_big.unwrap_or_default(),
        hIconSm: icon_small.unwrap_or_default(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        hbrBackground: bg_brush,
        lpszClassName: w!("WaystoneSetupWizard"),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&class) };

    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: scale(BASE_W, dpi),
        bottom: scale(BASE_H, dpi),
    };
    unsafe {
        let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE(0));
    }
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;

    let mut work = RECT::default();
    let (x, y) = if unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
    {
        (
            work.left + (work.right - work.left - win_w) / 2,
            work.top + (work.bottom - work.top - win_h) / 2,
        )
    } else {
        (CW_USEDEFAULT, CW_USEDEFAULT)
    };

    let title = HSTRING::from(format!("Waystone 安装程序 v{VERSION}"));
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("WaystoneSetupWizard"),
            &title,
            style,
            x,
            y,
            win_w,
            win_h,
            None,
            None,
            Some(instance),
            None,
        )
        .context("creating setup wizard window")?
    };

    let font_title = make_font(dpi, 18, FW_BOLD.0 as i32, w!("Microsoft YaHei UI"));
    let font_body = make_font(dpi, 14, FW_NORMAL.0 as i32, w!("Microsoft YaHei UI"));
    let font_mono = make_font(dpi, 12, FW_NORMAL.0 as i32, w!("Consolas"));

    let initial_page = if args.uninstall {
        Page::UninstallConfirm
    } else {
        Page::Configure
    };

    let wizard = Wizard {
        hwnd,
        instance,
        dpi,
        font_title,
        font_body,
        font_mono,
        bg_brush,
        white_brush,
        icon_big,
        mailbox: Mailbox {
            queue: Arc::new(Mutex::new(Vec::new())),
            hwnd: hwnd.0 as isize,
        },
        existing,
        page: initial_page,
        install_dir: default_dir,
        install_service: true,
        autostart_tray: true,
        create_shortcuts: true,
        pair_text: initial_pair,
        remove_data: args.remove_data,
        launch_tray: true,
        controls: Vec::new(),
        header_title: None,
        header_sub: None,
        progress_bar: None,
        progress_label: None,
        log_edit: None,
        log_buffer: String::new(),
        summary_text: String::new(),
    };
    WIZARD.with(|cell| *cell.borrow_mut() = Some(wizard));
    with_wizard(|w| w.render_page());

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    WIZARD.with(|cell| {
        if let Some(w) = cell.borrow_mut().take() {
            unsafe {
                let _ = DeleteObject(w.font_title.into());
                let _ = DeleteObject(w.font_body.into());
                let _ = DeleteObject(w.font_mono.into());
                let _ = DeleteObject(w.bg_brush.into());
            }
        }
    });
    Ok(())
}

fn with_wizard<R>(f: impl FnOnce(&mut Wizard) -> R) -> Option<R> {
    WIZARD.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

impl Wizard {
    fn s(&self, px: i32) -> i32 {
        scale(px, self.dpi)
    }

    fn clear_controls(&mut self) {
        for hwnd in self.controls.drain(..) {
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        self.header_title = None;
        self.header_sub = None;
        self.progress_bar = None;
        self.progress_label = None;
        self.log_edit = None;
    }

    fn add_control(
        &mut self,
        class: PCWSTR,
        text: &str,
        style: WINDOW_STYLE,
        ex_style: WINDOW_EX_STYLE,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        control_id: usize,
        font: HFONT,
    ) -> HWND {
        let text_w = HSTRING::from(text);
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                class,
                &text_w,
                WS_CHILD | WS_VISIBLE | style,
                self.s(x),
                self.s(y),
                self.s(w),
                self.s(h),
                Some(self.hwnd),
                Some(HMENU(control_id as *mut _)),
                Some(self.instance),
                None,
            )
            .expect("creating wizard child control")
        };
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
        self.controls.push(hwnd);
        hwnd
    }

    fn render_header(&mut self, title: &str, subtitle: &str) {
        let text_x = if self.icon_big.is_some() { 76 } else { 20 };
        let text_w = BASE_W - text_x - 20;
        let t = self.add_control(
            w!("STATIC"),
            title,
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            text_x,
            14,
            text_w,
            24,
            0,
            self.font_title,
        );
        let sub = self.add_control(
            w!("STATIC"),
            subtitle,
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            text_x,
            42,
            text_w,
            20,
            0,
            self.font_body,
        );
        self.header_title = Some(t);
        self.header_sub = Some(sub);
    }

    fn render_page(&mut self) {
        self.clear_controls();
        match self.page {
            Page::Configure => self.render_configure(),
            Page::UninstallConfirm => self.render_uninstall_confirm(),
            Page::Working { uninstall } => self.render_working(uninstall),
            Page::Done { success, uninstall } => self.render_done(success, uninstall),
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    fn render_configure(&mut self) {
        let is_upgrade = self.existing.installed_dir.is_some() || self.existing.service_installed;
        let title = if is_upgrade {
            format!("升级 Waystone 被控端 v{VERSION}")
        } else {
            format!("安装 Waystone 被控端 v{VERSION}")
        };
        self.render_header(
            &title,
            "Session 0 系统特权架构 · 支持管理员窗口、UAC 弹窗与锁屏界面",
        );

        let mut y = HEADER_H + 16;
        self.add_control(
            w!("STATIC"),
            "安装位置：",
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            22,
            y,
            BASE_W - 44,
            18,
            0,
            self.font_body,
        );
        y += 22;

        let dir_text = self.install_dir.display().to_string();
        self.add_control(
            w!("EDIT"),
            &dir_text,
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            WS_EX_CLIENTEDGE,
            22,
            y,
            BASE_W - 132,
            26,
            id::EDIT_DIR,
            self.font_body,
        );
        self.add_control(
            w!("BUTTON"),
            "浏览…",
            WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            BASE_W - 102,
            y - 1,
            80,
            28,
            id::BTN_BROWSE,
            self.font_body,
        );
        y += 40;

        let chk_svc = self.add_control(
            w!("BUTTON"),
            "安装为 Windows 系统服务（推荐：开机后台在线，穿透 UAC / 管理员窗口 / 锁屏）",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            WINDOW_EX_STYLE(0),
            22,
            y,
            BASE_W - 44,
            22,
            id::CHK_SERVICE,
            self.font_body,
        );
        set_checked(chk_svc, self.install_service);
        y += 28;

        let chk_auto = self.add_control(
            w!("BUTTON"),
            "登录 Windows 时自动显示任务栏托盘图标（轻量状态控制端）",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            WINDOW_EX_STYLE(0),
            22,
            y,
            BASE_W - 44,
            22,
            id::CHK_AUTOSTART,
            self.font_body,
        );
        set_checked(chk_auto, self.autostart_tray);
        y += 28;

        let chk_lnk = self.add_control(
            w!("BUTTON"),
            "在开始菜单创建 Waystone 与卸载快捷方式",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            WINDOW_EX_STYLE(0),
            22,
            y,
            BASE_W - 44,
            22,
            id::CHK_SHORTCUTS,
            self.font_body,
        );
        set_checked(chk_lnk, self.create_shortcuts);
        y += 38;

        if self.existing.already_paired {
            self.add_control(
                w!("STATIC"),
                "配对状态：检测到本机已有配对配置，安装/升级将自动迁移并保留原设备身份。",
                WINDOW_STYLE(0),
                WINDOW_EX_STYLE(0),
                22,
                y,
                BASE_W - 44,
                40,
                0,
                self.font_body,
            );
        } else {
            self.add_control(
                w!("STATIC"),
                "配对链接（可选，留空可安装后在托盘中从剪贴板配对）：",
                WINDOW_STYLE(0),
                WINDOW_EX_STYLE(0),
                22,
                y,
                BASE_W - 44,
                18,
                0,
                self.font_body,
            );
            y += 22;
            let pair_text = self.pair_text.clone();
            self.add_control(
                w!("EDIT"),
                &pair_text,
                WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                WS_EX_CLIENTEDGE,
                22,
                y,
                BASE_W - 44,
                26,
                id::EDIT_PAIR,
                self.font_body,
            );
        }

        let btn_y = BASE_H - FOOTER_H + 14;
        if is_upgrade {
            self.add_control(
                w!("BUTTON"),
                "卸载 Waystone",
                WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                WINDOW_EX_STYLE(0),
                22,
                btn_y,
                116,
                28,
                id::BTN_UNINSTALL_SWITCH,
                self.font_body,
            );
        }
        let primary_label = if is_upgrade {
            "立即升级"
        } else {
            "立即安装"
        };
        self.add_control(
            w!("BUTTON"),
            primary_label,
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            BASE_W - 208,
            btn_y,
            90,
            28,
            id::BTN_PRIMARY,
            self.font_body,
        );
        self.add_control(
            w!("BUTTON"),
            "取消",
            WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            BASE_W - 110,
            btn_y,
            88,
            28,
            id::BTN_SECONDARY,
            self.font_body,
        );
    }

    fn render_uninstall_confirm(&mut self) {
        self.render_header(
            "卸载 Waystone 被控端",
            "将停止并移除 WaystoneHost 系统服务、程序文件与快捷方式",
        );

        let dir_msg = format!(
            "确认从以下位置卸载 Waystone？\r\n\r\n安装目录：{}",
            self.install_dir.display()
        );
        self.add_control(
            w!("STATIC"),
            &dir_msg,
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            22,
            HEADER_H + 20,
            BASE_W - 44,
            72,
            0,
            self.font_body,
        );

        let chk_data = self.add_control(
            w!("BUTTON"),
            "同时删除设备密钥和配对配置（C:\\ProgramData\\Waystone）",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            WINDOW_EX_STYLE(0),
            22,
            HEADER_H + 108,
            BASE_W - 44,
            24,
            id::CHK_REMOVE_DATA,
            self.font_body,
        );
        set_checked(chk_data, self.remove_data);

        let btn_y = BASE_H - FOOTER_H + 14;
        self.add_control(
            w!("BUTTON"),
            "确认卸载",
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            BASE_W - 208,
            btn_y,
            90,
            28,
            id::BTN_PRIMARY,
            self.font_body,
        );
        self.add_control(
            w!("BUTTON"),
            "取消",
            WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            BASE_W - 110,
            btn_y,
            88,
            28,
            id::BTN_SECONDARY,
            self.font_body,
        );
    }

    fn render_working(&mut self, uninstall: bool) {
        let (title, sub) = if uninstall {
            ("正在卸载 Waystone…", "请稍候，正在停止服务并清理系统配置")
        } else {
            (
                "正在安装 Waystone…",
                "请稍候，正在部署程序与 Session 0 系统服务",
            )
        };
        self.render_header(title, sub);

        let lbl = self.add_control(
            w!("STATIC"),
            "准备中…",
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            22,
            HEADER_H + 18,
            BASE_W - 44,
            20,
            0,
            self.font_body,
        );
        let pb = self.add_control(
            PROGRESS_CLASSW,
            "",
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            22,
            HEADER_H + 44,
            BASE_W - 44,
            20,
            0,
            self.font_body,
        );
        unsafe {
            SendMessageW(pb, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(100)));
            SendMessageW(pb, PBM_SETPOS, Some(WPARAM(5)), Some(LPARAM(0)));
        }

        let log_h = BASE_H - HEADER_H - FOOTER_H - 96;
        let log = self.add_control(
            w!("EDIT"),
            "",
            WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32),
            WS_EX_CLIENTEDGE,
            22,
            HEADER_H + 76,
            BASE_W - 44,
            log_h,
            0,
            self.font_mono,
        );
        self.progress_label = Some(lbl);
        self.progress_bar = Some(pb);
        self.log_edit = Some(log);
    }

    fn render_done(&mut self, success: bool, uninstall: bool) {
        let (title, sub) = match (uninstall, success) {
            (false, true) => ("Waystone 安装完成", "Session 0 特权服务与托盘已准备就绪"),
            (false, false) => ("Waystone 安装未完成", "请查看下方详细信息后重试"),
            (true, true) => ("Waystone 卸载完成", "系统服务与程序文件已清理完毕"),
            (true, false) => ("Waystone 卸载未完成", "请查看下方错误信息"),
        };
        self.render_header(title, sub);

        let summary = self.summary_text.clone();
        self.add_control(
            w!("EDIT"),
            &summary,
            WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32),
            WS_EX_CLIENTEDGE,
            22,
            HEADER_H + 18,
            BASE_W - 44,
            BASE_H - HEADER_H - FOOTER_H - 72,
            0,
            self.font_body,
        );

        if success && !uninstall {
            let chk = self.add_control(
                w!("BUTTON"),
                "立即启动 Waystone 托盘图标",
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                WINDOW_EX_STYLE(0),
                22,
                BASE_H - FOOTER_H - 36,
                BASE_W - 44,
                22,
                id::CHK_LAUNCH_TRAY,
                self.font_body,
            );
            set_checked(chk, self.launch_tray);
        }

        let btn_y = BASE_H - FOOTER_H + 14;
        self.add_control(
            w!("BUTTON"),
            "完成",
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            BASE_W - 112,
            btn_y,
            90,
            28,
            id::BTN_PRIMARY,
            self.font_body,
        );
    }

    fn read_configure_inputs(&mut self) {
        if let Some(dir) = get_control_text(self.hwnd, id::EDIT_DIR) {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                self.install_dir = PathBuf::from(trimmed);
            }
        }
        self.install_service = is_checked(self.hwnd, id::CHK_SERVICE);
        self.autostart_tray = is_checked(self.hwnd, id::CHK_AUTOSTART);
        self.create_shortcuts = is_checked(self.hwnd, id::CHK_SHORTCUTS);
        if let Some(pair) = get_control_text(self.hwnd, id::EDIT_PAIR) {
            self.pair_text = pair.trim().to_string();
        }
    }

    fn start_install(&mut self) {
        self.read_configure_inputs();
        let pair_uri = if self.pair_text.is_empty() {
            None
        } else {
            match validate_pairing_uri(&self.pair_text) {
                Ok(valid) => Some(valid),
                Err(err) => {
                    show_error(
                        self.hwnd,
                        &format!("配对链接格式不正确，请重新粘贴或留空：\n\n{err}"),
                    );
                    return;
                }
            }
        };

        let options = InstallOptions {
            install_dir: self.install_dir.clone(),
            install_service: self.install_service,
            autostart_tray: self.autostart_tray,
            create_shortcuts: self.create_shortcuts,
            pair_uri,
        };
        if let Err(err) = options.validate() {
            show_error(self.hwnd, &format!("{err:#}"));
            return;
        }

        self.log_buffer.clear();
        self.page = Page::Working { uninstall: false };
        self.render_page();

        let mailbox = self.mailbox.clone();
        std::thread::spawn(move || {
            let progress_box = mailbox.clone();
            let result = ops::perform_install(&options, move |p| {
                progress_box.push(WorkerEvent::Progress(p));
            });
            mailbox.push(WorkerEvent::InstallDone(result));
        });
    }

    fn start_uninstall(&mut self) {
        self.remove_data = is_checked(self.hwnd, id::CHK_REMOVE_DATA);
        let install_dir = self.install_dir.clone();
        let remove_data = self.remove_data;

        self.log_buffer.clear();
        self.page = Page::Working { uninstall: true };
        self.render_page();

        let mailbox = self.mailbox.clone();
        std::thread::spawn(move || {
            let progress_box = mailbox.clone();
            let result = ops::perform_uninstall(&install_dir, remove_data, move |p| {
                progress_box.push(WorkerEvent::Progress(p));
            });
            mailbox.push(WorkerEvent::UninstallDone(result));
        });
    }

    fn handle_command(&mut self, control_id: usize) {
        match (self.page, control_id) {
            (Page::Configure, id::BTN_BROWSE) => {
                if let Some(chosen) = ops::browse_for_folder(self.hwnd) {
                    let final_dir = if chosen
                        .file_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case("Waystone"))
                    {
                        chosen
                    } else {
                        chosen.join("Waystone")
                    };
                    self.install_dir = final_dir.clone();
                    if let Ok(edit) = unsafe { GetDlgItem(Some(self.hwnd), id::EDIT_DIR as i32) } {
                        let text = HSTRING::from(final_dir.display().to_string());
                        let _ = unsafe { SetWindowTextW(edit, &text) };
                    }
                }
            }
            (Page::Configure, id::BTN_UNINSTALL_SWITCH) => {
                self.read_configure_inputs();
                self.page = Page::UninstallConfirm;
                self.render_page();
            }
            (Page::Configure, id::BTN_PRIMARY) => self.start_install(),
            (Page::Configure, id::BTN_SECONDARY) => unsafe { PostQuitMessage(0) },
            (Page::UninstallConfirm, id::BTN_PRIMARY) => self.start_uninstall(),
            (Page::UninstallConfirm, id::BTN_SECONDARY) => unsafe { PostQuitMessage(0) },
            (Page::Done { success, uninstall }, id::BTN_PRIMARY) => {
                if success && !uninstall && is_checked(self.hwnd, id::CHK_LAUNCH_TRAY) {
                    let host_exe = self.install_dir.join(HOST_EXE_NAME);
                    ops::launch_tray_unelevated(&host_exe);
                }
                unsafe { PostQuitMessage(0) };
            }
            _ => {}
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Progress(p) => {
                if let Some(lbl) = self.progress_label {
                    let text = HSTRING::from(&p.step);
                    let _ = unsafe { SetWindowTextW(lbl, &text) };
                }
                if let Some(pb) = self.progress_bar {
                    unsafe {
                        SendMessageW(
                            pb,
                            PBM_SETPOS,
                            Some(WPARAM(p.percent as usize)),
                            Some(LPARAM(0)),
                        );
                    }
                }
                if let Some(line) = p.log_line {
                    if !self.log_buffer.is_empty() {
                        self.log_buffer.push_str("\r\n");
                    }
                    self.log_buffer.push_str(&line);
                    if let Some(edit) = self.log_edit {
                        let text = HSTRING::from(&self.log_buffer);
                        let _ = unsafe { SetWindowTextW(edit, &text) };
                    }
                }
            }
            WorkerEvent::InstallDone(Ok(summary)) => {
                let mut lines = vec![
                    format!("安装位置：{}", summary.install_dir.display()),
                    format!(
                        "系统服务：{}",
                        if summary.service_installed {
                            "WaystoneHost 已启动（Session 0 特权模式，支持管理员窗口/UAC/锁屏）"
                        } else {
                            "未安装（仅独立模式）"
                        }
                    ),
                    format!("配对状态：{}", summary.paired_note),
                    format!("日志路径：C:\\ProgramData\\Waystone\\host.log"),
                ];
                if !summary.warnings.is_empty() {
                    lines.push(String::new());
                    lines.push("注意事项：".into());
                    for w in summary.warnings {
                        lines.push(format!("• {w}"));
                    }
                }
                self.summary_text = lines.join("\r\n");
                self.page = Page::Done {
                    success: true,
                    uninstall: false,
                };
                self.render_page();
            }
            WorkerEvent::InstallDone(Err(err)) => {
                self.summary_text = format!(
                    "安装过程中出现错误：\r\n\r\n{err:#}\r\n\r\n执行日志：\r\n{}",
                    self.log_buffer
                );
                self.page = Page::Done {
                    success: false,
                    uninstall: false,
                };
                self.render_page();
            }
            WorkerEvent::UninstallDone(Ok(())) => {
                self.summary_text = format!(
                    "Waystone 已从 {} 卸载。\r\n{}",
                    self.install_dir.display(),
                    if self.remove_data {
                        "设备密钥和配置目录 C:\\ProgramData\\Waystone 也已删除。"
                    } else {
                        "设备密钥与配对配置保留在 C:\\ProgramData\\Waystone，重装后无需重新配对。"
                    }
                );
                self.page = Page::Done {
                    success: true,
                    uninstall: true,
                };
                self.render_page();
            }
            WorkerEvent::UninstallDone(Err(err)) => {
                self.summary_text = format!("卸载失败：\r\n\r\n{err:#}");
                self.page = Page::Done {
                    success: false,
                    uninstall: true,
                };
                self.render_page();
            }
        }
    }

    fn paint(&self, hdc: HDC) {
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut client) };

        // Top white header banner.
        let header_rect = RECT {
            left: 0,
            top: 0,
            right: client.right,
            bottom: self.s(HEADER_H),
        };
        unsafe {
            FillRect(hdc, &header_rect, self.white_brush);
        }

        // 1px dividers below header and above footer.
        let divider_brush = unsafe { CreateSolidBrush(COLORREF(0x00DCDCDC)) };
        let top_line = RECT {
            left: 0,
            top: self.s(HEADER_H),
            right: client.right,
            bottom: self.s(HEADER_H) + 1,
        };
        let bottom_line = RECT {
            left: 0,
            top: client.bottom - self.s(FOOTER_H),
            right: client.right,
            bottom: client.bottom - self.s(FOOTER_H) + 1,
        };
        unsafe {
            FillRect(hdc, &top_line, divider_brush);
            FillRect(hdc, &bottom_line, divider_brush);
            let _ = DeleteObject(divider_brush.into());
        }

        if let Some(icon) = self.icon_big {
            let icon_size = self.s(48);
            let _ = unsafe {
                DrawIconEx(
                    hdc,
                    self.s(18),
                    self.s(13),
                    icon,
                    icon_size,
                    icon_size,
                    0,
                    None,
                    DI_NORMAL,
                )
            };
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            with_wizard(|w| w.paint(hdc));
            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wparam.0 as *mut _);
            let ctrl = HWND(lparam.0 as *mut _);
            if let Some(brush) = with_wizard(|w| {
                if w.header_title == Some(ctrl) || w.header_sub == Some(ctrl) {
                    unsafe {
                        SetBkMode(hdc, TRANSPARENT);
                        SetBkColor(hdc, COLORREF(0x00FFFFFF));
                        SetTextColor(
                            hdc,
                            if w.header_title == Some(ctrl) {
                                COLORREF(0x001A1A1A)
                            } else {
                                COLORREF(0x005A5A5A)
                            },
                        );
                    }
                    w.white_brush
                } else {
                    unsafe {
                        SetBkMode(hdc, TRANSPARENT);
                        SetTextColor(hdc, COLORREF(0x00222222));
                    }
                    w.bg_brush
                }
            }) {
                return LRESULT(brush.0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_COMMAND => {
            let control_id = wparam.0 & 0xffff;
            let code = (wparam.0 >> 16) & 0xffff;
            if code == BN_CLICKED as usize {
                with_wizard(|w| w.handle_command(control_id));
            }
            LRESULT(0)
        }
        WM_WORKER => {
            let events = with_wizard(|w| std::mem::take(&mut *w.mailbox.queue.lock().unwrap()))
                .unwrap_or_default();
            for event in events {
                with_wizard(|w| w.handle_worker_event(event));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let busy = with_wizard(|w| matches!(w.page, Page::Working { .. })).unwrap_or(false);
            if !busy {
                unsafe { PostQuitMessage(0) };
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn scale(px: i32, dpi: i32) -> i32 {
    (px * dpi + 48) / 96
}

fn make_font(dpi: i32, px: i32, weight: i32, face: PCWSTR) -> HFONT {
    unsafe {
        CreateFontW(
            -scale(px, dpi),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            face,
        )
    }
}

fn set_checked(hwnd: HWND, checked: bool) {
    unsafe {
        SendMessageW(
            hwnd,
            BM_SETCHECK,
            Some(WPARAM(if checked { 1 } else { 0 })),
            Some(LPARAM(0)),
        );
    }
}

fn is_checked(parent: HWND, control_id: usize) -> bool {
    let Ok(ctrl) = (unsafe { GetDlgItem(Some(parent), control_id as i32) }) else {
        return false;
    };
    unsafe { SendMessageW(ctrl, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1 }
}

fn get_control_text(parent: HWND, control_id: usize) -> Option<String> {
    let ctrl = unsafe { GetDlgItem(Some(parent), control_id as i32) }.ok()?;
    let len = unsafe { GetWindowTextLengthW(ctrl) };
    if len <= 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u16; len as usize + 1];
    let copied = unsafe { GetWindowTextW(ctrl, &mut buf) } as usize;
    Some(String::from_utf16_lossy(&buf[..copied]))
}

fn show_error(owner: HWND, text: &str) {
    let msg = HSTRING::from(text);
    unsafe {
        MessageBoxW(
            Some(owner),
            &msg,
            w!("Waystone 安装程序"),
            MB_OK | MB_ICONERROR,
        );
    }
}
