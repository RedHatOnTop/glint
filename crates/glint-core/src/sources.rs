// Search sources: installed apps (Start Menu .lnk), files (Everything IPC), web.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    App,
    Setting,
    File,
    Web,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    pub title: String,
    /// Full path for apps/files; URL for web; ms-settings: URI for settings.
    pub target: String,
    /// Dimmed second line (parent dir for files, "" for apps).
    pub subtitle: String,
}

// ---------------------------------------------------------------- apps

pub type AppIndex = Arc<Mutex<Vec<Item>>>;

/// Scan Start Menu .lnk files (all-users + per-user) into an in-memory index.
pub fn build_app_index() -> AppIndex {
    let index: AppIndex = Arc::new(Mutex::new(Vec::new()));
    let out = index.clone();
    std::thread::spawn(move || {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Ok(pd) = std::env::var("ProgramData") {
            roots.push(PathBuf::from(pd).join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        if let Ok(ad) = std::env::var("APPDATA") {
            roots.push(PathBuf::from(ad).join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        let mut found: Vec<Item> = Vec::new();
        for root in roots {
            scan_lnk(&root, &mut found, 0);
        }
        // De-dup by title (per-user shadows all-users)
        found.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
        found.dedup_by(|a, b| a.title.eq_ignore_ascii_case(&b.title));
        *out.lock().unwrap() = found;
    });
    index
}

fn scan_lnk(dir: &PathBuf, out: &mut Vec<Item>, depth: u32) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            scan_lnk(&p, out, depth + 1);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
            let title = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
            // Skip uninstaller/website noise
            let tl = title.to_lowercase();
            if tl.contains("uninstall") || tl.contains("제거") || tl.starts_with("website") {
                continue;
            }
            out.push(Item {
                kind: Kind::App,
                title,
                target: p.to_string_lossy().to_string(),
                subtitle: String::new(),
            });
        }
    }
}

// ---------------------------------------------------------------- files (Everything)

pub struct FileSearch {
    tx: mpsc::Sender<(u64, String)>,
    pub results: Arc<Mutex<(u64, Vec<Item>)>>,
    seq: AtomicU64,
}

impl FileSearch {
    /// Worker thread owns the Everything SDK global lock; latest query wins.
    pub fn spawn(ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel::<(u64, String)>();
        let results: Arc<Mutex<(u64, Vec<Item>)>> = Arc::new(Mutex::new((0, Vec::new())));
        let out = results.clone();
        std::thread::spawn(move || {
            while let Ok(mut msg) = rx.recv() {
                // Drain to the newest pending query.
                while let Ok(newer) = rx.try_recv() {
                    msg = newer;
                }
                let (seq, query) = msg;
                if query.trim().is_empty() {
                    *out.lock().unwrap() = (seq, Vec::new());
                    ctx.request_repaint();
                    continue;
                }
                let items = everything_query(&query).unwrap_or_default();
                let mut slot = out.lock().unwrap();
                if seq >= slot.0 {
                    *slot = (seq, items);
                }
                drop(slot);
                ctx.request_repaint();
            }
        });
        Self { tx, results, seq: AtomicU64::new(0) }
    }

    pub fn query(&self, q: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send((seq, q.to_string()));
    }
}

fn everything_query(q: &str) -> anyhow::Result<Vec<Item>> {
    use everything_sdk::{global, RequestFlags};
    let mut everything = global().try_lock().map_err(|_| anyhow::anyhow!("sdk busy"))?;
    let mut searcher = everything.searcher();
    searcher.set_search(q);
    searcher
        .set_request_flags(RequestFlags::EVERYTHING_REQUEST_FILE_NAME | RequestFlags::EVERYTHING_REQUEST_PATH)
        .set_max(24);
    let results = searcher.query();
    let mut items = Vec::new();
    for item in results.iter() {
        let full = item.filepath().unwrap_or_default();
        let p = PathBuf::from(&full);
        items.push(Item {
            kind: Kind::File,
            title: p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| full.to_string_lossy().to_string()),
            subtitle: p
                .parent()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            target: full.to_string_lossy().to_string(),
        });
    }
    Ok(items)
}

// ---------------------------------------------------------------- web

pub fn web_item(q: &str) -> Item {
    Item {
        kind: Kind::Web,
        title: format!("\u{201c}{q}\u{201d} 웹 검색"),
        subtitle: "기본 브라우저로 열기".into(),
        target: format!("https://www.google.com/search?q={}", urlencode(q)),
    }
}

// ---------------------------------------------------------------- settings

/// Curated Windows 11 settings deep links (title, ms-settings URI, EN/KR aliases).
const SETTINGS: &[(&str, &str, &str)] = &[
    ("디스플레이", "ms-settings:display", "display monitor resolution 해상도 화면"),
    ("야간 모드", "ms-settings:nightlight", "night light blue 블루라이트"),
    ("소리", "ms-settings:sound", "sound audio volume 볼륨 오디오 스피커"),
    ("블루투스 및 장치", "ms-settings:bluetooth", "bluetooth pair 페어링"),
    ("Wi-Fi", "ms-settings:network-wifi", "wifi wireless 와이파이 무선"),
    ("이더넷", "ms-settings:network-ethernet", "ethernet lan 유선"),
    ("VPN", "ms-settings:network-vpn", "vpn"),
    ("프록시", "ms-settings:network-proxy", "proxy 프록시"),
    ("비행기 모드", "ms-settings:network-airplanemode", "airplane flight 에어플레인"),
    ("알림", "ms-settings:notifications", "notifications 노티"),
    ("집중 지원", "ms-settings:quiethours", "focus assist dnd 방해금지"),
    ("전원 및 배터리", "ms-settings:powersleep", "power battery sleep 절전 배터리"),
    ("저장소", "ms-settings:storagesense", "storage disk 디스크 용량"),
    ("멀티태스킹", "ms-settings:multitasking", "multitasking snap 스냅"),
    ("설치된 앱", "ms-settings:appsfeatures", "apps programs uninstall 앱 제거 프로그램"),
    ("기본 앱", "ms-settings:defaultapps", "default apps browser 기본 브라우저"),
    ("시작 프로그램", "ms-settings:startupapps", "startup autostart 자동 시작"),
    ("선택적 기능", "ms-settings:optionalfeatures", "optional features 기능"),
    ("개인 설정", "ms-settings:personalization", "personalization 테마 꾸미기"),
    ("배경", "ms-settings:personalization-background", "background wallpaper 배경화면 월페이퍼"),
    ("색", "ms-settings:personalization-colors", "colors accent dark mode 다크 모드 악센트"),
    ("잠금 화면", "ms-settings:lockscreen", "lock screen 락스크린"),
    ("테마", "ms-settings:themes", "themes 테마"),
    ("작업 표시줄", "ms-settings:taskbar", "taskbar 태스크바"),
    ("글꼴", "ms-settings:fonts", "fonts 폰트"),
    ("날짜 및 시간", "ms-settings:dateandtime", "date time timezone 시계 시간대"),
    ("언어 및 지역", "ms-settings:regionlanguage", "language region ime 언어 지역"),
    ("입력", "ms-settings:typing", "typing keyboard 키보드 입력기"),
    ("마우스", "ms-settings:mousetouchpad", "mouse 마우스 포인터"),
    ("터치패드", "ms-settings:devices-touchpad", "touchpad 터치패드"),
    ("프린터 및 스캐너", "ms-settings:printers", "printers scanner 프린터 스캐너 인쇄"),
    ("연결된 장치", "ms-settings:connecteddevices", "devices usb 장치"),
    ("게임 바", "ms-settings:gaming-gamebar", "game bar gaming 게임"),
    ("접근성", "ms-settings:easeofaccess-display", "accessibility ease of access 돋보기"),
    ("개인 정보", "ms-settings:privacy", "privacy 프라이버시 권한"),
    ("카메라 권한", "ms-settings:privacy-webcam", "camera webcam 카메라 웹캠"),
    ("마이크 권한", "ms-settings:privacy-microphone", "microphone mic 마이크"),
    ("Windows 업데이트", "ms-settings:windowsupdate", "windows update 업데이트"),
    ("복구", "ms-settings:recovery", "recovery reset 초기화 복원"),
    ("정보", "ms-settings:about", "about version specs 버전 사양 시스템"),
    ("계정", "ms-settings:yourinfo", "account 계정"),
    ("로그인 옵션", "ms-settings:signinoptions", "sign in pin hello 지문 얼굴 핀"),
    ("개발자 옵션", "ms-settings:developers", "developer mode 개발자 모드"),
    ("클립보드", "ms-settings:clipboard", "clipboard history 클립보드 기록"),
    ("Windows 보안", "windowsdefender://", "security defender antivirus 백신 보안"),
];

/// Score settings against title AND aliases; best match wins.
pub fn search_settings(q: &str) -> Vec<(i32, Item)> {
    SETTINGS
        .iter()
        .filter_map(|(title, uri, kw)| {
            let s = score(q, title).max(score(q, kw));
            s.map(|s| {
                (
                    s,
                    Item {
                        kind: Kind::Setting,
                        title: (*title).into(),
                        target: (*uri).into(),
                        subtitle: "시스템 설정".into(),
                    },
                )
            })
        })
        .collect()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------- scoring

/// Simple launcher-style scorer: prefix > word-boundary > contains > subsequence.
pub fn score(query: &str, title: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q = query.to_lowercase();
    let t = title.to_lowercase();
    if t == q {
        return Some(2000);
    }
    if t.starts_with(&q) {
        return Some(1000 - title.len() as i32);
    }
    if t.split([' ', '-', '_', '.']).any(|w| w.starts_with(&q)) {
        return Some(700 - title.len() as i32);
    }
    if t.contains(&q) {
        return Some(400 - title.len() as i32);
    }
    // subsequence
    let mut it = t.chars();
    if q.chars().all(|c| it.any(|tc| tc == c)) {
        return Some(50 - title.len() as i32);
    }
    None
}

/// Launch an item. Returns after spawning; never blocks the UI.
pub fn launch(item: &Item) {
    let target = item.target.clone();
    std::thread::spawn(move || {
        let _ = open::that(target);
    });
}
