#![cfg_attr(windows, windows_subsystem = "windows")]

use rsduck::config::{self, RsduckConfig};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(target_os = "macos")]
use tray_icon::menu::Submenu;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

const SERVICE_NAME: &str = "rsduck";
#[cfg(target_os = "macos")]
const MACOS_SERVICE_LABEL: &str = "com.dripai.rsduck";
const UPDATE_MANIFEST_URL: &str =
    "https://github.com/dripai/rsduck/releases/latest/download/rsduck-update.json";

#[derive(Debug, Clone, Copy)]
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
}

struct TrayMenu {
    status: MenuItem,
    open_web_sql: MenuItem,
    open_logs: MenuItem,
    start: MenuItem,
    stop: MenuItem,
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
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    let mut tray_menu: Option<TrayMenu> = None;
    let mut _tray_icon = None;
    let mut last_refresh = Instant::now() - Duration::from_secs(60);

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
            if let Some(menu) = tray_menu.as_ref() {
                refresh_status(menu, &cfg);
            }
            last_refresh = Instant::now();
        }

        let Event::UserEvent(UserEvent::Menu(event)) = event else {
            return;
        };
        let Some(menu) = tray_menu.as_ref() else {
            return;
        };

        if event.id == *menu.open_web_sql.id() {
            set_action_result(menu, open_web_sql(&cfg));
        } else if event.id == *menu.open_logs.id() {
            set_action_result(menu, open_logs(&cfg));
        } else if event.id == *menu.start.id() {
            set_action_result(menu, invoke_service_action(ServiceAction::Start));
        } else if event.id == *menu.stop.id() {
            set_action_result(menu, invoke_service_action(ServiceAction::Stop));
        } else if event.id == *menu.restart.id() {
            set_action_result(menu, invoke_service_action(ServiceAction::Restart));
        } else if event.id == *menu.upgrade.id() {
            let result = check_and_start_upgrade();
            let upgrade_started = result
                .as_ref()
                .is_ok_and(|message| message.starts_with("已启动"));
            set_action_result(menu, result);
            if upgrade_started {
                *control_flow = ControlFlow::Exit;
            }
        } else if event.id == *menu.quit.id() {
            *control_flow = ControlFlow::Exit;
        }
    });
}

fn create_tray_menu() -> Result<(Menu, TrayMenu), String> {
    let menu = Menu::new();
    let status = MenuItem::new("状态：检查中", false, None);
    let open_web_sql = MenuItem::new("打开 Web SQL", true, None);
    let open_logs = MenuItem::new("打开日志目录", true, None);
    let start = MenuItem::new("启动服务", true, None);
    let stop = MenuItem::new("停止服务", true, None);
    let restart = MenuItem::new("重启服务", true, None);
    let upgrade = MenuItem::new("检查并升级", true, None);
    let quit = MenuItem::new("退出托盘", true, None);
    let separator = PredefinedMenuItem::separator();
    let separator_after_service = PredefinedMenuItem::separator();
    let separator_before_quit = PredefinedMenuItem::separator();

    let items: [&dyn tray_icon::menu::IsMenuItem; 11] = [
        &status,
        &separator,
        &open_web_sql,
        &open_logs,
        &separator_after_service,
        &start,
        &stop,
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
            start,
            stop,
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

fn refresh_status(menu: &TrayMenu, cfg: &RsduckConfig) {
    let manager = service_manager_status();
    let application = application_health_status(cfg);
    let service_status = format!("状态：服务 {manager}；Web {application}");
    if let Some(action_result) = take_windows_action_result() {
        menu.status
            .set_text(format!("{service_status}；{action_result}"));
    } else {
        menu.status.set_text(service_status);
    }
}

fn set_action_result(menu: &TrayMenu, result: Result<String, String>) {
    match result {
        Ok(message) => menu.status.set_text(format!("状态：{message}")),
        Err(error) => menu.status.set_text(format!("状态：操作失败：{error}")),
    }
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

fn application_health_status(cfg: &RsduckConfig) -> &'static str {
    if !cfg.web.enabled {
        return "已禁用";
    }
    let Ok(url) = web_console_url(&cfg.web.bind) else {
        return "配置无效";
    };
    let Ok(host_port) = url.strip_prefix("http://").ok_or(()) else {
        return "配置无效";
    };
    let Ok(addresses) = host_port.to_socket_addrs() else {
        return "不可用";
    };
    for address in addresses {
        if health_request(address) {
            return "可用";
        }
    }
    "不可用"
}

fn health_request(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_secs(1)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = [0_u8; 64];
    match stream.read(&mut response) {
        Ok(size) => response[..size].starts_with(b"HTTP/1.1 200"),
        Err(_) => false,
    }
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
    let Ok(output) = Command::new("sc.exe")
        .args(["query", SERVICE_NAME])
        .output()
    else {
        return "未知";
    };
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains("RUNNING") {
        "运行中"
    } else if text.contains("STOPPED") {
        "已停止"
    } else if text.contains("START_PENDING") {
        "启动中"
    } else if text.contains("STOP_PENDING") {
        "停止中"
    } else if text.contains("FAILED 1060") {
        "未安装"
    } else {
        "异常"
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
    match action {
        ServiceAction::Start => run_checked("sc.exe", &["start", SERVICE_NAME]),
        ServiceAction::Stop => run_checked("sc.exe", &["stop", SERVICE_NAME]),
        ServiceAction::Restart => {
            run_checked("sc.exe", &["stop", SERVICE_NAME])?;
            wait_for_windows_stop()?;
            run_checked("sc.exe", &["start", SERVICE_NAME])
        }
    }
    .map(|_| format!("服务{}完成", action_label(action)))
}

#[cfg(windows)]
fn wait_for_windows_stop() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let output = Command::new("sc.exe")
            .args(["query", SERVICE_NAME])
            .output()
            .map_err(|error| format!("query service status failed: {error}"))?;
        if String::from_utf8_lossy(&output.stdout).contains("STOPPED") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for service to stop".into());
        }
        std::thread::sleep(Duration::from_secs(1));
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
fn run_checked(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("run {program} failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(format!("{program} {} failed: {}", args.join(" "), detail))
    }
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

fn check_and_start_upgrade() -> Result<String, String> {
    let manifest_url = std::env::var("RSDUCK_UPDATE_MANIFEST_URL")
        .unwrap_or_else(|_| UPDATE_MANIFEST_URL.to_string());
    let manifest = reqwest::blocking::get(&manifest_url)
        .map_err(|error| format!("download update manifest failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("update manifest request failed: {error}"))?
        .json::<UpdateManifest>()
        .map_err(|error| format!("parse update manifest failed: {error}"))?;

    if !version_is_newer(&manifest.version, env!("CARGO_PKG_VERSION"))? {
        return Ok(format!("当前已是最新版本 v{}", env!("CARGO_PKG_VERSION")));
    }
    let platform = update_platform_key()?;
    let asset = manifest
        .assets
        .get(platform)
        .ok_or_else(|| format!("update manifest has no asset for {platform}"))?;
    let installer = download_update_asset(asset, platform)?;
    launch_update_installer(&installer, platform)?;
    Ok(format!("已启动 v{} 升级安装", manifest.version))
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

fn download_update_asset(asset: &UpdateAsset, platform: &str) -> Result<PathBuf, String> {
    let bytes = reqwest::blocking::get(&asset.url)
        .map_err(|error| format!("download update package failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("update package request failed: {error}"))?
        .bytes()
        .map_err(|error| format!("read update package failed: {error}"))?;
    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
    if actual_hash != asset.sha256.to_ascii_lowercase() {
        return Err(format!(
            "update package checksum mismatch: expected {}, got {actual_hash}",
            asset.sha256
        ));
    }

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
    fs::write(&path, bytes).map_err(|error| format!("write update package failed: {error}"))?;
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
    use super::{parse_release_version, version_is_newer, web_console_url};

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
}
