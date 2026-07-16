#![cfg_attr(windows, windows_subsystem = "windows")]

use rsduck::config::{self, RsduckConfig};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
#[cfg(target_os = "macos")]
use tray_icon::menu::Submenu;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};
#[cfg(windows)]
use windows_service::{
    service::{Service, ServiceAccess, ServiceState},
    service_manager::{ServiceManager, ServiceManagerAccess},
    Error as WindowsServiceError,
};

const SERVICE_NAME: &str = "rsduck";
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;
#[cfg(windows)]
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
#[cfg(windows)]
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
#[cfg(windows)]
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
#[cfg(windows)]
const WINDOWS_SERVICE_ACTION_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(windows)]
const WINDOWS_SERVICE_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(target_os = "macos")]
const MACOS_SERVICE_LABEL: &str = "com.dripai.rsduck";
const UPDATE_MANIFEST_URL: &str =
    "https://github.com/dripai/rsduck/releases/latest/download/rsduck-update.json";
#[cfg(windows)]
const WINDOWS_NOTIFICATION_APP_ID: &str = "com.dripai.rsduck.tray";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl ServiceAction {
    fn command_name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    StatusRefreshed(StatusRefresh),
    BackgroundActionProgress(String),
    BackgroundActionCompleted {
        action: BackgroundAction,
        result: Result<String, String>,
        exit_after: bool,
    },
}

#[derive(Debug)]
struct StatusRefresh {
    manager: String,
    action_result: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum BackgroundAction {
    Service(ServiceAction),
    Upgrade,
}

struct TrayMenu {
    status: MenuItem,
    open_web_sql: MenuItem,
    open_logs: MenuItem,
    service_toggle: MenuItem,
    restart: MenuItem,
    upgrade: MenuItem,
    quit: MenuItem,
}

#[derive(Debug, Deserialize)]
struct UpdateManifest {
    version: String,
    assets: HashMap<String, UpdateAsset>,
}

#[derive(Debug, Deserialize)]
struct UpdateAsset {
    url: String,
    sha256: String,
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--elevated") {
        run_elevated_command(&args[1..]);
        return;
    }

    prepare_runtime_directory().unwrap_or_else(|error| {
        eprintln!("prepare tray runtime directory failed: {error}");
        std::process::exit(1);
    });
    prepare_system_notifications();
    let cfg = config::load_config();
    run_tray(cfg).unwrap_or_else(|error| {
        eprintln!("rsduck tray failed: {error}");
        std::process::exit(1);
    });
}

fn prepare_runtime_directory() -> Result<(), String> {
    if let Some(directory) = std::env::var_os("RSDUCK_RUNTIME_DIR") {
        return std::env::set_current_dir(&directory).map_err(|error| {
            format!(
                "set tray runtime directory {} failed: {error}",
                PathBuf::from(directory).display()
            )
        });
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve tray executable failed: {error}"))?;
    let Some(directory) = executable.parent() else {
        return Err(format!(
            "tray executable has no parent directory: {}",
            executable.display()
        ));
    };
    std::env::set_current_dir(directory).map_err(|error| {
        format!(
            "set tray working directory {} failed: {error}",
            directory.display()
        )
    })
}

fn prepare_system_notifications() {
    #[cfg(windows)]
    if let Err(error) = register_windows_notification_app() {
        eprintln!("register Windows notifications failed: {error}");
    }
}

#[cfg(windows)]
fn register_windows_notification_app() -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let registry_path = wide_null(format!(
        "Software\\Classes\\AppUserModelId\\{WINDOWS_NOTIFICATION_APP_ID}"
    ));
    let mut key: HKEY = std::ptr::null_mut();
    let create_status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            registry_path.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if create_status != 0 {
        return Err(format!(
            "create notification registry key failed: {}",
            std::io::Error::from_raw_os_error(create_status as i32)
        ));
    }

    let value_name = wide_null("DisplayName");
    let display_name = wide_null("RSDuck");
    let set_status = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_SZ,
            display_name.as_ptr().cast(),
            (display_name.len() * std::mem::size_of::<u16>()) as u32,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if set_status != 0 {
        return Err(format!(
            "set notification display name failed: {}",
            std::io::Error::from_raw_os_error(set_status as i32)
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn show_system_notification(title: &str, message: &str) {
    use winrt_toast::{Toast, ToastManager};

    let mut toast = Toast::new();
    let tag = if title.contains("升级") {
        "upgrade"
    } else {
        "operation"
    };
    toast
        .text1(title)
        .text2(message)
        .tag(tag)
        .group("rsduck-tray");
    if let Err(error) = ToastManager::new(WINDOWS_NOTIFICATION_APP_ID).show(&toast) {
        eprintln!("show Windows notification failed: {error}");
    }
}

#[cfg(target_os = "macos")]
fn show_system_notification(title: &str, message: &str) {
    let script = format!(
        "display notification {} with title {}",
        apple_script_string(message),
        apple_script_string(title)
    );
    if let Err(error) = Command::new("osascript").args(["-e", &script]).spawn() {
        eprintln!("show macOS notification failed: {error}");
    }
}

#[cfg(target_os = "linux")]
fn show_system_notification(title: &str, message: &str) {
    if let Err(error) = Command::new("notify-send")
        .args(["--app-name=RSDuck", title, message])
        .spawn()
    {
        eprintln!("show Linux notification failed: {error}");
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn show_system_notification(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}

fn show_menu_action_error(title: &str, result: Result<String, String>) {
    if let Err(error) = result {
        show_system_notification(title, &error);
    }
}

fn show_background_action_result(action: BackgroundAction, result: &Result<String, String>) {
    let title = match action {
        BackgroundAction::Service(_) => "RSDuck 服务",
        BackgroundAction::Upgrade => "RSDuck 升级",
    };
    match result {
        Ok(message) => show_system_notification(title, message),
        Err(error) => show_system_notification(title, &format!("失败：{error}")),
    }
}

fn should_notify_upgrade_progress(message: &str) -> bool {
    message.starts_with("升级：发现 ")
        || message == "升级：下载完成，正在校验…"
        || message == "升级：校验完成，正在启动安装器…"
}

fn trim_upgrade_prefix(message: &str) -> &str {
    message.strip_prefix("升级：").unwrap_or(message)
}

fn run_elevated_command(args: &[String]) {
    #[cfg(windows)]
    let result = match args {
        [scope, action] if scope == "service" => match parse_service_action(action) {
            Some(action) => run_windows_service_action(action),
            None => Err(format!("unsupported elevated service action: {action}")),
        },
        _ => Err("usage: rsduck-tray --elevated service <start|stop|restart>".into()),
    };
    #[cfg(not(windows))]
    let result: Result<String, String> = {
        let _ = args;
        Err("elevated tray commands are only available on Windows".into())
    };

    match result {
        Ok(message) => {
            persist_windows_action_result(true, &message);
            println!("{message}");
        }
        Err(error) => {
            persist_windows_action_result(false, &error);
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn parse_service_action(value: &str) -> Option<ServiceAction> {
    match value {
        "start" => Some(ServiceAction::Start),
        "stop" => Some(ServiceAction::Stop),
        "restart" => Some(ServiceAction::Restart),
        _ => None,
    }
}

fn run_tray(cfg: RsduckConfig) -> Result<(), String> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event();
    let event_loop = event_loop.build();
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let mut tray_menu: Option<TrayMenu> = None;
    let mut _tray_icon = None;
    let mut service_toggle_action: Option<ServiceAction> = None;
    let mut last_refresh = Instant::now() - Duration::from_secs(60);
    let refresh_in_flight = Arc::new(AtomicBool::new(false));
    let action_in_flight = Arc::new(AtomicBool::new(false));

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(1));

        if matches!(event, Event::NewEvents(StartCause::Init)) {
            match create_tray_menu() {
                Ok((menu, items)) => match TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("RSDuck")
                    .with_icon(tray_icon_image())
                    .build()
                {
                    Ok(icon) => {
                        _tray_icon = Some(icon);
                        tray_menu = Some(items);
                    }
                    Err(error) => {
                        eprintln!("create tray icon failed: {error}");
                        *control_flow = ControlFlow::ExitWithCode(1);
                    }
                },
                Err(error) => {
                    eprintln!("create tray menu failed: {error}");
                    *control_flow = ControlFlow::ExitWithCode(1);
                }
            }
        }

        if last_refresh.elapsed() >= Duration::from_secs(3) {
            if tray_menu.is_some() {
                schedule_status_refresh(&proxy, refresh_in_flight.clone());
            }
            last_refresh = Instant::now();
        }

        match event {
            Event::UserEvent(UserEvent::StatusRefreshed(status)) => {
                if let Some(menu) = tray_menu.as_ref() {
                    if let Some(action_result) = status.action_result.as_deref() {
                        show_system_notification("RSDuck 服务", action_result);
                    }
                    let action_busy = action_in_flight.load(Ordering::Acquire);
                    service_toggle_action =
                        apply_status_refresh(menu, &status.manager, action_busy);
                }
            }
            Event::UserEvent(UserEvent::BackgroundActionProgress(message)) => {
                if action_in_flight.load(Ordering::Acquire)
                    && should_notify_upgrade_progress(&message)
                {
                    show_system_notification("RSDuck 升级", trim_upgrade_prefix(&message));
                }
            }
            Event::UserEvent(UserEvent::BackgroundActionCompleted {
                action,
                result,
                exit_after,
            }) => {
                action_in_flight.store(false, Ordering::Release);
                if let Some(menu) = tray_menu.as_ref() {
                    set_background_actions_enabled(menu, false);
                    show_background_action_result(action, &result);
                    schedule_status_refresh(&proxy, refresh_in_flight.clone());
                }
                if exit_after {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Menu(event)) => {
                let Some(menu) = tray_menu.as_ref() else {
                    return;
                };

                if event.id == *menu.open_web_sql.id() {
                    show_menu_action_error("打开 Web 失败", open_web_sql(&cfg));
                } else if event.id == *menu.open_logs.id() {
                    show_menu_action_error("打开日志失败", open_logs(&cfg));
                } else if event.id == *menu.service_toggle.id() {
                    if let Some(action) = service_toggle_action.take() {
                        run_background_action(
                            menu,
                            &proxy,
                            action_in_flight.clone(),
                            BackgroundAction::Service(action),
                        );
                    }
                } else if event.id == *menu.restart.id() {
                    run_background_action(
                        menu,
                        &proxy,
                        action_in_flight.clone(),
                        BackgroundAction::Service(ServiceAction::Restart),
                    );
                } else if event.id == *menu.upgrade.id() {
                    show_system_notification("RSDuck 升级", "正在检查新版本");
                    run_background_action(
                        menu,
                        &proxy,
                        action_in_flight.clone(),
                        BackgroundAction::Upgrade,
                    );
                } else if event.id == *menu.quit.id() {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
}

fn create_tray_menu() -> Result<(Menu, TrayMenu), String> {
    let menu = Menu::new();
    let status = MenuItem::new("检查中", false, None);
    let open_web_sql = MenuItem::new("打开Web", true, None);
    let open_logs = MenuItem::new("日志", true, None);
    let service_toggle = MenuItem::new("启动/停止", false, None);
    let restart = MenuItem::new("重启", false, None);
    let upgrade = MenuItem::new("升级", true, None);
    let quit = MenuItem::new("退出", true, None);
    let separator = PredefinedMenuItem::separator();
    let separator_after_service = PredefinedMenuItem::separator();
    let separator_before_quit = PredefinedMenuItem::separator();

    let items: [&dyn tray_icon::menu::IsMenuItem; 10] = [
        &status,
        &separator,
        &open_web_sql,
        &open_logs,
        &separator_after_service,
        &service_toggle,
        &restart,
        &upgrade,
        &separator_before_quit,
        &quit,
    ];
    #[cfg(target_os = "macos")]
    {
        let submenu = Submenu::new("RSDuck", true);
        submenu
            .append_items(&items)
            .map_err(|error| format!("build tray submenu failed: {error}"))?;
        menu.append(&submenu)
            .map_err(|error| format!("build tray menu failed: {error}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    menu.append_items(&items)
        .map_err(|error| format!("build tray menu failed: {error}"))?;

    Ok((
        menu,
        TrayMenu {
            status,
            open_web_sql,
            open_logs,
            service_toggle,
            restart,
            upgrade,
            quit,
        },
    ))
}

fn tray_icon_image() -> Icon {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let center = (x as i32 - 16).pow(2) + (y as i32 - 16).pow(2) <= 13_i32.pow(2);
            if center {
                rgba.extend_from_slice(&[31, 111, 235, 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, 32, 32).expect("generated tray icon RGBA must be valid")
}

fn schedule_status_refresh(proxy: &EventLoopProxy<UserEvent>, refresh_in_flight: Arc<AtomicBool>) {
    if refresh_in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let proxy = proxy.clone();
    thread::spawn(move || {
        let status = StatusRefresh {
            manager: service_manager_status().to_string(),
            action_result: take_windows_action_result(),
        };
        refresh_in_flight.store(false, Ordering::Release);
        let _ = proxy.send_event(UserEvent::StatusRefreshed(status));
    });
}

fn apply_status_refresh(
    menu: &TrayMenu,
    manager_status: &str,
    action_busy: bool,
) -> Option<ServiceAction> {
    menu.status.set_text(manager_status);
    let toggle = service_toggle_for_status(manager_status);
    if let Some((action, label)) = toggle {
        menu.service_toggle.set_text(label);
        menu.service_toggle.set_enabled(!action_busy);
        menu.restart.set_enabled(!action_busy);
        menu.upgrade.set_enabled(!action_busy);
        Some(action)
    } else {
        menu.service_toggle.set_text("启动/停止");
        menu.service_toggle.set_enabled(false);
        menu.restart.set_enabled(false);
        menu.upgrade.set_enabled(!action_busy);
        None
    }
}

fn service_toggle_for_status(status: &str) -> Option<(ServiceAction, &'static str)> {
    match status {
        "运行中" => Some((ServiceAction::Stop, "停止")),
        "已停止" => Some((ServiceAction::Start, "启动")),
        _ => None,
    }
}

fn set_background_actions_enabled(menu: &TrayMenu, enabled: bool) {
    menu.service_toggle.set_enabled(enabled);
    menu.restart.set_enabled(enabled);
    menu.upgrade.set_enabled(enabled);
}

fn run_background_action(
    menu: &TrayMenu,
    proxy: &EventLoopProxy<UserEvent>,
    action_in_flight: Arc<AtomicBool>,
    action: BackgroundAction,
) {
    if action_in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    set_background_actions_enabled(menu, false);
    let proxy = proxy.clone();
    thread::spawn(move || {
        let (result, exit_after) = match action {
            BackgroundAction::Service(action) => (invoke_service_action(action), false),
            BackgroundAction::Upgrade => {
                let progress_proxy = proxy.clone();
                let mut report_progress = move |message| {
                    let _ = progress_proxy.send_event(UserEvent::BackgroundActionProgress(message));
                };
                let result = check_and_start_upgrade(&mut report_progress);
                let exit_after = result
                    .as_ref()
                    .is_ok_and(|message| message.starts_with("已启动"));
                (result, exit_after)
            }
        };
        let _ = proxy.send_event(UserEvent::BackgroundActionCompleted {
            action,
            result,
            exit_after,
        });
    });
}

fn open_web_sql(cfg: &RsduckConfig) -> Result<String, String> {
    if !cfg.web.enabled {
        return Err("Web 服务已在 rsduck.toml 中禁用".into());
    }
    let url = web_console_url(&cfg.web.bind)?;
    open::that_detached(&url).map_err(|error| format!("open Web SQL failed: {error}"))?;
    Ok("已打开 Web SQL".into())
}

fn open_logs(cfg: &RsduckConfig) -> Result<String, String> {
    let path = resolve_runtime_path(&cfg.log.dir);
    open::that_detached(&path)
        .map_err(|error| format!("open log directory {} failed: {error}", path.display()))?;
    Ok("已打开日志目录".into())
}

fn resolve_runtime_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn web_console_url(bind: &str) -> Result<String, String> {
    let address = bind
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid web.bind {bind}: {error}"))?;
    let host = if address.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else if address.ip().is_ipv6() {
        format!("[{}]", address.ip())
    } else {
        address.ip().to_string()
    };
    Ok(format!("http://{host}:{}", address.port()))
}

fn service_manager_status() -> &'static str {
    #[cfg(windows)]
    {
        return windows_service_status();
    }
    #[cfg(target_os = "linux")]
    {
        return linux_service_status();
    }
    #[cfg(target_os = "macos")]
    {
        return macos_service_status();
    }
    #[allow(unreachable_code)]
    "不支持"
}

#[cfg(windows)]
fn windows_service_status() -> &'static str {
    let service = match open_windows_service(ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(error) if windows_service_error_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return "未安装";
        }
        Err(_) => return "未知",
    };
    match service.query_status() {
        Ok(status) => windows_service_state_label(status.current_state),
        Err(_) => "异常",
    }
}

#[cfg(windows)]
fn windows_service_state_label(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Stopped => "已停止",
        ServiceState::StartPending | ServiceState::ContinuePending => "启动中",
        ServiceState::StopPending => "停止中",
        ServiceState::Running => "运行中",
        ServiceState::PausePending => "暂停中",
        ServiceState::Paused => "已暂停",
    }
}

#[cfg(target_os = "linux")]
fn linux_service_status() -> &'static str {
    match Command::new("systemctl")
        .args(["is-active", SERVICE_NAME])
        .output()
    {
        Ok(output) if output.status.success() => "运行中",
        Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "inactive" => "已停止",
        Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "activating" => "启动中",
        Ok(_) => "异常",
        Err(_) => "未知",
    }
}

#[cfg(target_os = "macos")]
fn macos_service_status() -> &'static str {
    match Command::new("launchctl")
        .args(["print", &format!("system/{MACOS_SERVICE_LABEL}")])
        .output()
    {
        Ok(output) if output.status.success() => "运行中",
        Ok(_) => "已停止",
        Err(_) => "未知",
    }
}

fn invoke_service_action(action: ServiceAction) -> Result<String, String> {
    #[cfg(windows)]
    {
        launch_windows_elevated_service_action(action)?;
        return Ok(format!(
            "已请求管理员执行服务{}，正在等待状态刷新",
            action_label(action)
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let result = Command::new("pkexec")
            .args(["systemctl", action.command_name(), SERVICE_NAME])
            .status()
            .map_err(|error| format!("launch pkexec failed: {error}"))?;
        if result.success() {
            return Ok(format!("服务{}完成", action_label(action)));
        }
        return Err(format!(
            "systemctl {} exited with {result}",
            action.command_name()
        ));
    }
    #[cfg(target_os = "macos")]
    {
        return run_macos_service_action(action);
    }
    #[allow(unreachable_code)]
    Err("当前平台不支持服务管理".into())
}

fn action_label(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Start => "启动",
        ServiceAction::Stop => "停止",
        ServiceAction::Restart => "重启",
    }
}

#[cfg(windows)]
fn run_windows_service_action(action: ServiceAction) -> Result<String, String> {
    let service = open_windows_service(
        ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP,
    )
    .map_err(|error| windows_service_operation_error("open service", error))?;
    match action {
        ServiceAction::Start => start_windows_service(&service),
        ServiceAction::Stop => stop_windows_service(&service),
        ServiceAction::Restart => {
            stop_windows_service(&service)?;
            start_windows_service(&service)
        }
    }
    .map(|_| format!("服务{}完成", action_label(action)))
}

#[cfg(windows)]
fn open_windows_service(access: ServiceAccess) -> windows_service::Result<Service> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    manager.open_service(SERVICE_NAME, access)
}

#[cfg(windows)]
fn start_windows_service(service: &Service) -> Result<(), String> {
    match query_windows_service_state(service)? {
        ServiceState::Running => return Ok(()),
        ServiceState::StartPending | ServiceState::ContinuePending => {
            return wait_for_windows_service_state(service, ServiceState::Running, "start");
        }
        ServiceState::StopPending => {
            wait_for_windows_service_state(service, ServiceState::Stopped, "stop before start")?;
        }
        ServiceState::Paused | ServiceState::PausePending => {
            return Err("cannot start a paused Windows service".into());
        }
        ServiceState::Stopped => {}
    }
    match service.start::<&str>(&[]) {
        Ok(()) => {}
        Err(error) if windows_service_error_code(&error) == Some(ERROR_SERVICE_ALREADY_RUNNING) => {
        }
        Err(error) => return Err(windows_service_operation_error("start service", error)),
    }
    wait_for_windows_service_state(service, ServiceState::Running, "start")
}

#[cfg(windows)]
fn stop_windows_service(service: &Service) -> Result<(), String> {
    match query_windows_service_state(service)? {
        ServiceState::Stopped => return Ok(()),
        ServiceState::StopPending => {
            return wait_for_windows_service_state(service, ServiceState::Stopped, "stop");
        }
        ServiceState::StartPending | ServiceState::ContinuePending => {
            wait_for_windows_service_state(service, ServiceState::Running, "start before stop")?;
        }
        ServiceState::PausePending => {
            wait_for_windows_service_state(service, ServiceState::Paused, "pause before stop")?;
        }
        ServiceState::Running | ServiceState::Paused => {}
    }
    match service.stop() {
        Ok(_) => {}
        Err(error) if windows_service_error_code(&error) == Some(ERROR_SERVICE_NOT_ACTIVE) => {
            return Ok(());
        }
        Err(error) => return Err(windows_service_operation_error("stop service", error)),
    }
    wait_for_windows_service_state(service, ServiceState::Stopped, "stop")
}

#[cfg(windows)]
fn query_windows_service_state(service: &Service) -> Result<ServiceState, String> {
    service
        .query_status()
        .map(|status| status.current_state)
        .map_err(|error| windows_service_operation_error("query service status", error))
}

#[cfg(windows)]
fn wait_for_windows_service_state(
    service: &Service,
    expected: ServiceState,
    operation: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + WINDOWS_SERVICE_ACTION_TIMEOUT;
    loop {
        let current = query_windows_service_state(service)?;
        if current == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for Windows service to {operation}: current_state={current:?}, expected_state={expected:?}"
            ));
        }
        std::thread::sleep(WINDOWS_SERVICE_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn windows_service_error_code(error: &WindowsServiceError) -> Option<i32> {
    match error {
        WindowsServiceError::Winapi(error) => error.raw_os_error(),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_service_operation_error(operation: &str, error: WindowsServiceError) -> String {
    match windows_service_error_code(&error) {
        Some(ERROR_ACCESS_DENIED) => {
            format!("{operation} failed: administrator permission is required")
        }
        Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            format!("{operation} failed: Windows service {SERVICE_NAME} is not installed")
        }
        Some(code) => format!("{operation} failed with Windows error {code}: {error}"),
        None => format!("{operation} failed: {error}"),
    }
}

#[cfg(windows)]
fn launch_windows_elevated_service_action(action: ServiceAction) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve tray executable failed: {error}"))?;
    let arguments = format!("--elevated service {}", action.command_name());
    let operation = wide_null("runas");
    let executable = wide_null(executable.as_os_str());
    let arguments = wide_null(arguments.as_str());
    let working_directory = std::env::current_dir()
        .map_err(|error| format!("resolve tray working directory failed: {error}"))?;
    let working_directory = wide_null(working_directory.as_os_str());

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            executable.as_ptr(),
            arguments.as_ptr(),
            working_directory.as_ptr(),
            0,
        )
    };
    if result as isize <= 32 {
        return Err(format!("UAC service command failed with code {result:?}"));
    }
    Ok(())
}

#[cfg(windows)]
fn wide_null(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn persist_windows_action_result(success: bool, message: &str) {
    let path = windows_action_result_path();
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let payload = serde_json::json!({
        "success": success,
        "message": message,
    });
    let _ = fs::write(path, payload.to_string());
}

#[cfg(windows)]
fn take_windows_action_result() -> Option<String> {
    let path = windows_action_result_path();
    let payload = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(path);
    let payload = serde_json::from_str::<serde_json::Value>(&payload).ok()?;
    let success = payload.get("success")?.as_bool()?;
    let message = payload.get("message")?.as_str()?;
    Some(if success {
        format!("操作完成：{message}")
    } else {
        format!("操作失败：{message}")
    })
}

#[cfg(windows)]
fn windows_action_result_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("RSDuck").join("tray-action.json")
}

#[cfg(not(windows))]
fn persist_windows_action_result(_success: bool, _message: &str) {}

#[cfg(not(windows))]
fn take_windows_action_result() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn run_macos_service_action(action: ServiceAction) -> Result<String, String> {
    let command = match action {
        ServiceAction::Start => format!(
            "/bin/launchctl enable system/{MACOS_SERVICE_LABEL}; /bin/launchctl bootstrap system /Library/LaunchDaemons/{MACOS_SERVICE_LABEL}.plist 2>/dev/null || true; /bin/launchctl kickstart -k system/{MACOS_SERVICE_LABEL}"
        ),
        ServiceAction::Stop => format!(
            "/bin/launchctl disable system/{MACOS_SERVICE_LABEL}; /bin/launchctl bootout system/{MACOS_SERVICE_LABEL}"
        ),
        ServiceAction::Restart => format!(
            "/bin/launchctl kickstart -k system/{MACOS_SERVICE_LABEL}"
        ),
    };
    let script = format!(
        "do shell script {} with administrator privileges",
        apple_script_string(&command)
    );
    let result = Command::new("osascript")
        .args(["-e", &script])
        .status()
        .map_err(|error| format!("launch administrator service command failed: {error}"))?;
    if result.success() {
        Ok(format!("服务{}完成", action_label(action)))
    } else {
        Err(format!(
            "launchctl {} exited with {result}",
            action.command_name()
        ))
    }
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn check_and_start_upgrade(report_progress: &mut dyn FnMut(String)) -> Result<String, String> {
    report_progress("升级：正在检查新版本…".into());
    let manifest_url = std::env::var("RSDUCK_UPDATE_MANIFEST_URL")
        .unwrap_or_else(|_| UPDATE_MANIFEST_URL.to_string());
    let manifest = update_http_client(Duration::from_secs(20))?
        .get(&manifest_url)
        .send()
        .map_err(|error| format!("download update manifest failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("update manifest request failed: {error}"))?
        .json::<UpdateManifest>()
        .map_err(|error| format!("parse update manifest failed: {error}"))?;

    if !version_is_newer(&manifest.version, env!("CARGO_PKG_VERSION"))? {
        return Ok(format!("当前已是最新版本 v{}", env!("CARGO_PKG_VERSION")));
    }
    report_progress(format!("升级：发现 v{}，准备下载…", manifest.version));
    let platform = update_platform_key()?;
    let asset = manifest
        .assets
        .get(platform)
        .ok_or_else(|| format!("update manifest has no asset for {platform}"))?;
    let installer = download_update_asset(asset, platform, report_progress)?;
    report_progress("升级：校验完成，正在启动安装器…".into());
    launch_update_installer(&installer, platform)?;
    Ok(format!("已启动 v{} 升级安装", manifest.version))
}

fn update_http_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|error| format!("create update HTTP client failed: {error}"))
}

fn version_is_newer(candidate: &str, current: &str) -> Result<bool, String> {
    let candidate = parse_release_version(candidate)?;
    let current = parse_release_version(current)?;
    Ok(candidate > current)
}

fn parse_release_version(value: &str) -> Result<(u64, u64, u64), String> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let mut parts = value.split('.');
    let major = parts
        .next()
        .ok_or_else(|| format!("invalid release version {value}"))?
        .parse()
        .map_err(|_| format!("invalid release version {value}"))?;
    let minor = parts
        .next()
        .ok_or_else(|| format!("invalid release version {value}"))?
        .parse()
        .map_err(|_| format!("invalid release version {value}"))?;
    let patch = parts
        .next()
        .ok_or_else(|| format!("invalid release version {value}"))?
        .parse()
        .map_err(|_| format!("invalid release version {value}"))?;
    if parts.next().is_some() {
        return Err(format!("invalid release version {value}"));
    }
    Ok((major, minor, patch))
}

fn update_platform_key() -> Result<&'static str, String> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        return Ok("windows-x64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Ok("linux-x64");
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Ok("macos-x64");
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok("macos-arm64");
    }
    #[allow(unreachable_code)]
    Err("current platform has no supported update package".into())
}

fn download_update_asset(
    asset: &UpdateAsset,
    platform: &str,
    report_progress: &mut dyn FnMut(String),
) -> Result<PathBuf, String> {
    let mut response = update_http_client(Duration::from_secs(600))?
        .get(&asset.url)
        .send()
        .map_err(|error| format!("download update package failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("update package request failed: {error}"))?;
    let total_bytes = response.content_length();

    let extension = if platform == "windows-x64" {
        "exe"
    } else if platform.starts_with("macos-") {
        "pkg"
    } else {
        "tar.gz"
    };
    let dir = std::env::temp_dir().join(format!(
        "rsduck-update-{}-{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));
    fs::create_dir_all(&dir)
        .map_err(|error| format!("create update temporary directory failed: {error}"))?;
    let path = dir.join(format!("rsduck-update.{extension}"));
    let mut file = fs::File::create(&path)
        .map_err(|error| format!("create update package {} failed: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut downloaded_bytes = 0_u64;
    let mut last_reported_percent = 0_u64;
    let mut last_reported_bytes = 0_u64;
    loop {
        let bytes_read = response
            .read(&mut buffer)
            .map_err(|error| format!("read update package failed: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read])
            .map_err(|error| format!("write update package failed: {error}"))?;
        hasher.update(&buffer[..bytes_read]);
        downloaded_bytes += bytes_read as u64;
        if let Some(total_bytes) = total_bytes.filter(|total| *total > 0) {
            let percent = (downloaded_bytes.saturating_mul(100) / total_bytes).min(100);
            if percent == 100 || percent >= last_reported_percent.saturating_add(5) {
                report_progress(format!(
                    "升级：正在下载… {percent}%（{:.1}/{:.1} MB）",
                    downloaded_bytes as f64 / 1_048_576.0,
                    total_bytes as f64 / 1_048_576.0
                ));
                last_reported_percent = percent;
            }
        } else if downloaded_bytes >= last_reported_bytes.saturating_add(10 * 1_048_576) {
            report_progress(format!(
                "升级：正在下载… {:.1} MB",
                downloaded_bytes as f64 / 1_048_576.0
            ));
            last_reported_bytes = downloaded_bytes;
        }
    }
    file.flush()
        .map_err(|error| format!("flush update package failed: {error}"))?;
    report_progress("升级：下载完成，正在校验…".into());
    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != asset.sha256.to_ascii_lowercase() {
        return Err(format!(
            "update package checksum mismatch: expected {}, got {actual_hash}",
            asset.sha256
        ));
    }
    Ok(path)
}

fn launch_update_installer(installer: &Path, platform: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = platform;
        open::that_detached(installer)
            .map_err(|error| format!("start Windows update installer failed: {error}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let _ = platform;
        open::that_detached(installer)
            .map_err(|error| format!("open macOS update installer failed: {error}"))?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let _ = platform;
        let extract_dir = installer
            .parent()
            .ok_or_else(|| {
                format!(
                    "update package has no parent directory: {}",
                    installer.display()
                )
            })?
            .join("package");
        fs::create_dir_all(&extract_dir)
            .map_err(|error| format!("create update extraction directory failed: {error}"))?;
        let result = Command::new("tar")
            .args(["-xzf"])
            .arg(installer)
            .args(["-C"])
            .arg(&extract_dir)
            .status()
            .map_err(|error| format!("extract Linux update package failed: {error}"))?;
        if !result.success() {
            return Err(format!("extract Linux update package exited with {result}"));
        }
        let script = extract_dir.join("install-service.sh");
        let result = Command::new("pkexec")
            .arg(&script)
            .status()
            .map_err(|error| format!("launch Linux update installer failed: {error}"))?;
        if result.success() {
            return Ok(());
        }
        return Err(format!("Linux update installer exited with {result}"));
    }
    #[allow(unreachable_code)]
    Err("current platform has no supported update installer".into())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::windows_service_state_label;
    use super::{
        parse_release_version, service_toggle_for_status, should_notify_upgrade_progress,
        trim_upgrade_prefix, version_is_newer, web_console_url, ServiceAction,
    };
    #[cfg(windows)]
    use windows_service::service::ServiceState;

    #[test]
    fn release_versions_use_numeric_semver_order() {
        assert!(version_is_newer("v0.1.14", "0.1.13").unwrap());
        assert!(!version_is_newer("0.1.13", "0.1.13").unwrap());
        assert_eq!(parse_release_version("1.2.3").unwrap(), (1, 2, 3));
    }

    #[test]
    fn web_console_url_rewrites_unspecified_bind_to_loopback() {
        assert_eq!(
            web_console_url("0.0.0.0:13307").unwrap(),
            "http://127.0.0.1:13307"
        );
        assert_eq!(
            web_console_url("[::]:13307").unwrap(),
            "http://127.0.0.1:13307"
        );
    }

    #[test]
    fn service_toggle_follows_stable_service_status() {
        assert_eq!(
            service_toggle_for_status("运行中"),
            Some((ServiceAction::Stop, "停止"))
        );
        assert_eq!(
            service_toggle_for_status("已停止"),
            Some((ServiceAction::Start, "启动"))
        );
        assert_eq!(service_toggle_for_status("启动中"), None);
        assert_eq!(service_toggle_for_status("未知"), None);
    }

    #[test]
    fn upgrade_notifications_skip_chatty_download_percentages() {
        assert!(should_notify_upgrade_progress(
            "升级：发现 v0.1.25，准备下载…"
        ));
        assert!(should_notify_upgrade_progress("升级：下载完成，正在校验…"));
        assert!(!should_notify_upgrade_progress(
            "升级：正在下载… 50%（10.0/20.0 MB）"
        ));
        assert_eq!(
            trim_upgrade_prefix("升级：校验完成，正在启动安装器…"),
            "校验完成，正在启动安装器…"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_service_states_have_stable_tray_labels() {
        assert_eq!(windows_service_state_label(ServiceState::Stopped), "已停止");
        assert_eq!(
            windows_service_state_label(ServiceState::StartPending),
            "启动中"
        );
        assert_eq!(
            windows_service_state_label(ServiceState::StopPending),
            "停止中"
        );
        assert_eq!(windows_service_state_label(ServiceState::Running), "运行中");
        assert_eq!(
            windows_service_state_label(ServiceState::ContinuePending),
            "启动中"
        );
        assert_eq!(
            windows_service_state_label(ServiceState::PausePending),
            "暂停中"
        );
        assert_eq!(windows_service_state_label(ServiceState::Paused), "已暂停");
    }
}
