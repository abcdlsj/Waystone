//! Cross-platform installer options, CLI parsing, and validation helpers.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use waystone_proto::pairing::PairingUri;

pub const APP_NAME: &str = "Waystone";
pub const DISPLAY_NAME: &str = "Waystone Host";
pub const PUBLISHER: &str = "Waystone";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const UNINSTALL_REG_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\WaystoneHost";
pub const HOST_EXE_NAME: &str = "waystone-host.exe";
pub const UNINSTALLER_EXE_NAME: &str = "waystone-uninstall.exe";
pub const DEFAULT_INSTALL_DIR: &str = r"C:\Program Files\Waystone";

/// Parsed command-line flags for `WaystoneSetup.exe`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchArgs {
    pub uninstall: bool,
    pub silent: bool,
    pub install_dir: Option<PathBuf>,
    pub remove_data: bool,
    pub pair_uri: Option<String>,
}

impl LaunchArgs {
    pub fn parse<I, S>(iter: I) -> LaunchArgs
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut args = LaunchArgs::default();
        let mut it = iter.into_iter().skip(1);
        while let Some(raw) = it.next() {
            let arg = raw.as_ref();
            match arg {
                "--uninstall" | "/uninstall" | "-u" => args.uninstall = true,
                "--silent" | "/S" | "/silent" | "-q" => args.silent = true,
                "--remove-data" => args.remove_data = true,
                "--dir" => {
                    if let Some(val) = it.next() {
                        let trimmed = val.as_ref().trim();
                        if !trimmed.is_empty() {
                            args.install_dir = Some(PathBuf::from(trimmed));
                        }
                    }
                }
                "--pair" => {
                    if let Some(val) = it.next() {
                        let trimmed = val.as_ref().trim();
                        if !trimmed.is_empty() {
                            args.pair_uri = Some(trimmed.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        args
    }
}

/// User-selected installation options from the wizard (or silent defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    pub install_dir: PathBuf,
    pub install_service: bool,
    pub autostart_tray: bool,
    pub create_shortcuts: bool,
    pub pair_uri: Option<String>,
}

impl InstallOptions {
    pub fn defaults(install_dir: PathBuf) -> InstallOptions {
        InstallOptions {
            install_dir,
            install_service: true,
            autostart_tray: true,
            create_shortcuts: true,
            pair_uri: None,
        }
    }

    /// Validates paths and optional pairing URI before modifying the system.
    pub fn validate(&self) -> Result<()> {
        let dir_str = self.install_dir.to_string_lossy();
        if dir_str.trim().is_empty() {
            bail!("install directory cannot be empty");
        }
        if let Some(uri) = &self.pair_uri {
            validate_pairing_uri(uri)?;
        }
        Ok(())
    }

    pub fn host_exe(&self) -> PathBuf {
        self.install_dir.join(HOST_EXE_NAME)
    }

    pub fn uninstaller_exe(&self) -> PathBuf {
        self.install_dir.join(UNINSTALLER_EXE_NAME)
    }
}

/// Validates a non-empty `waystone://pair?...` string using `waystone-proto`.
pub fn validate_pairing_uri(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("pairing link is empty");
    }
    let _parsed: PairingUri = trimmed
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid pairing link: {err}"))?;
    Ok(trimmed.to_string())
}

/// Returns `Some(uri)` if `text` is a valid Waystone pairing URI.
pub fn extract_pairing_uri(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with("waystone://pair") {
        return None;
    }
    validate_pairing_uri(trimmed).ok()
}

/// Builds the command line stored in `HKLM\...\Uninstall\WaystoneHost\UninstallString`.
pub fn uninstall_command_line(install_dir: &Path, silent: bool) -> String {
    let exe = install_dir.join(UNINSTALLER_EXE_NAME);
    if silent {
        format!("\"{}\" --uninstall --silent", exe.display())
    } else {
        format!("\"{}\" --uninstall", exe.display())
    }
}

/// Outcome summary presented on the final wizard page.
#[derive(Debug, Clone)]
pub struct InstallSummary {
    pub install_dir: PathBuf,
    pub service_installed: bool,
    pub paired_note: String,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launch_flags() {
        let args = LaunchArgs::parse([
            "WaystoneSetup.exe",
            "--uninstall",
            "--silent",
            "--remove-data",
            "--dir",
            r"D:\Apps\Waystone",
        ]);
        assert!(args.uninstall);
        assert!(args.silent);
        assert!(args.remove_data);
        assert_eq!(args.install_dir, Some(PathBuf::from(r"D:\Apps\Waystone")));
    }

    #[test]
    fn validates_options_and_pairing_links() {
        let valid_uri = format!(
            "waystone://pair?ep=192.168.1.10:8443&turn=192.168.1.10:3478&fp={}&tk=0123456789abcdef0123",
            "a".repeat(64)
        );
        assert_eq!(extract_pairing_uri(&valid_uri), Some(valid_uri.clone()));
        assert_eq!(extract_pairing_uri("https://example.com"), None);

        let mut opts = InstallOptions::defaults(PathBuf::from(DEFAULT_INSTALL_DIR));
        assert!(opts.validate().is_ok());

        opts.pair_uri = Some("not-a-uri".into());
        assert!(opts.validate().is_err());

        opts.pair_uri = Some(valid_uri);
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn formats_uninstall_command_lines() {
        let dir = Path::new(r"C:\Program Files\Waystone");
        let expected_exe = dir.join(UNINSTALLER_EXE_NAME);
        assert_eq!(
            uninstall_command_line(dir, false),
            format!("\"{}\" --uninstall", expected_exe.display())
        );
        assert_eq!(
            uninstall_command_line(dir, true),
            format!("\"{}\" --uninstall --silent", expected_exe.display())
        );
    }
}
