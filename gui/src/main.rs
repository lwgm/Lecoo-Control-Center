#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
use chrono::Local;
use eframe::egui;
use ipc::{DaemonCommand, IpcClient, IpcError, IpcRequest, IpcResponse};
use lecoo_types::{
    caps::Capabilities,
    ec_types::{
        BreathConfig, BreathDelay, BreathStep, ChargeIntent, ChargeRange, FanIndex, FanMode,
        KeyboardBacklightLevel, PowerLedMode, PowerProfile,
    },
    settings::CurrentSettings,
};
use single_instance::SingleInstance;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use serde::Deserialize;
#[cfg(target_os = "windows")]
use std::env;
#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use std::ptr;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Services::*;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::ShellExecuteW;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
#[cfg(target_os = "windows")]
use wmi::{COMLibrary, WMIConnection};
#[cfg(target_os = "windows")]
use std::cell::OnceCell;

#[cfg(target_os = "windows")]
fn is_service_running(service_name: &str) -> bool {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return false;
        }
        let name: Vec<u16> = OsStr::new(service_name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let svc = OpenServiceW(scm, name.as_ptr(), SERVICE_QUERY_STATUS);
        if svc.is_null() {
            CloseServiceHandle(scm);
            return false;
        }
        let mut status: SERVICE_STATUS_PROCESS = std::mem::zeroed();
        let mut needed = 0u32;
        let ok = QueryServiceStatusEx(
            svc,
            SC_STATUS_PROCESS_INFO,
            &mut status as *mut _ as *mut u8,
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        );
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        if ok == 0 {
            return false;
        }
        status.dwCurrentState == SERVICE_RUNNING
    }
}

#[cfg(not(target_os = "windows"))]
fn is_service_running(_service_name: &str) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn start_service(service_name: &str) -> bool {
    unsafe {
        // Trigger UAC elevation
        let exe = env::current_exe().unwrap_or_default();
        let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let param = format!(
            "--elevate-start-service --service-name \"{}\"",
            service_name
        );
        let param_w: Vec<u16> = OsStr::new(&param).encode_wide().chain(Some(0)).collect();
        let verb = OsStr::new("runas")
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        let r = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            param_w.as_ptr(),
            ptr::null(),
            1,
        );
        if r as isize <= 32 {
            let msg = format!("Failed to open UAC prompt, ShellExecuteW returned: {}", r as isize);
            let wide: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
            MessageBoxW(std::ptr::null_mut(), wide.as_ptr(), wide.as_ptr(), 0);
        } else {
            // UAC prompt opened successfully, waiting for user action
            return true;
        }
        false
    }
}

#[cfg(not(target_os = "windows"))]
fn start_service(_service_name: &str) -> bool {
    false
}
const MY_APP_WINDOW_CLASS: &str = "Lecoo-Control-Center-550e8400-e29b-41d4-a716-446655440000";

const NOTICE_TTL_SECS: u64 = 3;
const WINDOW_WIDTH: f32 = 600.0;
const WINDOW_HEIGHT: f32 = 450.0;
const BATTERY_WORKER_INTERVAL_SECS: u64 = 15;
const SERVICE_WAIT_TIMEOUT_SECS: u64 = 20;
const UI_VERSION: &str = "0.3.0-beta1";

#[derive(Clone, Copy)]
enum PowerSourceStatus {
    Ac,
    Charging,
    Discharging,
    Unknown,
}

enum AppEvent {
    Connecting(bool),
    ConnectionFailed(String),
}
impl PowerSourceStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ac => "AC",
            Self::Charging => "Charging",
            Self::Discharging => "Discharging",
            Self::Unknown => "Unknown",
        }
    }
}

fn main() {
    // Check if launched in elevated mode for service startup
    #[cfg(target_os = "windows")]
    {
        if let Some(idx) = env::args().position(|a| a == "--elevate-start-service") {
            // Parse service name parameter
            let mut service_name = "LecooControlDaemon".to_string();
            let args: Vec<String> = env::args().collect();
            if args.len() > idx + 2 && args[idx + 1] == "--service-name" {
                service_name = args[idx + 2].clone();
            }
            // Start service as administrator
            unsafe {
                let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
                if !scm.is_null() {
                    let name: Vec<u16> = std::ffi::OsStr::new(&service_name)
                        .encode_wide()
                        .chain(Some(0))
                        .collect();
                    let svc = OpenServiceW(scm, name.as_ptr(), SERVICE_START);
                    if !svc.is_null() {
                        let _ = StartServiceW(svc, 0, std::ptr::null());
                        CloseServiceHandle(svc);
                    }
                    CloseServiceHandle(scm);
                }
            }
            // Exit immediately after startup attempt
            return;
        }
    }
    let instance = SingleInstance::new(MY_APP_WINDOW_CLASS).unwrap();
    if !instance.is_single() {
        #[cfg(target_os = "windows")]
        {
            if activate_existing_window() {
                return;
            }
        }
        return;
    }

    let mut options = eframe::NativeOptions::default();

    // Set window icon (title bar)
    options.viewport = egui::ViewportBuilder::default()
        .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
        .with_icon(Arc::new(
            eframe::icon_data::from_png_bytes(include_bytes!("../src/icon.png"))
                .expect("icon.png decode failed"),
        ));

    if let Err(err) = eframe::run_native(
        "Lecoo Control Center",
        options,
        Box::new(|_cc| Ok(Box::new(LecooApp::new()))),
    ) {
        eprintln!("Failed to start GUI: {}", err);
    }
}

#[derive(Clone)]
struct AppDraft {
    power_profile: PowerProfile,
    keyboard_backlight: KeyboardBacklightLevel,
    fan_mode_cpu: FanMode,
    fan_mode_gpu: FanMode,
    /// 直接持有 daemon 的 `ChargeIntent`，不再降级成本地枚举。
    charge_intent: ChargeIntent,
    led_mode: PowerLedMode,
    telemetry_enabled: bool,
}

impl From<CurrentSettings> for AppDraft {
    fn from(value: CurrentSettings) -> Self {
        Self {
            power_profile: value.power_profile,
            keyboard_backlight: value.keyboard_backlight,
            fan_mode_cpu: value.fan_mode_cpu,
            fan_mode_gpu: value.fan_mode_gpu,
            charge_intent: value.charge,
            led_mode: value.led_mode,
            telemetry_enabled: value.telemetry_enabled,
        }
    }
}

// ---------- 能力集辅助 ----------

/// 控件不可用时的说明。`None` = 还没拿到能力集。
fn caps_note(available: Option<bool>) -> &'static str {
    match available {
        None => "Capabilities not available",
        Some(false) => "Not supported by this board",
        Some(true) => "",
    }
}

/// daemon 的 preset 名 -> GUI 显示名。只影响显示，不影响发出去的值。
/// 认不出来的名字原样显示，绝不隐藏。
fn preset_label(name: &str) -> String {
    match name {
        "full" => "Full".to_string(),
        "high" => "High".to_string(),
        "balanced" => "Balanced".to_string(),
        "lifespan" => "Lifespan".to_string(),
        "desk" => "Desk".to_string(),
        "hold" => "Hold".to_string(),
        other => other.to_string(),
    }
}

/// 只影响按钮顺序，未知档位排在最后但仍然显示。
fn preset_rank(name: &str) -> u8 {
    match name {
        "full" => 0,
        "high" => 1,
        "balanced" => 2,
        "lifespan" => 3,
        "desk" => 4,
        "hold" => 5,
        _ => 99,
    }
}

/// daemon 给的是 `(显示名, 意图)`，按 GUI 的习惯顺序排一下。
fn sorted_presets(caps: &Capabilities) -> Vec<(String, ChargeIntent)> {
    let mut list = caps.charge.presets.clone();
    list.sort_by_key(|(name, _)| preset_rank(name));
    list
}

/// 充电卡片的标签页。只有板子支持任意区间（`caps.charge.custom_range`）时才会出现。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChargeTab {
    Presets,
    Custom,
}

/// 当前意图是不是"不在 daemon preset 表里的自定义区间"。
fn is_custom_intent(intent: ChargeIntent, presets: &[(String, ChargeIntent)]) -> bool {
    matches!(intent, ChargeIntent::Preserve(Some(_)))
        && !presets.iter().any(|(_, preset)| *preset == intent)
}

/// daemon 报的是自定义区间就停在 Custom 页，否则停在 Presets 页。
fn charge_tab_for(intent: ChargeIntent, presets: &[(String, ChargeIntent)]) -> ChargeTab {
    if is_custom_intent(intent, presets) {
        ChargeTab::Custom
    } else {
        ChargeTab::Presets
    }
}

/// 把一对 min/max 夹进板子允许的区间，并保证 `min < max`（daemon 会拒 `min >= max`）。
/// 调用方需保证 `hi > lo + 1` —— Custom 页只在那种情况下才会出现。
fn clamp_charge_range(min: u8, max: u8, lo: u8, hi: u8) -> (u8, u8) {
    let mut min = min.clamp(lo, hi - 1);
    let max = max.clamp(lo + 1, hi);
    if min >= max {
        min = (max - 1).max(lo);
    }
    (min, max)
}

/// Custom 页滑块的初值：draft 带区间就用它（和其他卡片一样反映 draft），
/// 否则（`Full` / `Freeze` / `Preserve(None)`）用当前生效区间兜底。
fn custom_seed(
    draft: ChargeIntent,
    current_min: Option<u8>,
    current_max: Option<u8>,
    lo: u8,
    hi: u8,
) -> (u8, u8) {
    match draft {
        ChargeIntent::Preserve(Some(r)) => clamp_charge_range(r.min, r.max, lo, hi),
        _ => clamp_charge_range(current_min.unwrap_or(lo), current_max.unwrap_or(hi), lo, hi),
    }
}

/// `(duty_max, available)` —— `None` = 能力集没拿到，`Some(false)` = 这块板子没这个风扇。
fn fan_caps(caps: Option<&Capabilities>, index: FanIndex) -> (Option<u8>, Option<bool>) {
    match caps {
        None => (None, None),
        Some(c) => match c.fans.iter().find(|f| f.index == index) {
            Some(f) => (Some(f.duty_max), Some(true)),
            None => (None, Some(false)),
        },
    }
}

fn fmt_charge_intent(intent: ChargeIntent) -> String {
    match intent {
        ChargeIntent::Full => "Full".to_string(),
        ChargeIntent::Preserve(Some(range)) => format!("Preserve {}-{}%", range.min, range.max),
        ChargeIntent::Preserve(None) => "Preserve (firmware)".to_string(),
        ChargeIntent::Freeze => "Freeze".to_string(),
    }
}

/// daemon 在 `UnsupportedHardware` 时把 message 写成空字符串，真正有用的是
/// `unsupported.board` / `unsupported.chip`。空 message 时用它们拼一句，
/// 其余情况原样显示 daemon 给的英文文案。
fn fmt_ipc_error(err: &IpcError) -> String {
    if !err.message.is_empty() {
        return err.message.clone();
    }
    match &err.unsupported {
        Some(info) => match &info.chip {
            Some(chip) => format!("Unsupported board: {} (chip {})", info.board, chip),
            None => format!("Unsupported board: {}", info.board),
        },
        None => format!("{:?}", err.code),
    }
}

#[derive(Default, Clone, Copy)]
struct LiveMetrics {
    cpu_temp: Option<u8>,
    sys_temp: Option<u8>,
    cpu_rpm: Option<u16>,
    gpu_rpm: Option<u16>,
    battery_percent: Option<u8>,
    battery_health: Option<u8>,
    power_source: Option<PowerSourceStatus>,
    charge_min: Option<u8>,
    charge_max: Option<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshRate {
    Sec1,
    Sec2,
    Sec5,
}

impl RefreshRate {
    fn as_duration(self) -> Duration {
        match self {
            Self::Sec1 => Duration::from_secs(1),
            Self::Sec2 => Duration::from_secs(2),
            Self::Sec5 => Duration::from_secs(5),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Sec1 => "1s",
            Self::Sec2 => "2s",
            Self::Sec5 => "5s",
        }
    }
}

impl Default for RefreshRate {
    fn default() -> Self {
        Self::Sec1
    }
}

#[derive(Clone, Copy)]
enum NoticeLevel {
    Info,
    Success,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BreathingPreset {
    Smooth,
    Sleep,
    Alert,
    Zen,
    Ping,
    Energetic,
    Warning,
    Vacuum,
    Panic,
    Sonar,
    Toxic,
}

impl BreathingPreset {
    fn label(self) -> &'static str {
        match self {
            Self::Smooth => "Smooth",
            Self::Sleep => "Sleep",
            Self::Alert => "Alert",
            Self::Zen => "Zen",
            Self::Ping => "Ping",
            Self::Energetic => "Energetic",
            Self::Warning => "Warning",
            Self::Vacuum => "Vacuum",
            Self::Panic => "Panic",
            Self::Sonar => "Sonar",
            Self::Toxic => "Toxic",
        }
    }

    fn to_breath_config(self) -> BreathConfig {
        match self {
            Self::Smooth => BreathConfig::smooth(),
            Self::Sleep => BreathConfig::sleep(),
            Self::Alert => BreathConfig::alert(),
            Self::Zen => BreathConfig::zen(),
            Self::Ping => BreathConfig::ping(),
            Self::Energetic => BreathConfig::energetic(),
            Self::Warning => BreathConfig::warning(),
            Self::Vacuum => BreathConfig::vacuum(),
            Self::Panic => BreathConfig::panic(),
            Self::Sonar => BreathConfig::sonar(),
            Self::Toxic => BreathConfig::toxic(),
        }
    }
}

fn preset_from_breath_config(config: BreathConfig) -> BreathingPreset {
    for preset in [
        BreathingPreset::Smooth,
        BreathingPreset::Sleep,
        BreathingPreset::Alert,
        BreathingPreset::Zen,
        BreathingPreset::Ping,
        BreathingPreset::Energetic,
        BreathingPreset::Warning,
        BreathingPreset::Vacuum,
        BreathingPreset::Panic,
        BreathingPreset::Sonar,
        BreathingPreset::Toxic,
    ] {
        let base = preset.to_breath_config();
        if base.max_brightness == config.max_brightness
            && base.step_up == config.step_up
            && base.step_down == config.step_down
        {
            return preset;
        }
    }

    BreathingPreset::Smooth
}

struct Notice {
    message: String,
    level: NoticeLevel,
    created_at: Instant,
    expires_at: Option<Instant>,
}

#[derive(PartialEq, Eq)]
enum ServiceWaitState {
    None,
    Starting,
    Started,
}

struct LecooApp {
    client: Option<IpcClient>,
    daemon_version: String,
    ec_chip: String,
    ec_revision: String,
    connected: bool,

    /// 连接后拉一次能力集；断连时清空，重连后重新拉。
    caps: Option<Capabilities>,
    /// 能力集拉取失败的原因。有值时不再重试，避免每个刷新周期刷屏。
    caps_error: Option<String>,

    current: CurrentSettings,
    draft: AppDraft,
    metrics: LiveMetrics,
    /// `ChargeStatus.pending`：daemon 给的英文原文，意图已记下但尚未生效。
    charge_pending: Option<String>,
    /// 充电卡片的标签页（Presets / Custom），只有支持任意区间的板子才用得上。
    charge_tab: ChargeTab,

    refresh_rate: RefreshRate,
    last_refresh: Instant,

    status: String,
    last_error: Option<String>,
    notice: Option<Notice>,
    restore_defaults_confirm_open: bool,
    led_breathing_preset: BreathingPreset,
    led_breathing_advanced_open: bool,
    led_step_up: BreathStep,
    led_step_down: BreathStep,
    led_delay_at_max: BreathDelay,
    led_delay_at_min: BreathDelay,

    battery_rx: Receiver<(Option<PowerSourceStatus>, Option<u8>)>,

    service_wait_state: ServiceWaitState,
    service_wait_started_at: Option<Instant>,
    service_check_last: Instant,

    connect_tx: Sender<AppEvent>,
    connect_rx: Receiver<AppEvent>,
    is_connecting: bool,
}

impl LecooApp {
    fn new() -> Self {
        let current = CurrentSettings::default();
        let draft = AppDraft::from(current.clone());
        let (tx, rx) = mpsc::channel();
        let (connect_tx, connect_rx) = mpsc::channel();
        let mut app = Self {
            client: None,
            daemon_version: "unknown".to_string(),
            ec_chip: String::new(),
            ec_revision: String::new(),
            connected: false,
            caps: None,
            caps_error: None,
            current,
            draft,
            metrics: LiveMetrics::default(),
            charge_pending: None,
            charge_tab: ChargeTab::Presets,
            refresh_rate: RefreshRate::default(),
            last_refresh: Instant::now() - Duration::from_secs(5),
            status: "Connecting...".to_string(),
            last_error: None,
            notice: None,
            restore_defaults_confirm_open: false,
            led_breathing_preset: BreathingPreset::Smooth,
            led_breathing_advanced_open: false,
            led_step_up: BreathStep::Medium,
            led_step_down: BreathStep::Medium,
            led_delay_at_max: BreathDelay::Ms250,
            led_delay_at_min: BreathDelay::Ms250,
            battery_rx: rx,
            service_wait_state: ServiceWaitState::None,
            service_wait_started_at: None,
            service_check_last: Instant::now(),
            connect_tx,
            connect_rx,
            is_connecting: false,
        };

        std::thread::Builder::new()
            .name("battery-info-worker".into())
            .spawn(move || {
                // WMI 连接建一次就复用。原来每轮都重建最多 4 套 COM/WMI 连接，
                // 既拖慢首轮（Health/Power 长时间显示占位符），也每 15 秒白做一次。
                let wmi = BatteryWmi::new();
                loop {
                    let status = read_power_source_windows(&wmi);
                    let health = read_battery_health_windows(&wmi);
                    if tx.send((status, health)).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(BATTERY_WORKER_INTERVAL_SECS));
                }
            })
            .expect("failed to spawn battery-info-worker");

        app.ensure_connected();
        let _ = app.refresh_data(true);
        app
    }

    fn ensure_connected(&mut self) {
        if self.client.is_some() {
            self.connected = true;
            return;
        }
        match IpcClient::connect() {
            Ok(client) => {
                self.client = Some(client);
                self.connected = true;
                self.last_error = None;
                self.status = "Connected".to_string();
            }
            Err(err) => {
                self.connected = false;
                self.last_error = Some(format!("Connect failed: {}", err));
                self.status = "Disconnected".to_string();
				let _ = self.connect_tx.send(AppEvent::ConnectionFailed(err.to_string()));
            }
        }
    }

    fn request(&mut self, req: &IpcRequest) -> Result<IpcResponse, String> {
        #[cfg(target_os = "windows")]
        if !is_service_running("LecooControlDaemon") {
            self.client = None;
            self.connected = false;
            self.caps = None;
            self.caps_error = None;
            self.status = "Disconnected".to_string();
            self.daemon_version = "unknown".to_string();
            self.ec_chip.clear();
            self.ec_revision.clear();
            self.last_error = Some("Daemon service is not running".to_string());
            return Err("Daemon service is not running".to_string());
        }

        self.ensure_connected();
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| "No daemon connection".to_string())?;
        match client.request(req) {
            Ok(resp) => Ok(resp),
            Err(err) => {
                self.client = None;
                self.connected = false;
                self.caps = None;
                self.caps_error = None;
                Err(err.to_string())
            }
        }
    }

    /// 拉一次能力集。传输层失败就保持 `None`，下次 refresh 再试；
    /// 但如果是 daemon 明确拒绝（板子未识别 / 请求不认识），就记下原因不再重试。
    fn fetch_capabilities(&mut self) {
        if self.caps.is_some() || self.caps_error.is_some() {
            return;
        }

        match self.request(&IpcRequest::DaemonCommand(DaemonCommand::GetCapabilities)) {
            Ok(IpcResponse::Capabilities(caps)) => {
                self.caps = Some(*caps);
                self.caps_error = None;
            }
            Ok(IpcResponse::Error(err)) => {
                let msg = fmt_ipc_error(&err);
                self.set_notice(
                    &msg,
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
                self.caps_error = Some(msg);
            }
            Ok(other) => {
                let msg = format!("Unexpected GetCapabilities response: {other:?}");
                self.set_notice(
                    &msg,
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
                self.caps_error = Some(msg);
            }
            // 传输层错误：上面的 request 已经把连接清掉了，不记原因，等重连后再试。
            Err(err) => {
                self.caps = None;
                self.caps_error = None;
                self.last_error = Some(err);
            }
        }
    }

    fn refresh_data(&mut self, force_sync_draft: bool) -> Result<(), String> {
        if self.caps.is_none() {
            self.fetch_capabilities();
        }

        if let IpcResponse::SystemInfo(info) = self.request(&IpcRequest::GetSystemState)? {
            self.ec_chip = info.chip;
            self.ec_revision = info.revision;
            if !info.daemon_version.is_empty() {
                self.daemon_version = info.daemon_version;
            }
        }

        if let IpcResponse::Temps { cpu_c, sys_c } = self.request(&IpcRequest::GetTemperatures)? {
            self.metrics.cpu_temp = Some(cpu_c);
            self.metrics.sys_temp = Some(sys_c);
        }

        if let IpcResponse::FanRpm { cpu, gpu } = self.request(&IpcRequest::GetFansRPM)? {
            self.metrics.cpu_rpm = Some(cpu);
            self.metrics.gpu_rpm = Some(gpu);
        }

        if let IpcResponse::ChargeStatus(status) = self.request(&IpcRequest::GetChargeStatus)? {
            self.metrics.charge_min = status.thresholds.map(|t| t.0);
            self.metrics.charge_max = status.thresholds.map(|t| t.1);
            self.metrics.battery_percent = Some(status.soc);
            // daemon 给的英文原文，直接显示。
            self.charge_pending = status.pending;
        }

        let settings_resp = self.request(&IpcRequest::DaemonCommand(DaemonCommand::GetSettings))?;
        if let IpcResponse::Settings(settings) = settings_resp {
            let settings = *settings;
            self.current = settings.clone();
            if force_sync_draft || !self.has_pending_changes() {
                self.draft = AppDraft::from(settings);
                self.sync_led_ui_from_mode(self.draft.led_mode);
            }
            if force_sync_draft {
                // 只在"重新从 daemon 读一遍"时对齐标签页（启动 / 应用后 / 重连 / 手动读取）。
                // 周期刷新绝不能碰它：否则用户点了 Custom 但还没拖滑块时（draft == current，
                // 没有待应用改动），会被每个刷新周期踢回 Presets 页。
                let presets = self.caps.as_ref().map(sorted_presets).unwrap_or_default();
                self.charge_tab = charge_tab_for(self.draft.charge_intent, &presets);
            }
        }

        self.last_refresh = Instant::now();
        self.last_error = None;
        Ok(())
    }

    fn sync_led_ui_from_mode(&mut self, mode: PowerLedMode) {
        if let PowerLedMode::Animation(config) = mode {
            self.led_breathing_preset = preset_from_breath_config(config);
            self.led_step_up = config.step_up;
            self.led_step_down = config.step_down;
            self.led_delay_at_max = config.delay_at_max;
            self.led_delay_at_min = config.delay_at_min;
        }
    }

    fn has_pending_changes(&self) -> bool {
        self.pending_change_count() > 0
    }

    fn pending_change_count(&self) -> usize {
        let mut count = 0usize;

        if self.draft.power_profile != self.current.power_profile {
            count += 1;
        }
        if self.draft.keyboard_backlight != self.current.keyboard_backlight {
            count += 1;
        }
        if self.draft.fan_mode_cpu != self.current.fan_mode_cpu {
            count += 1;
        }
        if self.draft.fan_mode_gpu != self.current.fan_mode_gpu {
            count += 1;
        }
        if self.draft.charge_intent != self.current.charge {
            count += 1;
        }
        if self.draft.led_mode != self.current.led_mode {
            count += 1;
        }
        if self.draft.telemetry_enabled != self.current.telemetry_enabled {
            count += 1;
        }

        count
    }

    fn apply_all(&mut self) {
        let mut errors = Vec::new();
        self.set_notice(
            "Applying changes...",
            NoticeLevel::Info,
            Some(Duration::from_secs(NOTICE_TTL_SECS)),
        );

        if self.draft.power_profile != self.current.power_profile
            && let Err(err) = self.request_action(
                "Set power profile",
                &IpcRequest::SetPowerProfile(self.draft.power_profile),
            )
        {
            errors.push(format!("Power profile: {}", err));
        }

        if self.draft.keyboard_backlight != self.current.keyboard_backlight
            && let Err(err) = self.request_action(
                "Set keyboard backlight",
                &IpcRequest::SetKeyboardBacklight(self.draft.keyboard_backlight),
            )
        {
            errors.push(format!("Keyboard backlight: {}", err));
        }

        if self.draft.fan_mode_cpu != self.current.fan_mode_cpu
            && let Err(err) = self.request_action(
                "Set CPU fan mode",
                &IpcRequest::SetFanMode {
                    fan: FanIndex::Cpu,
                    mode: self.draft.fan_mode_cpu,
                },
            )
        {
            errors.push(format!("CPU fan mode: {}", err));
        }

        if self.draft.fan_mode_gpu != self.current.fan_mode_gpu
            && let Err(err) = self.request_action(
                "Set GPU fan mode",
                &IpcRequest::SetFanMode {
                    fan: FanIndex::Gpu,
                    mode: self.draft.fan_mode_gpu,
                },
            )
        {
            errors.push(format!("GPU fan mode: {}", err));
        }

        if self.draft.charge_intent != self.current.charge
            && let Err(err) = self.request_action(
                "Set charge intent",
                &IpcRequest::SetChargeIntent(self.draft.charge_intent),
            )
        {
            errors.push(format!("Charge intent: {}", err));
        }

        if self.draft.led_mode != self.current.led_mode
            && let Err(err) =
                self.request_action("Set LED mode", &IpcRequest::SetLedMode(self.draft.led_mode))
        {
            errors.push(format!("LED mode: {}", err));
        }

        if self.draft.telemetry_enabled != self.current.telemetry_enabled
            && let Err(err) = self.request_action(
                "Set telemetry",
                &IpcRequest::DaemonCommand(DaemonCommand::ActivateTelemetry(
                    self.draft.telemetry_enabled,
                )),
            )
        {
            errors.push(format!("Telemetry: {}", err));
        }

        if errors.is_empty() {
            match self.refresh_data(true) {
                Ok(()) => {
                    self.last_error = None;
                    self.set_notice(
                        "Done",
                        NoticeLevel::Success,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
                Err(err) => {
                    self.last_error = Some(err);
                    self.set_notice(
                        "Applied with refresh error",
                        NoticeLevel::Error,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
            }
        } else {
            self.last_error = Some(errors.join(" | "));
            self.set_notice(
                "Apply failed",
                NoticeLevel::Error,
                Some(Duration::from_secs(NOTICE_TTL_SECS)),
            );
        }
    }

    fn restore_defaults(&mut self) {
        match self.request_action(
            "Restore defaults",
            &IpcRequest::DaemonCommand(DaemonCommand::RestoreDefaults),
        ) {
            Ok(_) => match self.refresh_data(true) {
                Ok(()) => {
                    self.last_error = None;
                    self.set_notice(
                        "Defaults restored",
                        NoticeLevel::Success,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
                Err(err) => {
                    self.last_error = Some(err);
                    self.set_notice(
                        "Defaults restored with refresh error",
                        NoticeLevel::Error,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
            },
            Err(err) => {
                self.last_error = Some(err);
                self.set_notice(
                    "Restore defaults failed",
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
        }
    }

    fn apply_saved_state(&mut self) {
        match self.request_action(
            "Apply saved state",
            &IpcRequest::DaemonCommand(DaemonCommand::ApplySettings),
        ) {
            Ok(_) => match self.refresh_data(true) {
                Ok(()) => {
                    self.last_error = None;
                    self.set_notice(
                        "Saved state applied",
                        NoticeLevel::Success,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
                Err(err) => {
                    self.last_error = Some(err);
                    self.set_notice(
                        "Saved state applied with refresh error",
                        NoticeLevel::Error,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
            },
            Err(err) => {
                self.last_error = Some(err);
                self.set_notice(
                    "Apply saved state failed",
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
        }
    }

    fn read_now(&mut self) {
        match self.refresh_data(true) {
            Ok(()) => {
                self.last_error = None;
                self.set_notice(
                    "Settings refreshed",
                    NoticeLevel::Info,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
            Err(err) => {
				println!("Error refreshing data: {}", err);
                self.last_error = Some(err);
                self.set_notice(
                    "Read current settings failed",
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
        }
    }

    fn reconnect_now(&mut self) {
        // Check whether the service is already running
        if !is_service_running("LecooControlDaemon") {
            let started = start_service("LecooControlDaemon");
            if started {
                self.connect_tx.send(AppEvent::Connecting(true)).ok();
            } else {
                self.set_notice(
                    "Failed to start service",
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
            return;
        }
        // Service is running, perform normal reconnect
        self.client = None;
        self.connected = false;
        self.caps = None;
        self.caps_error = None;
        self.status = "Reconnecting...".to_string();
		self.ensure_connected();

        if !self.connected {
            self.set_notice(
                "Reconnect failed",
                NoticeLevel::Error,
                Some(Duration::from_secs(NOTICE_TTL_SECS)),
            );
            return;
        }

        match self.refresh_data(true) {
            Ok(()) => {
                self.last_error = None;
                self.set_notice(
                    "Reconnected",
                    NoticeLevel::Success,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
            Err(err) => {
                self.last_error = Some(err);
                self.set_notice(
                    "Reconnected, refresh failed",
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
        }
    }

    fn set_notice(&mut self, message: &str, level: NoticeLevel, ttl: Option<Duration>) {
        let now = Instant::now();
        self.notice = Some(Notice {
            message: format!("{} {}", local_timestamp(), message),
            level,
            created_at: now,
            expires_at: ttl.map(|t| now + t),
        });
    }

    fn clear_expired_notice(&mut self) {
        if let Some(notice) = &self.notice {
            if let Some(expires_at) = notice.expires_at {
                if Instant::now() >= expires_at {
                    self.notice = None;
                }
            }
        }
    }

    fn notice_alpha(&self) -> f32 {
        if let Some(notice) = &self.notice {
            let elapsed = Instant::now().duration_since(notice.created_at);
            let elapsed_secs = elapsed.as_secs_f32();
            let total_secs = NOTICE_TTL_SECS as f32;
            let fade_duration = 0.5;

            if elapsed_secs < fade_duration {
                // Fade in
                elapsed_secs / fade_duration
            } else if elapsed_secs < total_secs - fade_duration {
                // Full opacity
                1.0
            } else {
                // Fade out
                ((total_secs - elapsed_secs) / fade_duration).max(0.0)
            }
        } else {
            1.0
        }
    }

    fn request_action(&mut self, label: &str, request: &IpcRequest) -> Result<(), String> {
        let _ = label;
        match self.request(request)? {
            IpcResponse::Success => Ok(()),
            IpcResponse::TelemetryDisabledInfo => Ok(()),
            IpcResponse::Error(err) => Err(fmt_ipc_error(&err)),
            other => Err(format!("Unexpected response: {:?}", other)),
        }
    }

    fn render_cards_grid(&mut self, ui: &mut egui::Ui) {
        let available_width = ui.available_width().max(200.0);
        let spacing = 10.0;
        let min_card_width = 200.0;

        let mut columns =
            ((available_width + spacing) / (min_card_width + spacing)).floor() as usize;
        columns = columns.clamp(1, 3);

        let total_spacing = spacing * (columns.saturating_sub(1) as f32);
        let card_width = ((available_width - total_spacing) / columns as f32).max(280.0);

        egui::Grid::new("settings_grid")
            .num_columns(columns)
            .spacing([spacing, spacing])
            .show(ui, |ui| {
                for idx in 0..7 {
                    let card_height = self.card_height_for(idx);
                    ui.vertical(|ui| {
                        ui.set_min_width(card_width);
                        ui.allocate_ui_with_layout(
                            egui::vec2(card_width, card_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| self.render_card_by_index(ui, idx, card_height),
                        );
                    });
                    if (idx + 1) % columns == 0 {
                        ui.end_row();
                    }
                }
            });
    }

    fn card_height_for(&self, idx: usize) -> f32 {
        match idx {
            0 => 120.0, // Monitoring card has a second column for battery health + power source.
            2 => 150.0, // Fan card needs extra space for two editors + sliders.
            4 => 120.0, // Power LED card needs space for breathing presets + Advanced panel
            5 => 130.0, // Charge card: two info labels + Presets/Custom tabs + buttons or sliders
            _ => 80.0,
        }
    }

    fn render_card_by_index(&mut self, ui: &mut egui::Ui, idx: usize, card_height: f32) {
        match idx {
            0 => {
                render_fixed_card(ui, card_height, "Realtime Monitoring", |ui| {
                    ui.columns(2, |columns| {
                        columns[0].label(format!(
                            "CPU temp: {} °C",
                            fmt_opt_u8(self.metrics.cpu_temp)
                        ));
                        columns[0].label(format!(
                            "System temp: {} °C",
                            fmt_opt_u8(self.metrics.sys_temp)
                        ));
                        columns[0].label(format!(
                            "CPU fan: {} RPM",
                            fmt_opt_u16(self.metrics.cpu_rpm)
                        ));
                        columns[0].label(format!(
                            "GPU fan: {} RPM",
                            fmt_opt_u16(self.metrics.gpu_rpm)
                        ));
                        columns[0].label(format!(
                            "Battery: {}%",
                            fmt_opt_u8(self.metrics.battery_percent)
                        ));

                        columns[1].label(format!(
                            "Health: {}",
                            fmt_battery_health(self.metrics.battery_health)
                        ));
                        columns[1].label(format!(
                            "Power: {}",
                            fmt_power_source(self.metrics.power_source)
                        ));
                    });
                });
            }
            1 => {
                let profiles: Option<Vec<PowerProfile>> =
                    self.caps.as_ref().map(|c| c.power_profiles.clone());
                let profiles_available = profiles.as_ref().map(|p| !p.is_empty());
                render_fixed_card(ui, card_height, "Power Profile", |ui| {
                    ui.label(format!(
                        "Current: {}",
                        fmt_power_profile(self.current.power_profile)
                    ));
                    ui.add_enabled_ui(profiles_available == Some(true), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if let Some(list) = &profiles {
                                for profile in list {
                                    ui.selectable_value(
                                        &mut self.draft.power_profile,
                                        *profile,
                                        fmt_power_profile(*profile),
                                    );
                                }
                            }
                        });
                    });
                    if profiles_available != Some(true) {
                        ui.label(caps_note(profiles_available));
                    }
                    render_sync_state(ui, self.draft.power_profile == self.current.power_profile);
                });
            }
            2 => {
                let (cpu_duty, cpu_avail) = fan_caps(self.caps.as_ref(), FanIndex::Cpu);
                let (gpu_duty, gpu_avail) = fan_caps(self.caps.as_ref(), FanIndex::Gpu);
                render_fixed_card(ui, card_height, "Fan Control", |ui| {
                    fan_editor(
                        ui,
                        "CPU",
                        self.current.fan_mode_cpu,
                        &mut self.draft.fan_mode_cpu,
                        cpu_duty,
                        caps_note(cpu_avail),
                    );
                    ui.separator();
                    fan_editor(
                        ui,
                        "GPU",
                        self.current.fan_mode_gpu,
                        &mut self.draft.fan_mode_gpu,
                        gpu_duty,
                        caps_note(gpu_avail),
                    );
                    render_sync_state(
                        ui,
                        self.draft.fan_mode_cpu == self.current.fan_mode_cpu
                            && self.draft.fan_mode_gpu == self.current.fan_mode_gpu,
                    );
                });
            }
            3 => {
                // 注意：KbdCaps 的 levels/custom 目前恒为 true，只有 on_off 可信。
                let kbd_available = self.caps.as_ref().map(|c| c.kbd.on_off);
                render_fixed_card(ui, card_height, "Keyboard Backlight", |ui| {
                    ui.label(format!(
                        "Current: {}",
                        fmt_kbd(self.current.keyboard_backlight)
                    ));
                    ui.add_enabled_ui(kbd_available == Some(true), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.selectable_value(
                                &mut self.draft.keyboard_backlight,
                                KeyboardBacklightLevel::Off,
                                "0",
                            );
                            ui.selectable_value(
                                &mut self.draft.keyboard_backlight,
                                KeyboardBacklightLevel::Low,
                                "1",
                            );
                            ui.selectable_value(
                                &mut self.draft.keyboard_backlight,
                                KeyboardBacklightLevel::Medium,
                                "2",
                            );
                            ui.selectable_value(
                                &mut self.draft.keyboard_backlight,
                                KeyboardBacklightLevel::High,
                                "3",
                            );
                        });
                    });
                    if kbd_available != Some(true) {
                        ui.label(caps_note(kbd_available));
                    }
                    render_sync_state(
                        ui,
                        self.draft.keyboard_backlight == self.current.keyboard_backlight,
                    );
                });
            }
            4 => {
                let led_available = self.caps.as_ref().map(|c| c.led.on_off);
                let led_enabled = led_available == Some(true);
                let brightness_enabled =
                    led_enabled && self.caps.as_ref().map(|c| c.led.brightness).unwrap_or(false);
                let animation_enabled =
                    led_enabled && self.caps.as_ref().map(|c| c.led.animation).unwrap_or(false);
                render_fixed_card(ui, card_height, "Power LED", |ui| {
                    ui.label(format!("Current: {}", fmt_led_mode(self.current.led_mode)));

                    let mut led_custom = match self.draft.led_mode {
                        PowerLedMode::Custom(val) => val,
                        _ => 128,
                    };

                    ui.horizontal(|ui| {
                        let auto_selected = matches!(self.draft.led_mode, PowerLedMode::Auto);
                        ui.add_enabled_ui(led_enabled, |ui| {
                            if ui.selectable_label(auto_selected, "Auto").clicked() {
                                self.draft.led_mode = PowerLedMode::Auto;
                            }
                        });

                        let custom_selected =
                            matches!(self.draft.led_mode, PowerLedMode::Custom(_));
                        ui.add_enabled_ui(brightness_enabled, |ui| {
                            if ui.selectable_label(custom_selected, "Custom").clicked() {
                                self.draft.led_mode = PowerLedMode::Custom(led_custom);
                            }
                        });

                        let breathing_selected =
                            matches!(self.draft.led_mode, PowerLedMode::Animation(_));
                        ui.add_enabled_ui(animation_enabled, |ui| {
                            if ui
                                .selectable_label(breathing_selected, "Breathing")
                                .clicked()
                            {
                                let config = self.led_breathing_preset.to_breath_config();
                                self.led_step_up = config.step_up;
                                self.led_step_down = config.step_down;
                                self.led_delay_at_max = config.delay_at_max;
                                self.led_delay_at_min = config.delay_at_min;
                                self.draft.led_mode = PowerLedMode::Animation(config);
                            }
                        });
                    });

                    if !led_enabled {
                        ui.label(caps_note(led_available));
                    } else if !animation_enabled {
                        ui.small("Breathing not supported by this board");
                    }

                    let custom_active = matches!(self.draft.led_mode, PowerLedMode::Custom(_));
                    ui.add_enabled_ui(custom_active && brightness_enabled, |ui| {
                        if ui
                            .add(egui::Slider::new(&mut led_custom, 0..=255).text("Brightness"))
                            .changed()
                        {
                            self.draft.led_mode = PowerLedMode::Custom(led_custom);
                        }
                    });

                    if custom_active {
                        self.draft.led_mode = PowerLedMode::Custom(led_custom);
                    }

                    let breathing_active =
                        matches!(self.draft.led_mode, PowerLedMode::Animation(_));
                    ui.add_enabled_ui(breathing_active && animation_enabled, |ui| {
                        let previous_preset = self.led_breathing_preset;
                        egui::ComboBox::from_id_salt("breathing_preset")
                            .selected_text(self.led_breathing_preset.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Smooth,
                                    "Smooth",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Sleep,
                                    "Sleep",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Alert,
                                    "Alert",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Zen,
                                    "Zen",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Ping,
                                    "Ping",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Energetic,
                                    "Energetic",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Warning,
                                    "Warning",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Vacuum,
                                    "Vacuum",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Panic,
                                    "Panic",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Sonar,
                                    "Sonar",
                                );
                                ui.selectable_value(
                                    &mut self.led_breathing_preset,
                                    BreathingPreset::Toxic,
                                    "Toxic",
                                );
                            });
                        if self.led_breathing_preset != previous_preset {
                            let config = self.led_breathing_preset.to_breath_config();
                            self.led_step_up = config.step_up;
                            self.led_step_down = config.step_down;
                            self.led_delay_at_max = config.delay_at_max;
                            self.led_delay_at_min = config.delay_at_min;
                        }

                        ui.horizontal(|ui| {
                            let advanced_button_text = if self.led_breathing_advanced_open {
                                "▼ Advanced"
                            } else {
                                "▶ Advanced"
                            };
                            if ui.button(advanced_button_text).clicked() {
                                self.led_breathing_advanced_open =
                                    !self.led_breathing_advanced_open;
                            }
                        });

                        if self.led_breathing_advanced_open {
                            ui.separator();
                            ui.label("Timing (ms/sec):");
                            ui.columns(2, |columns| {
                                columns[0].vertical(|ui| {
                                    ui.label("At Max:");
                                    egui::ComboBox::from_id_salt("delay_at_max")
                                        .width(120.0)
                                        .selected_text(fmt_breath_delay(self.led_delay_at_max))
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Ms15,
                                                "15ms",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Ms125,
                                                "125ms",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Ms250,
                                                "250ms",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Sec0_5,
                                                "0.5s",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Sec1,
                                                "1.0s",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Sec2,
                                                "2.0s",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_max,
                                                BreathDelay::Sec4,
                                                "4.0s",
                                            );
                                        });
                                    ui.add_space(4.0);
                                    ui.label("At Min:");
                                    egui::ComboBox::from_id_salt("delay_at_min")
                                        .width(120.0)
                                        .selected_text(fmt_breath_delay(self.led_delay_at_min))
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Ms15,
                                                "15ms",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Ms125,
                                                "125ms",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Ms250,
                                                "250ms",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Sec0_5,
                                                "0.5s",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Sec1,
                                                "1.0s",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Sec2,
                                                "2.0s",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_delay_at_min,
                                                BreathDelay::Sec4,
                                                "4.0s",
                                            );
                                        });
                                });
                                columns[1].vertical(|ui| {
                                    ui.label("Step Up:");
                                    egui::ComboBox::from_id_salt("step_up")
                                        .width(120.0)
                                        .selected_text(fmt_breath_step(self.led_step_up))
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.led_step_up,
                                                BreathStep::Slow,
                                                "Slow",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_step_up,
                                                BreathStep::Medium,
                                                "Medium",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_step_up,
                                                BreathStep::Fast,
                                                "Fast",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_step_up,
                                                BreathStep::Instant,
                                                "Instant",
                                            );
                                        });
                                    ui.add_space(4.0);
                                    ui.label("Step Down:");
                                    egui::ComboBox::from_id_salt("step_down")
                                        .width(120.0)
                                        .selected_text(fmt_breath_step(self.led_step_down))
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.led_step_down,
                                                BreathStep::Slow,
                                                "Slow",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_step_down,
                                                BreathStep::Medium,
                                                "Medium",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_step_down,
                                                BreathStep::Fast,
                                                "Fast",
                                            );
                                            ui.selectable_value(
                                                &mut self.led_step_down,
                                                BreathStep::Instant,
                                                "Instant",
                                            );
                                        });
                                });
                            });
                        }

                        if breathing_active {
                            let mut breath_config = self.led_breathing_preset.to_breath_config();
                            // Keep selected timing in sync even when Advanced panel is collapsed.
                            breath_config.step_up = self.led_step_up;
                            breath_config.step_down = self.led_step_down;
                            breath_config.delay_at_max = self.led_delay_at_max;
                            breath_config.delay_at_min = self.led_delay_at_min;
                            self.draft.led_mode = PowerLedMode::Animation(breath_config);
                        }
                    });

                    render_sync_state(ui, self.draft.led_mode == self.current.led_mode);
                });
            }
            5 => {
                // 档位完全由 daemon 的 presets 决定：有几档画几个按钮，
                // 点下去存的是 daemon 给的 ChargeIntent，发出时原样发回。
                let charge_available = self.caps.as_ref().map(|c| c.charge.supported);
                let charge_enabled = charge_available == Some(true);
                let charge_presets = self.caps.as_ref().map(sorted_presets).unwrap_or_default();
                // 只有板子真的支持任意区间、且区间至少能装下两个不同值时才给 Custom 页。
                let custom_bounds = self
                    .caps
                    .as_ref()
                    .and_then(|c| c.charge.custom_range)
                    .filter(|(lo, hi)| *hi > lo.saturating_add(1));
                // 当前意图优先用 daemon 的 preset 名显示（N155A 上就是 "Balanced"），
                // 匹配不到任何 preset 时才退回通用格式（自定义区间会走到这里）。
                let current_charge_label = charge_presets
                    .iter()
                    .find(|(_, intent)| *intent == self.current.charge)
                    .map(|(name, _)| preset_label(name))
                    .unwrap_or_else(|| fmt_charge_intent(self.current.charge));

                render_fixed_card(ui, card_height, "Battery Charge Limit", |ui| {
                    ui.label(format!("Current profile: {}", current_charge_label));
                    ui.label(format!(
                        "Current range: {}-{}%",
                        fmt_opt_u8(self.metrics.charge_min),
                        fmt_opt_u8(self.metrics.charge_max)
                    ));

                    // 板子不支持任意区间就不画标签页，直接就是 preset 按钮行。
                    // 点标签本身会写进 draft，所以底栏的 Pending changes、Apply all 的
                    // 可用状态、卡片里的同步指示都会立刻跟着变（和 LED 卡片一致）。
                    if charge_enabled && let Some((lo, hi)) = custom_bounds {
                        ui.horizontal(|ui| {
                            let on_presets = self.charge_tab == ChargeTab::Presets;
                            if ui.selectable_label(on_presets, "Presets").clicked() && !on_presets {
                                self.charge_tab = ChargeTab::Presets;
                                // 能回到 daemon 报的那个预设就回它（等于没有改动），
                                // 否则退回档位表第一项。
                                let rollback =
                                    charge_presets.iter().any(|(_, i)| *i == self.current.charge);
                                let target = if rollback {
                                    Some(self.current.charge)
                                } else {
                                    charge_presets.first().map(|(_, i)| *i)
                                };
                                if let Some(intent) = target {
                                    self.draft.charge_intent = intent;
                                }
                            }

                            let on_custom = self.charge_tab == ChargeTab::Custom;
                            if ui.selectable_label(on_custom, "Custom").clicked() && !on_custom {
                                self.charge_tab = ChargeTab::Custom;
                                let (min, max) = custom_seed(
                                    self.draft.charge_intent,
                                    self.metrics.charge_min,
                                    self.metrics.charge_max,
                                    lo,
                                    hi,
                                );
                                self.draft.charge_intent =
                                    ChargeIntent::Preserve(Some(ChargeRange { min, max }));
                            }
                        });
                    }

                    ui.add_enabled_ui(charge_enabled, |ui| {
                        match custom_bounds {
                            Some((lo, hi)) if self.charge_tab == ChargeTab::Custom => {
                                // 和其他卡片一样，滑块始终反映 draft。
                                let (mut min, mut max) = custom_seed(
                                    self.draft.charge_intent,
                                    self.metrics.charge_min,
                                    self.metrics.charge_max,
                                    lo,
                                    hi,
                                );

                                // egui 的 Slider 默认只用 slider_width(100) 宽，右边会空一大片。
                                // 减去数值框 + "Min"/"Max" 标签占的余量，把轨道拉到卡片宽度。
                                ui.spacing_mut().slider_width =
                                    (ui.available_width() - 100.0).max(80.0);

                                // 两个滑块竖着放（上下各一条），标签在各自右侧
                                let mut changed = false;
                                changed |= ui
                                    .add(egui::Slider::new(&mut min, lo..=(hi - 1)).text("Min"))
                                    .changed();
                                changed |= ui
                                    .add(egui::Slider::new(&mut max, (lo + 1)..=hi).text("Max"))
                                    .changed();
                                if min >= max {
                                    min = (max - 1).max(lo);
                                    changed = true;
                                }
                                ui.small(format!(
                                    "Custom: {min}-{max}%  (board allows {lo}-{hi}%)"
                                ));
                                if changed {
                                    self.draft.charge_intent =
                                        ChargeIntent::Preserve(Some(ChargeRange { min, max }));
                                }
                            }
                            _ => {
                                ui.horizontal_wrapped(|ui| {
                                    for (name, intent) in &charge_presets {
                                        ui.selectable_value(
                                            &mut self.draft.charge_intent,
                                            *intent,
                                            preset_label(name),
                                        );
                                    }
                                });
                                if charge_presets.is_empty() {
                                    ui.small("No presets reported by this daemon");
                                }
                            }
                        }
                    });

                    if !charge_enabled {
                        ui.label(caps_note(charge_available));
                    }
                    // daemon 给的英文原文，直接显示。
                    if let Some(reason) = &self.charge_pending {
                        ui.small(format!("Pending: {reason}"));
                    }
                    render_sync_state(ui, self.draft.charge_intent == self.current.charge);
                });
            }
            6 => {
                render_fixed_card(ui, card_height, "Advanced", |ui| {
                    ui.label(format!(
                        "Telemetry ID: 0x{:016X}",
                        self.current.telemetry_client_id
                    ));
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Current telemetry: {}",
                            if self.current.telemetry_enabled {
                                "Enabled"
                            } else {
                                "Disabled"
                            }
                        ));
                        ui.checkbox(&mut self.draft.telemetry_enabled, "Enabled");
                    });
                    render_sync_state(
                        ui,
                        self.draft.telemetry_enabled == self.current.telemetry_enabled,
                    );
                });
            }
            _ => {}
        }
    }
}

impl eframe::App for LecooApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.clear_expired_notice();

        // Strict service wait path: only reconnect after service is really Running, with timeout.
        if self.service_wait_state == ServiceWaitState::Starting {
            if let Some(started_at) = self.service_wait_started_at {
                if started_at.elapsed() >= Duration::from_secs(SERVICE_WAIT_TIMEOUT_SECS) {
                    self.service_wait_state = ServiceWaitState::None;
                    self.service_wait_started_at = None;
                    self.set_notice(
                        "Service start timeout",
                        NoticeLevel::Error,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                } else if self.service_check_last.elapsed() >= Duration::from_secs(1) {
                    self.service_check_last = Instant::now();
                    if is_service_running("LecooControlDaemon") {
                        self.set_notice(
                            "Service is Running, reconnecting...",
                            NoticeLevel::Info,
                            Some(Duration::from_secs(2)),
                        );
                        self.service_wait_state = ServiceWaitState::Started;
                        self.client = None;
                        self.connected = false;
                        self.status = "Reconnecting...".to_string();
                        self.service_check_last = Instant::now() - Duration::from_secs(1);
                    }
                }
            }
        } else if self.service_wait_state == ServiceWaitState::Started {
            if let Some(started_at) = self.service_wait_started_at {
                if started_at.elapsed() >= Duration::from_secs(SERVICE_WAIT_TIMEOUT_SECS) {
                    self.service_wait_state = ServiceWaitState::None;
                    self.service_wait_started_at = None;
                    self.set_notice(
                        "Reconnect timeout",
                        NoticeLevel::Error,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                } else if self.service_check_last.elapsed() >= Duration::from_secs(1) {
                    self.service_check_last = Instant::now();
                    self.ensure_connected();
                    if self.connected {
                        self.service_wait_state = ServiceWaitState::None;
                        self.service_wait_started_at = None;
                        match self.refresh_data(true) {
                            Ok(()) => {
                                self.last_error = None;
                                self.set_notice(
                                    "Reconnected",
                                    NoticeLevel::Success,
                                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                                );
                            }
                            Err(err) => {
                                self.last_error = Some(err);
                                self.set_notice(
                                    "Reconnected, refresh failed",
                                    NoticeLevel::Error,
                                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                                );
                            }
                        }
                    }
                }
            }
        }
        // Receive asynchronous battery info updates
        while let Ok((status, health)) = self.battery_rx.try_recv() {
            self.metrics.power_source = status;
            self.metrics.battery_health = health;
            // Ensure UI refresh after receiving new data
            ctx.request_repaint();
        }
        while let Ok(event) = self.connect_rx.try_recv() {
            match event {
                AppEvent::ConnectionFailed(_e) => {
                    self.is_connecting = false;
                }
                AppEvent::Connecting(flag) => {
                    if flag {
                        self.set_notice(
                            "Service starting, waiting...",
                            NoticeLevel::Info,
                            Some(Duration::from_secs(3)),
                        );
                        self.service_wait_state = ServiceWaitState::Starting;
                        self.service_wait_started_at = Some(Instant::now());
                        self.service_check_last = Instant::now() - Duration::from_secs(1);
                    } else {
                        self.set_notice(
                            "Failed to start service",
                            NoticeLevel::Error,
                            Some(Duration::from_secs(NOTICE_TTL_SECS)),
                        );
                        self.service_wait_state = ServiceWaitState::None;
                        self.service_wait_started_at = None;
                    }
                }
            }
        }

        if self.last_refresh.elapsed() >= self.refresh_rate.as_duration() {
            if self.connected {
                if let Err(err) = self.refresh_data(false) {
                    self.last_error = Some(err);
                    self.connected = false;
                    self.client = None;
                    self.status = "Disconnected".to_string();
                    self.ec_chip.clear();
                    self.ec_revision.clear();
                    self.set_notice(
                        "Background refresh failed",
                        NoticeLevel::Error,
                        Some(Duration::from_secs(NOTICE_TTL_SECS)),
                    );
                }
            } else {
                self.last_error = Some("Not connected".to_string());
                self.status = "Disconnected".to_string();
                self.daemon_version = "unknown".to_string();
                self.ec_chip.clear();
                self.ec_revision.clear();
                self.set_notice(
                    "Disconnected",
                    NoticeLevel::Error,
                    Some(Duration::from_secs(NOTICE_TTL_SECS)),
                );
            }
        }

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("UI Version: {}", UI_VERSION));
                ui.separator();
                ui.label(format!("Daemon: {}", self.status));
                ui.label(format!("Version: {}", self.daemon_version));
                if !self.ec_chip.is_empty() {
                    ui.separator();
                    ui.label(format!("Model: {} Rev {}", self.ec_chip, self.ec_revision));
                }
            });
        });

        egui::TopBottomPanel::bottom("bottom_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Refresh:");
                egui::ComboBox::from_id_salt("refresh_rate")
                    .selected_text(self.refresh_rate.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.refresh_rate, RefreshRate::Sec1, "1s");
                        ui.selectable_value(&mut self.refresh_rate, RefreshRate::Sec2, "2s");
                        ui.selectable_value(&mut self.refresh_rate, RefreshRate::Sec5, "5s");
                    });

                ui.separator();
                let pending_count = self.pending_change_count();
                if pending_count > 0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(190, 120, 20),
                        format!("Pending changes: {}", pending_count),
                    );
                } else {
                    ui.colored_label(egui::Color32::from_rgb(40, 140, 60), "Pending changes: 0");
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(notice) = self.notice.as_ref() {
                        let base_color = match notice.level {
                            NoticeLevel::Info => egui::Color32::from_rgb(70, 120, 200),
                            NoticeLevel::Success => egui::Color32::from_rgb(40, 140, 60),
                            NoticeLevel::Error => egui::Color32::from_rgb(190, 60, 50),
                        };
                        let alpha = self.notice_alpha();
                        let color = egui::Color32::from_rgba_unmultiplied(
                            base_color.r(),
                            base_color.g(),
                            base_color.b(),
                            (base_color.a() as f32 * alpha) as u8,
                        );
                        ui.colored_label(color, &notice.message);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
			ui.horizontal_wrapped(|ui| {
				if ui
					.add_enabled(self.has_pending_changes(), egui::Button::new("Apply all"))
					.clicked()
				{
					self.apply_all();
				}
				if ui.button("Restore defaults").clicked() {
					self.restore_defaults_confirm_open = true;
				}
				if ui.button("Read current settings").clicked() {
					self.read_now();
				}
				if ui.button("Apply saved state").clicked() {
					self.apply_saved_state();
				}
				if ui.button("Reconnect").on_hover_text("will request administrator privileges , if not already started the service").clicked() {
					self.reconnect_now();
				}
				// ui.add(egui::Button::new("Reconnect").on_hover_text("tt"))
				// 	.clicked()
				// 	.then(|| self.reconnect_now());
			});

			ui.separator();

			egui::ScrollArea::vertical().show(ui, |ui| {
				self.render_cards_grid(ui);
			});
		});

        if self.restore_defaults_confirm_open {
            let mut open = self.restore_defaults_confirm_open;
            let mut confirm_restore = false;
            let mut close_dialog = false;
            egui::Window::new("Confirm Restore Defaults")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("This will reset all settings to defaults.");
                    ui.label("Are you sure you want to continue?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close_dialog = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Confirm").color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(190, 60, 50)),
                            )
                            .clicked()
                        {
                            confirm_restore = true;
                            close_dialog = true;
                        }
                    });
                });

            if close_dialog {
                open = false;
            }

            self.restore_defaults_confirm_open = open;
            if confirm_restore {
                self.restore_defaults();
            }
        }

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn fmt_opt_u8(v: Option<u8>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

fn local_timestamp() -> String {
    format!("[{}]", Local::now().format("%H:%M:%S"))
}

fn fmt_opt_u16(v: Option<u16>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

/// 这两项走 WMI，比 IPC 慢，启动后有一小段时间还没有值。
/// 用 "…" 表示"正在读"，避免和"这块板子没有这项数据"混淆。
fn fmt_battery_health(v: Option<u8>) -> String {
    v.map(|n| format!("{}%", n))
        .unwrap_or_else(|| "…".to_string())
}

fn fmt_power_source(v: Option<PowerSourceStatus>) -> &'static str {
    v.map(|p| p.label()).unwrap_or("…")
}

#[cfg(target_os = "windows")]
fn activate_existing_window() -> bool {
    // This title must exactly match the first argument passed to run_native
    let title_str = "Lecoo Control Center";
    let title: Vec<u16> = title_str.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // Passing NULL as class name means search by title only
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            // Restore window if it is minimized
            ShowWindow(hwnd, SW_RESTORE);
            // Bring window to foreground
            SetForegroundWindow(hwnd);
            return true;
        }
    }
    false
}
/// 一次性建好、循环复用的 WMI 连接。
/// `WMIConnection::with_namespace_path` 会消费 `COMLibrary`，所以每个命名空间
/// 各持有一个连接。
#[cfg(target_os = "windows")]
struct BatteryWmi {
    /// `ROOT\wmi`：`BatteryStatus` / `BatteryStaticData` 等类都在这个命名空间。
    root_wmi: Option<WMIConnection>,
    /// `ROOT\CIMV2`：只在 `ROOT\wmi` 查不到时才连。绝大多数机器用不上它，
    /// 所以做成懒加载，免得启动时白等一次 WMI 连接建立。
    cimv2: OnceCell<Option<WMIConnection>>,
}

#[cfg(target_os = "windows")]
impl BatteryWmi {
    fn new() -> Self {
        let root_wmi = COMLibrary::new()
            .ok()
            .and_then(|com| WMIConnection::with_namespace_path("ROOT\\wmi", com).ok());
        Self {
            root_wmi,
            cimv2: OnceCell::new(),
        }
    }

    /// 回退连接，第一次真正用到时才建立。
    fn cimv2(&self) -> Option<&WMIConnection> {
        self.cimv2
            .get_or_init(|| {
                COMLibrary::new()
                    .ok()
                    .and_then(|com| WMIConnection::with_namespace_path("ROOT\\CIMV2", com).ok())
            })
            .as_ref()
    }
}

#[cfg(not(target_os = "windows"))]
struct BatteryWmi;

#[cfg(not(target_os = "windows"))]
impl BatteryWmi {
    fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "windows")]
fn read_power_source_windows(wmi: &BatteryWmi) -> Option<PowerSourceStatus> {
    #[derive(Deserialize, Debug)]
    struct BatteryStatus {
        #[serde(rename = "ChargeRate")]
        charge_rate: Option<i32>,
    }
    #[derive(Deserialize, Debug)]
    struct Win32Battery {
        #[serde(rename = "BatteryStatus")]
        battery_status: Option<u16>,
    }

    let mut status = SYSTEM_POWER_STATUS {
        ACLineStatus: 255,
        BatteryFlag: 255,
        BatteryLifePercent: 255,
        SystemStatusFlag: 0,
        BatteryLifeTime: 0,
        BatteryFullLifeTime: 0,
    };
    let ok = unsafe { GetSystemPowerStatus(&mut status as *mut SYSTEM_POWER_STATUS) };
    if ok == 0 {
        return None;
    }
    if status.ACLineStatus == 0 {
        return Some(PowerSourceStatus::Discharging);
    }
    if status.ACLineStatus != 1 {
        return Some(PowerSourceStatus::Unknown);
    }

    // AC 在线：再问 WMI 到底是在充电还是已经充满。
    if let Some(con) = &wmi.root_wmi
        && let Ok(mut results) =
            con.raw_query::<BatteryStatus>("SELECT ChargeRate FROM BatteryStatus")
        && let Some(charge_rate) = results.pop().and_then(|b| b.charge_rate)
    {
        return Some(if charge_rate == 0 {
            PowerSourceStatus::Ac
        } else {
            PowerSourceStatus::Charging
        });
    }

    if let Some(con) = wmi.cimv2()
        && let Ok(mut results) =
            con.raw_query::<Win32Battery>("SELECT BatteryStatus FROM Win32_Battery")
        && let Some(bat) = results.pop()
    {
        match bat.battery_status {
            Some(2) => return Some(PowerSourceStatus::Charging),
            Some(3) => return Some(PowerSourceStatus::Ac),
            Some(1) => return Some(PowerSourceStatus::Discharging),
            _ => {}
        }
    }

    // 查不到就老实说不知道。以前这里无条件 return Some(Ac)，
    // 结果是 WMI 一失败就永远显示 "AC"，哪怕电池正在充电。
    Some(PowerSourceStatus::Unknown)
}
#[cfg(not(target_os = "windows"))]
fn read_power_source_windows(_wmi: &BatteryWmi) -> Option<PowerSourceStatus> {
    None
}

#[cfg(target_os = "windows")]
fn read_battery_health_windows(wmi: &BatteryWmi) -> Option<u8> {
    #[derive(Deserialize, Debug)]
    struct BatteryStaticData {
        #[serde(rename = "DesignedCapacity")]
        designed_capacity: Option<u32>,
    }
    #[derive(Deserialize, Debug)]
    struct BatteryFullChargedCapacity {
        #[serde(rename = "FullChargedCapacity")]
        full_charged_capacity: Option<u32>,
    }
    #[derive(Deserialize, Debug)]
    struct Win32Battery {
        #[serde(rename = "DesignCapacity")]
        design_capacity: Option<u32>,
        #[serde(rename = "FullChargeCapacity")]
        full_charge_capacity: Option<u32>,
    }

    fn health(designed: u32, full: u32) -> Option<u8> {
        (designed > 0).then(|| ((full as f64 * 100.0 / designed as f64).round() as u8).min(100))
    }

    if let Some(con) = &wmi.root_wmi {
        let designed = con
            .raw_query::<BatteryStaticData>("SELECT DesignedCapacity FROM BatteryStaticData")
            .ok()
            .and_then(|mut v| v.pop().and_then(|d| d.designed_capacity));
        let full = con
            .raw_query::<BatteryFullChargedCapacity>(
                "SELECT FullChargedCapacity FROM BatteryFullChargedCapacity",
            )
            .ok()
            .and_then(|mut v| v.pop().and_then(|d| d.full_charged_capacity));
        if let (Some(designed), Some(full)) = (designed, full)
            && let Some(h) = health(designed, full)
        {
            return Some(h);
        }
    }

    if let Some(con) = wmi.cimv2()
        && let Ok(mut v) = con
            .raw_query::<Win32Battery>("SELECT DesignCapacity, FullChargeCapacity FROM Win32_Battery")
        && let Some(bat) = v.pop()
        && let (Some(designed), Some(full)) = (bat.design_capacity, bat.full_charge_capacity)
        && let Some(h) = health(designed, full)
    {
        return Some(h);
    }

    None
}

#[cfg(not(target_os = "windows"))]
fn read_battery_health_windows(_wmi: &BatteryWmi) -> Option<u8> {
    None
}

fn fmt_power_profile(profile: PowerProfile) -> &'static str {
    match profile {
        PowerProfile::Silent => "Silent",
        PowerProfile::Default => "Default",
        PowerProfile::Performance => "Performance",
    }
}

fn fmt_kbd(level: KeyboardBacklightLevel) -> String {
    match level {
        KeyboardBacklightLevel::Off => "0".to_string(),
        KeyboardBacklightLevel::Low => "1".to_string(),
        KeyboardBacklightLevel::Medium => "2".to_string(),
        KeyboardBacklightLevel::High => "3".to_string(),
        KeyboardBacklightLevel::Custom(u) => format!("Custom: {u}/255"),
    }
}

fn fmt_fan_mode(mode: FanMode) -> String {
    match mode {
        FanMode::Auto => "Auto".to_string(),
        FanMode::Full => "Full".to_string(),
        FanMode::Turbo => "Turbo".to_string(),
        FanMode::Custom(v) => format!("Custom ({})", v),
    }
}

fn fmt_led_mode(mode: PowerLedMode) -> String {
    match mode {
        PowerLedMode::Auto => "Auto".to_string(),
        PowerLedMode::Custom(v) => format!("Custom ({})", v),
        PowerLedMode::Animation(_) => "Animation".to_string(),
    }
}

fn fmt_breath_delay(delay: BreathDelay) -> &'static str {
    match delay {
        BreathDelay::Ms15 => "15ms",
        BreathDelay::Ms125 => "125ms",
        BreathDelay::Ms250 => "250ms",
        BreathDelay::Sec0_5 => "0.5s",
        BreathDelay::Sec1 => "1.0s",
        BreathDelay::Sec2 => "2.0s",
        BreathDelay::Sec4 => "4.0s",
    }
}

fn fmt_breath_step(step: BreathStep) -> &'static str {
    match step {
        BreathStep::Slow => "Slow",
        BreathStep::Medium => "Medium",
        BreathStep::Fast => "Fast",
        BreathStep::Instant => "Instant",
    }
}

fn render_sync_state(ui: &mut egui::Ui, is_synced: bool) {
    if is_synced {
        ui.colored_label(egui::Color32::from_rgb(40, 140, 60), "Synced");
    } else {
        ui.colored_label(egui::Color32::from_rgb(190, 120, 20), "Pending apply");
    }
}

fn render_fixed_card(
    ui: &mut egui::Ui,
    height: f32,
    title: &str,
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_size(egui::vec2(ui.available_width(), height));
        ui.heading(title);
        content(ui);
    });
}

/// `duty_max` 来自能力集（不再硬编码 220）；拿不到时（未连接 / 该板子没这个风扇）
/// 整个编辑器灰掉，并在 `note` 里说明原因。
fn fan_editor(
    ui: &mut egui::Ui,
    title: &str,
    current: FanMode,
    draft: &mut FanMode,
    duty_max: Option<u8>,
    note: &str,
) {
    ui.label(format!("{} current: {}", title, fmt_fan_mode(current)));

    let Some(duty_max) = duty_max else {
        ui.add_enabled_ui(false, |ui| {
            ui.label(note);
        });
        return;
    };

    let mut selected = match *draft {
        FanMode::Auto => 0,
        FanMode::Full => 1,
        FanMode::Turbo => 2,
        FanMode::Custom(_) => 3,
    };

    let mut custom_pwm = match *draft {
        FanMode::Custom(v) => v.min(duty_max),
        _ => 128.min(duty_max),
    };

    ui.horizontal(|ui| {
        ui.radio_value(&mut selected, 0, "Auto");
        ui.radio_value(&mut selected, 1, "Full");
        ui.radio_value(&mut selected, 2, "Turbo");
        ui.radio_value(&mut selected, 3, "Custom");
    });

    ui.add_enabled_ui(selected == 3, |ui| {
        ui.add(egui::Slider::new(&mut custom_pwm, 0..=duty_max).text(format!("{} PWM", title)));
        ui.small(format!("Safe max: {duty_max}"));
    });

    *draft = match selected {
        0 => FanMode::Auto,
        1 => FanMode::Full,
        2 => FanMode::Turbo,
        _ => FanMode::Custom(custom_pwm),
    };
}
