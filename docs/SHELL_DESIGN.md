# glide-shell — 풀 셸 교체 설계

작성 2026-07-21. 라운드 12(`8c182f5`, single-instance IPC + 우클릭 verb) 직후 기준.
구현 전 설계 문서. 여기 나온 수치는 전부 이 머신(Zenbook Duo, 155H, 16GB)에서 실측.

## 0. 요약

explorer.exe를 이 계정 한정(HKCU)으로 `glide-shell.exe`로 교체한다.
새 crate `crates/glide-shell` = raw win32 상주 셸(taskbar + tray + desktop + autostart 실행기),
기존 glint(런처)와 glide(파일 관리자)가 각각 시작 메뉴와 탐색기 역할을 맡는다.
**모든 컴포넌트는 explorer를 켜둔 채(alongside) 개발·검증하고, Shell= 스왑은 마지막
마일스톤(M7)에서만 한다.** 복구 사다리 6겹(§7)과 광범위 v1(M1~M6, §9)이 스왑의
선행 조건이다.

핵심 결정 3개:

| 결정 | 선택 | 근거 |
|---|---|---|
| 상주 셸 UI 스택 | raw win32 + **Direct2D/DirectWrite/DirectComposition** + DWM acrylic (egui 아님, GDI 아님, 웹뷰는 논외) | UI/UX 철학(§1) "쉽고 빠르고 **아름답게**". egui는 release 실측 WS 127MB(§4)로 상주 불가 + tray는 `Shell_TrayWnd` **정확한 클래스명** 창이 필요한데 winit은 클래스명을 못 정함. GDI는 가볍지만 AA/알파/애니메이션 미감 상한이 낮아 "아름답게" 탈락. D2D = GPU 가속 + 진짜 AA + DComp 컴포지터 애니메이션 + acrylic, RAM 목표 <40MB (M1에서 실측 게이트) |
| 스왑 범위 | HKCU `Winlogon\Shell`만 | 타 계정·세이프모드 복구 경로 보존. HKLM은 절대 안 건드림 |
| **스왑 게이트** | **광범위 v1 완성 전 스왑 금지** (유저 확정 0721) | 토스트·데스크톱 아이콘·볼륨 OSD·양 패널 바까지 **전부 스왑 전 필수 스코프**. 최소 v1로 스왑했다가 반쪽 셸로 생활하는 시나리오 자체를 배제 |
| 알림(토스트) | **스왑 전 필수 (M4)** | Action Center는 explorer 생태계 소속. 카톡·디스코드 상주 실측 → NIF_INFO 벌룬 + `UserNotificationListener` 자체 토스트 렌더러까지 갖춰야 스왑. 이 API가 막히면 스왑 보류하고 재논의 |

## 1. 목표와 원칙

- **목표**: 부팅하면 glide-shell 바 + glint 런처 + glide 탐색기만으로 하루 종일 생활 가능.
- **원칙 1 — 되돌리기 우선**: 스왑보다 복구를 먼저 만든다. 복구 리허설 없이는 Shell= 안 바꾼다.
- **원칙 2 — alongside 개발**: M1~M6은 explorer 살아있는 상태에서 개발/검증
  (Cairo/ManagedShell이 이 모드가 성립함을 증명함 — §부록 B).
- **원칙 3 — 상주는 가볍게**: 상주 프로세스는 raw win32. egui는 온디맨드 창(glint 오버레이,
  glide)에만.
- **원칙 4 — UI/UX 철학 (유저 확정 0721): "쉽고, 빠르고, 아름답게"**. 셋 다 게이트다 —
  하나라도 죽이는 선택은 기각. MS가 시스템 앱을 웹뷰로 재작성한 길("빠르고"의 시체)은
  금지: 셸 어디에도 WebView2/Tauri/HTML 없음. "아름답게"는 GDI를 기각시킨다(§5 렌더링).
  기본 Win11을 흉내내지 않는다 — glide teal(`theme.rs` ACCENT) 기반의 독자 룩,
  "stock 같으면 실패" 기준은 Zetile과 동일.

## 2. 현재 자산 (이미 있는 것)

| 자산 | 상태 | 셸에서의 역할 |
|---|---|---|
| glint (Alt+Space 런처) | 라이브 검증 완료 | 시작 메뉴 대체. 앱/파일/웹/ms-settings 소스 보유. Win키 훅만 추가하면 됨 |
| glide (파일 관리자) | 라운드 12까지 검증 | 탐색기 대체. 폴더 핸들러 + 우클릭 verb + single-instance IPC 완비 |
| `glide/src/ipc.rs` | 검증 완료 | named pipe 패턴 재사용: glide-shell ↔ glint ↔ glide 제어 채널 |
| `glide/src/register.rs` | 검증 완료 | HKCU 등록/해제 패턴. 셸 등록도 같은 파일 계보로 |
| `glint-core` | 공유 crate | 아이콘 추출, 폰트, Win11 크롬 — taskbar 아이콘 렌더에 재사용 |
| ms-settings 딥링크 테이블 (glint sources.rs) | 검증 완료 | 잃어버리는 퀵설정 플라이아웃의 대체 진입점 |

## 3. explorer.exe 기능 인벤토리 — 대체/유지/포기

### 대체한다 (glide-shell이 구현)

| 기능 | 대체 | 마일스톤 |
|---|---|---|
| 작업표시줄(창 목록, 활성 표시) | taskbar 모듈 (§6.1) | M1 |
| 시계 | taskbar 우측 상태 영역 | M1 |
| 알림 영역(tray icons) | `Shell_TrayWnd` 프로토콜 구현 (§6.2) | M2 |
| 바탕화면(배경, 우클릭) | desktop 모듈 (§6.3) | M3 |
| **Run 키 + 시작프로그램 폴더 실행** | autostart 실행기 (§6.5) — **explorer가 하던 일, 셸이 안 하면 아무도 안 함** | M3 |
| Win+E | RegisterHotKey(MOD_WIN, 'E') → glide. explorer 없으면 등록 가능 (26200에서 registry 우회는 죽었음 — 라운드 11 실측) | M3 |
| Win키 → 시작 | WH_KEYBOARD_LL bare-Win 감지 → glint | M3 |
| 시작 메뉴 | glint (이미 있음) | M3 |
| 파일 창 | glide (이미 있음) | 완료 |
| 볼륨/배터리/네트워크 표시 | 상태 글리프 + 클릭 시 ms-settings (§6.6) | M2 |

### 유지된다 (explorer와 무관, 검증만)

- Alt+Tab, Win+Tab 스위처 — OS 소관
- Ctrl+Alt+Del 보안 화면, UAC, 잠금 화면 — winlogon 소관, **어떤 경우에도 살아있음**
- 한글 IME — `MsCtfMonitor` 스케줄드 태스크가 로그온 시 별도 기동. **M7 체크리스트 1순위 검증 항목** (이 유저에게 치명 요소)
- GLM-Proxy — 스케줄드 태스크라 셸 무관
- 공통 파일 대화상자(열기/저장) — comdlg32 인프로세스
- ms-settings:, UWP 앱 활성화 — AppX 서비스 소관
- 스냅 기능 자체(Win+화살표) — OS 소관. 스냅 레이아웃 **호버 UI**는 미확인(§8)

### 스왑 전 필수로 추가 대체한다 (광범위 v1 — 유저 확정 0721)

| 기능 | 대체 | 마일스톤 |
|---|---|---|
| 토스트 알림 (카톡·디스코드) | `UserNotificationListener`(WinRT) 리스너 + 자체 토스트 카드 렌더. 실패 시 **스왑 보류 + 재논의** | M4 |
| tray 벌룬 (NIF_INFO) | 자체 팝업 렌더 | M2 |
| 볼륨/밝기 OSD | IAudioEndpointVolume 콜백 + 자체 OSD 오버레이, tray 볼륨 글리프 클릭 = 슬라이더 팝업 | M4 |
| 데스크톱 아이콘 | desktop 창에 glide grid 뷰 재사용 (아이콘/선택/더블클릭/우클릭 shellmenu — 이미 glide에 다 있음) | M3 |
| 양 패널 taskbar (Duo) | 모니터별 appbar + 키보드 도킹으로 인한 디스플레이 탈착 전환 (WM_DISPLAYCHANGE) | M5 |

### 포기한다 (v1 명시적 손실 — 축소됨)

| 손실 | 영향 | 미티게이션 |
|---|---|---|
| 퀵 설정 (Win+A) 플라이아웃 | 통합 플라이아웃 없음 | 상태 글리프 클릭 → ms-settings 딥링크 + 볼륨 슬라이더 팝업(M4)으로 대부분 커버 |
| 위젯, Copilot, 검색 하이라이트 | 없음 | 유저가 이미 디블로트로 죽여둠 — 손실 아님 |
| 벽지 슬라이드쇼 | 고정 벽지만 | 필요해지면 v2 |

## 4. 실측 데이터 (2026-07-21, 이 머신)

### RAM — explorer 생태계 vs 목표

| 프로세스 | WS | Private |
|---|---|---|
| explorer.exe | 353 MB | 303 MB |
| StartMenuExperienceHost | 161 MB | 107 MB |
| SearchHost | 115 MB | 74 MB |
| ShellExperienceHost | 83 MB | 50 MB |
| **합계 (죽는 것들)** | **≈712 MB** | **≈534 MB** |
| glide (debug) | 142 MB | 98 MB |
| glide (**release**, exe 4.3MB) | **127 MB** | **94 MB** |

- **egui 상주 = ~100MB급이 release에서도 유지됨** → 상주 셸은 raw win32 확정.
- 목표 예산: glide-shell **<40MB** (D2D/D3D 디바이스 비용 포함 — 정직한 숫자.
  GDI였으면 <20MB지만 "아름답게"에 지불하는 값이고, egui 127MB의 1/3. M1 게이트에서 실측),
  **M1 첫 컷 실측(0721, release): WS 60MB / Private 41MB — WS 기준 예산 초과.**
  본체는 D3D 디바이스 인프라(스왑체인 자체는 바 폭 기준 ~0.7MB×2). 최적화 후보:
  창 늘어날 때 디바이스 공유(트레이/토스트가 같은 디바이스 쓰면 증분 ~0),
  `PREVENT_INTERNAL_THREADING_OPTIMIZATIONS`, 아이콘 캐시 상한. Private 41은 예산 턱걸이 —
  게이트 판정은 M1 마감 시 재실측으로,
  glint 상주(egui, 어쩔 수 없음) ~100MB, glide는 온디맨드.
  합계 ≈ 140~170MB로 explorer 생태계 712MB 대비 **~550MB 순절감**.
  16GB(실측 free 3.2GB) 박스에서 이게 이 프로젝트의 실질 보상.

### 레지스트리 현황

- `HKLM\...\Winlogon\Shell` = `explorer.exe` (기본값, 건드리지 않음)
- `HKCU\...\Winlogon\Shell` = **없음** (우리가 쓸 슬롯)
- `HKLM\...\Winlogon\AutoRestartShell` = **1** → winlogon이 등록된 셸이 죽으면
  자동 재시작해 줌. 커스텀 셸에도 적용 — 복구 사다리 2번째 겹이 공짜.

## 5. 아키텍처

### 프로세스 모델

```
winlogon ── Shell= ──► glide-shell.exe  (raw win32 + D2D, <40MB, 단일 프로세스)
                        ├─ taskbar 스레드: 바 창 + 메시지 루프
                        ├─ tray: Shell_TrayWnd 창 (같은 루프)
                        ├─ desktop: 최하단 배경 창
                        ├─ watchdog: 자식/자신 감시, 크래시 카운터
                        ├─ autostart: Run 키 + Startup 폴더 1회 실행
                        └─ spawn ──► glint.exe (상주, Alt+Space/Win)
                                └─ (온디맨드) glide.exe — 기존 IPC로 단일 인스턴스
```

- glide-shell 안의 창 3개(taskbar/tray/desktop)는 **한 프로세스 한 메시지 루프**.
  스레드 쪼개지 않는다 — win32 창은 만든 스레드에 묶이고, 셋 다 가벼움.
- glint를 자식으로 스폰하고 watchdog이 재시작. glide는 지금처럼 독립.

### crate 구성

```
crates/glide-shell/
  src/main.rs        — 엔트리, 메시지 루프, watchdog, 크래시 카운터
  src/render.rs      — D2D 팩토리/디바이스, DirectWrite, DComp 공유 (창들이 함께 씀)
  src/taskbar.rs     — 바 창, appbar 예약, 창 목록, 렌더(Direct2D)
  src/tray.rs        — Shell_TrayWnd, WM_COPYDATA 프로토콜, 아이콘 스토어
  src/desktop.rs     — 배경 창, 벽지 렌더, 우클릭 메뉴
  src/autostart.rs   — Run/RunOnce 키 + shell:startup 실행
  src/hotkeys.rs     — Win키 LL 훅, Win+E/D 핫키
  src/status.rs      — 시계/배터리/넷/볼륨 글리프 데이터
  src/theme.rs       — glide theme.rs와 동일 팔레트 (수동 동기화)
```

`glint-core` 재사용: 아이콘 추출(taskbar 버튼 아이콘), 폰트 상수.
egui 의존 **없음** — glint-core에서 아이콘 부분이 egui 타입을 반환하면 raw RGBA 반환
헬퍼를 분리한다(icons.rs는 이미 HICON→RGBA 변환을 갖고 있어 분리 쉬움).

### 렌더링 — Direct2D + DirectWrite + DirectComposition (원칙 4가 강제)

**v1 스택** (초안의 "GDI 더블버퍼, DirectWrite는 v2"를 유저 UI/UX 철학 확정으로 상향):

- **Direct2D 1.1** (D3D11 디바이스 위): 도형 전부 진짜 AA — 라운드 사각, 알파 브러시,
  그라디언트. GDI 기각 사유: AA 없는 도형, per-pixel 알파 합성 고통(UpdateLayeredWindow
  수동 조립), 애니메이션 = 타이머 강제 리드로 → "아름답게" 상한 미달.
- **DirectWrite**: 글자 품질을 "불만 시 v2"로 미루지 않는다. 셸 바는 하루 종일 보는
  글자다 — 처음부터 ClearType/grayscale AA 제대로.
- **DirectComposition**: 애니메이션을 컴포지터가 구동 — CPU가 바빠도(cargo -j 2 중에도)
  toast 슬라이드·호버 페이드가 안 끊긴다. 타이머-리드로 방식은 셸 프로세스가 바쁘면
  바로 janky — 이 박스 워크로드(RAM 포화 + 빌드 상주)에선 실질 차이.
- **DWM acrylic 백드롭**: glint이 이미 쓰는 acrylic 경로 재사용 — 바/toast 카드가
  반투명 블러 위 teal 액센트. 웹뷰 한 줄 없이 "예쁜 셸"의 재료는 OS에 다 있다.
- 비용: D3D 디바이스 ~15-25MB WS. 예산 <40MB(§4), M1 게이트에서 실측으로 검증.
  windows crate에 D2D/DWrite/DComp 전체 바인딩 있음 — 추가 의존성 0.

**디자인 언어** (glint/glide/glide-shell 3앱 공통 — 파편화되면 "아름답게" 실패):

- 팔레트: glide `theme.rs` 그대로 — SURFACE #17181C 계열 + teal ACCENT. 수동 동기화가
  아니라 glide-shell `theme.rs`에 상수 복제 후 주석으로 원본 명시(egui 의존 못 가져옴).
- 타이포: Segoe UI Variable(라틴) + 맑은 고딕 폴백(한글) — DirectWrite 폰트 폴백 체인.
- 글리프: Segoe Fluent Icons (glide 사이드바와 동일 계보).
- 모션: 120~180ms ease-out 단일 어휘. glide `animation_time 0.12`와 체감 일치 —
  호버 페이드, 활성 언더라인 폭 성장(glide 액센트 필과 같은 제스처), toast 슬라이드+페이드.
- 레이어: acrylic 바탕 → 솔리드 카드 → teal 액센트. 3겹 이상 안 쌓는다.

DPI: Per-Monitor v2 매니페스트 + WM_DPICHANGED에서 메트릭 재계산 — 이 머신 150% 스케일이
기본 테스트 케이스.

## 6. 컴포넌트 각론

### 6.1 taskbar (M1)

- **창**: `WS_POPUP`, 모니터 하단 풀폭 × 40px(150% DPI에서 60px 물리). 클래스명 자유
  (tray와 분리 — §6.2 참고).
- **화면 예약**: `SHAppBarMessage(ABM_NEW/ABM_SETPOS)` — 최대화 창이 바를 안 덮게.
  모니터별 1개. M1은 주 패널로 시작하되 구조는 처음부터 per-monitor(바 인스턴스 목록),
  M5에서 Duo 양 패널 + 키보드 도킹 전환까지 완성(유저 확정: 양 패널 스왑 전 필수).
- **창 목록**:
  - 초기 채움: `EnumWindows` + 필터(보이는 + 소유자 없는 + `WS_EX_TOOLWINDOW` 아님
    + cloaked 아님(`DwmGetWindowAttribute(DWMWA_CLOAKED)` — UWP 유령 창 걸러냄)).
  - 갱신: `RegisterShellHookWindow(hwnd)` + `RegisterWindowMessage("SHELLHOOK")` →
    `HSHELL_WINDOWCREATED/DESTROYED/WINDOWACTIVATED/FLASH/REDRAW`.
  - 폴백: 훅이 새는 경우 대비 2초 주기 EnumWindows 재대조(diff만 반영).
- **버튼**: 아이콘(WM_GETICON → 클래스 아이콘 → exe 아이콘 순) + 활성 teal 언더라인 +
  FLASH 시 amber 점멸. 클릭 = 활성/최소화 토글, 우클릭 = 닫기/최소화 메뉴(v1 최소).
- **풀스크린 회피**: 포그라운드 창 rect가 모니터 전체를 덮으면 바 숨김(게임/동영상).
  explorer도 같은 휴리스틱.

**SHIPPED 추가 (0721, 3494f71) — 빈 영역 우클릭 메뉴**: 버튼/트레이/상태 셀 밖 우클릭
= 작업 관리자 / glide-shell 다시 시작 / glide-shell 종료. 다시 시작 =
`current_exe` respawn(`CREATE_NO_WINDOW`) 후 PostQuitMessage — 새 인스턴스 appbar
슬롯은 ABN_POSCHANGED로 자연 정착. 하우스 패턴(SFW → TrackPopupMenu
TPM_RETURNCMD|TPM_BOTTOMALIGN, 프로세스-와이드 다크 메뉴) 그대로. 라이브 검증:
유휴 게이트 + WM_RBUTTONUP 주입, 다크 렌더 + WM_CANCELMODE 해제 확인. 다시
시작/종료 항목 자체는 스크립트 실행 금지(도그푸드 바가 죽음) — 도그푸드에서 검증.

### 6.2 tray — Shell_TrayWnd 프로토콜 (M2, 최고 난이도)

모든 셸 대체 프로젝트가 쓰는 리버스드-그러나-30년-안정 프로토콜:

1. **클래스명이 곧 API**: `Shell_TrayWnd`라는 클래스명의 톱레벨 창을 만든다.
   shell32의 `Shell_NotifyIconW`는 `FindWindowW("Shell_TrayWnd", NULL)`로 찾아
   `WM_COPYDATA`를 보낸다. 호환성용 자식 체인(`TrayNotifyWnd` → `SysPager` →
   `ToolbarWindow32`)도 만들어 둔다 — 위치를 뒤지는 구형 앱 대비.
2. **WM_COPYDATA 디스패치** (`COPYDATASTRUCT.dwData`):
   - `0` = appbar 메시지(SHAppBarMessage 릴레이) — 다른 앱의 독/바 등록 요청
   - `1` = `Shell_NotifyIcon` 본체. 페이로드 = 매직+버전+`NOTIFYICONDATAW`.
     `NIM_ADD/MODIFY/DELETE/SETVERSION` 처리, `(hwnd, uID)`/GUID 키 스토어 유지
   - `2`/`3` = 아이콘 rect 질의(`Shell_NotifyIconGetRect`) — 우리가 계산한 rect 반환
3. **기동 브로드캐스트**: `SendNotifyMessageW(HWND_BROADCAST,
   RegisterWindowMessage("TaskbarCreated"), …)` → 살아있는 모든 앱이 아이콘 재등록.
   explorer가 재시작할 때 아이콘이 돌아오는 원리 그대로.
4. **이벤트 포워딩**: 아이콘 클릭/호버 → 등록된 `uCallbackMessage`를 소유 hwnd로
   PostMessage (버전 4면 `WM_CONTEXTMENU`/`NIN_SELECT` 계열). 앱 메뉴가 뜨려면
   포워딩 직전 `SetForegroundWindow(앱 hwnd)` — 안 하면 메뉴가 즉시 닫히는 유명 함정.
5. **벌룬**(NIF_INFO): 자체 팝업 렌더(다크 카드, 우하단 슬라이드). v1 알림의 전부.
6. **툴팁**: NIF_TIP 문자열 자체 렌더.

**alongside 개발의 함정**: explorer가 살아있으면 진짜 `Shell_TrayWnd`가 이미 있어
FindWindow가 explorer 것을 먼저 찾는다 → **M2 테스트는 `taskkill /f /im explorer.exe`
한 세션에서 수행**(Shell= 는 안 건드림, 끝나면 explorer 재실행). 이 테스트 절차 자체가
M7 스왑의 예행연습이 된다.

### 6.3 desktop (M3)

- 최하단 풀스크린 창: `WS_POPUP` + `HWND_BOTTOM` 고정(WM_WINDOWPOSCHANGING에서
  Z 상승 거부), `WS_EX_NOACTIVATE`.
- 벽지: 현재 시스템 벽지 경로(`SystemParametersInfoW(SPI_GETDESKWALLPAPER)`) 렌더.
  슬라이드쇼는 포기(v1).
- 우클릭 메뉴: glide 열기 / 새로고침 / 디스플레이 설정 / 개인 설정(ms-settings 딥링크).
- 더블클릭 빈 공간 → glide로 Desktop 폴더.
- `SetShellWindow(hwnd)` 호출(문서화 안 된 user32 export, 모든 셸 대체가 사용) —
  `GetShellWindow()` 의존 코드와 Z-순서 시맨틱 보정.
- **아이콘 그리드 = M3 스코프**(유저 확정: 광범위 v1). Desktop 폴더를 glide grid
  뷰(아이콘·다중선택·마키·더블클릭·우클릭 shellmenu 전부 기존 코드)로 배경 창에 호스팅.
  단 desktop 창은 raw win32, glide grid는 egui — v1 절충: **desktop 아이콘은 별도
  glide 모드(`glide.exe --desktop`)로 배경 창 위에 자식으로 얹는 방안 vs D2D로
  아이콘+라벨만 재구현하는 방안 중 M3 착수 시 스파이크로 결정** (전자는 egui 상주
  +100MB 비용, 후자는 구현 비용 — 트레이드오프를 코드로 확인 후 선택).
  D2D 상향으로 후자의 구현 비용이 GDI 시절보다 내려감: AA 라벨 텍스트(DirectWrite),
  알파 선택 사각/마키가 공짜 — taskbar 렌더 코드(§5)와 같은 `render.rs`를 그대로 씀.

**구현됨 (0721, desktop.rs) — 스파이크 결정: D2D 재구현안 채택.** egui 상주안은
기각(RAM). 실측으로 확정한 사실들:

- **GPU 공유 선행 리팩터**: 창마다 자체 D3D/D2D/DComp 디바이스를 만들던 것을
  thread-local `render::gpu()`(D3D+D2D+DXGI+DComp+DWrite+WIC 한 벌, per-window는
  DeviceContext+스왑체인만)로 통합. 효과 실측: 데스크톱 없이 private
  79.2→**41.4MB**. 정직한 40MB 목표를 바 스택이 달성.
- 창: 풀 모니터 `WS_POPUP`+`WS_EX_NOACTIVATE|TOOLWINDOW|NOREDIRECTIONBITMAP`,
  `WM_WINDOWPOSCHANGING`에서 `hwndInsertAfter=HWND_BOTTOM` 강제(+`SWP_NOZORDER`
  비트 제거 — 안 하면 강제가 무시됨). 탐색기 공존 상태에서 Progman **위**,
  일반 창 아래 레이어로 검증됨(WindowFromPoint 3지점 = glide_shell_desktop).
  `SetShellWindow`는 아직 안 함(스왑 시점 M7에서).
- 벽지: WIC 디코드 → **`IWICBitmapScaler`(Fant)로 cover 크기까지 축소 후 업로드
  필수.** ASUS Duo 자산이 3600×3600(16.9MB PNG)이라 풀해상도 경로는 private
  167.9MB를 만들었다. 스케일러 적용 후 92.4MB. 나머지 비용은 셸 아이콘 추출
  머신저리(SHCreateItemFromParsingName이 셸 네임스페이스 그래프를 in-proc 로드).
- 아이콘: `IShellItemImageFactory::GetImage`(RESIZETOFIT|BIGGERSIZEOK, 2×
  논리 48px) → HBITMAP → GetDIBits 32bpp → 알파 전-0이면 불투명 처리 →
  premultiply → D2D 비트맵 (icons.rs 패턴). 그림 파일은 실제 썸네일이 나옴.
- 그리드: 유저+공용 Desktop 병합, 숨김/desktop.ini 제외, 폴더 우선 정렬,
  작업영역 안에서 세로 컬럼(셀 84×98, 아이콘 48, `.lnk/.url`은 stem 라벨).
  라벨 = DWrite 11.5 center + ellipsis trimming + 1.2px 오프셋 그림자 이중 드로우.
- 입력: 호버 워시/클릭 선택/Ctrl 토글/빈 곳 마퀴(ACCENT)/더블클릭
  ShellExecuteW/`WM_MOUSEACTIVATE=MA_NOACTIVATE`. 선택 렌더는 SendMessage 주입 +
  PrintWindow로 검증(전역 입력 오염 없이). 호버는 캡처 불가 — TrackMouseEvent가
  실커서 위치를 보고 즉시 WM_MOUSELEAVE를 쏨. **마퀴·더블클릭은 라이브 미검증**
  (유저 활동 중 입력 주입 금지 규칙).
- `WM_SETTINGCHANGE` 방어: ScreenXpert가 스팸 → 아이템 시그니처(경로 목록+작업
  영역)와 벽지 경로+mtime이 같으면 재추출 스킵. `WM_DISPLAYCHANGE`는 시그니처
  무효화 + 벽지 재스케일.
- 진단 스위치: `GLIDE_DESK_OFF=1`(창 자체 생략), `GLIDE_DESK_BARE=1`(벽지·아이콘
  생략) — RAM 3단 측정용(41.4 / 45.0 / 92.4MB).
- **우클릭 구현됨 (0721, shellmenu.rs)** — glide shellmenu.rs 이식판.
  아이템 = 셸 verb 전부 유지(자체 파일 연산 없음 → 필터 없음), 다중 선택은
  클릭 아이템과 같은 부모의 선택 항목들을 child pidl 배열로 하나의
  GetUIObjectOf에 전달(유저/공용 Desktop 혼합 선택은 클릭 쪽 부모만).
  배경 = CreateViewObject + 커스텀(새로 고침/glide로 열기/디스플레이·개인 설정,
  §6.3 원설계). 보기/정렬은 DefView 전용이라 원래 안 나옴(수용).
  다크 메뉴 = uxtheme ordinal 135 SetPreferredAppMode(AllowDark)+136
  FlushMenuThemes, GetProcAddress 가드(실패=밝은 메뉴). NOACTIVATE 창의 모달
  메뉴: SetForegroundWindow 선행 + Track 후 WM_NULL 포스트. **wndproc 재진입
  주의**: TrackPopupMenuEx가 이 wndproc을 펌프하므로 desk &mut 빌림을 메뉴
  호출 전에 끊고 결과 처리 때 GWLP_USERDATA 재역참조. 검증: 배경 메뉴
  라이브(유휴 15s 게이트 + WM_CANCELMODE 강제 해제 rig) — 다크+커스텀+셸 확장
  항목 렌더 확인; 셸 확장 하나가 index 0에 "새 폴더"를 끼워 커스텀 위에 뜸
  (코스메틱 수용). **아이템 메뉴는 라이브 미검증**(위험 verb 때문에 rig 금지 —
  유저 자연 사용으로 검증). 메뉴 verb 실행 후 refresh_all(시그니처 무효화+
  재로드).
- v1 미포함(다음 라운드): 아이콘 위치 저장(탐색기 ItemPos 블롭 미사용 — 자동
  정렬), 파일 워처 새로고침, 키보드, 빈 곳 더블클릭 glide 열기.
- **glide-shell.exe는 콘솔 서브시스템** — 직접 실행하면 콘솔 창이 뜬다. 도그푸드
  실행은 `Start-Process -WindowStyle Hidden`(stderr 리다이렉트 겸용).

### 6.4 start = glint 통합 (M3)

- glide-shell이 glint를 스폰·감시.
- `WH_KEYBOARD_LL` 훅: bare-Win 눌렀다 뗌(다른 키 조합 없이) → glint 토글.
  훅 콜백은 <1ms 유지(무거우면 OS가 훅 강제 해제).
- glint 쪽은 기존 Alt+Space 경로 재사용 — 셸 쪽에서 named pipe로 "토글" 신호만 보냄
  (glide ipc.rs 패턴 복사).
- **유저 확정 0721: "언젠가 시작 메뉴도 구현해야 함"** — glint 런처 통합은 중간
  단계고, 앱 목록(시작 메뉴 폴더 열거 + UWP)·전원 메뉴·핀 그리드를 갖춘 진짜 시작
  메뉴 패널이 최종 목표. 팝업 기계(preview/flyout) 위에 얹는다. 스왑 게이트에는
  미포함, 로드맵에는 존재.

**구현됨 (0721, winkey.rs + glint-core/toggle_pipe.rs).**

- 훅: `WH_KEYBOARD_LL`, bare-Win 판정 = Win down→up 사이에 다른 키 없음
  (injected 이벤트는 장부에서 제외). bare 릴리즈는 **삼키고** dummy(VK 0xFF)
  down/up + Win up을 SendInput으로 재합성 — 시스템은 Win+dummy 콤보로 보므로
  스톡 시작 메뉴가 안 뜨고, Win 키 up은 전달되어 키 상태 안 꼬임 (PowerToys
  마스킹 기법). Win+콤보는 그대로 통과. 콜백은 플래그+PostMessage(WM_APP+2)만.
- 토글: UI 스레드에서 `\\.\pipe\glint-toggle`에 write; 파이프 없으면(글린트
  미실행) 셸 옆의 glint.exe를 ShellExecuteW로 스폰 (시작 시 보이는 상태라 첫
  bare-Win = "메뉴 열림"으로 읽힘). glint 쪽 리스너는 glint-core
  `toggle_pipe::listen`(연결 자체가 신호, 페이로드 무시) → 핫키 핸들러와 동일
  토글. **파이프 이름은 glide-shell에 문자열 중복** — egui 워크스페이스 절반에
  의존하지 않기 위해 의도적.
- 검증: PostMessage(WM_WINKEY) 4연타로 스폰→숨김→표시→숨김 전부 스크린샷 확인.
  **물리 Win 키 경로(훅 판정+dummy 주입+시작메뉴 억제)는 미검증** — 훅이
  injected를 무시하므로 SendInput으로 못 흉내냄, 실제 키만 가능. 도그푸드에서
  자연 검증.
- 주의: glide-shell 실행 중엔 bare-Win이 스톡 시작 메뉴 대신 glint을 연다
  (설계 의도). 셸 종료 시 훅도 사라져 스톡 동작 복귀.
- v1 미포함: glint 감시/재스폰 (죽으면 다음 bare-Win이 다시 스폰하므로 사실상
  커버), Win 길게 눌러 다른 동작 등.

**진짜 시작 메뉴 구현됨 (0722, startmenu.rs + taskbar.rs start 버튼).**

- 팝업 기계 4번째 사용자 (flyout 입력 계보: activatable, WA_INACTIVE/Esc/재클릭/
  1s foreground-check 폴백으로 닫힘). 바 좌단 Win11 4-사각 로고 버튼이 토글.
- **Win10 2-패널 레이아웃 (유저 확정)**: 폭 604 — 좌측 240 앱 목록(맨 위
  "자주 사용" 6행, 아래 초성/A–Z 섹션 전체 앱), 우측 3열 Metro 타일 그리드
  (고정 앱 전용). 패널별 독립 스크롤, 휠은 커서 아래 패널로 라우팅
  (WM_MOUSEWHEEL은 스크린 좌표 → ScreenToClient).
- 앱 열거 = `shell:AppsFolder` 한 번 (win32 .lnk + UWP 동일 취급), 실행 =
  `ShellExecuteW("shell:AppsFolder\{parsing}")`. 열거·아이콘 추출 전부 **MTA COM
  워커 스레드** (IShellItemImageFactory 호출당 5–50ms — UI 스레드에서 하면 호버
  리페인트가 얼어붙는다, v1에서 실증). UI는 완성된 픽셀 버퍼 → D2D 비트맵만.
- 아이콘: 셸이 요청 크기와 다른 HBITMAP을 줄 수 있음 → GetObjectW로 실측 후
  실크기 DIB 추출, aspect-fit 그리기 (v1 아이콘 깨짐 원인).
- 타일: 아이콘 지배색 틴트 배경, 라벨 좌하단, 우클릭 = 고정 해제 / 1×1↔2×1
  토글, first-fit 패킹(와이드 2칸, 정사각이 구멍 메움). 핀은
  `%APPDATA%\glide-shell\start_pins.txt`.
- 타이핑 즉시 검색 (WM_CHAR): 초성 매칭(ㅋㄹ→크롬), ↑↓/Enter, Esc는 검색부터
  해제. 실행 횟수는 start_counts.txt에 영속 → 자주 사용 목록.
- 푸터: 사용자 칩(프로필 폴더) + 문서/다운로드 + 잠금/절전/재시작/종료.
- 검증: 컴파일 0 경고 + 유저 실물 확인 ("오 괜찮네"). 아이콘 수정도 유저 확인
  ("오 아이콘 고쳤네").

**타일 폴더 추가됨 (0722, `31253f1`) — Win10 그룹.**

- 데이터: `Entry.folder: Option<String>` = start_pins.txt 4번째 탭 필드
  (`splitn(4)` — 구 3필드 파일 그대로 읽힘, 하위 호환). 그리드는
  `TileItem::{Single, Folder(name, members)}`로 승격, first-fit `pack(spans)`은
  span 제네릭.
- 폴더 타일 = 중립 슬래브 + 멤버 2×2 미니 아이콘 + 이름. 클릭 → 멤버 그리드
  뷰(고정 FOLDER_HEAD 헤더 + back 칩, 타일이 헤더 밑으로 스크롤). Esc 순서 =
  검색 → 폴더 → 숨김. 마지막 멤버 빠지면 폴더 자동 해체.
- 우클릭 메뉴: 타일 "그룹에 추가 ▸"(기존 그룹 + 새 그룹), 폴더 타일 "그룹 해제",
  멤버 "그룹에서 빼기". **TrackPopupMenu 재진입 펌프 동안 &self 무효 가능** →
  대상은 owned `RTarget` enum으로 복사, 복귀 후 GWLP_USERDATA 재역참조.
- 검증: 컴파일 + 재기동 + 유저 실핀 9개(구포맷) 정상 로드. 폴더 상호작용은
  도그푸드 검증.

**드래그 & 드롭 추가됨 (0722, `6bd79a3`) — 유저 요청 "드래그로".**

- LBUTTONDOWN이 Drag 무장(payload는 **identity 기반** — parsing/그룹명; 타일
  인덱스는 드롭이 수행하는 변이에 걸쳐 무효), 4px 슬롭 넘어야 라이브 —
  클릭 경로 무손상. **ReleaseCapture 전에 drag를 구조체에서 꺼낸다** —
  동기 WM_CAPTURECHANGED 재진입이 취소할 것을 못 찾아야 함(하우스 재진입 규칙).
- 제스처(Win10 동일): 타일 가장자리 = 재배열(teal 삽입 바; TileItem 레벨
  reorder 후 pin 순서로 reflow — 폴더 멤버가 파일에서 인접하게 나옴), 타일
  중앙 위 = 새 그룹(타겟 슬롯 유지 — 드래그된 엔트리를 마지막 멤버 뒤에
  주차), 폴더 타일 위 = 합류(teal 아웃라인), 폴더 뷰 = 멤버 재배열 + 헤더
  밴드 드롭 = 그룹에서 빼기, 좌측 앱 행(목록/자주 사용/검색) 드래그 = 드롭
  위치에 고정(이미 고정이면 타일 이동). 고스트 타일이 grab 오프셋 유지하며
  커서 추적; Esc/캡처 강탈 = 제자리 취소.
- 검증(자체 창 posted 메시지, 실핀 백업→원복→재기동): 재배열/그룹
  생성(그룹 2, Discord first member = 타겟 슬롯 유지)/헤더 빼기 3단계 모두
  파일 정확 일치, 스크린샷으로 인디케이터 3종 + 고스트 렌더 확인. 손맛은
  도그푸드.

explorer가 셸 기동 시 하던 일. 우리가 안 하면 **아무도 안 한다**:

- `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 전 값 실행
- `HKLM\...\Run` (64/32 양쪽 뷰) 실행
- `HKCU/HKLM\...\RunOnce` 실행 후 값 삭제(RunOnce 시맨틱)
- `shell:startup` + `shell:common startup` 폴더의 .lnk/.exe 실행
- 중복 기동 방지: 셸 프로세스 수명당 1회(winlogon이 셸을 재시작해도 재실행 안 하도록
  세션 단위 마커 — `CreateEvent`에 세션 스코프 이름)

**실측(0721): 이 박스 Run 키 8개 — 비어있지 않다.**
HKCU: Discord, KakaoTalk, Docker Desktop, Parsec / HKLM: SecurityHealth(트레이 아이콘),
**Everything**, Cloudflare WARP, Intel Endurance Gaming.
**Everything이 여기 있음** → autostart 실행기가 빠지면 Everything이 안 떠서
**glint 파일 검색과 glide Ctrl+P 파인더가 같이 죽는다**. autostart는 완전성 항목이
아니라 자기 기능 의존성. M3 게이트에 "Everything 자동 기동 + Ctrl+P 동작" 명시.

**출하 0721 (`autostart.rs`)**: enumerate = HKCU/HKLM Run(64/32 뷰) + RunOnce +
Startup 폴더 2종. **StartupApproved 존중이 핵심** — Task Manager에서 끈 항목은 Run
키에 그대로 남고 `Explorer\StartupApproved\{Run,Run32,StartupFolder}`의 12바이트
blob 첫 바이트 홀수=disabled 로만 거부된다. 이걸 무시하면 유저가 디블로트로 죽인
시작 앱이 전부 부활(이 박스 12개 중 8개가 disabled). 실행 = 레지스트리 항목은
`CreateProcessW`(인자 포함 커맨드라인, env 확장), 폴더 항목은 `ShellExecuteW`(.lnk).
RunOnce는 실행 전 값 삭제. 기동 와이어링 = `is_system_shell()`(Winlogon Shell= 이
우리를 가리킬 때만, HKCU 우선) → 오늘은 explorer라 무동작, M7 스왑 순간부터 살아남.
검증: `--autostart-list` 12항목 전원 StartupApproved 원본과 일치(짝수 first_byte=6
케이스 포함), `--autostart-selftest` = 합성 HKCU Run 값 심고 enumerate→execute→마커
파일 확인→자체 정리, PASS. 미구현: 세션당 1회 마커(CreateEvent) — 셸 재시작 시나리오,
M6 안전망과 같이. '!'-접두 RunOnce(성공 후 삭제 시맨틱)는 v1 미지원(플레인만).

### 6.6 상태 영역 (M2)

시계(1분 타이머) + 글리프 3종: 배터리(`GetSystemPowerStatus`),
네트워크(`INetworkListManager` COM), 볼륨(`IAudioEndpointVolume` + 변경 콜백).
클릭 → 각각 ms-settings 딥링크(glint 테이블 재사용). 한/영 표시(현재 입력 로케일,
`GetKeyboardLayout`) 포함 — IME 상태가 안 보이면 이 유저 워크플로에서 불편.

**출하 0721 (`bc322a1`)**: 셀 4종(한영/넷/볼륨/배터리, 없는 정보는 셀 탈락) +
Fluent 글리프 DWrite 포맷, 1초 시계 타이머에 폴링. 볼륨 좌클릭=음소거 토글(라이브
검증), 휠=±2%(휠 lParam은 화면좌표 — 다른 마우스 메시지와 다름), 한영 클릭=VK_HANGUL
탭. IME 판정 = 포그라운드 스레드 `GetKeyboardLayout`==0x412 + `ImmGetDefaultIMEWnd`에
`IMC_GETOPENSTATUS`. ms-settings 딥링크는 아직 없음(넷/배터리 클릭 무동작).

**트레이 클릭 포워딩 라이브 증명 0721 (같은 커밋)**: 핵심 함정 — 우리 바는
`WS_EX_NOACTIVATE`라 클릭해도 포그라운드가 안 바뀌어서 owner 앱의
ShowWindow/SetForegroundWindow가 조용히 거부됨. **received-last-input 자격으로
`AllowSetForegroundWindow(owner pid)` 선행이 해법** (탐색기 태스크바가 하는 일).
추가로 v4 아이콘엔 LBUTTONUP 뒤 `NIN_SELECT`(0x400), 구식 아이콘엔 `CS_DBLCLKS` +
WM_LBUTTONDBLCLK 포워딩. explorer-kill 세션에서 Everything 트레이 클릭 → 창 열림 →
셸훅으로 러닝 버튼 등장까지 전체 루프 확인.

### 6.6.1 호버 프리뷰 팝업 + 버튼 overflow (0721)

`preview.rs` = 바 위 팝업 창 1개(acrylic dark, DWM-rounded, NOREDIRECTIONBITMAP +
자체 Renderer). **§6.7 벌룬/토스트/플라이아웃이 재사용할 팝업 기계의 첫 사용자.**
호버 350ms 타이머 후 표시, 이미 떠 있으면 즉시 재타겟(탐색기 동작), 클릭/이탈/드래그에
숨김.

- 창 버튼: 전체 제목 + **DWM 라이브 썸네일** — `DwmRegisterThumbnail`이 DComp 팝업
  위에도 그대로 합성됨(증명됨). dest rect는 **디바이스 px**, 축소만(fit ≤ 1.0),
  소스 크기 0이면 텍스트 팁으로 폴백.
- 트레이 아이콘: NIF_TIP 텍스트 팁. 상태 셀: 한글 팁(IME/넷/볼륨/배터리 %).
  런처: exe 이름 팁.
- overflow: `Entry.full_width`(자연폭)와 표시폭 분리 — tray_left 앞 공간을 넘으면
  창 버튼만 균등 축소, 하한 `BUTTON_MIN_W`(48 DIP). add/remove/reposition마다 재계산.

라이브 검증 0721: Zetile 창 라이브 썸네일 캡처, 배터리 팁, 더미 6창 → 13버튼 균등
축소 → 닫으면 원복. **RAM WS 84.5 / private 66.2 MB — 40 예산 초과.** 트림 경로 =
바/팝업 Renderer 간 D3D 디바이스 공유(각자 디바이스+스왑체인 소유 중).

### 6.6.2 상태 플라이아웃 (0721, 유저 요청 "Win10처럼")

셀 클릭 → 클러스터 위 패널 1창(`flyout.rs`). preview와 달리 **입력을 받는** 팝업:
NOACTIVATE/TRANSPARENT 없음, 포커스 잃으면/Esc/같은 셀 재클릭이면 닫힘(Win10 동작).
SetForegroundWindow 거부 대비 1Hz 타이머가 상태 갱신 + 닫힘 폴백 겸용.

- **볼륨**: 기본 엔드포인트 이름(IMMDevice property store) + 음소거 버튼 + 슬라이더
  (드래그=해제 동반, 휠 병행) + % 라벨. 셀 클릭의 음소거 토글은 패널 안으로 이동.
- **네트워크**: Wi-Fi 라디오 토글 pill(`WlanSetInterface` phy별 software state),
  SSID 중복 제거 스캔 목록(신호 4단계 글리프/자물쇠/연결됨), 저장 프로필 클릭 =
  `WlanConnect`, 미저장 = ms-settings 이관, 하단 설정 딥링크. `wifi.rs` = wlanapi
  래퍼(첫 인터페이스만).
- **배터리**: 글리프 30px + % 크게 + 상태 + 전원 설정 딥링크. `BatteryLifeTime`
  잔여 추정은 넣었다가 삭제 — "윈도우 배터리타임 추정은 쓰레기" (유저 판정 0721).

함정 재확인: **WM_MOUSELEAVE는 WindowsAndMessaging 미수출 — match 패턴에 쓰면
전부 잡아먹는 바인딩이 됨**(taskbar.rs처럼 로컬 const). WlanConnect는
`Win32_NetworkManagement_Ndis` 피처 게이트.

라이브 검증 0721: 볼륨(장치명 "헤드폰(WF-1000XM5)"·실제 40% 위치), 네트워크(실
SSID 5개·연결됨 표시·신호별 글리프·토글 on), 배터리(51%·"약 48분 사용 가능"),
바깥 클릭 닫힘. 미검증: 슬라이더 드래그, Wi-Fi 토글 off/on(통화 중 회선이라 불가),
프로필 connect. RAM private 79.2MB — D3D 디바이스 3개째, 공유가 트림 경로.

### 6.7 알림 (M4, 스왑 전 필수 — 유저 확정 0721)

- tray 벌룬(NIF_INFO): §6.2-5, 자체 팝업.
- **WNS 토스트**: `UserNotificationListener`(WinRT)로 알림 스트림 구독 → 자체 토스트
  카드(acrylic 다크, teal 액센트, 우하단 DComp 슬라이드+페이드 — §5 디자인 언어) 렌더.
  **스파이크 완료 0721 (`glide-shell --spike-toasts`, M1보다 먼저 실행): PASS-POLLING.**
  이 박스 실측: 언패키지드에서 접근 = Allowed(동의 UI 없이), 열거 완전 동작(실토스트
  13건 앱명+본문 추출 — Discord/휴대폰과 연결/Claude 등), 라이브로 발사한 토스트를
  폴링이 잡음. 단 `NotificationChanged` 이벤트는 `0x80070490 요소가 없습니다`
  (언패키지드 한계) → **v1 = 500ms~1s 폴링, `UserNotification.Id` diff로 신규 검출**.
  이벤트 업그레이드(sparse-MSIX 패키지 아이덴티티)는 M4 선택 실험, 게이트 아님.
  스파이크가 드러낸 추가 한계: 타 앱 토스트의 **액션 실행(버튼/딥링크 활성화)은
  리스너로 불가** — 카드 클릭 = 소스 앱 창 포커스 + `RemoveNotification(id)`로 해제까지.
- 볼륨/밝기 OSD: 하드웨어 키 입력 시 자체 오버레이. 볼륨 = IAudioEndpointVolume
  변경 콜백, 밝기 = WMI `WmiMonitorBrightnessEvent`. tray 볼륨 글리프 클릭 = 슬라이더.

**토스트 카드 구현됨 (0722, toasts.rs).**

- 워커 스레드가 1s 폴링 + `UserNotification.Id` diff (스파이크 판정대로 이벤트
  불가). 첫 폴 = 무음 베이스라인 — 셸 기동 시 백로그가 카드로 재생 안 됨.
  커맨드 채널이 틱 시계 겸용 (recv_timeout = 폴 주기), RemoveNotification 왕복.
- 카드: 작업 영역 우하단 스택(최신이 바 쪽), 최대 3+페이드아웃 1. teal 스트라이프
  + dim 앱명 + semibold 제목 + 줄바꿈 본문 + X. 180ms cubic 페이드+슬라이드,
  수명 8s, 호버 = 카운트다운 정지. **창은 절대 활성화 안 됨**
  (WS_EX_NOACTIVATE + MA_NOACTIVATE) — 포커스 뺏는 토스트는 없느니만 못하다.
  카드 사이 갭은 WM_NCHITTEST HTTRANSPARENT.
- 본문 클릭 = `shell:AppsFolder\{AUMID}` 활성화 + RemoveNotification (리스너로
  타 앱 액션 실행 불가 — 스파이크 발견 — 의 근사). X = 카드만 숨김(알림은
  액션 센터 유지).
- 검증 (실발사 토스트): 1s 내 픽업, 렌더+위치(창 rect = 스크린샷 카드 일치),
  X/본문 클릭 각 400ms 내 해제, 2-카드 스택 렌더 + 8s 만료 후 창 숨김.
  **AUMID 활성화·호버 정지는 도그푸드 검증** (테스트 AUMID 미등록이라 실 카톡/
  디스코드 토스트 필요 — M4 게이트 그대로). 주의: 텍스트 없는 토스트로는 클릭
  경로 검증이 헛돈다(발사 실패가 무음) — fire.ps1은 제목 필수.

**볼륨 OSD 구현됨 (0722, osd.rs, `129ad36`).**

- 이벤트 구동, 유휴 비용 0: `#[implement(IAudioEndpointVolumeCallback)]` VolWatch를
  `RegisterControlChangeNotify`로 등록, OnNotify가 WM_APP_VOL(WM_APP+12,
  wparam=muted, lparam=vol*1000)만 post. 폴링 스레드 없음. Osd가 자체
  enumerator/endpoint/callback 소유 — status.rs 폴러와 독립(글리프 모양만
  `status::volume_glyph` 공유).
- 필: 280×48 논리, 작업 영역 하단 중앙. 글리프 + 트랙 + fill + 퍼센트.
  표시 1.5s, 페이드 0.15s cubic. 팝업 하우스 패턴 + WS_EX_NOACTIVATE/
  MA_NOACTIVATE + 전면 HTTRANSPARENT — 포커스·클릭 절대 안 뺏음.
  toasts와 같은 late-arm/disarm 생명주기.
- **의존성 함정**: windows 0.62에 "implement" cargo feature는 **없다**(매크로는
  기본 제공), 그러나 생성 코드가 `windows_core::` 경로를 참조 → 소비 크레이트에
  `windows-core = "0.62"` 직접 의존 필수. 워크스페이스 + glide-shell에 추가.
- 검증: 바 Vol 셀에 WM_MOUSEWHEEL 스윕 post(자체 창 한정) → OSD 창 visible,
  device rect 785,970–1135,1030 (정확히 280×48@1.25 하단 중앙), 스크린샷으로
  글리프/트랙/fill/숫자 렌더 확인, 2.5s 후 자동 숨김(vis=False), 미러 다운스윕으로
  볼륨 원복. **하드웨어 볼륨 키는 동일 OnNotify 경로지만 미타건 — 도그푸드 검증.**

**밝기 OSD 구현됨 (0722, `df5d27d`) + 플라이아웃 억제 (`061efae`) — M4 코드 완료.**

- 같은 필의 두 번째 모드: 워커 스레드가 WMI 알림 쿼리(root\wmi,
  `WmiMonitorBrightnessEvent`)에 블로킹 → WM_APP_BRIGHT(WM_APP+13)로 퍼센트
  post. 해 글리프 + 트랙 + %. 핫키/ms-설정/WMI 쓰기 전부 같은 이벤트 소스.
  블로킹 Next()는 취소 불가 — 스레드는 프로세스와 함께 죽는다(그때가 맞다).
  RPC_C_AUTHN_WINNT는 로컬 const(Win32_System_Rpc feature 1개 u32에 안 태움).
- **볼륨 슬라이더는 이미 있음** — flyout.rs `Kind::Volume`(§6.6 플라이아웃
  라운드에서 출하: 트랙 드래그/뮤트 버튼/휠, 뮤트는 스피커 버튼으로 이동).
  이번에 추가한 것은 억제 배선: OSD가 플라이아웃 hwnd를 quiet peer로 받아
  **플라이아웃 visible 동안 볼륨 bump 무시**(슬라이더가 이미 같은 숫자를
  보여주는 중) — 밝기 bump는 통과.
- 검증: WmiSetBrightness 57→52 → OSD 필(해 글리프+52% fill+"52") 스크린샷,
  만료 숨김, 57 원복 확인. 슬라이더 리그: Vol 셀 클릭 post → 플라이아웃,
  트랙 30% 클릭 → 마스터 볼륨 48→30 정확(COM 리드백), **OSD 억제 확인**,
  Esc 닫힘, 외부 SetMasterVolumeLevelScalar 48 원복 → OSD 정상 발동 후 숨김.
  물리 밝기/볼륨 키는 도그푸드(동일 이벤트 경로). **M4 잔여 코드 없음** —
  게이트(카톡/디코 실토스트 + 볼륨 키 OSD)는 도그푸드 판정.

## 7. 안전망 — 복구 사다리 (M6, 스왑 전 필수)

겹겹이. 위에서부터 자동:

| # | 겹 | 동작 | 조건 |
|---|---|---|---|
| 1 | 인프로세스 watchdog | taskbar/tray/desktop 창 죽으면 재생성, glint 죽으면 재스폰 | 자동 |
| 2 | winlogon `AutoRestartShell=1` | glide-shell 프로세스 사망 → winlogon이 재기동 | 자동 (실측 확인됨) |
| 3 | **크래시 카운터 자폭** | 기동 시 마커 파일에 크래시 횟수 기록, **10분 내 3회 크래시 → 스스로 HKCU Shell= 삭제 후 explorer.exe 스폰** — 부팅 루프 원천 차단 | 자동 |
| 4 | 레스큐 핫키 | 셸 내 `Ctrl+Alt+Shift+E` → explorer.exe 스폰(풀 explorer 세션 복귀), `Ctrl+Alt+Shift+R` → HKCU Shell= 삭제 + 재로그온 안내 | 수동 1키 |
| 5 | Ctrl+Alt+Del | winlogon 보안 화면은 **셸과 무관하게 항상 뜸** → 작업 관리자 → 새 작업 → `explorer.exe` 또는 `reg delete "HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon" /v Shell /f` | 수동, 최후 |
| 6 | 레스큐 계정 | 로컬 admin 계정 1개 사전 생성 — HKCU 스코프라 그 계정은 무조건 stock explorer로 부팅 | 사전 준비 |

**롤백 리허설(M7 첫 단계)**: 스왑 → 로그온 확인 → 즉시 사다리 4번으로 복귀 →
재스왑. 리허설이 통과해야 도그푸드 시작. 리허설 전 `RESTORE-SHELL.ps1`(사다리 5의
reg delete + explorer 스폰)을 바탕화면과 `%USERPROFILE%\system-optimize-backup\`에
복사해 둔다(디블로트 RESTORE.ps1과 같은 장소 = 유저가 아는 장소).

등록 UI는 glide gear 팝업이 아니라 **glide-shell 자체의 `--register`/`--unregister`
CLI + 확인 프롬프트**로 한다(파일 관리자에 셸 스왑 버튼은 오클릭 위험).

## 8. 정직한 리스크 목록

| 항목 | 등급 | 내용 |
|---|---|---|
| 한글 IME | **검증 필수** | 메커니즘은 이 박스에 존재 확인(0721 실측: `MsCtfMonitor` 태스크 등록 + ctfmon/TextInputHost 실행 중 — explorer가 아니라 태스크 스케줄러 소관). 다만 **explorer 부재 세션에서의 동작은 미실측** → M2/M3의 explorer-kill 게이트에서 선행 확인, M7 체크리스트 1번 유지. 실패 시 스왑 중단 사유 |
| `UserNotificationListener` 스파이크 실패 | ~~높음~~ **해소 (0721 실측)** | PASS-POLLING: 접근 Allowed + 열거/본문 추출 전부 동작, 이벤트만 언패키지드 불가(0x80070490) → v1 폴링 확정(§6.7). 스왑 게이트에서 제거. 잔여: 타 앱 토스트 액션 실행 불가(포커스+해제로 갈음) |
| 데스크톱 아이콘 스택 미정 | 중간 | egui-자식(+100MB 상주) vs D2D 재구현(구현 비용, GDI 시절보다 하락) — M3 착수 스파이크로 결정(§6.3). 어느 쪽이든 M3 게이트는 동일 |
| D2D/DComp 구현 비용 | 낮음~중간 | GDI 대비 초기 셋업(디바이스/스왑체인/타겟) 코드가 김. windows crate 전체 바인딩 + COM은 이미 일상(tray/shellmenu) → 학습 리스크보다 M1 일정 +0.5세션 정도. RAM 실측이 40MB 초과하면 acrylic/DComp만 끄는 격하 경로 있음(D2D 자체는 유지) |
| 스냅 레이아웃 호버 UI | unknown | OS 소관인지 explorer 소관인지 미확인. 스냅 자체(Win+화살표)는 OS |
| Win+V 클립보드 히스토리 | unknown | TextInputHost 소관으로 추정, 미실측 |
| OneDrive/클라우드 상태 아이콘 | 낮음 | tray 프로토콜로 수용 가능, 오버레이 아이콘(체크마크)은 glide 쪽 v2 |
| 게임 풀스크린 (PUBG) | 중간 | 바 숨김 휴리스틱 + appbar 해제 경로 검증 필요. 실패 시 게임 전 explorer 복귀가 워크어라운드 |
| 절전/복귀, 모니터 탈착 (Duo!) | 중간 | WM_DISPLAYCHANGE/WM_DEVICECHANGE에서 appbar 재예약. Duo는 하단 패널이 붙었다 떨어지는 머신 — 전용 테스트 케이스 |
| UWP cloaked 창 유령 버튼 | 낮음 | DWMWA_CLOAKED 필터로 기지 문제 |
| tray 메뉴 즉시 닫힘 | 낮음 | SetForegroundWindow 선행으로 기지 문제 |
| 미지의 explorer 의존 | 존재함 | 그래서 사다리 3(자폭)과 6(레스큐 계정)이 있다 |

## 9. 마일스톤

각 M은 "빌드 green"이 아니라 **라이브 검증 게이트** 통과가 완료 조건 (프로젝트 룰).
광범위 v1 확정(0721)에 따라 M1~M6 **전부**가 스왑(M7)의 선행 조건이다.

- **M1 — taskbar alongside** (1~2세션, D2D 셋업 포함으로 +0.5세션 가능)
  scaffold + `render.rs`(D2D/DWrite/DComp) + 바 창(하단, per-monitor 구조로 시작) +
  appbar 예약 + 창 목록/활성/클릭 + 시계. explorer 바는 자동 숨김으로 공존.
  게이트: 하루 도그푸드 — 창 전환을 우리 바로만 — **+ 미감 게이트**(acrylic + teal +
  호버/활성 애니메이션이 보일 것; stock 흉내면 실패 — 원칙 4) **+ RAM 실측 <40MB**(§4).
  **첫 컷 SHIPPED 0721**: 전 스택(D2D/DWrite/DComp/acrylic) + appbar 스택 공존 +
  창 목록/아이콘/활성 teal 언더라인/FLASH amber/시계(한글 요일) 라이브 렌더 검증,
  활성 추적 실시간 동작 스크린샷 확인. 잔여: 도그푸드, 호버/클릭 육안 검증(스크립트
  클릭은 유저 사용 중이라 미실행), RAM 초과분(§4), alongside 모드에서 explorer 바와의
  갭 밴드(코스메틱, explorer-kill 세션에서 재확인). 함정 기록: windows-rs에서
  `WM_MOUSELEAVE`는 `Win32_UI_Controls` 소속 — 스코프에 없으면 match에서 바인딩
  패턴으로 전락해 **이후 모든 arm을 삼킴**(클릭/appbar 콜백 전사). 로컬 const로 해결.
- **M2 — tray + 상태** (2~3세션, 최고 난이도)
  Shell_TrayWnd 프로토콜 전체 + 벌룬 + 상태 글리프. 게이트: explorer 죽인 세션에서
  TaskbarCreated 브로드캐스트 → 기존 앱 아이콘 등장, 클릭 메뉴 정상, 벌룬 렌더.
  (이 게이트가 M7 예행연습.)
- **M3 — desktop + 아이콘 + autostart + 키** (2~3세션)
  배경 창 + 우클릭 + **데스크톱 아이콘**(§6.3 스파이크로 egui-자식 vs D2D 재구현 결정),
  Run/Startup 실행기, Win키→glint, Win+E→glide.
  게이트: explorer 죽인 세션에서 데스크톱 아이콘 조작/키/자동시작 전부 동작 —
  특히 **Everything 자동 기동 + glide Ctrl+P 검색 성공**(§6.5), 한글 입력 정상.
- **M4 — 알림 + OSD** (1~2세션)
  ~~첫 작업 = UserNotificationListener 스파이크~~ → **스파이크 0721 완료, PASS-POLLING**
  (§6.7 — M1보다 먼저 실행해 스왑 블로커 조기 해소).
  자체 토스트 카드(폴링 diff) + 볼륨/밝기 OSD + 볼륨 슬라이더 팝업.
  게이트: 카톡/디스코드 실메시지가 자체 토스트로 뜸, 볼륨 키에 OSD 뜸.
- **M5 — 멀티모니터** (1세션) *(유저 재정의 0722: Duo 전용이 아니라 일반 다중 화면
  지원. Duo 하단 패널은 "모니터가 생겼다 사라지는" 특수 케이스일 뿐.)*
  양 패널 바 + 키보드 도킹 탈착 전환(WM_DISPLAYCHANGE 재예약) + 150% DPI.
  게이트: 키보드 얹었다 떼기 반복하며 바가 양쪽에서 정상 재배치.
  **첫 컷 SHIPPED 0722 (9dfd12c)**: 비주 모니터마다 경량 세컨더리 바
  (`secondary.rs`, 클래스 `glide_shell_bar2`) — 아이콘-온리 창 버튼(48px, 활성
  언더라인/FLASH 미러) + 시계. tray/상태/시작은 주 바 전용(Win11식 분담). 구조:
  Bar가 `Vec<Box<Secondary>>` 소유, `WM_DISPLAYCHANGE`에 통째 재구축(도킹 탈착 =
  모니터 목록 변화로 흡수), shellhook activate/flash + refresh마다 sync. appbar
  협상은 `appbar_negotiate_on/requery(mon)`으로 분리(주 바는 primary rect 위임).
  **D2D 비트맵은 디바이스 종속** — 세컨더리가 자기 아이콘 캐시 보유. 라이브 검증:
  Duo 하단 패널(0,1200-1920,2400)에서 0,2290-1920,2340 렌더 + work 예약
  2400→2290(스톡 explorer 바 위에 스택), 아이콘/시계 정상, 활성 언더라인이
  포그라운드 전환 따라감(스크린샷 2장). 클릭 활성화는 포커스 강탈이라 스크립트
  미실행(주 바와 동일 `force_foreground` 경로). **잔여 게이트**: 물리 도킹
  탈착 반복 도그푸드.
- **Win10 머슬 메모리 팩 SHIPPED 0722 (67f8a6d)** *(마일스톤 외 유저 요청 "좀 더
  Windows 10 태스크바처럼 — 머슬 메모리가 크더라")*: ① 시계 클릭 → 달력 플라이아웃
  (초 단위 라이브 시계 + 풀 날짜 + 월 그리드, 오늘 = 악센트 사각, 이월 날짜 딤,
  셰브런/휠 = 월 이동, 제목 클릭 = 오늘 복귀, 1s 타이머) ② 우측 끝 바탕화면 보기
  슬리버 8px(주 바 전용, 1클릭 전체 최소화 SW_SHOWMINNOACTIVE → 재클릭 동일 세트
  복원, 시계/상태 클러스터 DESK_W만큼 좌측 시프트) ③ 버튼 가운데 클릭 = 새 인스턴스
  ④ 버튼 우클릭 메뉴에 Win10 점프리스트식 앱 행(exe stem, 클릭 = 새 창) + 구분선.
  라이브 검증: 달력 열림/초 틱/7월 그리드 정확(22 수요일)/6월 네비/Esc 닫힘 —
  전부 posted 메시지. 미발사(교란): 슬리버 토글·가운데 클릭 스폰 = 도그푸드.
- **M6 — 안전망** (1세션)
  watchdog + 크래시 자폭 + 레스큐 핫키 + RESTORE-SHELL.ps1 + `--register` CLI +
  **레스큐 admin 계정 생성(유저 동의 확보됨 0721)**. 게이트: 셸 프로세스 kill →
  winlogon 재기동 확인, 3-크래시 자폭 시뮬레이션 통과.
  **첫 컷 SHIPPED 0722 (ec13045)**: `safety.rs` = `--register`/`--unregister`
  (YES 확인 + 에코백), 크래시 카운터(session.state 센티널로 비정상 종료 감지,
  **크래시 기인 기동 && 10분 내 3회 → Shell= 삭제 + explorer 스폰 + 경고 + exit**;
  clean exit 후 잔존 스탬프는 재발동 안 함), panic 훅 → crash.log,
  `--selftest-crashloop`(임시 상태 디렉터리, 레지스트리 무접촉). 레스큐 핫키
  Ctrl+Alt+Shift+E(explorer 즉시 스폰)/R(Shell= 삭제 + 안내) = bar WM_HOTKEY.
  스크립트: RESTORE-SHELL.ps1(사다리 5, ~\system-optimize-backup\ 복사 완료; 바탕
  화면 복사는 M7 리허설 때) + CREATE-RESCUE-ACCOUNT.ps1(사다리 6, 유저가 승격
  실행 + 비밀번호 직접 입력, admin 그룹은 well-known SID). 사다리 1은 panic 로그 +
  glint on-demand 재스폰(winkey 기존 경로)으로 한정 — 창 사망 = 프로세스 사망 →
  사다리 2(AutoRestartShell=1 이 박스 실측). **검증**: 셀프테스트 5/5 PASS,
  CLI 취소/no-op 경로 레지스트리 무변 확인, 레스큐 E 암 posted WM_HOTKEY로 라이브
  발사(explorer 창 스폰 확인 후 닫음). **미검증**: 실제 핫키 타건(등록 반환값
  미확인 — 도그푸드), armed 자폭(등록 셸 상태 필요 — M7 리허설), winlogon 재기동
  게이트(동일), 레스큐 계정 생성(유저 실행 대기).
- **설정 앱 + Win키 라우팅 SHIPPED 0722 (cbaa622)** *(마일스톤 외 유저 요청 "윈도우
  키 누르면 시작이 떠야… Win+S가 검색… 설정 앱을 새로 만들던가" + "Windows 설정
  앱처럼 잘 만들어줘")*: ① `config.rs` — `%APPDATA%\glide-shell\settings.txt`
  (labels/clock_seconds/desk_sliver/secondary_bars/winkey_start, 파일이 단일
  진실원, 누락 키 = 기본값) ② `settings.rs` — Win11 설정 앱 룩 실창
  (WS_OVERLAPPEDWINDOW + Mica(backdrop=2) + 다크 타이틀바, 좌측 내비 3분류
  작업 표시줄/단축키/정보 + 악센트 인디케이터, 토글 카드 + Win11식 스위치, 정보
  행 = 버전/로그온 셸(query_shell 라이브)/CLI 힌트/crash.log 경로; WM_CLOSE·Esc =
  숨김, 바 메뉴 "작업 표시줄 설정"으로 재소생; `--settings` 단독 실행 가능 —
  **함정: 단독 프로세스는 CoInitializeEx 직접 호출 필수**, 없으면 Renderer가
  0x800401F0) ③ 바 적용 — WM_SETTINGS_CHANGED(WM_APP+14) 수신 시 재로드:
  라벨/아이콘-온리 버튼, HH:MM:SS 시계 폭, 슬리버 on/off, 보조 바 철거/재건
  ④ **Win키 라우팅 재정의(§10-5 번복, 유저 지시)**: bare Win → 시작 메뉴
  (winkey_start=0이면 구 glint 동작), Win+S → glint 검색. `winkey.rs`가 Win 다운
  중 S를 전부 삼키고(오토리피트 + 대응 keyup 포함) 더미 VK 주입으로 bare-Win
  해제 오인 차단. **라이브 검증(posted 리그 setrig2.ps1)**: 카드 클릭 →
  settings.txt 원문 단언(labels=0/clock_seconds=1), 3페이지 전부 렌더 스크린샷,
  WM_CLOSE = 숨김(파괴 아님), 바 라이브 적용(아이콘-온리 + 초 시계 스크린샷),
  bar2 철거/재건, WM_WINKEY → 시작 메뉴 개폐. **미검증**: 실물 Win/Win+S 타건
  (LL 훅 실경로 — 도그푸드), 바 TrackPopupMenu 경유 열기(메뉴 스크립트 불가,
  직접 open() 경로로 갈음). **리그 함정 기록**: 두 프로세스가 같은 설정 창
  클래스를 등록하면 FindWindowW가 바의 *숨은* 창을 잡음 — WM_CLOSE가
  USERDATA-null 창에서 DefWindowProc→DestroyWindow로 추락해 바 재시작 유발.
  FindWindowExW + IsWindowVisible 순회로 해결.
- **click-away 해제 + Win+Shift+S 통과 SHIPPED 0722 (c815616)** *(유저 "버튼 한번 더
  누르면 사라지게 + 다른데 눌러도 사라지게" + "캡처(Win+Shift+S) 안 켜짐")*:
  ① `clickaway.rs` — WH_MOUSE_LL 훅(바 스레드). 팝업 열림 동안(START_OPEN/
  FLYOUT_OPEN 아토믹, show/hide가 셋) 실물 버튼-다운마다 WM_CLICKAWAY(WM_APP+15,
  스크린 좌표)를 바에 post. `Bar::click_away` = 팝업 자신/바 위 클릭은 무시(바
  핸들러가 토글), 그 외 전부 해제. **WA_INACTIVE만으로는 부족한 이유: Win키로 연
  메뉴는 입력 못 받은 프로세스라 SetForegroundWindow가 포그라운드 락에 거부 →
  활성화 자체가 없어 비활성 통지도 영원히 없음** ② 바 자체 비-토글 타겟(창 버튼/
  트레이/슬리버/빈 영역/우클릭/가운데클릭)도 close_popups — 스톡 태스크바 동일
  ③ winkey.rs — 수식키(Shift/Ctrl/Alt) 동반 S는 통과 → Win+Shift+S 캡처 도구 복구.
  검증(clickrig2.ps1 12/12): Win키 토글 개폐, 바깥 CLICKAWAY로 메뉴·달력 해제,
  메뉴 안/바 안 좌표는 유지, 시계 클릭 달력 토글, 메뉴↔달력 스왑. 미검증(실입력
  필요): LL 훅 다리 자체(리그는 WM_CLICKAWAY 직접 post — 훅은 posted 메시지 못 봄),
  실물 Win+Shift+S. **리그 대함정 기록: DPI 비인지 리그 프로세스의 posted 마우스
  lparam을 Windows가 1.25× 재스케일(120dpi) — x1870이 2338로 도착, 테스트 클릭이
  바탕화면 슬리버에 꽂혀 toggle_desktop 연발(유저 창 전체 최소화 수 회). 모든
  리그 첫 줄 = SetProcessDpiAwarenessContext(-4) 필수.**
- **실클릭 토글 가드 + 사운드 장치 전환 SHIPPED 0722 (af69147)** *(유저 "시작 한번
  더 누르면 사라지게, 와이파이/사운드/배터리도. 사운드는 입출력 장치 변경도")*:
  ① **실클릭 레이스 근인**: 클릭 #2의 마우스-DOWN이 WA_INACTIVE로 팝업을 먼저
  해제 → UP의 토글이 "닫힘"을 보고 재오픈. posted 리그는 활성화를 안 움직여
  재현 불가. 해법 = **해제 타임스탬프 가드**: dismiss()가 Instant(+플라이아웃은
  Kind) 스탬프, 마우스 토글은 같은 타겟 해제 후 400ms 내 재오픈 스킵.
  WM_WINKEY는 mouse=false로 가드 우회(선행 마우스-다운이 없으므로). 시작 존
  WM_LBUTTONDBLCLK = 의도적 no-op(빠른 더블클릭이 "메뉴 유지"로 귀결되던 것).
  ② **사운드 플라이아웃 입출력 장치 전환**: `audiopolicy.rs` — 비공개
  IPolicyConfig COM(EarTrumpet 경로), 12-슬롯 vtable 전체 선언(오프셋 보전),
  SetDefaultEndpoint만 호출(eConsole+eMultimedia+eCommunications).
  flyout.rs poll_devices()가 eRender/eCapture 활성 엔드포인트 열거, 출력/입력
  섹션 + 디폴트 체크 글리프. **전환 시 캐시 3종 무효화 필수**: 플라이아웃
  슬라이더 endpoint, OSD 구독(WM_APP_REBIND=WM_APP+16 → resubscribe), 바 상태
  셀(WM_AUDIO_REBIND=WM_APP+17 → rebind_volume). 구 endpoint는 구 장치로 계속
  정상 응답하므로 에러-주도 자가복구가 영원히 안 걸림. 검증(clickrig3 13/13):
  가드 사이클(열림→어웨이 해제→즉시클릭 삼킴→늦은클릭 열림→토글 닫힘) 달력+
  시작 메뉴 양쪽, Win키 가드 우회, 해제 직후 딴 셀은 정상 오픈; 볼륨
  플라이아웃 장치 섹션 스크린샷 + 디폴트 행 클릭 no-op. **리그 함정: 어웨이
  좌표가 팝업 rect 밖이어야 함 — (500,500)은 시작 메뉴 안이라 정상 무시,
  동일 재현 2회로 "제품 버그"처럼 보임. 셀 순서는 [Ime,Net,Vol,Bat]
  (Vol ≈ 디바이스 x1753).** 미검증(도그푸드): 실클릭 WA_INACTIVE 다리,
  실제 장치 전환(유저 라이브 오디오를 끊게 되어 리그 불가).
- **Win10 통합형 알림 센터 SHIPPED 0722 (199b55b)** *(유저 "전자(Win10 QS+알림
  통합)부터 하자 — BT/자동회전 등 바에 못 박는 세팅 수납처")*: 시계 오른쪽 벨 셀 →
  단일 패널(위=놓친 토스트 백로그, 아래=Quick Settings 타일 그리드).
  ① `actioncenter.rs` — flyout.rs 형제(해제 가드/click-away/WA_INACTIVE 해제/DWM
  아크릴·라운드). 알림 목록 = 실제 OS 액션센터의 **라이브 뷰**(UserNotificationListener,
  자체 히스토리 없음): 행 X = RemoveNotification, "모두 지우기" = ClearNotifications →
  Windows에서도 사라짐. 안 들어가는 백로그는 "이전 알림 N개" 푸터로 접힘.
  **모든 WinRT `.join()`·라디오 작업은 단명 MTA 워커 스레드**(창은 바 STA에 삶) →
  워커가 WM_AC_REFRESH(WM_APP+18) post, UI가 스냅샷 drain → **페인트는 락을 절대
  안 만짐**. ② `quicksettings.rs` — 타일 액추에이터: BT/비행기 = WinRT
  Windows.Devices.Radios(**신규 Devices_Radios feature**), 자동 회전 = 비공개 user32
  `GetAutoRotationState`/`SetAutoRotation` 쌍(문서화된 setter 없음 = 스톡 잠금 타일
  경로), Wi-Fi = wifi.rs 재사용. 부재/미지원 라디오는 dim 렌더. ③ taskbar: 벨 셀
  =시계와 슬리버 사이(레이아웃 산수 status_left/clock_right_edge에 NOTIF_CELL_W 반영),
  ac_toggle는 start/flyout과 동일 가드, close_popups/click_away/상호배제에 합류.
  clickaway: AC_OPEN이 훅 "팝업 열림" 조건에 합류. **검증(posted 리그+스크린샷):
  벨 개폐, 실제 백로그 렌더(카드 5 + "이전 알림 14개"), Wi-Fi·BT 실제 라디오 상태
  액센트-ON, 패널이 포그라운드 잡음(실 click-away 해제 성립), 2회차 벨 = 토글 닫힘.**
  미검증(도그푸드 — 전 경로가 실상태 변경): 행 해제/모두 지우기(유저 실알림 삭제),
  BT/비행기/자동회전 토글(연결 끊김·화면 회전), 야간/설정 딥링크, 실클릭 WA_INACTIVE
  가드 다리. **함정: 새 PS 프로세스가 포그라운드를 뺏어 AC를 해제시킴 → 열기+스샷은
  반드시 한 프로세스에서(같은 프로세스가 sleep만 하면 포커스 유지).**
- **tray overflow `^` chevron SHIPPED 0723 (65cb269)** *(유저 "당장 트레이 아이콘도
  구현 안돼있잖아" → 트레이 프로토콜은 이미 구현+증명(199b55b 세션 트레이 rig)이었고,
  진짜 갭 = 아이콘 많으면 바 넘침 → chevron 오버플로가 명확한 buildable 갭. 유저가
  우선순위로 chevron 선택)*: 스트립은 `TRAY_MAX_VISIBLE`(6)까지만, 나머지는 `^`
  chevron이 여는 그리드 플라이아웃(Win10/11 숨긴 아이콘 오버플로, 자체 호스팅).
  ① `trayoverflow.rs` — flyout.rs 형제 팝업(자체 창/DWM 라운드·아크릴/해제 가드/
  click-away/WA_INACTIVE). **아이콘 비트맵은 래스터화한 dc에 종속 → dc 간 못 넘김**,
  그래서 바가 각 아이콘 HICON을 보관(`TrayIcon.hicon`)하고 플라이아웃이 demoted
  아이콘을 **자기 dc에서 재래스터화**. refresh 타이머의 죽은-owner 스윕이 사라진
  아이콘 제거+비면 자동 닫힘. ② `tray.rs` — 버전인지 Shell_NotifyIcon 콜백을
  **`tray::forward` 자유함수로 추출**(demoted 아이콘 클릭이 스트립 아이콘과 동일 경로),
  NIN_SELECT도 이관. ③ `taskbar.rs` — `tray_promoted`/`tray_overflow_ids`가 visible을
  cap에서 분할, chevron 셀은 승격 아이콘 왼쪽에 위치하고 호버셋/상호배제/close_popups/
  click_away 합류; **버튼 shrink(apply_overflow)는 이제 chevron까지(tray_cluster_left)**.
  ④ clickaway: OVERFLOW_OPEN이 훅 조건 합류. **검증(posted 리그+스크린샷): 9개 주입 →
  스트립 6 + `^` chevron 렌더, chevron 클릭 → 위에 앵커된 플라이아웃이 정확히 demoted 3개
  + "숨긴 아이콘" 헤더 렌더(9−6=3), 아이콘 클릭 → forward 경로 실행+플라이아웃 hide
  (visible True→False).** 미검증(도그푸드): 실입력 WA_INACTIVE 해제·실앱 클릭 왕복
  (posted 입력은 activation 안 바꿈, 합성 아이콘은 실 owner 없음), ESC/우클릭 경로.
  **함정: `WM_MOUSELEAVE`(=0x2A3)는 WindowsAndMessaging이 아니라 UI::Controls 소속 →
  glob 임포트로 안 잡혀 match arm이 catch-all 바인딩이 되어 뒤 arm 전부 unreachable;
  바/flyout처럼 로컬 const 정의로 우회. chevron device x는 formula 말고 스샷 실측(1434).**
- **디자인 방향 확정 + 플로팅 라운드 패널 SHIPPED 0723 (26b1920)** *(유저 스샷 투척
  "아무리 못해도 이것보단 잘 만들어야지" + 궁극 방향 "KDE Plasma/GNOME처럼 완전한 DE —
  코어만 Windows 기본, 나머지 우리껄로 대체")*: 현 바가 엣지투엣지+라벨 = 스톡 Windows
  태스크바랑 실루엣 동일 = slop 바가 거부하는 "아무 스캐폴드에서나 나올 룩". **북극성 =
  완전한 DE**: 유지(Windows 코어 = 커널/DWM 컴포지터/드라이버/창관리 프리미티브), 대체(전부
  우리 것 + 단일 디자인 언어 = 바·패널·런처·알림/QS·데스크탑·세션/잠금·설정·결국 창 장식+테마
  시스템). **디자인 언어 = 플로팅 라운드 패널**(유저 선택, Plasma식이되 우리 고유). ①
  플로팅 슬래브: strut = BAR_HEIGHT + PANEL_MARGIN_BOTTOM 예약 → 창을 PANEL_MARGIN_X만큼
  좌우 인셋 + strut 상단 고정 → 가장자리 갭으로 데스크탑 비침, DWM 라운드 코너로 패널처럼
  읽힘. `panel_rect()`가 인셋 중앙화, run()+reposition()(디스플레이 변경) 공유. ② running
  점: **모든 열린 창이 아이콘 밑 흐린 액센트 점**, 포그라운드는 밝은 pill로 확장(flash=amber)
  — 중앙확장 언더라인(활성 창만 표시, 나머지 실행앱 무표시) 대체. ③ **icon-only 기본**
  (labels off): 도크가 디자인된 표면으로 읽힘(Win10 라벨 스트립 X), 설정서 라벨 토글 복원.
  **검증(스샷): 슬래브가 라운드 코너+가장자리 갭으로 떠 있음, 아이콘이 균일 도크로 렌더+각
  열린 앱 밑 teal 점, 호버/액티브 pill·상태 클러스터 유지.** 함정: explorer 공존 시 우리
  appbar가 그 위에 스택돼 하단 갭 커 보임(스왑하면 사라짐, 버그 아님). 후속: 세컨더리
  모니터 바는 아직 엣지투엣지 — 동일 플로팅 처리 필요.
- **실용성 패스 (0723, 유저 "제발 실용적으로 바로 바꿔도 안 불편하도록")** — 데모 그만,
  스왑-실용이 목표. Duo 하단 태블릿모드는 2차("하던가")로 파킹. 유저 "먼저 코드 갭 더 닫고"
  선택(explorer-kill 리허설 보류). ① **세컨더리 바 플로팅화(18d900c)** — Duo 하단 바가
  edge-to-edge 각진 풀폭이라 프라이머리 플로팅과 불일치였음. `panel_rect` pub(crate)化 +
  secondary new()/reposition()에 strut+인셋+DWMWCP_ROUND. Duo 하단 화면 라이브 스샷 검증.
  ② **시작메뉴 첫-오픈 프리즈 픽스(0b525fa)** — 첫 Win키가 5-6초 "앱 목록 불러오는 중"+
  블랭크 타일. 근본원인=콜드 shell:AppsFolder 열거(200앱 UWP 매니페스트)가 단일 워커 큐
  머리라 뒤 타일-아이콘 잡까지 블록. 데이터경로는 정상(진단으로 enum 200·epoch매치·on_replies
  통합 확증). 픽스=StartMenu::new()서 워커 스폰 직후 Job::Apps+핀 Job::Icon 프리워밍(부팅
  유휴에 열거 끝냄), show() `!self.loading` 가드. Stale 재오픈은 캐시 즉시렌더+백그라운드갱신
  (loading텍스트는 apps.is_empty()때만). **검증: 7초 유휴 후 오픈+1초→완전로드.** ③ 바/알림센터
  감사=양호(기능버그 없음). **결론: 시작메뉴 프리즈가 유일 진짜 기능갭, 해결. 남은 게이트=
  실앱 트레이 라우팅(--tray-claim 배선됨, 스왑/explorer-kill로만 증명 — M7).**
- **설정 앱 = 시스템 컨트롤 센터 SHIPPED 0723 (`d1500a7`)** *(유저 "Windows에서 가져올
  수 있는 설정 다 가져오고, 광고판 된 시작 앱·파편화의 정수 제어판을 부분적으로나마 대체
  하게 대폭 업그레이드")*. 설정 앱이 glide 자체 토글 5개뿐이던 걸 실제 시스템 표면으로
  키움. 스왑해도 Windows 설정 접근을 잃지 않게. 새 `winsettings.rs`가 Windows 쪽 담당,
  전부 HKCU(권한상승 없음): ① **개인화** — 다크/투명/제목강조를 Themes\Personalize+DWM
  DWORD로 읽고 쓰고 WM_SETTINGCHANGE(ImmersiveColorSet) 브로드캐스트로 실행 중 앱까지
  즉시 재테마(stock 설정앱과 동일 동작). ② **시작 프로그램** — HKCU\Run 값 + 시작폴더
  바로가기, 각각 Task Manager의 StartupApproved 비트(byte0 짝수=사용)로 열거·토글 =
  작업관리자 시작 탭을 glide 안에. ③ **시스템 정보** — OS 빌드/CPU/RAM/장치·사용자를
  레지스트리+GlobalMemoryStatusEx로 = 시스템 애플릿 통합. 그리고 **Windows 설정** 카테고리
  = glide가 아직 안 가진 stock 패널 10개(네트워크·BT·소리·디스플레이·전원·앱·업데이트·
  시간·계정·저장소)로의 딥링크 카드(ms-settings: URI, ShellExecute) → 파편화된 제어판/
  ms-settings 미로를 한 문으로. 카테고리 5→6, 마우스휠 스크롤 + 제목 고정 클립 추가.
  **검증: --settings 띄워 6개 카테고리 전부 + 휠 스크롤을 자체 창 PostMessage로 구동,
  각각 스크린샷. 개인화=실 레지스트리 상태 반영, 시작=실제 자동실행 6개+정확한 승인비트,
  시스템정보=실측(Win 25H2 26200.8875·155H·15.4GB·UX8406MA), 스크롤=제목 고정 클램프.**
- **광고판 축출 + glide 설정 4종 녹임 SHIPPED 0723 (`35a2988`·`6e867a0`·`367975f`·
  `9a32f44` + 밀도 라운드)** *(유저 "설정/도구의 ms-settings를 밖으로 빼고 glide 설정을
  자연스럽게 녹여 — 저 Win11 설정앱은 광고판이나 다름없다" + "강조색·시계·바 밀도·알림·
  자동시작 그냥 다 알아서 녹여")*. 직전 컨트롤센터가 stock 패널 10개로의 ms-settings
  딥링크 카테고리를 뒀는데, 그게 M365 업셀·"권장 설정" 광고판으로 되돌아가는 문이라
  **전량 제거**. 대신 카테고리 = 개인화·**작업 표시줄**·시작 프로그램·Windows 조정·단축키·
  **고급·도구**·시스템 정보 7개로 재편, 딥링크는 고급·도구의 클래식 .cpl/.msc/regedit
  유틸(광고 없는 실속 패널)만 남김. glide 자체 설정 4종을 앱에 녹임: ① **강조색** —
  개인화 최상단 8-스와치 스트립(`theme::ACCENT_PRESETS`), 클릭 = 런타임 원자
  `ACCENT_RGB` 재지정(`set_accent`) + config + 바로 `WM_SETTINGS_CHANGED` → **재시작
  없이** 나브 인디케이터·스위치·바 강조 전부 즉시 재색(라이브 검증: 스와치 클릭에
  인디케이터/스위치 재색). ② **시계 형식·날짜** — 24시간제/초 표시/날짜 표시 토글,
  `clock_time_string(cfg,now)` 4-way 분기 + 날짜줄 게이팅. ③ **알림·자동시작** — 토스트
  on/off(`toasts::ENABLED` 원자, `on_arrivals` 게이트) + 로그온 시 glide 자동시작
  (HKCU\Run, `winsettings::glide_autostart`). ④ **바 밀도** — 작업 표시줄 최상단 3-세그
  (컴팩트 34/보통 40/크게 48px) 컨트롤, `theme::BAR_H` 원자 + `bar_height()`; **strut·전
  오프셋이 여기서 굳으므로 재시작 후 적용**(라이브 re-strut 회피, 부제에 명시). 시계
  세로 오프셋을 `bar_height()`에서 재유도(`time_top=(bar_h-33)/2`, 보통=40에서 3.5 ≈ 구
  하드코드 4.0로 동일 렌더). `BAR_HEIGHT` const → `bar_height()` 런타임으로 18개 사이트
  치환(taskbar+secondary). **검증(스샷): --settings 작업 표시줄 카테고리를 자체 창
  PostMessage로 열어 밀도 세그(보통=teal) + 시계/라벨 토글 행 렌더 확인. 강조색 재색은
  이전 라운드 스와치 클릭으로 라이브 검증. 밀도 컴팩트/크게 시계 오프셋은 산술 유도만
  (풀 appbar 기동은 유저 작업 창을 밀어내 미실행) — 보통은 구 값과 동일.**
- **설정 인라인 컨트롤 + 자체 작업 관리자 SHIPPED 0724 (`b237c93`)** *(유저 "일단
  구현부터 다 하고 말해" → "아래쪽 윈도우로 떠넘기는 설정들도 마저 다 구현" → "일부
  기능은 자체 제작 작업 관리자로 빼던가" → "프로세서 해커 2 정도 기능" → "CPU랑 메모리
  표시 고쳐" → "이정도면 나쁘지 않네 - 인정")*. ① **설정 인라인화** — 클래식 패널을
  `[열기]` 런치 카드 대신 glide 렌더 컨트롤로: 소리(볼륨/음소거/출력장치, Core Audio
  `IAudioEndpointVolume`+`IMMDeviceEnumerator`), 전원(배터리/전원 계획,
  `GetSystemPowerStatus`+powrprof, 권한상승 없이 계획 전환), 네트워크(Wi-Fi/BT/비행기,
  wlanapi + WinRT `Radios`는 **STA 데드락 회피 위해 단명 MTA 워커**), 앱(설치 목록 3-하이브
  Uninstall 열거 + 제거), 날짜·시간(타임존, `SetDynamicTimeZoneInformation` + SE_TIME_ZONE
  권한). 진짜 관리 콘솔(장치관리자·디스크·regedit)은 런처 유지 — Win11 설정·KDE/GNOME도
  런치하고 재구현은 slop. ② **자체 작업 관리자**(`--taskmgr`, 설정 항목서도 스폰) — Win11
  작업 관리자 골격: **제품(FileDescription) 단위 앱 그룹**(개수+접기+합계, ppid-subtree
  아님 → explorer가 전 유저 프로세스 삼키던 버그 제거), **앱별 아이콘**(`icons::exe_icon`
  재사용 path별 캐시), 라이브 검색, **값 비례 폭 CPU/메모리 히트바**, 소유자 컬럼, 2-step
  끝내기/트리 종료/중단/재개/우선순위(그룹은 전 멤버 적용). 서비스 탭=SCM 시작/중지.
  백엔드 새 모듈 procs/services/apps/datetime/power. **검증(스샷 5회 반복): --taskmgr 자체
  창 화면 BitBlt로 그룹·아이콘·히트바·컬럼 정렬 라이브 확인, 탐색기 2.8GB→133MB 정상화
  확증.** 아이콘 인프라는 taskbar/secondary와 동일 idiom(DrawBitmap+`Option<ID2D1Bitmap1>`
  캐시). 남은 nice-to-have=헤더 집계 총계(22% CPU식).
- **M7 — 스왑 + 도그푸드** (1세션 + 2h)
  롤백 리허설 → HKCU Shell= 스왑 → 체크리스트: **한글 IME**, GLM-Proxy 태스크,
  오디오/볼륨 키, 150% DPI, Duo 패널 탈착, 절전/복귀, 게임 풀스크린, UAC, 파일
  대화상자, 스크린샷 리그, **토스트 수신**, **실앱 트레이 재등록**. 2시간 실사용 도그푸드 후 유지/롤백 판정.

## 10. 유저 결정 — 확정 기록 (2026-07-21)

전 항목 유저 확정. 총론: **"v1 스코프를 광범위하게 안 잡을 거면 스왑하면 안 됨"**
→ 스왑 게이트 = M1~M6 전부.

1. **바 위치**: 하단 확정.
2. **데스크톱 아이콘**: 스왑 전 필수 (M3). 광범위-v1 총론에 따름.
3. **레스큐 계정**: 생성 동의 (M6에서 생성, 비밀번호는 유저가 직접 설정).
4. **Duo 모니터**: 양 패널 스왑 전 필수 (M5). M1은 주 패널로 시작하되 per-monitor 구조.
5. **Win키**: bare-Win → glint 확정 (Win+E/D/화살표 조합키 유지).
   **→ 0722 유저 지시로 번복**: bare-Win → 시작 메뉴, Win+S → glint 검색
   (설정 winkey_start=0으로 구 동작 복원 가능). §9 설정 앱 블록 참조.
6. **토스트**: 손실 수용 안 함 — `UserNotificationListener` 자체 토스트가 스왑 전 필수
   (M4). 스파이크 실패 시 스왑 보류하고 재논의.
7. **UI/UX 철학**: "쉽고, 빠르고, 아름답게" + 웹뷰 전면 금지. 설계 반영: 렌더링 스택
   GDI → Direct2D/DirectWrite/DirectComposition 상향(§5), RAM 예산 <20 → <40MB 정직
   조정(§4), M1에 미감 게이트 추가(§9), 디자인 언어 3앱 공통 명문화(§5).

## 부록 A — tray 프로토콜 메시지 요약

| 채널 | 값 | 의미 |
|---|---|---|
| WM_COPYDATA dwData | 0 | appbar 릴레이 (SHAppBarMessage) |
| | 1 | Shell_NotifyIcon (NIM_ADD/MODIFY/DELETE/SETVERSION + NOTIFYICONDATAW) |
| | 2, 3 | 아이콘 rect/식별 질의 (Shell_NotifyIconGetRect) |
| 브로드캐스트 | "TaskbarCreated" | 재등록 유도 (기동 시 1회) |
| 셸 훅 | "SHELLHOOK" | HSHELL_WINDOWCREATED/DESTROYED/ACTIVATED/FLASH… |
| 콜백 | NOTIFYICONDATA.uCallbackMessage | 마우스 이벤트를 앱 hwnd로 PostMessage |

## 부록 B — 참고 구현

- **Cairo Shell** (C#, 활발) — Win11에서 풀 셸 교체 실사용 사례. alongside 모드의
  explorer 바 숨김 이슈, 22H2+ 네트워크/전원 플라이아웃 연동 사례 참고:
  <https://github.com/cairoshell/cairoshell>
- **ManagedShell** (Cairo의 하부 라이브러리) — Tasks(작업표시줄)/Tray(알림 영역)/AppBar
  모듈 구조가 본 설계 §5-6의 원형. "as replacement + alongside" 양모드 지원 증명:
  <https://github.com/cairoshell/ManagedShell>
- **Open-Shell** (시작 메뉴만, Win11 호환 유지) — 부분 대체 접근의 대표:
  <https://github.com/Open-Shell-Windows-11/>
- RetroBar, ExplorerPatcher — 바/트레이 개별 대체 사례 (프로토콜 검증 참고용)
