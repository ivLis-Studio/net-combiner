#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayLanguage {
    English,
    Korean,
}

#[derive(Debug)]
pub enum TrayEvent {
    ShowMain,
    OpenOptions,
    OpenConnections,
    Quit,
}

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use super::{TrayEvent, TrayLanguage};
    use anyhow::{Result, anyhow};
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::{Arc, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
        DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
        GetSystemMetrics, GetWindowLongPtrW, HICON, HMENU, IDI_APPLICATION, LoadIconW,
        MF_SEPARATOR, MF_STRING, MSG, PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW,
        SM_CXSMICON, SM_CYSMICON, SetForegroundWindow, SetWindowLongPtrW, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND,
        WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_QUIT, WM_RBUTTONUP, WNDCLASSW,
    };

    const WM_TRAY: u32 = WM_APP + 31;
    const WM_TRAY_SHUTDOWN: u32 = WM_APP + 32;
    const WM_TRAY_LANGUAGE: u32 = WM_APP + 33;
    const TRAY_UID: u32 = 1;

    const ID_SHOW: usize = 1001;
    const ID_CONNECTIONS: usize = 1002;
    const ID_OPTIONS: usize = 1003;
    const ID_QUIT: usize = 1004;

    #[derive(Debug)]
    enum TrayCommand {
        SetLanguage(TrayLanguage),
        Shutdown,
    }

    pub struct TrayHandle {
        tx: mpsc::Sender<TrayCommand>,
        hwnd: isize,
        thread: Option<JoinHandle<()>>,
    }

    impl TrayHandle {
        pub fn new(
            language: TrayLanguage,
            event_tx: mpsc::Sender<TrayEvent>,
            wake: Arc<dyn Fn() + Send + Sync>,
        ) -> Result<Self> {
            let (cmd_tx, cmd_rx) = mpsc::channel();
            let (hwnd_tx, hwnd_rx) = mpsc::channel();

            let thread = thread::Builder::new()
                .name("net-combiner-tray".to_owned())
                .spawn(move || tray_thread(language, event_tx, wake, cmd_rx, hwnd_tx))?;

            let hwnd = hwnd_rx
                .recv_timeout(Duration::from_secs(3))
                .map_err(|_| anyhow!("tray window did not initialize"))??;

            Ok(Self {
                tx: cmd_tx,
                hwnd,
                thread: Some(thread),
            })
        }

        pub fn set_language(&self, language: TrayLanguage) {
            let _ = self.tx.send(TrayCommand::SetLanguage(language));
            unsafe {
                let _ = PostMessageW(self.hwnd as HWND, WM_TRAY_LANGUAGE, 0, 0);
            }
        }
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            let _ = self.tx.send(TrayCommand::Shutdown);
            unsafe {
                let _ = PostMessageW(self.hwnd as HWND, WM_TRAY_SHUTDOWN, 0, 0);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub fn is_supported() -> bool {
        true
    }

    fn tray_thread(
        language: TrayLanguage,
        event_tx: mpsc::Sender<TrayEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
        cmd_rx: mpsc::Receiver<TrayCommand>,
        hwnd_tx: mpsc::Sender<Result<isize>>,
    ) {
        let mut runtime = Box::new(TrayRuntime {
            language,
            event_tx,
            wake,
            icon: ptr::null_mut(),
            custom_icon: false,
        });
        let result = unsafe { init_tray_window(runtime.as_mut()) };
        let hwnd = match result {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = hwnd_tx.send(Err(error));
                return;
            }
        };
        let _ = hwnd_tx.send(Ok(hwnd as isize));

        loop {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, 0 as HWND, 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        cleanup_tray(hwnd);
                        return;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            while let Ok(command) = cmd_rx.try_recv() {
                match command {
                    TrayCommand::SetLanguage(language) => {
                        runtime.language = language;
                        unsafe {
                            modify_tip(hwnd, runtime.language);
                        }
                    }
                    TrayCommand::Shutdown => unsafe {
                        cleanup_tray(hwnd);
                        DestroyWindow(hwnd);
                        return;
                    },
                }
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    struct TrayRuntime {
        language: TrayLanguage,
        event_tx: mpsc::Sender<TrayEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
        icon: HICON,
        custom_icon: bool,
    }

    unsafe fn init_tray_window(runtime: &mut TrayRuntime) -> Result<HWND> {
        let class_name = wide_null("NetCombinerTrayWindow");
        let instance = GetModuleHandleW(ptr::null());
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance as HINSTANCE,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide_null("net-combiner tray").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            0 as HWND,
            0 as HMENU,
            instance as HINSTANCE,
            ptr::null::<c_void>(),
        );
        if hwnd.is_null() {
            return Err(anyhow!("failed to create tray window"));
        }

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, runtime as *mut TrayRuntime as isize);
        add_tray_icon(hwnd, runtime)?;
        Ok(hwnd)
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TRAY => {
                let mouse_msg = lparam as u32;
                if let Some(runtime) = runtime_for(hwnd) {
                    match mouse_msg {
                        WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                            emit_event(runtime, TrayEvent::ShowMain);
                        }
                        WM_RBUTTONUP => show_menu(hwnd, runtime),
                        _ => {}
                    }
                }
                0
            }
            WM_COMMAND => {
                if let Some(runtime) = runtime_for(hwnd) {
                    send_menu_event(wparam, runtime);
                }
                0
            }
            WM_TRAY_SHUTDOWN | WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                cleanup_tray(hwnd);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn runtime_for(hwnd: HWND) -> Option<&'static mut TrayRuntime> {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayRuntime;
        ptr.as_mut()
    }

    unsafe fn add_tray_icon(hwnd: HWND, runtime: &mut TrayRuntime) -> Result<()> {
        let mut data = notify_data(hwnd, runtime.language);
        let (icon, custom_icon) = match create_tray_icon() {
            Ok(icon) => (icon, true),
            Err(_) => (LoadIconW(0 as HINSTANCE, IDI_APPLICATION), false),
        };
        data.hIcon = icon;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            if custom_icon && !icon.is_null() {
                let _ = DestroyIcon(icon);
            }
            return Err(anyhow!("failed to add tray icon"));
        }
        runtime.icon = icon;
        runtime.custom_icon = custom_icon;
        Ok(())
    }

    unsafe fn modify_tip(hwnd: HWND, language: TrayLanguage) {
        let mut data = notify_data(hwnd, language);
        data.uFlags = NIF_TIP;
        let _ = Shell_NotifyIconW(windows_sys::Win32::UI::Shell::NIM_MODIFY, &data);
    }

    unsafe fn cleanup_tray(hwnd: HWND) {
        let data = notify_data(hwnd, TrayLanguage::English);
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        if let Some(runtime) = runtime_for(hwnd) {
            if runtime.custom_icon && !runtime.icon.is_null() {
                let _ = DestroyIcon(runtime.icon);
            }
            runtime.icon = ptr::null_mut();
            runtime.custom_icon = false;
        }
    }

    fn notify_data(hwnd: HWND, language: TrayLanguage) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            uCallbackMessage: WM_TRAY,
            ..Default::default()
        };
        fill_wide_array(&mut data.szTip, labels(language).tooltip);
        data
    }

    unsafe fn show_menu(hwnd: HWND, runtime: &TrayRuntime) {
        let labels = labels(runtime.language);
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }

        append_item(menu, ID_SHOW, labels.show);
        append_item(menu, ID_CONNECTIONS, labels.connections);
        append_item(menu, ID_OPTIONS, labels.options);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        append_item(menu, ID_QUIT, labels.quit);

        let mut point = POINT::default();
        if GetCursorPos(&mut point) != 0 {
            SetForegroundWindow(hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                point.x,
                point.y,
                0,
                hwnd,
                ptr::null(),
            );
            if command != 0 {
                send_menu_event(command as usize, runtime);
            }
            let _ = PostMessageW(hwnd, WM_NULL, 0, 0);
        }

        DestroyMenu(menu);
    }

    unsafe fn append_item(menu: HMENU, id: usize, text: &str) {
        let text = wide_null(text);
        let _ = AppendMenuW(menu, MF_STRING, id, text.as_ptr());
    }

    fn send_menu_event(id: usize, runtime: &TrayRuntime) {
        let event = match id {
            ID_SHOW => TrayEvent::ShowMain,
            ID_CONNECTIONS => TrayEvent::OpenConnections,
            ID_OPTIONS => TrayEvent::OpenOptions,
            ID_QUIT => TrayEvent::Quit,
            _ => return,
        };
        let _ = runtime.event_tx.send(event);
        (runtime.wake)();
    }

    fn emit_event(runtime: &TrayRuntime, event: TrayEvent) {
        let _ = runtime.event_tx.send(event);
        (runtime.wake)();
    }

    unsafe fn create_tray_icon() -> Result<HICON> {
        let icon =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/net-combiner-icon.png"))
                .map_err(|error| anyhow!("failed to load tray icon: {error}"))?;
        let size = tray_icon_size();
        let source = image::RgbaImage::from_raw(icon.width, icon.height, icon.rgba)
            .ok_or_else(|| anyhow!("invalid tray icon pixels"))?;
        let resized =
            image::imageops::resize(&source, size, size, image::imageops::FilterType::Lanczos3);

        let mut xor_bits = Vec::with_capacity((size * size * 4) as usize);
        for y in (0..size).rev() {
            for x in 0..size {
                let [r, g, b, a] = resized.get_pixel(x, y).0;
                xor_bits.extend_from_slice(&[b, g, r, a]);
            }
        }

        let mask_stride = size.div_ceil(32) * 4;
        let and_bits = vec![0_u8; (mask_stride * size) as usize];
        let hicon = CreateIcon(
            0 as HINSTANCE,
            size as i32,
            size as i32,
            1,
            32,
            and_bits.as_ptr(),
            xor_bits.as_ptr(),
        );
        if hicon.is_null() {
            return Err(anyhow!("failed to create tray icon"));
        }
        Ok(hicon)
    }

    unsafe fn tray_icon_size() -> u32 {
        let width = GetSystemMetrics(SM_CXSMICON).max(16);
        let height = GetSystemMetrics(SM_CYSMICON).max(16);
        width.max(height) as u32
    }

    struct Labels {
        tooltip: &'static str,
        show: &'static str,
        connections: &'static str,
        options: &'static str,
        quit: &'static str,
    }

    fn labels(language: TrayLanguage) -> Labels {
        match language {
            TrayLanguage::English => Labels {
                tooltip: "net-combiner",
                show: "Show net-combiner",
                connections: "Connection activity",
                options: "Options",
                quit: "Quit",
            },
            TrayLanguage::Korean => Labels {
                tooltip: "net-combiner",
                show: "net-combiner 열기",
                connections: "연결 상태",
                options: "옵션",
                quit: "종료",
            },
        }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn fill_wide_array<const N: usize>(target: &mut [u16; N], value: &str) {
        let encoded = value.encode_utf16().take(N.saturating_sub(1));
        for (slot, code) in target.iter_mut().zip(encoded) {
            *slot = code;
        }
    }

    pub use TrayHandle as PlatformTrayHandle;
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{TrayEvent, TrayLanguage};
    use anyhow::{Result, anyhow};
    use std::sync::{Arc, mpsc};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    const ID_SHOW: &str = "net-combiner.show";
    const ID_CONNECTIONS: &str = "net-combiner.connections";
    const ID_OPTIONS: &str = "net-combiner.options";
    const ID_QUIT: &str = "net-combiner.quit";

    pub struct PlatformTrayHandle {
        _tray_icon: TrayIcon,
        show_item: MenuItem,
        connections_item: MenuItem,
        options_item: MenuItem,
        quit_item: MenuItem,
    }

    impl PlatformTrayHandle {
        pub fn new(
            language: TrayLanguage,
            event_tx: mpsc::Sender<TrayEvent>,
            wake: Arc<dyn Fn() + Send + Sync>,
        ) -> Result<Self> {
            install_event_handlers(event_tx, wake);

            let labels = labels(language);
            let show_item = MenuItem::with_id(ID_SHOW, labels.show, true, None);
            let connections_item =
                MenuItem::with_id(ID_CONNECTIONS, labels.connections, true, None);
            let options_item = MenuItem::with_id(ID_OPTIONS, labels.options, true, None);
            let separator = PredefinedMenuItem::separator();
            let quit_item = MenuItem::with_id(ID_QUIT, labels.quit, true, None);
            let menu = Menu::with_items(&[
                &show_item,
                &connections_item,
                &options_item,
                &separator,
                &quit_item,
            ])?;

            let tray_icon = TrayIconBuilder::new()
                .with_tooltip(labels.tooltip)
                .with_icon(load_icon()?)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(true)
                .with_menu_on_right_click(true)
                .build()?;

            Ok(Self {
                _tray_icon: tray_icon,
                show_item,
                connections_item,
                options_item,
                quit_item,
            })
        }

        pub fn set_language(&self, language: TrayLanguage) {
            let labels = labels(language);
            self.show_item.set_text(labels.show);
            self.connections_item.set_text(labels.connections);
            self.options_item.set_text(labels.options);
            self.quit_item.set_text(labels.quit);
        }
    }

    pub fn is_supported() -> bool {
        true
    }

    fn install_event_handlers(
        event_tx: mpsc::Sender<TrayEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) {
        let menu_tx = event_tx.clone();
        let menu_wake = wake.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let tray_event = match event.id().as_ref() {
                ID_SHOW => Some(TrayEvent::ShowMain),
                ID_CONNECTIONS => Some(TrayEvent::OpenConnections),
                ID_OPTIONS => Some(TrayEvent::OpenOptions),
                ID_QUIT => Some(TrayEvent::Quit),
                _ => None,
            };
            if let Some(tray_event) = tray_event {
                let _ = menu_tx.send(tray_event);
                menu_wake();
            }
        }));

        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                let _ = event_tx.send(TrayEvent::ShowMain);
                wake();
            }
        }));
    }

    fn load_icon() -> Result<Icon> {
        let icon =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/net-combiner-icon.png"))
                .map_err(|error| anyhow!("failed to load tray icon: {error}"))?;
        Icon::from_rgba(icon.rgba, icon.width, icon.height)
            .map_err(|error| anyhow!("failed to create tray icon: {error}"))
    }

    struct Labels {
        tooltip: &'static str,
        show: &'static str,
        connections: &'static str,
        options: &'static str,
        quit: &'static str,
    }

    fn labels(language: TrayLanguage) -> Labels {
        match language {
            TrayLanguage::English => Labels {
                tooltip: "net-combiner",
                show: "Show net-combiner",
                connections: "Connection activity",
                options: "Options",
                quit: "Quit",
            },
            TrayLanguage::Korean => Labels {
                tooltip: "net-combiner",
                show: "net-combiner 열기",
                connections: "연결 상태",
                options: "옵션",
                quit: "종료",
            },
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::{TrayEvent, TrayLanguage};
    use anyhow::Result;
    use std::sync::{Arc, mpsc};

    pub struct PlatformTrayHandle;

    impl PlatformTrayHandle {
        pub fn new(
            _language: TrayLanguage,
            _event_tx: mpsc::Sender<TrayEvent>,
            _wake: Arc<dyn Fn() + Send + Sync>,
        ) -> Result<Self> {
            Ok(Self)
        }

        pub fn set_language(&self, _language: TrayLanguage) {}
    }

    pub fn is_supported() -> bool {
        false
    }
}

pub use platform::PlatformTrayHandle as TrayHandle;

pub fn is_supported() -> bool {
    platform::is_supported()
}
