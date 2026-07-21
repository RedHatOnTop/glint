# glide-shell — 풀 셸 교체 설계

작성 2026-07-21. 라운드 12(`8c182f5`, single-instance IPC + 우클릭 verb) 직후 기준.
구현 전 설계 문서. 여기 나온 수치는 전부 이 머신(Zenbook Duo, 155H, 16GB)에서 실측.

## 0. 요약

explorer.exe를 이 계정 한정(HKCU)으로 `glide-shell.exe`로 교체한다.
새 crate `crates/glide-shell` = raw win32 상주 셸(taskbar + tray + desktop + autostart 실행기),
기존 glint(런처)와 glide(파일 관리자)가 각각 시작 메뉴와 탐색기 역할을 맡는다.
**모든 컴포넌트는 explorer를 켜둔 채(alongside) 개발·검증하고, Shell= 스왑은 마지막
마일스톤(M5)에서만 한다.** 복구 사다리 6겹(§7)이 스왑 전에 먼저 갖춰져야 한다.

핵심 결정 3개:

| 결정 | 선택 | 근거 |
|---|---|---|
| 상주 셸 UI 스택 | raw win32 + GDI (egui 아님) | release egui 앱 실측 WS 127MB(§4). 24/7 상주에 100MB는 이 박스에서 불가. 목표 <20MB. 게다가 tray는 `Shell_TrayWnd`라는 **정확한 클래스명**의 창이 필요한데 winit은 클래스명을 못 정함 |
| 스왑 범위 | HKCU `Winlogon\Shell`만 | 타 계정·세이프모드 복구 경로 보존. HKLM은 절대 안 건드림 |
| 알림(토스트) | v1 포기, 명시적 손실 처리 | Action Center는 explorer 생태계 소속. NIF_INFO 벌룬만 자체 렌더. 카톡·디스코드 상주 실측됨 → WNS 토스트는 v2 (`UserNotificationListener`) |

## 1. 목표와 원칙

- **목표**: 부팅하면 glide-shell 바 + glint 런처 + glide 탐색기만으로 하루 종일 생활 가능.
- **원칙 1 — 되돌리기 우선**: 스왑보다 복구를 먼저 만든다. 복구 리허설 없이는 Shell= 안 바꾼다.
- **원칙 2 — alongside 개발**: M1~M4는 explorer 살아있는 상태에서 개발/검증
  (Cairo/ManagedShell이 이 모드가 성립함을 증명함 — §부록 B).
- **원칙 3 — 상주는 가볍게**: 상주 프로세스는 raw win32. egui는 온디맨드 창(glint 오버레이,
  glide)에만.
- **원칙 4 — 보이는 디자인**: 기본 Win11을 흉내내지 않는다. glide teal(`theme.rs` ACCENT)
  기반의 독자 룩. "stock 같으면 실패" 기준은 Zetile과 동일.

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
- 한글 IME — `MsCtfMonitor` 스케줄드 태스크가 로그온 시 별도 기동. **M5 체크리스트 1순위 검증 항목** (이 유저에게 치명 요소)
- GLM-Proxy — 스케줄드 태스크라 셸 무관
- 공통 파일 대화상자(열기/저장) — comdlg32 인프로세스
- ms-settings:, UWP 앱 활성화 — AppX 서비스 소관
- 스냅 기능 자체(Win+화살표) — OS 소관. 스냅 레이아웃 **호버 UI**는 미확인(§8)

### 포기한다 (v1 명시적 손실)

| 손실 | 영향 | 미티게이션 |
|---|---|---|
| 토스트 알림 / Action Center (Win+N) | 앱 알림 팝업 안 뜸. **실측: 이 박스는 KakaoTalk·Discord 상주(Run 키 확인) — 메신저 알림 손실은 실사용 통증. v3 아닌 v2로 상향 검토** | tray 벌룬(NIF_INFO)은 자체 렌더(M2). 트레이 아이콘 FLASH/변경은 보임. WNS 토스트는 v2에 `UserNotificationListener` 검토 |
| 퀵 설정 (Win+A) | 플라이아웃 없음 | 상태 글리프 클릭 → ms-settings 딥링크 |
| 볼륨/밝기 OSD 오버레이 | 키는 동작, 화면 표시만 없음 | v2에서 자체 OSD (IAudioEndpointVolume 콜백) |
| 위젯, Copilot, 검색 하이라이트 | 없음 | 유저가 이미 디블로트로 죽여둠 — 손실 아님 |
| 데스크톱 아이콘 | v1엔 없음 | §10 결정사항 2. v2에 glide grid 재사용안 |

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
- 목표 예산: glide-shell **<20MB**, glint 상주(egui, 어쩔 수 없음) ~100MB,
  glide는 온디맨드. 합계 ≈ 120~150MB로 explorer 생태계 712MB 대비 **~550MB 순절감**.
  16GB(실측 free 3.2GB) 박스에서 이게 이 프로젝트의 실질 보상.

### 레지스트리 현황

- `HKLM\...\Winlogon\Shell` = `explorer.exe` (기본값, 건드리지 않음)
- `HKCU\...\Winlogon\Shell` = **없음** (우리가 쓸 슬롯)
- `HKLM\...\Winlogon\AutoRestartShell` = **1** → winlogon이 등록된 셸이 죽으면
  자동 재시작해 줌. 커스텀 셸에도 적용 — 복구 사다리 2번째 겹이 공짜.

## 5. 아키텍처

### 프로세스 모델

```
winlogon ── Shell= ──► glide-shell.exe  (raw win32, <20MB, 단일 프로세스)
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
  src/taskbar.rs     — 바 창, appbar 예약, 창 목록, 렌더(GDI)
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

### 렌더링

GDI 더블버퍼(CreateCompatibleDC + BitBlt). 다크 솔리드 배경(#17181C, glide SURFACE),
Segoe Fluent Icons 글리프, 활성 창 teal 언더라인, DirectWrite는 v2(글자 품질 불만 시).
DPI: Per-Monitor v2 매니페스트 + WM_DPICHANGED에서 메트릭 재계산 — 이 머신 150% 스케일이
기본 테스트 케이스.

## 6. 컴포넌트 각론

### 6.1 taskbar (M1)

- **창**: `WS_POPUP`, 모니터 하단 풀폭 × 40px(150% DPI에서 60px 물리). 클래스명 자유
  (tray와 분리 — §6.2 참고).
- **화면 예약**: `SHAppBarMessage(ABM_NEW/ABM_SETPOS)` — 최대화 창이 바를 안 덮게.
  모니터별 1개, v1은 주 모니터만(이 머신 Duo 하단 패널 = 모니터 2 — §10 결정사항 4).
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
M5 스왑의 예행연습이 된다.

### 6.3 desktop (M3)

- 최하단 풀스크린 창: `WS_POPUP` + `HWND_BOTTOM` 고정(WM_WINDOWPOSCHANGING에서
  Z 상승 거부), `WS_EX_NOACTIVATE`.
- 벽지: 현재 시스템 벽지 경로(`SystemParametersInfoW(SPI_GETDESKWALLPAPER)`) 렌더.
  슬라이드쇼는 포기(v1).
- 우클릭 메뉴: glide 열기 / 새로고침 / 디스플레이 설정 / 개인 설정(ms-settings 딥링크).
- 더블클릭 빈 공간 → glide로 Desktop 폴더.
- `SetShellWindow(hwnd)` 호출(문서화 안 된 user32 export, 모든 셸 대체가 사용) —
  `GetShellWindow()` 의존 코드와 Z-순서 시맨틱 보정.
- 아이콘 그리드는 v2 (§10 결정사항 2).

### 6.4 start = glint 통합 (M3)

- glide-shell이 glint를 스폰·감시.
- `WH_KEYBOARD_LL` 훅: bare-Win 눌렀다 뗌(다른 키 조합 없이) → glint 토글.
  훅 콜백은 <1ms 유지(무거우면 OS가 훅 강제 해제).
- glint 쪽은 기존 Alt+Space 경로 재사용 — 셸 쪽에서 named pipe로 "토글" 신호만 보냄
  (glide ipc.rs 패턴 복사).

### 6.5 autostart 실행기 (M3, 잊으면 큰일 나는 것)

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

### 6.6 상태 영역 (M2)

시계(1분 타이머) + 글리프 3종: 배터리(`GetSystemPowerStatus`),
네트워크(`INetworkListManager` COM), 볼륨(`IAudioEndpointVolume` + 변경 콜백).
클릭 → 각각 ms-settings 딥링크(glint 테이블 재사용). 한/영 표시(현재 입력 로케일,
`GetKeyboardLayout`) 포함 — IME 상태가 안 보이면 이 유저 워크플로에서 불편.

### 6.7 알림

v1 = tray 벌룬만(§6.2-5). WNS 토스트는 **v2**에서 `UserNotificationListener`(WinRT)
검토 — 이 박스에 KakaoTalk·Discord 상주가 실측(§6.5)되어 v3→v2로 상향. 권한 동의
UI가 필요하고 접근 거부 사례가 있는 API라 실패 가능성은 남음.
**v1 손실임을 유저가 인지한 상태로 스왑하는 게 조건**(§10 결정사항 6).

## 7. 안전망 — 복구 사다리 (M4, 스왑 전 필수)

겹겹이. 위에서부터 자동:

| # | 겹 | 동작 | 조건 |
|---|---|---|---|
| 1 | 인프로세스 watchdog | taskbar/tray/desktop 창 죽으면 재생성, glint 죽으면 재스폰 | 자동 |
| 2 | winlogon `AutoRestartShell=1` | glide-shell 프로세스 사망 → winlogon이 재기동 | 자동 (실측 확인됨) |
| 3 | **크래시 카운터 자폭** | 기동 시 마커 파일에 크래시 횟수 기록, **10분 내 3회 크래시 → 스스로 HKCU Shell= 삭제 후 explorer.exe 스폰** — 부팅 루프 원천 차단 | 자동 |
| 4 | 레스큐 핫키 | 셸 내 `Ctrl+Alt+Shift+E` → explorer.exe 스폰(풀 explorer 세션 복귀), `Ctrl+Alt+Shift+R` → HKCU Shell= 삭제 + 재로그온 안내 | 수동 1키 |
| 5 | Ctrl+Alt+Del | winlogon 보안 화면은 **셸과 무관하게 항상 뜸** → 작업 관리자 → 새 작업 → `explorer.exe` 또는 `reg delete "HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon" /v Shell /f` | 수동, 최후 |
| 6 | 레스큐 계정 | 로컬 admin 계정 1개 사전 생성 — HKCU 스코프라 그 계정은 무조건 stock explorer로 부팅 | 사전 준비 |

**롤백 리허설(M5 첫 단계)**: 스왑 → 로그온 확인 → 즉시 사다리 4번으로 복귀 →
재스왑. 리허설이 통과해야 도그푸드 시작. 리허설 전 `RESTORE-SHELL.ps1`(사다리 5의
reg delete + explorer 스폰)을 바탕화면과 `C:\Users\jin14\system-optimize-backup\`에
복사해 둔다(디블로트 RESTORE.ps1과 같은 장소 = 유저가 아는 장소).

등록 UI는 glide gear 팝업이 아니라 **glide-shell 자체의 `--register`/`--unregister`
CLI + 확인 프롬프트**로 한다(파일 관리자에 셸 스왑 버튼은 오클릭 위험).

## 8. 정직한 리스크 목록

| 항목 | 등급 | 내용 |
|---|---|---|
| 한글 IME | **검증 필수** | 메커니즘은 이 박스에 존재 확인(0721 실측: `MsCtfMonitor` 태스크 등록 + ctfmon/TextInputHost 실행 중 — explorer가 아니라 태스크 스케줄러 소관). 다만 **explorer 부재 세션에서의 동작은 미실측** → M2/M3의 explorer-kill 게이트에서 선행 확인, M5 체크리스트 1번 유지. 실패 시 스왑 중단 사유 |
| 메신저 알림 (카톡/디스코드) | **높음** | 토스트 손실(§3 포기 표). 트레이 플래시·벌룬만 남음. 유저가 이 손실을 수용해야 스왑 가능 — §10 결정사항 6 |
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

- **M1 — taskbar alongside** (1~2세션)
  scaffold + 바 창 + appbar 예약 + 창 목록/활성/클릭 + 시계. explorer 바는 자동 숨김
  설정으로 공존. 게이트: 하루 도그푸드 — 창 전환을 우리 바로만.
- **M2 — tray + 상태** (2~3세션, 최고 난이도)
  Shell_TrayWnd 프로토콜 전체 + 벌룬 + 상태 글리프. 게이트: explorer 죽인 세션에서
  TaskbarCreated 브로드캐스트 → 기존 앱 아이콘 등장, 클릭 메뉴 정상, 벌룬 렌더.
  (이 게이트가 M5 예행연습.)
- **M3 — desktop + autostart + 키** (1~2세션)
  배경 창 + 우클릭, Run/Startup 실행기, Win키→glint, Win+E→glide.
  게이트: explorer 죽인 세션에서 데스크톱/키/자동시작 전부 동작 —
  특히 **Everything 자동 기동 + glide Ctrl+P 검색 성공**(§6.5), 한글 입력 정상.
- **M4 — 안전망** (1세션)
  watchdog + 크래시 자폭 + 레스큐 핫키 + RESTORE-SHELL.ps1 + `--register` CLI +
  레스큐 계정 생성(유저 동의 필요 — §10). 게이트: 셸 프로세스 kill → winlogon 재기동
  확인, 3-크래시 자폭 시뮬레이션 통과.
- **M5 — 스왑 + 도그푸드** (1세션 + 2h)
  롤백 리허설 → HKCU Shell= 스왑 → 체크리스트: **한글 IME**, GLM-Proxy 태스크,
  오디오/볼륨 키, 150% DPI, Duo 패널 탈착, 절전/복귀, 게임 풀스크린, UAC, 파일
  대화상자, 스크린샷 리그. 2시간 실사용 도그푸드 후 유지/롤백 판정.

M1 착수 전 이 문서를 유저가 리뷰하고 §10에 답하는 게 선행 조건.

## 10. 유저 결정 대기 목록

1. **바 위치/스타일**: 하단 바(Win11 근접, 추천) vs 상단 바 vs 독(Cairo류)?
2. **데스크톱 아이콘**: v1 생략(추천 — 빈 데스크톱 + 더블클릭→glide) vs v2까지 대기 vs 필수?
3. **레스큐 계정**: 로컬 admin "rescue" 계정 생성 동의? (사다리 6겹째, 강력 추천)
4. **Duo 모니터**: 바를 어느 패널에? 주(상단) 패널만 v1? 양쪽?
5. **Win키 바인딩**: bare-Win → glint 괜찮은지 (기존 Win 단축키 Win+E/D/화살표는 유지됨),
   아니면 Win키는 건드리지 않고 Alt+Space 유지?
6. **메신저 알림 손실 수용**: KakaoTalk·Discord 토스트가 v1에서 안 뜸(트레이 플래시/벌룬만).
   수용하고 스왑 vs `UserNotificationListener` 자체 토스트(v2)가 될 때까지 스왑 보류?

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
