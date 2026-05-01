use anyhow::Result;

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
pub fn ensure_elevated() -> Result<()> {
    use std::path::Path;

    use anyhow::{Context, anyhow};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // Safety: IsUserAnAdmin and ShellExecuteW are process-level Windows APIs.
    // All pointers passed to ShellExecuteW are null-terminated and live until the call returns.
    unsafe {
        if IsUserAnAdmin() != 0 {
            return Ok(());
        }
    }

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let args = std::env::args_os()
        .skip(1)
        .map(quote_arg)
        .collect::<Vec<_>>()
        .join(" ");
    let workdir = exe.parent().unwrap_or_else(|| Path::new("."));

    let operation = wide_null("runas");
    let file = wide_null(exe.as_os_str());
    let parameters = wide_null(args.as_str());
    let directory = wide_null(workdir.as_os_str());

    let result = unsafe {
        ShellExecuteW(
            0 as HWND,
            operation.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            directory.as_ptr(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if result > 32 {
        std::process::exit(0);
    }

    Err(anyhow!("administrator elevation was cancelled or failed"))
}

#[cfg(not(windows))]
pub fn ensure_elevated() -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn quote_arg(arg: std::ffi::OsString) -> String {
    let value = arg.to_string_lossy();
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.into_owned()
    }
}

#[cfg(windows)]
fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
