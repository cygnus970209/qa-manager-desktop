//! QA Manager 데스크톱 셸.
//!
//! - 런처(로컬 UI)에서 서버를 추가/선택 → 같은 창의 웹뷰를 서버 URL 로 내비게이트
//! - 트레이 상주: 창을 닫아도 종료되지 않고 숨김 (SSE 연결 유지 → 알림 수신)
//! - 원격 페이지(웹앱)가 호출하는 브리지 커맨드: desktop_notify / set_badge
//!   (웹앱은 `window.__QAM_DESKTOP__` 만 알고 Tauri 에 종속되지 않는다)
//! - 알림 클릭: notify-rust 응답 대기 → 창 표시 → 웹앱이 등록한 `__QAM_DESKTOP__.onNotificationClick(tag)` 호출
//! - 자동 업데이트: 시작 5초 후 + 6시간마다 GitHub Release 의 latest.json 확인, 트레이 "업데이트 확인" 으로 수동 확인
//! - 새 창 요청(target=_blank 링크, window.open): 창은 하나뿐이므로 서버 내부 링크는 같은 창에서 이동,
//!   외부 http(s) 링크는 OS 기본 브라우저로 넘긴다 (handle_new_window)
//!
//! 커맨드 추가 시: `build.rs` 의 AppManifest 목록 + `capabilities/*.json` 권한(allow-<command>)을
//! 함께 갱신해야 한다. 원격 페이지에서 호출하는 커맨드는 `main-remote.json` 에도 넣는다.

use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
#[cfg(target_os = "macos")]
use tauri::RunEvent;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, State, Url, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

/* ─────────────── 서버 설정 (servers.json) ─────────────── */

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ServerConfig {
    #[serde(default)]
    servers: Vec<ServerEntry>,
    #[serde(default)]
    last: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerEntry {
    id: String,
    name: String,
    url: String,
}

struct AppState {
    config_path: PathBuf,
    config: Mutex<ServerConfig>,
    /// 런처(로컬 index.html)의 URL — "서버 선택" 시 되돌아갈 목적지
    launcher_url: Mutex<Option<Url>>,
    /// 자동 확인에서 이미 안내한 새 버전 (세션당 한 번만 묻기 위해)
    update_notified: Mutex<Option<String>>,
}

fn load_config(path: &PathBuf) -> ServerConfig {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_config(path: &PathBuf, config: &ServerConfig) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string_pretty(config) {
        let _ = fs::write(path, raw);
    }
}

/// URL 정규화: 공백/후행 슬래시 제거 + 스킴 검증
fn normalize_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let parsed = Url::parse(trimmed).map_err(|_| "URL 형식이 올바르지 않습니다.".to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("http/https URL 만 지원합니다.".to_string());
    }
    Ok(trimmed.to_string())
}

/* ─────────────── 커맨드: 서버 관리 (런처에서 호출) ─────────────── */

#[tauri::command]
fn list_servers(state: State<'_, AppState>) -> ServerConfig {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
async fn add_server(
    state: State<'_, AppState>,
    name: String,
    url: String,
) -> Result<ServerEntry, String> {
    let normalized = normalize_url(&url)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("서버 이름을 입력하세요.".to_string());
    }

    // 헬스체크: QA Manager 백엔드의 공개 엔드포인트 /api/ping
    let ping = format!("{normalized}/api/ping");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client.get(&ping).send().await.map_err(|_| {
        "서버에 연결할 수 없습니다. URL 과 네트워크를 확인하세요.".to_string()
    })?;
    if !res.status().is_success() {
        return Err(format!(
            "QA Manager 서버 응답이 아닙니다 (/api/ping → {}).",
            res.status()
        ));
    }

    let entry = ServerEntry {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        url: normalized,
    };
    let mut config = state.config.lock().unwrap();
    config.servers.push(entry.clone());
    save_config(&state.config_path, &config);
    Ok(entry)
}

#[tauri::command]
fn remove_server(state: State<'_, AppState>, id: String) -> ServerConfig {
    let mut config = state.config.lock().unwrap();
    config.servers.retain(|s| s.id != id);
    if config.last.as_deref() == Some(id.as_str()) {
        config.last = None;
    }
    save_config(&state.config_path, &config);
    config.clone()
}

#[tauri::command]
fn connect(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let url = {
        let mut config = state.config.lock().unwrap();
        let entry = config
            .servers
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| "서버를 찾을 수 없습니다.".to_string())?;
        config.last = Some(id);
        save_config(&state.config_path, &config);
        entry.url
    };
    let parsed = Url::parse(&url).map_err(|e| e.to_string())?;
    window.navigate(parsed).map_err(|e| e.to_string())
}

/// 서버 화면 → 런처(서버 선택)로 복귀
#[tauri::command]
fn open_launcher(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<(), String> {
    let launcher = state
        .launcher_url
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "런처 URL 을 찾을 수 없습니다.".to_string())?;
    // 다음 실행 시 자동 접속 해제 (사용자가 명시적으로 서버 선택 화면으로 나온 것)
    {
        let mut config = state.config.lock().unwrap();
        config.last = None;
        save_config(&state.config_path, &config);
    }
    window.navigate(launcher).map_err(|e| e.to_string())
}

/* ─────────────── 커맨드: 웹앱 브리지 (원격 페이지에서 호출) ─────────────── */

/// 네이티브 OS 알림. `tag` 가 있으면 클릭 시 웹앱의 `__QAM_DESKTOP__.onNotificationClick(tag)` 를 호출한다.
#[tauri::command]
fn desktop_notify(app: tauri::AppHandle, title: String, body: String, tag: Option<String>) {
    let title = if title.is_empty() { "QA Manager".to_string() } else { title };
    #[cfg(target_os = "macos")]
    notify_macos(app, title, body, tag);
    #[cfg(not(target_os = "macos"))]
    notify_other(app, title, body, tag);
}

/// macOS: UNUserNotificationCenter 의 async API 를 Tauri 의 tokio 런타임에서 실행한다.
/// (blocking API 는 호출 순간 메인 런루프가 "대기 중"인지 검사해 웹뷰가 바쁘면 "Mainthread not running" 으로
/// 실패하므로 쓰지 않는다.) 클릭 응답은 Tauri 가 돌리는 메인 런루프에서 delegate 로 전달된다.
#[cfg(target_os = "macos")]
fn notify_macos(app: tauri::AppHandle, title: String, body: String, tag: Option<String>) {
    tauri::async_runtime::spawn(async move {
        // UNUserNotificationCenter 는 번들(Info.plist)이 있어야 한다. `tauri dev`/cargo run 은 번들이 아니라 생략.
        if mac_usernotifications::check_bundle().is_err() {
            eprintln!("[qam] 번들이 아닌 실행(dev)에서는 네이티브 알림을 보낼 수 없습니다: {title}");
            return;
        }
        let n = mac_usernotifications::Notification::new()
            .title(&title)
            .message(&body);
        let handle = match n.send().await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[qam] 알림 표시 실패: {e}");
                return;
            }
        };
        // 버튼 없는 알림: 클릭이면 default action, 사용자가 지우면 dismissed 로 끝난다
        match handle.response().await {
            Ok(r) if r.is_default_action() => on_notification_click(&app, tag),
            Ok(_) => {}
            Err(e) => eprintln!("[qam] 알림 응답 대기 실패: {e}"),
        }
    });
}

/// Windows/Linux: notify-rust. 응답 대기는 블로킹이므로 알림마다 스레드를 하나 띄운다.
#[cfg(not(target_os = "macos"))]
fn notify_other(app: tauri::AppHandle, title: String, body: String, tag: Option<String>) {
    let identifier = app.config().identifier.clone();
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&title).body(&body).auto_icon();

        #[cfg(target_os = "windows")]
        {
            // 설치된 앱에서만 AppUserModelID 지정. target/debug|release 직접 실행은 등록된 ID 가 없어 토스트가 뜨지 않는다.
            if let Ok(exe) = tauri::utils::platform::current_exe() {
                let dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
                let sep = std::path::MAIN_SEPARATOR;
                if !(dir.ends_with(&format!("{sep}target{sep}debug"))
                    || dir.ends_with(&format!("{sep}target{sep}release")))
                {
                    n.app_id(&identifier);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // XDG 알림 서버는 "default" 액션이 있어야 본문 클릭을 응답으로 전달한다
            let _ = &identifier;
            n.action("default", "열기");
        }

        let handle = match n.show() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[qam] 알림 표시 실패: {e}");
                return;
            }
        };
        let waited = handle.wait_for_response(move |r: &notify_rust::NotificationResponse| {
            let clicked = match r {
                notify_rust::NotificationResponse::Default => true,
                notify_rust::NotificationResponse::Action(a) => a == "default",
                _ => false,
            };
            if clicked {
                on_notification_click(&app, tag);
            }
        });
        if let Err(e) = waited {
            eprintln!("[qam] 알림 응답 대기 실패: {e}");
        }
    });
}

/// 알림 클릭 처리: 창을 보이고 포커스한 뒤 웹앱의 클릭 핸들러에 tag 를 넘긴다 (메인 스레드에서 실행).
fn on_notification_click(app: &tauri::AppHandle, tag: Option<String>) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(w) = handle.get_webview_window("main") else { return };
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let Some(tag) = tag else { return };
        let arg = serde_json::to_string(&tag).unwrap_or_else(|_| "null".to_string());
        let _ = w.eval(format!(
            "(function(){{var b=window.__QAM_DESKTOP__;\
             if(b&&typeof b.onNotificationClick==='function'){{\
               try{{b.onNotificationClick({arg})}}catch(e){{console.warn('[QAM desktop] onNotificationClick 실패:',e)}}}}}})();"
        ));
    });
}

#[tauri::command]
fn set_badge(window: tauri::WebviewWindow, count: i64) {
    // macOS 독 / Linux 유니티 뱃지. 0 이면 제거. (Windows 는 미지원 — 무시)
    let _ = window.set_badge_count(if count > 0 { Some(count) } else { None });
}

/// macOS 알림 권한 확인. 미결정이면 시스템 권한 요청 팝업을 띄우고, 거부 상태면 설정 안내 다이얼로그를 띄운다.
/// 앱 시작 시 한 번 호출. async API 를 tokio 에서 실행한다 (blocking 계열은 메인 런루프 검사로 실패할 수 있음).
#[cfg(target_os = "macos")]
fn ensure_notification_permission(app: tauri::AppHandle) {
    use mac_usernotifications::AuthorizationStatus;
    tauri::async_runtime::spawn(async move {
        if mac_usernotifications::check_bundle().is_err() {
            eprintln!("[qam] 번들이 아닌 실행(dev): 알림 권한 확인 생략");
            return;
        }
        let status = match mac_usernotifications::get_notification_settings().await {
            Ok(s) => s.authorization_status,
            Err(e) => {
                eprintln!("[qam] 알림 설정 조회 실패: {e}");
                return;
            }
        };
        eprintln!("[qam] 알림 권한 상태: {status:?}");
        let denied = match status {
            // 첫 실행: macOS 표준 팝업 ("QA Manager"에서 알림을 보내려고 합니다)
            AuthorizationStatus::NotDetermined => match mac_usernotifications::request_auth().await {
                Ok(granted) => !granted,
                Err(e) => {
                    eprintln!("[qam] 알림 권한 요청 실패: {e}");
                    return;
                }
            },
            AuthorizationStatus::Denied => true,
            _ => false,
        };
        if denied {
            // 다이얼로그는 블로킹이므로 tokio 워커가 아닌 별도 스레드에서 띄운다
            std::thread::spawn(move || show_notification_guidance(&app));
        }
    });
}

/// 알림이 꺼져 있을 때 안내. "시스템 설정 열기"를 누르면 이 앱의 알림 설정 화면으로 이동한다.
#[cfg(target_os = "macos")]
fn show_notification_guidance(app: &tauri::AppHandle) {
    let open_settings = app
        .dialog()
        .message(
            "QA Manager 의 알림이 꺼져 있어 새 알림을 받을 수 없습니다.\n\
             시스템 설정 > 알림 > QA Manager 에서 알림 허용을 켜 주세요.",
        )
        .title("알림 권한 필요")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "시스템 설정 열기".into(),
            "나중에".into(),
        ))
        .blocking_show();
    if open_settings {
        let url = format!(
            "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id={}",
            app.config().identifier
        );
        if let Err(e) = open::that(&url) {
            eprintln!("[qam] 시스템 설정 열기 실패: {e}");
        }
    }
}

/* ─────────────── 자동 업데이트 ─────────────── */

/// 업데이트 확인. `manual`(트레이 메뉴)이면 결과를 항상 다이얼로그로 알리고, 자동 확인이면 새 버전이 있을 때만 묻는다.
/// 설치는 사용자가 "지금 업데이트"를 눌렀을 때만 진행하고, 끝나면 앱을 다시 시작한다 (Windows 는 설치기가 앱을 종료·재실행).
fn check_for_update(app: tauri::AppHandle, manual: bool) {
    tauri::async_runtime::spawn(async move {
        let updater = match app.updater() {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[qam] updater 초기화 실패: {e}");
                if manual {
                    info_dialog(&app, "업데이트 확인", &format!("업데이트를 확인할 수 없습니다.\n{e}"));
                }
                return;
            }
        };
        match updater.check().await {
            Ok(Some(update)) => {
                let version = update.version.clone();
                if !manual {
                    let state = app.state::<AppState>();
                    let mut notified = state.update_notified.lock().unwrap();
                    if notified.as_deref() == Some(version.as_str()) {
                        return;
                    }
                    *notified = Some(version.clone());
                }
                let app2 = app.clone();
                app.dialog()
                    .message(format!(
                        "새 버전 v{version} 이 있습니다 (현재 v{}).\n지금 다운로드해 설치하고 앱을 다시 시작할까요?",
                        update.current_version
                    ))
                    .title("QA Manager 업데이트")
                    .kind(MessageDialogKind::Info)
                    .buttons(MessageDialogButtons::OkCancelCustom("지금 업데이트".into(), "나중에".into()))
                    .show(move |ok| {
                        if !ok {
                            return;
                        }
                        tauri::async_runtime::spawn(async move {
                            match update.download_and_install(|_, _| {}, || {}).await {
                                Ok(()) => app2.restart(),
                                Err(e) => {
                                    eprintln!("[qam] 업데이트 설치 실패: {e}");
                                    info_dialog(&app2, "업데이트 실패", &format!("업데이트를 설치하지 못했습니다.\n{e}"));
                                }
                            }
                        });
                    });
            }
            Ok(None) => {
                if manual {
                    info_dialog(&app, "업데이트 확인", &format!("최신 버전입니다 (v{}).", app.package_info().version));
                }
            }
            Err(e) => {
                eprintln!("[qam] 업데이트 확인 실패: {e}");
                if manual {
                    info_dialog(&app, "업데이트 확인", &format!("업데이트 정보를 가져올 수 없습니다.\n{e}"));
                }
            }
        }
    });
}

fn info_dialog(app: &tauri::AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Info)
        .show(|_| {});
}

/* ─────────────── 웹앱 설정 화면(데스크톱 앱 항목)용 커맨드 ───────────────
 * 웹앱의 /settings/desktop 이 브리지로 호출한다. 결과 알림(업데이트 있음/없음/실패)은 기존과 같이 네이티브 다이얼로그. */

#[derive(Serialize)]
struct DesktopInfo {
    version: String,
    /// "macos" | "windows" | "linux"
    platform: &'static str,
}

#[tauri::command]
fn desktop_info(app: tauri::AppHandle) -> DesktopInfo {
    DesktopInfo {
        version: app.package_info().version.to_string(),
        platform: std::env::consts::OS,
    }
}

/// 수동 업데이트 확인 (트레이 메뉴와 동일).
#[tauri::command]
fn check_update(app: tauri::AppHandle) {
    check_for_update(app, true);
}

/// 알림 권한 상태: "granted" | "denied" | "not_determined" | "unsupported".
/// Windows/Linux 는 notify-rust 에 권한 개념이 없어(OS 설정에서만 제어) 항상 granted.
#[tauri::command]
async fn notification_permission() -> String {
    #[cfg(target_os = "macos")]
    {
        macos_notification_permission().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        "granted".into()
    }
}

#[cfg(target_os = "macos")]
async fn macos_notification_permission() -> String {
    use mac_usernotifications::AuthorizationStatus;
    if mac_usernotifications::check_bundle().is_err() {
        return "unsupported".into();
    }
    match mac_usernotifications::get_notification_settings().await {
        Ok(s) => match s.authorization_status {
            AuthorizationStatus::NotDetermined => "not_determined",
            AuthorizationStatus::Denied => "denied",
            _ => "granted",
        }
        .into(),
        Err(e) => {
            eprintln!("[qam] 알림 설정 조회 실패: {e}");
            "unsupported".into()
        }
    }
}

/// 아직 묻지 않은 상태에서 시스템 권한 팝업을 띄운다. 결과 상태를 돌려준다.
#[tauri::command]
async fn request_notification_permission() -> String {
    #[cfg(target_os = "macos")]
    {
        if mac_usernotifications::check_bundle().is_err() {
            return "unsupported".into();
        }
        match mac_usernotifications::request_auth().await {
            Ok(true) => "granted".into(),
            Ok(false) => "denied".into(),
            Err(e) => {
                eprintln!("[qam] 알림 권한 요청 실패: {e}");
                "unsupported".into()
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        "granted".into()
    }
}

/// OS 알림 설정 화면(이 앱 항목)을 연다.
#[tauri::command]
fn open_notification_settings(app: tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let url = format!(
            "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id={}",
            app.config().identifier
        );
        if let Err(e) = open::that(&url) {
            eprintln!("[qam] 시스템 설정 열기 실패: {e}");
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = &app;
        if let Err(e) = open::that("ms-settings:notifications") {
            eprintln!("[qam] 시스템 설정 열기 실패: {e}");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = &app;
    }
}

/* ─────────────── 앱 셋업 ─────────────── */

/// 원격 페이지에 주입되는 브리지. 웹앱은 이 전역 객체만 사용한다(Tauri 비종속 인터페이스).
/* ─────────────── 새 창 요청 (target=_blank 링크, window.open) ─────────────── */

/// 접속 중인(마지막으로 연결한) 서버 URL. 런처 화면이거나 설정이 없으면 None.
fn current_server_url(state: &AppState) -> Option<Url> {
    let config = state.config.lock().ok()?;
    let id = config.last.as_ref()?;
    let entry = config.servers.iter().find(|s| &s.id == id)?;
    Url::parse(&entry.url).ok()
}

/// 웹뷰의 새 창 요청 처리. 셸은 창을 하나만 쓰므로 새 창을 만들지 않고
/// - 접속 중인 서버와 같은 origin(앱 내부 링크) → 같은 창에서 이동
/// - 그 밖의 http(s) → OS 기본 브라우저, mailto → 메일 앱
/// - blob:/data:/about:blank 등은 무시
/// 처리기를 달지 않으면 웹뷰가 요청을 조용히 버려서 target=_blank 링크·window.open 이 아무 반응도 없다.
fn handle_new_window(app: &tauri::AppHandle, url: Url) {
    match url.scheme() {
        "http" | "https" => {
            let same_server = current_server_url(&app.state::<AppState>())
                .map(|server| server.origin() == url.origin())
                .unwrap_or(false);
            if same_server {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = window.navigate(url) {
                        eprintln!("[qam] 같은 창 이동 실패: {e}");
                    }
                }
                return;
            }
            if let Err(e) = open::that_detached(url.as_str()) {
                eprintln!("[qam] 외부 링크 열기 실패: {e}");
            }
        }
        "mailto" => {
            if let Err(e) = open::that_detached(url.as_str()) {
                eprintln!("[qam] 메일 링크 열기 실패: {e}");
            }
        }
        _ => {}
    }
}

const BRIDGE_JS: &str = r#"
(function () {
  if (window.__QAM_DESKTOP__) return;
  function inv(cmd, args) {
    var f = (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
    if (!f) return Promise.resolve();
    return f(cmd, args).catch(function (e) {
      // ACL 거부 등 실패를 삼키지 않고 남긴다 (웹앱 동작에는 영향 없음)
      console.warn('[QAM desktop] ' + cmd + ' 실패:', e);
    });
  }
  window.__QAM_DESKTOP__ = {
    notify: function (p) {
      return inv('desktop_notify', {
        title: (p && p.title) || 'QA Manager',
        body: (p && p.body) || '',
        tag: (p && p.tag != null) ? String(p.tag) : null,
      });
    },
    // 웹앱이 등록한다: 네이티브 알림 클릭 시 notify 에 넘긴 tag 로 호출된다
    onNotificationClick: null,
    setBadge: function (n) { return inv('set_badge', { count: Math.max(0, n | 0) }); },
    openLauncher: function () { return inv('open_launcher'); },
    // 설정 > 데스크톱 앱 화면용
    getInfo: function () { return inv('desktop_info'); },
    checkForUpdate: function () { return inv('check_update'); },
    getNotificationPermission: function () { return inv('notification_permission'); },
    requestNotificationPermission: function () { return inv('request_notification_permission'); },
    openNotificationSettings: function () { return inv('open_notification_settings'); },
  };
})();
"#;

pub fn run() {
    // RUST_LOG 가 설정된 경우에만 로그 출력 (알림 라이브러리 진단용)
    let _ = env_logger::try_init();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // macOS: 알림 권한 미결정이면 시스템 팝업, 거부면 안내 (별도 스레드)
            #[cfg(target_os = "macos")]
            ensure_notification_permission(app.handle().clone());

            // 설정 로드
            let config_path = app
                .path()
                .app_config_dir()
                .expect("app config dir")
                .join("servers.json");
            let config = load_config(&config_path);
            let last_url = config
                .last
                .as_ref()
                .and_then(|id| config.servers.iter().find(|s| &s.id == id))
                .map(|s| s.url.clone());
            app.manage(AppState {
                config_path,
                config: Mutex::new(config),
                launcher_url: Mutex::new(None),
                update_notified: Mutex::new(None),
            });

            // 메인 창 (런처 로드 + 브리지 주입 — 주입 스크립트는 이후 내비게이션에도 유지됨)
            let new_window_handle = app.handle().clone();
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("QA Manager")
                .inner_size(1280.0, 860.0)
                .min_inner_size(900.0, 600.0)
                .initialization_script(BRIDGE_JS)
                .on_new_window(move |url, _features| {
                    handle_new_window(&new_window_handle, url);
                    tauri::webview::NewWindowResponse::Deny
                })
                .build()?;

            // 런처 URL 저장 (서버 선택 화면 복귀용)
            if let Ok(url) = window.url() {
                *app.state::<AppState>().launcher_url.lock().unwrap() = Some(url);
            }

            // 마지막 서버 자동 접속
            if let Some(url) = last_url {
                if let Ok(parsed) = Url::parse(&url) {
                    let w = window.clone();
                    let _ = w.navigate(parsed);
                }
            }

            // 자동 업데이트: 시작 5초 후 한 번, 이후 6시간마다
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    loop {
                        check_for_update(handle.clone(), false);
                        tokio::time::sleep(std::time::Duration::from_secs(6 * 60 * 60)).await;
                    }
                });
            }

            // 트레이: 열기 / 서버 선택 / 업데이트 확인 / 종료
            let show_item = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let servers_item = MenuItem::with_id(app, "servers", "서버 선택", true, None::<&str>)?;
            let update_item = MenuItem::with_id(app, "update", "업데이트 확인", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &servers_item, &update_item, &quit_item])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let show = |app: &tauri::AppHandle| {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    };
                    match event.id().as_ref() {
                        "show" => show(app),
                        "servers" => {
                            show(app);
                            if let Some(w) = app.get_webview_window("main") {
                                let state = app.state::<AppState>();
                                let _ = open_launcher(w, state);
                            }
                        }
                        "update" => check_for_update(app.clone(), true),
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .build(app)?;

            Ok(())
        })
        // 창 닫기 = 숨김 (트레이 상주 — SSE 연결 유지)
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_servers,
            add_server,
            remove_server,
            connect,
            open_launcher,
            desktop_notify,
            set_badge,
            desktop_info,
            check_update,
            notification_permission,
            request_notification_permission,
            open_notification_settings
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // macOS: 독 아이콘 클릭 시 창 복원 (RunEvent::Reopen 은 macOS 전용 variant — 다른 OS 는 컴파일 제외)
            #[cfg(target_os = "macos")]
            if let RunEvent::Reopen { .. } = event {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}
