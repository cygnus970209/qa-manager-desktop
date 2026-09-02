# QA Manager Desktop

[QA Manager](https://github.com/intocns/qa-manager) 의 데스크톱 셸(Tauri v2).

AFFiNE 방식의 **서버 선택형** 앱입니다 — 셀프호스팅 서버 URL 을 추가해 접속하고,
추후 SaaS(공식 클라우드 서버)가 생기면 같은 목록에 추가하는 것으로 확장됩니다.

## 구조

- **런처(로컬 화면)** — 서버 목록/추가/삭제, `/api/ping` 헬스체크 후 저장
- **메인 웹뷰** — 선택한 서버 URL 로 전환. 쿠키 인증·SSE 등 웹앱 동작 그대로
- **트레이 상주** — 창을 닫아도 백그라운드 유지(SSE 연결 유지), 트레이에서 열기/서버 선택/종료
- **네이티브 알림 + 독 뱃지** — 웹앱이 노출하는 `window.__QAM_DESKTOP__` 브리지로
  새 알림 → OS 알림, 안읽음 수 → 앱 아이콘 뱃지(macOS/Linux)

웹앱 쪽 연동 코드는 qa-manager 리포의 `frontend/app/stores/notifications.ts` 에 있습니다
(브리지가 없으면 아무 동작도 하지 않는 선택적 인터페이스 — Tauri 에 종속되지 않음).

## 개발

```bash
npm install
npm run dev      # 개발 실행
npm run build    # 배포 번들 (.dmg / .msi / .AppImage)
```

요구사항: Rust(stable), macOS 는 Xcode CLT, Windows 는 WebView2(기본 내장).

## 서버 저장 위치

`{앱 설정 디렉터리}/servers.json` — `{ servers: [{ id, name, url }], last }`
