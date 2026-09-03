fn main() {
    // 앱 커맨드 권한(allow-<command>, 밑줄→하이픈)을 자동 생성한다.
    // Tauri v2 는 원격 페이지(서버 웹앱)가 앱 커맨드를 호출할 때 명시적 권한이 없으면 거부하므로
    // 여기에 커맨드를 등록하고 capabilities/*.json 에서 참조해야 한다.
    // (앱 매니페스트가 존재하면 로컬 런처의 커맨드도 권한이 필요해진다 — main-local.json 참고)
    // 커맨드를 추가하면 이 목록과 capabilities 양쪽에 함께 추가할 것.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "list_servers",
            "add_server",
            "remove_server",
            "connect",
            "open_launcher",
            "desktop_notify",
            "set_badge",
            "desktop_info",
            "check_update",
            "notification_permission",
            "request_notification_permission",
            "open_notification_settings",
        ])),
    )
    .expect("failed to run tauri-build");
}
