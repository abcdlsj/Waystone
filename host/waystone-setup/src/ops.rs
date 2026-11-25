//! Windows installation, upgrade, and uninstallation operations.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{w, Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HGLOBAL, HWND};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TokenElevation, TokenPrimary,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WRITE,
    REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceStatusEx, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_CONTROL_STOP,
    SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED,
};
use windows::Win32::System::Threading::{
    CreateProcessWithTokenW, GetCurrentProcess, OpenProcess, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
    FileOpenDialog, IFileOpenDialog, IShellLinkW, SHGetKnownFolderPath, ShellExecuteW, ShellLink,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, KF_FLAG_DEFAULT, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetShellWindow, GetWindowThreadProcessId, SW_SHOWNORMAL,
};

use crate::model::{
    extract_pairing_uri, uninstall_command_line, InstallOptions, InstallSummary, APP_NAME,
    DEFAULT_INSTALL_DIR, DISPLAY_NAME, HOST_EXE_NAME, PUBLISHER, UNINSTALL_REG_KEY, VERSION,
};

/// Embedded `waystone-host.exe` payload produced by `build.rs`.
const EMBEDDED_HOST_EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/waystone-host.payload"));

const SERVICE_NAME_W: PCWSTR = w!("WaystoneHost");
const SERVICE_DELETE: u32 = 0x0001_0000;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Waystone");

/// Snapshot of the machine's existing Waystone installation before changes.
#[derive(Debug, Clone)]
pub struct ExistingState {
    pub installed_dir: Option<PathBuf>,
    pub service_installed: bool,
    pub already_paired: bool,
}

/// Progress update emitted during install/uninstall.
#[derive(Debug, Clone)]
pub struct Progress {
    pub percent: u32,
    pub step: String,
    pub log_line: Option<String>,
}

pub fn current_exe() -> PathBuf {
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
}

pub fn default_install_dir() -> PathBuf {
    known_folder(&FOLDERID_ProgramFiles)
        .map(|p| p.join(APP_NAME))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INSTALL_DIR))
}

pub fn program_data_dir() -> PathBuf {
    known_folder(&FOLDERID_ProgramData)
        .map(|p| p.join(APP_NAME))
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\Waystone"))
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Re-launches the current setup binary with UAC (`runas`) preserving arguments.
/// Returns `Ok(true)` if the elevated child was launched (caller should exit),
/// or `Ok(false)` if the user declined the UAC prompt.
pub fn relaunch_elevated() -> Result<bool> {
    let exe = HSTRING::from(current_exe().as_os_str());
    let params_str = std::env::args()
        .skip(1)
        .map(quote_arg)
        .collect::<Vec<_>>()
        .join(" ");
    let params = HSTRING::from(&params_str);
    let params_pcwstr = if params_str.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(params.as_ptr())
    };
    let result =
        unsafe { ShellExecuteW(None, w!("runas"), &exe, params_pcwstr, None, SW_SHOWNORMAL) };
    Ok(result.0 as isize > 32)
}

pub fn detect_existing() -> ExistingState {
    let reg_dir = read_reg_string(HKEY_LOCAL_MACHINE, UNINSTALL_REG_KEY, "InstallLocation")
        .map(PathBuf::from)
        .filter(|p| p.join(HOST_EXE_NAME).exists());
    let default_dir = default_install_dir();
    let installed_dir = reg_dir.or_else(|| {
        default_dir
            .join(HOST_EXE_NAME)
            .exists()
            .then_some(default_dir)
    });

    let service_installed = query_service_installed();

    let pd_cfg = program_data_dir().join("config.json");
    let legacy_cfg =
        known_folder(&FOLDERID_LocalAppData).map(|p| p.join(APP_NAME).join("config.json"));
    let already_paired = pd_cfg.exists() || legacy_cfg.is_some_and(|p| p.exists());

    ExistingState {
        installed_dir,
        service_installed,
        already_paired,
    }
}

pub fn clipboard_pairing_uri() -> Option<String> {
    let text = unsafe {
        OpenClipboard(None).ok()?;
        let value = GetClipboardData(u32::from(CF_UNICODETEXT.0))
            .ok()
            .and_then(|handle| {
                let mem = HGLOBAL(handle.0);
                let ptr = GlobalLock(mem) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(mem);
                Some(s)
            });
        let _ = CloseClipboard();
        value
    }?;
    extract_pairing_uri(&text)
}

/// Opens the native Windows folder picker dialog.
pub fn browse_for_folder(owner: HWND) -> Option<PathBuf> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        dialog.SetTitle(w!("选择 Waystone 安装目录")).ok()?;
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = pwstr.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path
    }
}

/// Runs the full installation / upgrade workflow, reporting progress via `report`.
pub fn perform_install(
    options: &InstallOptions,
    mut report: impl FnMut(Progress),
) -> Result<InstallSummary> {
    options.validate()?;
    let payload = load_payload()?;
    let mut warnings = Vec::new();

    report(Progress {
        percent: 10,
        step: "正在停止旧版服务与进程…".into(),
        log_line: Some("Checking and stopping existing Waystone processes...".into()),
    });
    stop_service_if_running();
    terminate_host_processes();

    report(Progress {
        percent: 30,
        step: "正在写入程序文件…".into(),
        log_line: Some(format!(
            "Installing files into {}...",
            options.install_dir.display()
        )),
    });
    std::fs::create_dir_all(&options.install_dir).with_context(|| {
        format!(
            "creating install directory {}",
            options.install_dir.display()
        )
    })?;
    let host_exe = options.host_exe();
    write_file_with_retry(&host_exe, &payload)
        .with_context(|| format!("writing {}", host_exe.display()))?;

    let uninstaller = options.uninstaller_exe();
    let self_exe = current_exe();
    if !same_path(&self_exe, &uninstaller) {
        if let Err(err) = copy_file_with_retry(&self_exe, &uninstaller) {
            warnings.push(format!("Could not copy uninstaller: {err:#}"));
        }
    }

    report(Progress {
        percent: 55,
        step: if options.install_service {
            "正在安装并启动 Session 0 系统服务…".into()
        } else {
            "跳过系统服务安装…".into()
        },
        log_line: Some(if options.install_service {
            "Registering WaystoneHost LocalSystem service (Session 0)...".into()
        } else {
            "Skipping Session 0 service registration per user choice.".into()
        }),
    });

    let mut service_installed = false;
    if options.install_service {
        let output = run_hidden(&host_exe, &["service", "install"])?;
        service_installed = true;
        report(Progress {
            percent: 70,
            step: "Session 0 系统服务已启动".into(),
            log_line: Some(format!("Service installed: {}", first_line(&output))),
        });
    }

    let mut paired_note = if program_data_dir().join("config.json").exists() {
        "已保留现有配对配置".to_string()
    } else {
        "尚未配对（右键右下角托盘图标选「从剪贴板配对」即可）".to_string()
    };

    if let Some(uri) = &options.pair_uri {
        report(Progress {
            percent: 78,
            step: "正在配对这台电脑…".into(),
            log_line: Some("Pairing host with Waystone server...".into()),
        });
        match run_hidden(&host_exe, &["pair", uri]) {
            Ok(out) => {
                paired_note = "配对成功，主机已在线".to_string();
                report(Progress {
                    percent: 82,
                    step: "配对成功".into(),
                    log_line: Some(first_line(&out)),
                });
            }
            Err(err) => {
                warnings.push(format!("配对失败（可稍后在托盘中重新配对）：{err:#}"));
                report(Progress {
                    percent: 82,
                    step: "配对未完成，继续收尾…".into(),
                    log_line: Some(format!("Warning: pairing failed: {err:#}")),
                });
            }
        }
    }

    report(Progress {
        percent: 88,
        step: "正在写入系统卸载信息与开机项…".into(),
        log_line: Some("Registering Add/Remove Programs entry and startup settings...".into()),
    });

    let size_kb = ((payload.len() as u64) * 2 / 1024).max(1) as u32;
    if let Err(err) = register_uninstall_entry(&options.install_dir, size_kb) {
        warnings.push(format!("注册卸载信息失败：{err:#}"));
    }

    if let Err(err) = set_tray_autostart(&host_exe, options.autostart_tray) {
        warnings.push(format!("设置托盘开机自启失败：{err:#}"));
    }

    if options.create_shortcuts {
        report(Progress {
            percent: 94,
            step: "正在创建开始菜单快捷方式…".into(),
            log_line: Some("Creating Start Menu shortcuts...".into()),
        });
        if let Err(err) = create_start_menu_shortcuts(&host_exe, &uninstaller) {
            warnings.push(format!("创建快捷方式失败：{err:#}"));
        }
    }

    report(Progress {
        percent: 100,
        step: "安装完成".into(),
        log_line: Some("Waystone installation finished successfully.".into()),
    });

    Ok(InstallSummary {
        install_dir: options.install_dir.clone(),
        service_installed,
        paired_note,
        warnings,
    })
}

/// Runs the full uninstallation workflow.
pub fn perform_uninstall(
    install_dir: &Path,
    remove_data: bool,
    mut report: impl FnMut(Progress),
) -> Result<()> {
    report(Progress {
        percent: 15,
        step: "正在停止并移除 WaystoneHost 系统服务…".into(),
        log_line: Some("Stopping and deleting WaystoneHost Windows service...".into()),
    });
    let host_exe = install_dir.join(HOST_EXE_NAME);
    if host_exe.exists() {
        let _ = run_hidden(&host_exe, &["service", "uninstall"]);
    }
    delete_service_directly();

    report(Progress {
        percent: 40,
        step: "正在关闭托盘与后台进程…".into(),
        log_line: Some("Terminating running waystone-host.exe processes...".into()),
    });
    terminate_host_processes();

    report(Progress {
        percent: 60,
        step: "正在清理开机启动项与快捷方式…".into(),
        log_line: Some(
            "Removing autostart entries, Start Menu shortcuts, and uninstall key...".into(),
        ),
    });
    let _ = remove_tray_autostart();
    remove_start_menu_shortcuts();
    remove_uninstall_entry();

    report(Progress {
        percent: 80,
        step: "正在删除程序文件…".into(),
        log_line: Some(format!("Removing files from {}...", install_dir.display())),
    });
    if host_exe.exists() {
        let _ = remove_file_with_retry(&host_exe);
    }
    if remove_data {
        let data_dir = program_data_dir();
        if data_dir.exists() {
            let _ = std::fs::remove_dir_all(&data_dir);
        }
    }

    // If this process itself lives inside `install_dir`, schedule delayed folder
    // cleanup right after the uninstaller exits.
    let self_exe = current_exe();
    if self_exe.starts_with(install_dir) {
        schedule_delayed_dir_delete(install_dir);
    } else if install_dir.exists() {
        let _ = std::fs::remove_dir_all(install_dir);
    }

    report(Progress {
        percent: 100,
        step: "卸载完成".into(),
        log_line: Some("Waystone has been uninstalled.".into()),
    });
    Ok(())
}

/// Launches `waystone-host.exe tray` non-elevated using the desktop shell's
/// token (so the tray runs as the logged-in user), falling back to `ShellExecuteW`.
pub fn launch_tray_unelevated(host_exe: &Path) {
    if launch_via_explorer_token(host_exe, "tray").is_ok() {
        return;
    }
    let exe = HSTRING::from(host_exe.as_os_str());
    unsafe {
        ShellExecuteW(None, w!("open"), &exe, w!("tray"), None, SW_SHOWNORMAL);
    }
}

fn launch_via_explorer_token(exe: &Path, args: &str) -> Result<()> {
    unsafe {
        let shell_wnd = GetShellWindow();
        if shell_wnd.is_invalid() {
            bail!("no desktop shell window");
        }
        let mut shell_pid = 0u32;
        GetWindowThreadProcessId(shell_wnd, Some(&mut shell_pid));
        if shell_pid == 0 {
            bail!("cannot resolve explorer pid");
        }
        let proc_handle = OpenProcess(PROCESS_QUERY_INFORMATION, false, shell_pid)?;
        let mut shell_token = HANDLE::default();
        let opened = OpenProcessToken(proc_handle, TOKEN_DUPLICATE, &mut shell_token);
        let _ = CloseHandle(proc_handle);
        opened?;

        let mut primary = HANDLE::default();
        let dup = DuplicateTokenEx(
            shell_token,
            TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        let _ = CloseHandle(shell_token);
        dup?;

        let mut cmdline: Vec<u16> = format!("\"{}\" {}", exe.display(), args)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let res = CreateProcessWithTokenW(
            primary,
            Default::default(),
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            PROCESS_CREATION_FLAGS(0),
            None,
            None,
            &startup,
            &mut pi,
        );
        let _ = CloseHandle(primary);
        res?;
        if !pi.hThread.is_invalid() {
            let _ = CloseHandle(pi.hThread);
        }
        if !pi.hProcess.is_invalid() {
            let _ = CloseHandle(pi.hProcess);
        }
        Ok(())
    }
}

fn load_payload() -> Result<Vec<u8>> {
    if !EMBEDDED_HOST_EXE.is_empty() {
        return Ok(EMBEDDED_HOST_EXE.to_vec());
    }
    // Development fallback when built without `WAYSTONE_HOST_EXE`.
    let sibling = current_exe()
        .parent()
        .map(|dir| dir.join(HOST_EXE_NAME))
        .ok_or_else(|| anyhow!("cannot locate setup executable directory"))?;
    std::fs::read(&sibling).with_context(|| {
        format!(
            "installer has no embedded payload and {} was not found next to it",
            sibling.display()
        )
    })
}

fn run_hidden(exe: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .with_context(|| format!("running {} {:?}", exe.display(), args))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        bail!("{detail}");
    }
    Ok(if !stdout.is_empty() { stdout } else { stderr })
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("OK")
        .trim()
        .to_string()
}

fn query_service_installed() -> bool {
    let Ok(scm) = (unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }) else {
        return false;
    };
    let installed = match unsafe { OpenServiceW(scm, SERVICE_NAME_W, SERVICE_QUERY_STATUS) } {
        Ok(service) => {
            let _ = unsafe { CloseServiceHandle(service) };
            true
        }
        Err(_) => false,
    };
    let _ = unsafe { CloseServiceHandle(scm) };
    installed
}

fn stop_service_if_running() {
    let Ok(scm) = (unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }) else {
        return;
    };
    if let Ok(service) =
        unsafe { OpenServiceW(scm, SERVICE_NAME_W, SERVICE_STOP | SERVICE_QUERY_STATUS) }
    {
        let mut status = SERVICE_STATUS::default();
        let _ = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut ssp = SERVICE_STATUS_PROCESS::default();
            let mut needed = 0u32;
            let slice = unsafe {
                std::slice::from_raw_parts_mut(
                    (&mut ssp as *mut SERVICE_STATUS_PROCESS) as *mut u8,
                    std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
                )
            };
            if unsafe {
                QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(slice), &mut needed)
            }
            .is_ok()
                && ssp.dwCurrentState == SERVICE_STOPPED
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        let _ = unsafe { CloseServiceHandle(service) };
    }
    let _ = unsafe { CloseServiceHandle(scm) };
}

fn delete_service_directly() {
    let Ok(scm) = (unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }) else {
        return;
    };
    if let Ok(service) = unsafe {
        OpenServiceW(
            scm,
            SERVICE_NAME_W,
            SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_DELETE,
        )
    } {
        let mut status = SERVICE_STATUS::default();
        let _ = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) };
        std::thread::sleep(Duration::from_millis(300));
        let _ = unsafe { DeleteService(service) };
        let _ = unsafe { CloseServiceHandle(service) };
    }
    let _ = unsafe { CloseServiceHandle(scm) };
}

fn terminate_host_processes() {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case(HOST_EXE_NAME) {
                    if let Ok(proc) = OpenProcess(
                        PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                        false,
                        entry.th32ProcessID,
                    ) {
                        let _ = TerminateProcess(proc, 0);
                        let _ = WaitForSingleObject(proc, 2_000);
                        let _ = CloseHandle(proc);
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

fn write_file_with_retry(path: &Path, bytes: &[u8]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::write(path, bytes) {
            Ok(()) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn copy_file_with_retry(src: &Path, dst: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::copy(src, dst) {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn remove_file_with_retry(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn register_uninstall_entry(install_dir: &Path, estimated_size_kb: u32) -> Result<()> {
    let key_w = HSTRING::from(UNINSTALL_REG_KEY);
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &key_w,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .context("creating Uninstall registry key")?;
    }

    let icon = format!("{},0", install_dir.join(HOST_EXE_NAME).display());
    let uninstall = uninstall_command_line(install_dir, false);
    let quiet_uninstall = uninstall_command_line(install_dir, true);
    let install_loc = install_dir.display().to_string();

    let res = (|| -> Result<()> {
        set_reg_sz(key, "DisplayName", DISPLAY_NAME)?;
        set_reg_sz(key, "DisplayVersion", VERSION)?;
        set_reg_sz(key, "Publisher", PUBLISHER)?;
        set_reg_sz(key, "InstallLocation", &install_loc)?;
        set_reg_sz(key, "DisplayIcon", &icon)?;
        set_reg_sz(key, "UninstallString", &uninstall)?;
        set_reg_sz(key, "QuietUninstallString", &quiet_uninstall)?;
        set_reg_dword(key, "EstimatedSize", estimated_size_kb)?;
        set_reg_dword(key, "NoModify", 1)?;
        set_reg_dword(key, "NoRepair", 1)?;
        Ok(())
    })();
    let _ = unsafe { RegCloseKey(key) };
    res
}

fn remove_uninstall_entry() {
    let key_w = HSTRING::from(UNINSTALL_REG_KEY);
    let _ = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, &key_w) };
}

fn set_tray_autostart(host_exe: &Path, enabled: bool) -> Result<()> {
    // Clean any per-user Run key so only one machine-wide entry manages tray startup.
    delete_reg_value(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
    if enabled {
        let mut key = HKEY::default();
        unsafe {
            RegOpenKeyExW(HKEY_LOCAL_MACHINE, RUN_KEY, None, KEY_SET_VALUE, &mut key)
                .ok()
                .context("opening HKLM Run key")?;
        }
        let command = format!("\"{}\" tray", host_exe.display());
        let res = set_reg_sz(key, "Waystone", &command);
        let _ = unsafe { RegCloseKey(key) };
        res
    } else {
        delete_reg_value(HKEY_LOCAL_MACHINE, RUN_KEY, RUN_VALUE);
        Ok(())
    }
}

fn remove_tray_autostart() -> Result<()> {
    delete_reg_value(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
    delete_reg_value(HKEY_LOCAL_MACHINE, RUN_KEY, RUN_VALUE);
    Ok(())
}

fn create_start_menu_shortcuts(host_exe: &Path, uninstaller_exe: &Path) -> Result<()> {
    let Some(programs) = known_folder(&FOLDERID_CommonPrograms) else {
        bail!("cannot locate Start Menu Programs folder");
    };
    let folder = programs.join(APP_NAME);
    std::fs::create_dir_all(&folder)?;

    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    create_shortcut(
        &folder.join("Waystone.lnk"),
        host_exe,
        "tray",
        "Waystone Remote Game Streaming Host",
        host_exe,
    )?;
    if uninstaller_exe.exists() {
        create_shortcut(
            &folder.join("卸载 Waystone.lnk"),
            uninstaller_exe,
            "--uninstall",
            "Uninstall Waystone Host",
            uninstaller_exe,
        )?;
    }
    Ok(())
}

fn remove_start_menu_shortcuts() {
    if let Some(programs) = known_folder(&FOLDERID_CommonPrograms) {
        let folder = programs.join(APP_NAME);
        if folder.exists() {
            let _ = std::fs::remove_dir_all(&folder);
        }
    }
}

fn create_shortcut(
    link_path: &Path,
    target: &Path,
    arguments: &str,
    description: &str,
    icon_path: &Path,
) -> Result<()> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_ALL)?;
        let target_w = HSTRING::from(target.as_os_str());
        let args_w = HSTRING::from(arguments);
        let desc_w = HSTRING::from(description);
        let icon_w = HSTRING::from(icon_path.as_os_str());
        let work_dir = target.parent().map(|p| HSTRING::from(p.as_os_str()));

        link.SetPath(&target_w)?;
        link.SetArguments(&args_w)?;
        link.SetDescription(&desc_w)?;
        link.SetIconLocation(&icon_w, 0)?;
        if let Some(dir) = work_dir {
            link.SetWorkingDirectory(&dir)?;
        }
        let persist: IPersistFile = link.cast()?;
        let save_w = HSTRING::from(link_path.as_os_str());
        persist.Save(&save_w, true)?;
    }
    Ok(())
}

fn schedule_delayed_dir_delete(dir: &Path) {
    let cmd = format!(
        "timeout /t 2 /nobreak >nul & rmdir /s /q \"{}\"",
        dir.display()
    );
    let _ = Command::new("cmd.exe")
        .args(["/C", &cmd])
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn();
}

fn set_reg_sz(key: HKEY, name: &str, value: &str) -> Result<()> {
    let name_w = HSTRING::from(name);
    let bytes: Vec<u8> = OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    unsafe { RegSetValueExW(key, &name_w, None, REG_SZ, Some(&bytes)) }
        .ok()
        .with_context(|| format!("setting registry string {name}"))
}

fn set_reg_dword(key: HKEY, name: &str, value: u32) -> Result<()> {
    let name_w = HSTRING::from(name);
    let bytes = value.to_le_bytes();
    unsafe { RegSetValueExW(key, &name_w, None, REG_DWORD, Some(&bytes)) }
        .ok()
        .with_context(|| format!("setting registry dword {name}"))
}

fn delete_reg_value(root: HKEY, subkey: PCWSTR, value: PCWSTR) {
    let mut key = HKEY::default();
    if unsafe { RegOpenKeyExW(root, subkey, None, KEY_SET_VALUE, &mut key) }.is_ok() {
        let _ = unsafe { RegDeleteValueW(key, value) };
        let _ = unsafe { RegCloseKey(key) };
    }
}

fn read_reg_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let sub_w = HSTRING::from(subkey);
    let val_w = HSTRING::from(value);
    let mut size = 0u32;
    unsafe {
        RegGetValueW(
            root,
            &sub_w,
            &val_w,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    if size < 2 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    unsafe {
        RegGetValueW(
            root,
            &sub_w,
            &val_w,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn known_folder(id: &windows_core::GUID) -> Option<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

fn quote_arg(arg: String) -> String {
    if arg.contains(' ') || arg.contains('"') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg
    }
}
