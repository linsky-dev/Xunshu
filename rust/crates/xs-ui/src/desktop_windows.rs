//! Windows 原生托盘与渲染进程监管。宿主自身不初始化图形引擎或映射索引。
use super::{
    limit_query, receive, send, DoubleCtrl, HostMessage, Key, RendererMessage, View,
    MAX_FRAME_BYTES,
};
use anyhow::{ensure, Context as _, Result};
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    io::BufReader,
    mem::{size_of, size_of_val, zeroed},
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    System::{
        DataExchange::COPYDATASTRUCT,
        JobObjects::*,
        LibraryLoader::GetModuleHandleW,
        Registry::*,
        Threading::{CreateMutexW, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW},
    },
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const WINDOW_CLASS: &str = "Xunshu.DesktopHost";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const TRAY_MESSAGE: u32 = WM_APP + 1;
const HOTKEY_MESSAGE: u32 = WM_APP + 2;
const RENDERER_MESSAGE: u32 = WM_APP + 3;
const COPY_ID: usize = 0x5853_4855;
const TRAY_KEY_SELECT: u32 = NIN_SELECT | NINF_KEY;
const TIMER: usize = 1;
const MENU_QUICK: i32 = 101;
const MENU_MAIN: i32 = 102;
const MENU_AUTOSTART: i32 = 103;
const MENU_EXIT: i32 = 104;

enum Event {
    Show(View, Option<String>),
    ToggleQuick,
    TrayMenu,
    AddTrayIcon,
    Exit,
}

thread_local! {
    static EVENTS: RefCell<Option<SyncSender<Event>>> = const { RefCell::new(None) };
    static HWND: Cell<HWND> = const { Cell::new(null_mut()) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static DOUBLE_CTRL: RefCell<DoubleCtrl> = RefCell::new(DoubleCtrl::default());
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

fn os_error(message: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::last_os_error()).context(message.to_owned())
}

fn queue(event: Event) -> bool {
    EVENTS.with_borrow(|sender| sender.as_ref().is_some_and(|tx| tx.try_send(event).is_ok()))
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COPYDATA => {
            // WM_COPYDATA 的内存只在回调内有效。仅接受有界的激活消息。
            if lparam == 0 {
                return 0;
            }
            let data = unsafe { &*(lparam as *const COPYDATASTRUCT) };
            if data.dwData != COPY_ID
                || data.cbData as usize > MAX_FRAME_BYTES
                || data.lpData.is_null()
            {
                return 0;
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize)
            };
            if let Ok(HostMessage::Show { view, mut query }) = serde_json::from_slice(bytes) {
                if let Some(query) = &mut query {
                    limit_query(query);
                }
                return queue(Event::Show(view, query)) as LRESULT;
            }
            0
        }
        HOTKEY_MESSAGE => {
            remember_active_file_dialog();
            if !is_foreground_fullscreen() {
                queue(Event::ToggleQuick);
            }
            0
        }
        TRAY_MESSAGE => {
            match (lparam as u32) & 0xffff {
                WM_CONTEXTMENU | WM_RBUTTONUP => {
                    queue(Event::TrayMenu);
                }
                NIN_SELECT | TRAY_KEY_SELECT | WM_LBUTTONUP => {
                    queue(Event::Show(View::Quick, None));
                }
                _ => {}
            }
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            queue(Event::Exit);
            0
        }
        WM_CLOSE => {
            queue(Event::Exit);
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_TIMER | RENDERER_MESSAGE => 0,
        _ if message != 0 && TASKBAR_CREATED.with(Cell::get) == message => {
            queue(Event::AddTrayIcon);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam != 0 {
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if event.flags & LLKHF_INJECTED == 0 {
            let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let key = match event.vkCode as u16 {
                VK_LCONTROL => Key::LeftCtrl,
                VK_RCONTROL => Key::RightCtrl,
                VK_CONTROL if event.flags & LLKHF_EXTENDED != 0 => Key::RightCtrl,
                VK_CONTROL => Key::LeftCtrl,
                _ => Key::Other,
            };
            let held_modifier = !matches!(key, Key::Other)
                && [VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
                    .iter()
                    .any(|key| unsafe { GetAsyncKeyState(i32::from(*key)) } < 0);
            let toggle = DOUBLE_CTRL.with_borrow_mut(|state| {
                if held_modifier {
                    state.event(Key::Other, pressed, event.time);
                }
                let toggle = state.event(key, pressed, event.time);
                if held_modifier && pressed {
                    state.event(Key::Other, true, event.time);
                }
                toggle
            });
            if toggle {
                HWND.with(|hwnd| unsafe { PostMessageW(hwnd.get(), HOTKEY_MESSAGE, 0, 0) });
            }
        }
    }
    // 观察手势，不截获用户输入，也不把其它按键转发给应用。
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct HostWindow(HWND);
impl Drop for HostWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

struct KeyboardHook(HHOOK);
impl Drop for KeyboardHook {
    fn drop(&mut self) {
        unsafe { UnhookWindowsHookEx(self.0) };
    }
}

struct Tray(HWND);
impl Tray {
    fn data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.0;
        data.uID = 1;
        data
    }
    fn add(&self) -> bool {
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = TRAY_MESSAGE;
        data.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        let tip = wide("寻枢 · 双击 Ctrl 唤起快捷搜索");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
                return false;
            }
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
        true
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data()) };
    }
}

fn configure_job() -> Result<Handle> {
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(os_error("无法创建桌面进程监管对象"));
    }
    let job = Handle(job);
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(os_error("无法设置桌面进程监管对象"));
    }
    Ok(job)
}

struct Renderer {
    child: Child,
    _job: Handle,
    commands: SyncSender<HostMessage>,
    updates: Receiver<RendererMessage>,
    view: View,
    close_deadline: Option<Instant>,
}

impl Renderer {
    fn spawn(
        hwnd: HWND,
        executable: &Path,
        data_dir: &Path,
        font: Option<&Path>,
        view: View,
        queries: [String; 2],
    ) -> Result<Self> {
        let job = configure_job()?;
        let mut command = Command::new(executable);
        command
            .arg("--desktop-render")
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(font) = font {
            command.arg("--font").arg(font);
        }
        let mut child = command.spawn().context("无法启动寻枢界面")?;
        // 子进程先等待 Boot；分配到 Job 后才允许初始化窗口或启动工作。
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) } == 0 {
            let error = os_error("无法监管寻枢界面进程");
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        unsafe { AllowSetForegroundWindow(child.id()) };
        let mut pipe_in = child.stdin.take().context("界面控制管道未建立")?;
        let pipe_out = child.stdout.take().context("界面状态管道未建立")?;
        let (commands, control) = mpsc::sync_channel(8);
        let (state, updates) = mpsc::sync_channel(32);
        commands.try_send(HostMessage::Boot { view, queries })?;
        thread::spawn(move || {
            while let Ok(command) = control.recv() {
                if send(&mut pipe_in, &command).is_err() {
                    break;
                }
            }
        });
        let hwnd_address = hwnd as usize;
        thread::spawn(move || {
            let mut input = BufReader::new(pipe_out);
            while let Ok(Some(update)) = receive(&mut input) {
                if state.send(update).is_err() {
                    break;
                }
                unsafe { PostMessageW(hwnd_address as HWND, RENDERER_MESSAGE, 0, 0) };
            }
            unsafe { PostMessageW(hwnd_address as HWND, RENDERER_MESSAGE, 0, 0) };
        });
        Ok(Self {
            child,
            _job: job,
            commands,
            updates,
            view,
            close_deadline: None,
        })
    }

    fn hide(&mut self) {
        if self.close_deadline.is_none() {
            let _ = self.commands.try_send(HostMessage::Hide);
            self.close_deadline = Some(Instant::now() + Duration::from_secs(3));
        }
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|status| status.is_some())
            || self
                .close_deadline
                .is_some_and(|limit| Instant::now() >= limit)
    }
}

struct WatchProcess {
    child: Child,
    job: Option<Handle>,
}

impl WatchProcess {
    fn start(executable: &Path, data_dir: &Path) -> Result<Self> {
        xs_platform::validate_local_path(executable, false)?;
        let job = configure_job()?;
        let mut child = Command::new(executable)
            .args([
                OsStr::new("--data-dir"),
                data_dir.as_os_str(),
                OsStr::new("watch"),
            ])
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("无法启动寻枢后台监听")?;
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) } == 0 {
            let error = os_error("无法监管寻枢后台监听");
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            child,
            job: Some(job),
        })
    }
}

impl Drop for WatchProcess {
    fn drop(&mut self) {
        // 停止监听后才允许手工重扫持有写入锁。
        drop(self.job.take());
        let _ = self.child.wait();
    }
}

enum ScanEvent {
    Progress(xs_platform::ScanProgress),
    Finished(Result<crate::ScanSummary, String>),
}

struct ScanJob {
    cancel: Arc<AtomicBool>,
    events: Receiver<ScanEvent>,
    progress: Option<xs_platform::ScanProgress>,
}

impl Drop for ScanJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct Host {
    hwnd: HWND,
    executable: PathBuf,
    data_dir: PathBuf,
    font: Option<PathBuf>,
    autostart_name: String,
    renderer: Option<Renderer>,
    queries: [String; 2],
    pending_view: Option<View>,
    watch: Option<WatchProcess>,
    watch_paused: bool,
    watch_retry: Instant,
    scan: Option<ScanJob>,
    scan_finished: Option<Result<crate::ScanSummary, String>>,
}

impl Host {
    fn show(&mut self, view: View, query: Option<String>) -> Result<()> {
        if let Some(query) = &query {
            self.queries[view.index()] = query.clone();
        }
        if let Some(renderer) = &mut self.renderer {
            if renderer.close_deadline.is_some() {
                self.pending_view = Some(view);
            } else {
                unsafe { AllowSetForegroundWindow(renderer.child.id()) };
                let dialog_hwnd = LAST_FILE_DIALOG.load(Ordering::Acquire);
                if dialog_hwnd != 0 {
                    let _ = renderer
                        .commands
                        .try_send(HostMessage::FileDialog(dialog_hwnd));
                }
                if renderer
                    .commands
                    .try_send(HostMessage::Show { view, query })
                    .is_ok()
                {
                    renderer.view = view;
                } else {
                    self.pending_view = Some(view);
                    renderer.hide();
                }
            }
        } else {
            self.renderer = Some(Renderer::spawn(
                self.hwnd,
                &self.executable,
                &self.data_dir,
                self.font.as_deref(),
                view,
                self.queries.clone(),
            )?);
        }
        Ok(())
    }

    fn update(&mut self) -> Result<()> {
        self.update_scan();
        self.update_watch();
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };
        let mut ready = false;
        let mut start_scan = false;
        let mut cancel_scan = false;
        for update in renderer.updates.try_iter() {
            match update {
                RendererMessage::Ready => ready = true,
                RendererMessage::Failure(message) => show_error(&message),
                RendererMessage::View(view) => renderer.view = view,
                RendererMessage::Query { view, mut query } => {
                    limit_query(&mut query);
                    self.queries[view.index()] = query;
                }
                RendererMessage::Closing => {
                    renderer
                        .close_deadline
                        .get_or_insert(Instant::now() + Duration::from_secs(3));
                }
                RendererMessage::StartScan => start_scan = true,
                RendererMessage::CancelScan => cancel_scan = true,
            }
        }
        if renderer.exited() {
            self.renderer = None;
            if let Some(view) = self.pending_view.take() {
                self.show(view, None)?;
            }
        }
        if start_scan {
            self.start_scan();
        }
        if cancel_scan {
            if let Some(scan) = &self.scan {
                scan.cancel.store(true, Ordering::Relaxed);
            }
        }
        if let Some(renderer) = &self.renderer {
            if ready {
                if let Some(scan) = &self.scan {
                    let message = scan
                        .progress
                        .clone()
                        .map_or(HostMessage::ScanStarted, HostMessage::ScanProgress);
                    let _ = renderer.commands.try_send(message);
                }
            }
            if renderer.close_deadline.is_none() {
                if let Some(result) = self.scan_finished.take() {
                    if let Err(
                        mpsc::TrySendError::Full(HostMessage::ScanFinished(result))
                        | mpsc::TrySendError::Disconnected(HostMessage::ScanFinished(result)),
                    ) = renderer
                        .commands
                        .try_send(HostMessage::ScanFinished(result))
                    {
                        self.scan_finished = Some(result);
                    }
                }
            }
        }
        Ok(())
    }

    fn start_scan(&mut self) {
        if self.scan.is_some() {
            return;
        }
        self.scan_finished = None;
        self.watch = None;
        self.watch_paused = true;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let dir = self.data_dir.clone();
        let hwnd_address = self.hwnd as usize;
        let (tx, events) = mpsc::sync_channel(8);
        thread::spawn(move || {
            let result = crate::run_scan(&dir, &worker_cancel, |progress| {
                let _ = tx.try_send(ScanEvent::Progress(progress));
                unsafe { PostMessageW(hwnd_address as HWND, RENDERER_MESSAGE, 0, 0) };
            });
            let _ = tx.send(ScanEvent::Finished(result));
            unsafe { PostMessageW(hwnd_address as HWND, RENDERER_MESSAGE, 0, 0) };
        });
        self.scan = Some(ScanJob {
            cancel,
            events,
            progress: None,
        });
    }

    fn update_scan(&mut self) {
        let Some(scan) = &mut self.scan else {
            return;
        };
        let mut finished = None;
        for event in scan.events.try_iter() {
            match event {
                ScanEvent::Progress(progress) => {
                    scan.progress = Some(progress.clone());
                    if let Some(renderer) = &self.renderer {
                        let _ = renderer
                            .commands
                            .try_send(HostMessage::ScanProgress(progress));
                    }
                }
                ScanEvent::Finished(result) => finished = Some(result),
            }
        }
        if let Some(result) = finished {
            self.watch_paused = !result
                .as_ref()
                .is_ok_and(|summary| summary.published && !summary.cancelled);
            self.watch_retry = Instant::now() + Duration::from_secs(2);
            self.scan_finished = Some(result);
            self.scan = None;
        }
    }

    fn update_watch(&mut self) {
        if let Some(watch) = &mut self.watch {
            if !watch.child.try_wait().is_ok_and(|status| status.is_none()) {
                self.watch = None;
                self.watch_retry = Instant::now() + Duration::from_secs(5);
            }
            return;
        }
        if self.watch_paused || self.scan.is_some() || Instant::now() < self.watch_retry {
            return;
        }
        self.watch_retry = Instant::now() + Duration::from_secs(5);
        if xs_core::operations::load_settings(&self.data_dir)
            .is_ok_and(|settings| !settings.roots.is_empty())
        {
            let cli = self.executable.with_file_name("xunshu.exe");
            self.watch = WatchProcess::start(&cli, &self.data_dir).ok();
        }
    }

    fn toggle(&mut self) -> Result<()> {
        if let Some(renderer) = &mut self.renderer {
            if renderer.view == View::Quick && renderer.close_deadline.is_none() {
                renderer.hide();
                return Ok(());
            }
        }
        self.show(View::Quick, None)
    }

    fn tray_menu(&mut self) -> Result<bool> {
        let enabled = self.autostart_enabled()?;
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return Err(os_error("无法打开托盘菜单"));
        }
        unsafe {
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_QUICK as usize,
                wide("快捷搜索").as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_MAIN as usize,
                wide("打开主窗口").as_ptr(),
            );
            AppendMenuW(
                menu,
                if enabled { MF_CHECKED } else { MF_UNCHECKED },
                MENU_AUTOSTART as usize,
                wide("登录时启动").as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_EXIT as usize,
                wide("退出寻枢").as_ptr(),
            );
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor);
            SetForegroundWindow(self.hwnd);
            let selection = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                cursor.x,
                cursor.y,
                0,
                self.hwnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            match selection {
                MENU_QUICK => self.show(View::Quick, None)?,
                MENU_MAIN => self.show(View::Main, None)?,
                MENU_AUTOSTART => self.set_autostart(!enabled)?,
                MENU_EXIT => return Ok(true),
                _ => {}
            }
        }
        Ok(false)
    }

    fn autostart_value(&self) -> Vec<u16> {
        // 使用 Windows 参数规则保留 Unicode、引号与末尾反斜杠。
        let mut command = Vec::new();
        quote_arg(&mut command, self.executable.as_os_str());
        command.extend(" --background --data-dir ".encode_utf16());
        quote_arg(&mut command, self.data_dir.as_os_str());
        if let Some(font) = &self.font {
            command.extend(" --font ".encode_utf16());
            quote_arg(&mut command, font.as_os_str());
        }
        command.push(0);
        command
    }

    fn autostart_enabled(&self) -> Result<bool> {
        let Some(key) = run_key(KEY_QUERY_VALUE)? else {
            return Ok(false);
        };
        let expected = self.autostart_value();
        let mut actual = vec![0_u16; expected.len()];
        let mut size = (actual.len() * size_of::<u16>()) as u32;
        let mut kind = 0;
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                wide(&self.autostart_name).as_ptr(),
                null_mut(),
                &mut kind,
                actual.as_mut_ptr().cast(),
                &mut size,
            )
        };
        match result {
            ERROR_SUCCESS => Ok(kind == REG_SZ
                && size as usize == size_of_val(actual.as_slice())
                && actual == expected),
            ERROR_FILE_NOT_FOUND | ERROR_MORE_DATA => Ok(false),
            error => {
                Err(std::io::Error::from_raw_os_error(error as i32)).context("无法读取登录自启设置")
            }
        }
    }

    fn set_autostart(&self, enabled: bool) -> Result<()> {
        let key =
            run_key(KEY_QUERY_VALUE | KEY_SET_VALUE)?.context("Windows 登录自启位置不可用")?;
        let name = wide(&self.autostart_name);
        let result = if enabled {
            let value = self.autostart_value();
            unsafe {
                RegSetValueExW(
                    key.0,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr().cast(),
                    size_of_val(value.as_slice()) as u32,
                )
            }
        } else if self.autostart_enabled()? {
            unsafe { RegDeleteValueW(key.0, name.as_ptr()) }
        } else {
            ERROR_SUCCESS
        };
        ensure!(
            result == ERROR_SUCCESS,
            "无法更新登录自启设置：{}",
            std::io::Error::from_raw_os_error(result as i32)
        );
        Ok(())
    }
}

struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn run_key(access: REG_SAM_FLAGS) -> Result<Option<RegistryKey>> {
    let mut handle = null_mut();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            access,
            &mut handle,
        )
    };
    match result {
        ERROR_SUCCESS => Ok(Some(RegistryKey(handle))),
        ERROR_FILE_NOT_FOUND => Ok(None),
        error => Err(std::io::Error::from_raw_os_error(error as i32))
            .context("无法访问当前用户的登录自启设置"),
    }
}

fn quote_arg(output: &mut Vec<u16>, argument: &OsStr) {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    output.push(QUOTE);
    let mut slashes = 0;
    for unit in argument.encode_wide() {
        if unit == BACKSLASH {
            slashes += 1;
        } else {
            output.extend(std::iter::repeat_n(
                BACKSLASH,
                if unit == QUOTE {
                    2 * slashes + 1
                } else {
                    slashes
                },
            ));
            slashes = 0;
            output.push(unit);
        }
    }
    output.extend(std::iter::repeat_n(BACKSLASH, 2 * slashes));
    output.push(QUOTE);
}

fn profile_id(data_dir: &Path) -> String {
    let path = data_dir.to_string_lossy().replace('/', "\\");
    let normalized = path.trim_end_matches('\\').to_lowercase();
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in normalized.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("寻枢.{hash:016x}")
}

fn activate_existing(title: &[u16], view: View, query: String) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let class = wide(WINDOW_CLASS);
    let data = serde_json::to_vec(&HostMessage::Show {
        view,
        query: (!query.is_empty()).then_some(query),
    })?;
    let frame = COPYDATASTRUCT {
        dwData: COPY_ID,
        cbData: data.len() as u32,
        lpData: data.as_ptr().cast_mut().cast(),
    };
    while Instant::now() < deadline {
        let hwnd = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
        if !hwnd.is_null() {
            let mut response = 0;
            let mut process_id = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut process_id);
                AllowSetForegroundWindow(process_id);
            }
            let sent = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_COPYDATA,
                    0,
                    (&raw const frame) as LPARAM,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    500,
                    &mut response,
                )
            };
            if sent != 0 && response != 0 {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("寻枢已经运行，但暂时无法唤起现有窗口")
}

pub fn run(
    data_dir: PathBuf,
    font: Option<PathBuf>,
    query: String,
    view: View,
    background: bool,
) -> Result<()> {
    xs_platform::validate_local_path(&data_dir, true)?;
    if let Some(font) = &font {
        xs_platform::validate_local_path(font, false)?;
    }
    let profile = profile_id(&data_dir);
    let name = wide(format!("Local\\Xunshu.Desktop.{profile}"));
    let mutex = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    let last_error = unsafe { GetLastError() };
    if mutex.is_null() {
        return Err(os_error("无法创建寻枢实例锁"));
    }
    let _mutex = Handle(mutex);
    let title = wide(&profile);
    if last_error == ERROR_ALREADY_EXISTS {
        return if background {
            Ok(())
        } else {
            activate_existing(&title, view, query)
        };
    }
    let class = wide(WINDOW_CLASS);
    let instance = unsafe { GetModuleHandleW(null()) };
    let mut window_class: WNDCLASSW = unsafe { zeroed() };
    window_class.lpfnWndProc = Some(window_proc);
    window_class.hInstance = instance;
    window_class.lpszClassName = class.as_ptr();
    if unsafe { RegisterClassW(&window_class) } == 0 {
        return Err(os_error("无法注册寻枢常驻窗口"));
    }
    let (sender, events) = mpsc::sync_channel(32);
    EVENTS.with_borrow_mut(|slot| *slot = Some(sender));
    TASKBAR_CREATED
        .with(|slot| slot.set(unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }));
    // 需要接收 Explorer 广播，使用不可见的顶层窗口。
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(os_error("无法创建寻枢常驻窗口"));
    }
    let _window = HostWindow(hwnd);
    HWND.with(|slot| slot.set(hwnd));
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0) };
    if hook.is_null() {
        return Err(os_error("无法启用全局双 Ctrl"));
    }
    let _hook = KeyboardHook(hook);
    let tray = Tray(hwnd);
    ensure!(tray.add(), "Windows 托盘不可用，无法启动寻枢常驻模式");
    let mut queries = [String::new(), String::new()];
    queries[view.index()] = query;
    let mut host = Host {
        hwnd,
        executable: std::env::current_exe()?,
        data_dir,
        font,
        autostart_name: format!("Xunshu.{profile}"),
        renderer: None,
        queries,
        pending_view: None,
        watch: None,
        watch_paused: false,
        watch_retry: Instant::now(),
        scan: None,
        scan_finished: None,
    };
    ensure!(
        unsafe { SetTimer(hwnd, TIMER, 250, None) } != 0,
        "无法设置寻枢常驻状态计时器"
    );
    host.update_watch();
    if !background {
        host.show(view, None)?;
    }
    let mut message: MSG = unsafe { zeroed() };
    loop {
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(os_error("寻枢常驻消息循环失败")),
            0 => break,
            _ => unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
        host.update()?;
        for event in events.try_iter() {
            let result = match event {
                Event::Show(view, query) => host.show(view, query).map(|_| false),
                Event::ToggleQuick => host.toggle().map(|_| false),
                Event::TrayMenu => host.tray_menu(),
                Event::AddTrayIcon => {
                    tray.add();
                    Ok(false)
                }
                Event::Exit => Ok(true),
            };
            match result {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => show_error(&format!("{error:#}")),
            }
        }
    }
    Ok(())
}

pub fn show_error(message: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            wide("寻枢").as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

pub fn native_window(context: &eframe::CreationContext<'_>) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match context.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}

/// 只在用户显式唤起的短窗口期使用。原生窗口首次显示前由调用者重试。
pub fn focus_from_gesture(window: HWND) -> bool {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    unsafe {
        if IsWindowVisible(window) == 0 || IsIconic(window) != 0 {
            return false;
        }
        if GetForegroundWindow() == window || SetForegroundWindow(window) != 0 {
            return true;
        }
        let foreground = GetForegroundWindow();
        let current_thread = GetCurrentThreadId();
        if !foreground.is_null() {
            let foreground_thread = GetWindowThreadProcessId(foreground, null_mut());
            if foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, 1) != 0
            {
                SetForegroundWindow(window);
                AttachThreadInput(current_thread, foreground_thread, 0);
            }
        }
        true
    }
}

pub fn open_local_path(path: &Path) -> Result<()> {
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            null(),
            wide(path.as_os_str()).as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    ensure!(
        result > 32,
        "系统无法打开选中的本地文件（错误代码 {result}）"
    );
    Ok(())
}

/// 把已重新授权的单个文件交给系统拖放，仅允许复制。
pub fn drag_local_file(window: HWND, path: &Path) -> Result<()> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::HWND as OleWindow,
            System::{
                Com::{IBindCtx, IDataObject},
                Ole::{IDropSource, OleInitialize, OleUninitialize, DROPEFFECT_COPY},
            },
            UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName, SHDoDragDrop},
        },
    };

    struct OleGuard;
    impl Drop for OleGuard {
        fn drop(&mut self) {
            // SAFETY: 只在同一线程的 OleInitialize 成功之后建立守卫。
            unsafe { OleUninitialize() };
        }
    }

    // SAFETY: 在渲染窗口所属线程调用，保留参数为空。
    unsafe { OleInitialize(None) }.context("无法初始化 Windows 拖放")?;
    let _ole = OleGuard;
    let path = wide(path.as_os_str());
    // SAFETY: 路径已重新授权，UTF-16 缓冲区有终止符并覆盖整个调用。
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None::<&IBindCtx>) }
            .context("无法创建本地文件的拖放项目")?;
    // SAFETY: 绑定官方数据对象处理器，COM 接口在本线程内存活。
    let data: IDataObject = unsafe { item.BindToHandler(None::<&IBindCtx>, &BHID_DataObject) }
        .context("无法建立文件拖放数据")?;
    // SAFETY: 传入本线程的渲染窗口及有效 COM 接口，由 Shell 处理拖放和取消。
    unsafe {
        SHDoDragDrop(
            Some(OleWindow(window)),
            &data,
            None::<&IDropSource>,
            DROPEFFECT_COPY,
        )
    }
    .context("Windows 未能完成文件拖放")?;
    Ok(())
}

/// 检测当前前台窗口是否为全屏游戏或全屏演示，避免双 Ctrl 误触打断用户。
fn is_foreground_fullscreen() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() || fg == HWND.with(Cell::get) {
            return false;
        }
        let mut class_buf = [0u16; 64];
        let class_len = GetClassNameW(fg, class_buf.as_mut_ptr(), class_buf.len() as i32);
        if class_len > 0 {
            let class_name = String::from_utf16_lossy(&class_buf[..class_len as usize]);
            if matches!(class_name.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
                return false;
            }
        }
        let mut win_rect = zeroed::<RECT>();
        if GetWindowRect(fg, &mut win_rect) == 0 {
            return false;
        }
        let monitor = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return false;
        }
        let mut info = zeroed::<MONITORINFO>();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return false;
        }
        let mon = info.rcMonitor;
        win_rect.left <= mon.left
            && win_rect.top <= mon.top
            && win_rect.right >= mon.right
            && win_rect.bottom >= mon.bottom
    }
}

static LAST_FILE_DIALOG: AtomicIsize = AtomicIsize::new(0);

fn is_standard_file_dialog(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    unsafe {
        if IsWindow(hwnd) == 0 {
            return false;
        }
        let mut class_buf = [0u16; 64];
        let len = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
        len > 0 && String::from_utf16_lossy(&class_buf[..len as usize]) == "#32770"
    }
}

/// 在唤起寻枢窗口前缓存当前前台的 Windows 打开/保存对话框句柄，避免焦点切换后丢失目标。
fn remember_active_file_dialog() {
    let fg = unsafe { GetForegroundWindow() };
    if is_standard_file_dialog(fg) {
        LAST_FILE_DIALOG.store(fg as isize, Ordering::Release);
    }
}

pub(crate) fn set_cached_file_dialog(hwnd: isize) {
    LAST_FILE_DIALOG.store(hwnd, Ordering::Release);
}

/// Listary 式 Quick Switch 底层入口：优先使用唤起前缓存的标准 Windows 文件对话框（#32770）
/// 句柄（其次检查当前前台），将路径直接导航到目标目录。
pub(crate) fn jump_active_file_dialog(target_dir: &Path) -> Result<bool> {
    let target = target_dir.to_str().context("目标目录路径无效")?;
    unsafe {
        let cached = LAST_FILE_DIALOG.load(Ordering::Acquire) as HWND;
        let dialog = if is_standard_file_dialog(cached) {
            cached
        } else {
            let fg = GetForegroundWindow();
            if is_standard_file_dialog(fg) {
                fg
            } else {
                return Ok(false);
            }
        };
        let combo_ex = FindWindowExW(dialog, null_mut(), wide("ComboBoxEx32").as_ptr(), null());
        let combo = if !combo_ex.is_null() {
            FindWindowExW(combo_ex, null_mut(), wide("ComboBox").as_ptr(), null())
        } else {
            null_mut()
        };
        let edit = if !combo.is_null() {
            FindWindowExW(combo, null_mut(), wide("Edit").as_ptr(), null())
        } else {
            FindWindowExW(dialog, null_mut(), wide("Edit").as_ptr(), null())
        };
        if edit.is_null() {
            return Ok(false);
        }
        let path_w = wide(target);
        SendMessageW(edit, WM_SETTEXT, 0, path_w.as_ptr() as LPARAM);
        PostMessageW(edit, WM_KEYDOWN, VK_RETURN as WPARAM, 0);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    #[test]
    fn login_arguments_quote_spaces_quotes_and_trailing_slashes() {
        let mut output = Vec::new();
        quote_arg(&mut output, OsStr::new("D:\\含 空格\\"));
        assert_eq!(OsString::from_wide(&output), "\"D:\\含 空格\\\\\"");
        output.clear();
        quote_arg(&mut output, OsStr::new("a\"b"));
        assert_eq!(OsString::from_wide(&output), "\"a\\\"b\"");
    }

    #[test]
    fn single_instance_key_normalizes_windows_path_spelling() {
        assert_eq!(
            profile_id(Path::new("D:\\Xunshu\\")),
            profile_id(Path::new("d:/xunshu"))
        );
        assert_ne!(
            profile_id(Path::new("D:\\Xunshu")),
            profile_id(Path::new("D:\\other"))
        );
    }
}
