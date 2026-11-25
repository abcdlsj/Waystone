//! Self-contained Windows Setup Wizard (`WaystoneSetup.exe`).
//!
//! Double-clicking opens a native Win32 setup wizard that elevates via UAC,
//! deploys `waystone-host.exe` to `C:\Program Files\Waystone`, migrates legacy
//! `%LOCALAPPDATA%\Waystone` configuration to `C:\ProgramData\Waystone`,
//! installs and starts the `WaystoneHost` Session 0 LocalSystem service,
//! registers Add/Remove Programs uninstallation, creates Start Menu shortcuts,
//! and launches the taskbar tray.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod model;
#[cfg(windows)]
mod ops;
#[cfg(windows)]
mod ui;

use anyhow::Result;

#[cfg(windows)]
fn main() -> Result<()> {
    let args = model::LaunchArgs::parse(std::env::args());

    // Defense-in-depth if launched without the embedded UAC manifest taking effect.
    if !ops::is_elevated() {
        if ops::relaunch_elevated()? {
            return Ok(());
        }
        return Ok(());
    }

    if args.silent {
        let existing = ops::detect_existing();
        let install_dir = args
            .install_dir
            .or(existing.installed_dir)
            .unwrap_or_else(ops::default_install_dir);
        if args.uninstall {
            ops::perform_uninstall(&install_dir, args.remove_data, |_| {})?;
        } else {
            let mut options = model::InstallOptions::defaults(install_dir);
            options.pair_uri = args.pair_uri;
            let summary = ops::perform_install(&options, |_| {})?;
            ops::launch_tray_unelevated(&summary.install_dir.join(model::HOST_EXE_NAME));
        }
        return Ok(());
    }

    ui::run(args)
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    println!(
        "waystone-setup is a Windows installer binary. Build it with:\n  \
         host/waystone-host/scripts/build-windows-installer.sh"
    );
    Ok(())
}
