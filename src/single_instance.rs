use anyhow::Result;

#[cfg(windows)]
mod platform {
    use super::Result;
    use anyhow::anyhow;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};

    pub struct SingleInstanceGuard {
        handle: HANDLE,
    }

    impl Drop for SingleInstanceGuard {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    CloseHandle(self.handle);
                }
            }
        }
    }

    pub fn acquire_or_notify() -> Result<Option<SingleInstanceGuard>> {
        let name = wide_null("Local\\ivLis-Studio.net-combiner");
        let handle = unsafe { CreateMutexW(ptr::null(), 1, name.as_ptr()) };
        if handle.is_null() {
            return Err(anyhow!("failed to create single-instance mutex"));
        }

        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                MessageBoxW(
                    0 as HWND,
                    wide_null("net-combiner is already running.\n이미 실행 중입니다.").as_ptr(),
                    wide_null("net-combiner").as_ptr(),
                    MB_OK | MB_ICONINFORMATION,
                );
                CloseHandle(handle);
            }
            return Ok(None);
        }

        Ok(Some(SingleInstanceGuard { handle }))
    }

    fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
        value
            .as_ref()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Result;

    pub struct SingleInstanceGuard;

    pub fn acquire_or_notify() -> Result<Option<SingleInstanceGuard>> {
        Ok(Some(SingleInstanceGuard))
    }
}

pub use platform::{SingleInstanceGuard, acquire_or_notify};
