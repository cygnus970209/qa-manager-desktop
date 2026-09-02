//! QA Manager 데스크톱 셸.
//!
//! - 런처(로컬 UI)에서 서버를 추가/선택 → 같은 창의 웹뷰를 서버 URL 로 내비게이트
//! - 트레이 상주: 창을 닫아도 종료되지 않고 숨김 (SSE 연결 유지 → 알림 수신)
//! - 원격 페이지(웹앱)가 호출하는 브리지 커맨드: desktop_notify / set_badge
//!   (웹앱은 `window.__QAM_DESKTOP__` 만 알고 Tauri 에 종속되지 않는다)

use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, RunEvent, State, Url, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;

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
    let mut window = window;
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
    let mut window = window;
    window.navigate(launcher).map_err(|e| e.to_string())
}

/* ─────────────── 커맨드: 웹앱 브리지 (원격 페이지에서 호출) ─────────────── */

#[tauri::command]
fn desktop_notify(app: tauri::AppHandle, title: String, body: String) {
    let _ = app
        .notification()
        .builder()
        .title(if title.is_empty() { "QA Manager".into() } else { title })
        .body(body)
        .show();
}

#[tauri::command]
fn set_badge(window: tauri::WebviewWindow, count: i64) {
    // macOS 독 / Linux 유니티 뱃지. 0 이면 제거. (Windows 는 미지원 — 무시)
    let _ = window.set_badge_count(if count > 0 { Some(count) } else { None });
}

/* ─────────────── 앱 셋업 ─────────────── */

/// 원격 페이지에 주입되는 브리지. 웹앱은 이 전역 객체만 사용한다(Tauri 비종속 인터페이스).
const BRIDGE_JS: &str = r#"
(function () {
  if (window.__QAM_DESKTOP__) return;
  function inv(cmd, args) {
    var f = (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
    if (!f) return Promise.resolve();
    return f(cmd, args).catch(function () {});
  }
  window.__QAM_DESKTOP__ = {
    notify: function (p) { return inv('desktop_notify', { title: (p && p.title) || 'QA Manager', body: (p && p.body) || '' }); },
    setBadge: function (n) { return inv('set_badge', { count: Math.max(0, n | 0) }); },
    openLauncher: function () { return inv('open_launcher'); },
  };
})();
"#;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
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
            });

            // 메인 창 (런처 로드 + 브리지 주입 — 주입 스크립트는 이후 내비게이션에도 유지됨)
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("QA Manager")
                .inner_size(1280.0, 860.0)
                .min_inner_size(900.0, 600.0)
                .initialization_script(BRIDGE_JS)
                .build()?;

            // 런처 URL 저장 (서버 선택 화면 복귀용)
            if let Ok(url) = window.url() {
                *app.state::<AppState>().launcher_url.lock().unwrap() = Some(url);
            }

            // 마지막 서버 자동 접속
            if let Some(url) = last_url {
                if let Ok(parsed) = Url::parse(&url) {
                    let mut w = window.clone();
                    let _ = w.navigate(parsed);
                }
            }

            // 트레이: 열기 / 서버 선택 / 종료
            let show_item = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let servers_item = MenuItem::with_id(app, "servers", "서버 선택", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &servers_item, &quit_item])?;
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
            set_badge
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // macOS: 독 아이콘 클릭 시 창 복원
            if let RunEvent::Reopen { .. } = event {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        });
}
