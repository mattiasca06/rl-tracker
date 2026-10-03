#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use egui_plot::{Line, Plot, PlotBounds, PlotPoints};
use rlstatsapi::{ClientOptions, RocketLeagueStatsClient, StatsEvent};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc as tokio_mpsc;

/// Debug mode. When false (the default) the app writes no debug/junk files
/// next to the exe. Set to true to dump diagnostics like `xhr_urls.json`.
const DEBUG: bool = false;

/// Write a diagnostics file, but only when DEBUG is on.
fn debug_write(name: &str, contents: &str) {
    if DEBUG {
        let _ = std::fs::write(name, contents);
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PlayerInfo {
    name: String,
    primary_id: String,
}

#[derive(Clone, Debug)]
struct MmrPoint {
    pub timestamp: i64,
    pub mmr: i64,
}

/// (primary_id, playlist)
type Key = (String, u32);

struct FetchReq {
    key: Key,
    url: String,
}

/// UI -> worker.
enum WorkerCmd {
    Fetch(FetchReq),
    /// The playlist on screen changed; fetch that one first.
    SetPlaylist(u32),
    /// Manual mode was toggled; just wake the worker so it re-checks the flag.
    Wake,
}

enum WorkerMsg {
    Status(String),
    DisplayName(Key, String),
    Started(Key),
    Finished(Key, Result<Vec<MmrPoint>, String>),
}

enum Entry {
    Queued,
    Loading,
    Done(Vec<MmrPoint>),
    Failed(String),
}

struct TrackerApp {
    players: Vec<PlayerInfo>,
    rx: std_mpsc::Receiver<Vec<PlayerInfo>>,
    status: String,

    playlist: u32,
    full_scale: bool,

    entries: HashMap<Key, Entry>,
    req_tx: tokio_mpsc::UnboundedSender<WorkerCmd>,
    sent_playlist: u32,
    manual_flag: Arc<AtomicBool>,
    res_rx: std_mpsc::Receiver<WorkerMsg>,
    icons: RankIcons,
    browser_status: String,
    names: HashMap<Key, String>,
}

// ---------------------------------------------------------------------------
// Player / URL helpers
// ---------------------------------------------------------------------------

/// The game censors some names as "*****". Steam players can still be looked
/// up by their numeric id; other platforms are looked up by name, so a
/// censored name there is a dead end.
fn is_filtered_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && n.chars().all(|c| c == '*')
}

/// "<name>'s Rocket League Stats - Rocket League Tracker" -> "<name>"
fn name_from_title(title: &str) -> Option<String> {
    for sep in ["'s Rocket League", "\u{2019}s Rocket League"] {
        if let Some(idx) = title.find(sep) {
            let n = title[..idx].trim();
            if !n.is_empty() {
                return Some(n.to_string());
            }
        }
    }
    None
}

fn parse_player(name: &str, primary_id: &str) -> Option<(&'static str, String)> {
    let parts: Vec<&str> = primary_id.split('|').collect();
    if parts.is_empty() {
        return None;
    }
    let platform = parts[0];
    let mid = parts.get(1).copied().unwrap_or("");

    let slug = match platform {
        "Steam" => "steam",
        "Epic" => "epic",
        "PS4" => "psn",
        "XboxOne" => "xbl",
        "Switch" => "switch",
        _ => return None,
    };

    let identifier = match platform {
        "Steam" => {
            if mid.is_empty() || mid == "0" {
                return None;
            }
            mid.to_string()
        }
        _ => {
            if name.trim().is_empty() || is_filtered_name(name) {
                return None;
            }
            name.to_string()
        }
    };

    Some((slug, identifier))
}

fn tracker_url(name: &str, primary_id: &str, playlist: u32) -> Option<String> {
    let (slug, id) = parse_player(name, primary_id)?;
    let encoded = urlencoding::encode(&id);
    Some(format!(
        "https://rocketleague.tracker.network/rocket-league/profile/{}/{}/mmr?playlist={}",
        slug, encoded, playlist
    ))
}

fn open_tracker(name: &str, primary_id: &str, playlist: u32) {
    if let Some(url) = tracker_url(name, primary_id, playlist) {
        if let Err(e) = webbrowser::open(&url) {
            eprintln!("Failed to open browser: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Rank table (standard-mode MMR bands)
// ---------------------------------------------------------------------------

/// (family, tier number, lower bound of Division I..IV). Ascending by MMR.
const RANK_TABLE: [(&str, u8, [i64; 4]); 21] = [
    ("Bronze", 1, [-100, 125, 139, 157]),
    ("Bronze", 2, [164, 180, 199, 217]),
    ("Bronze", 3, [229, 243, 260, 277]),
    ("Silver", 1, [295, 303, 319, 337]),
    ("Silver", 2, [355, 361, 378, 397]),
    ("Silver", 3, [415, 421, 438, 457]),
    ("Gold", 1, [475, 480, 498, 517]),
    ("Gold", 2, [535, 540, 558, 577]),
    ("Gold", 3, [595, 600, 618, 637]),
    ("Platinum", 1, [655, 660, 678, 697]),
    ("Platinum", 2, [715, 720, 738, 757]),
    ("Platinum", 3, [775, 780, 798, 817]),
    ("Diamond", 1, [835, 845, 872, 892]),
    ("Diamond", 2, [915, 925, 948, 972]),
    ("Diamond", 3, [995, 1005, 1028, 1052]),
    ("Champion", 1, [1075, 1095, 1128, 1162]),
    ("Champion", 2, [1195, 1215, 1251, 1282]),
    ("Champion", 3, [1315, 1335, 1371, 1402]),
    ("Grand Champion", 1, [1435, 1460, 1498, 1537]),
    ("Grand Champion", 2, [1575, 1603, 1647, 1677]),
    ("Grand Champion", 3, [1715, 1744, 1788, 1832]),
];

const SSL_MMR: i64 = 1867;
const TIER_NAMES: [&str; 3] = ["I", "II", "III"];
const DIV_NAMES: [&str; 4] = ["I", "II", "III", "IV"];
const FALLBACK_ICON: &str = "yousuck.png";

struct Rank {
    text: String,
    color: egui::Color32,
    /// Lower-case icon filename, e.g. "grand_champion3_rank_icon.png"
    icon_key: String,
}

fn family_color(family: &str) -> egui::Color32 {
    match family {
        "Bronze" => egui::Color32::from_rgb(190, 120, 70),
        "Silver" => egui::Color32::from_rgb(190, 195, 205),
        "Gold" => egui::Color32::from_rgb(240, 200, 60),
        "Platinum" => egui::Color32::from_rgb(90, 200, 220),
        "Diamond" => egui::Color32::from_rgb(90, 140, 255),
        "Champion" => egui::Color32::from_rgb(180, 110, 255),
        "Grand Champion" => egui::Color32::from_rgb(235, 80, 80),
        _ => egui::Color32::from_rgb(245, 245, 245),
    }
}

fn icon_key(family: &str, tier: u8) -> String {
    let base = family.to_lowercase().replace(' ', "_");
    if tier == 0 {
        format!("{base}_rank_icon.png")
    } else {
        format!("{base}{tier}_rank_icon.png")
    }
}

fn rank_label(mmr: i64) -> Rank {
    if mmr >= SSL_MMR {
        return Rank {
            text: "Supersonic Legend".to_string(),
            color: family_color("Supersonic Legend"),
            icon_key: icon_key("Supersonic Legend", 0),
        };
    }

    // Start at Bronze I Div I; the last band whose lower bound we clear wins.
    // Gaps between divisions therefore belong to the division below.
    let mut found = (RANK_TABLE[0].0, RANK_TABLE[0].1, 0usize);
    for (family, tier, lows) in RANK_TABLE.iter() {
        for (d, low) in lows.iter().enumerate() {
            if mmr >= *low {
                found = (family, *tier, d);
            }
        }
    }
    let (family, tier, div) = found;
    Rank {
        text: format!(
            "{} {} Div {}",
            family,
            TIER_NAMES[(tier - 1) as usize],
            DIV_NAMES[div]
        ),
        color: family_color(family),
        icon_key: icon_key(family, tier),
    }
}

// ---------------------------------------------------------------------------
// Rank icons (loaded from ./rankicons, resized to fit)
// ---------------------------------------------------------------------------

struct RankIcons {
    /// lower-case filename -> path
    files: HashMap<String, PathBuf>,
    cache: HashMap<String, Option<egui::TextureHandle>>,
}

impl RankIcons {
    fn new() -> Self {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                dirs.push(d.join("rankicons"));
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd.join("rankicons"));
        }
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("rankicons"));

        let mut files = HashMap::new();
        for dir in dirs {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut n = 0;
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.ends_with(".png") {
                    files.entry(name).or_insert(entry.path());
                    n += 1;
                }
            }
            eprintln!("[icons] {} png(s) in {}", n, dir.display());
        }
        if files.is_empty() {
            eprintln!("[icons] no rankicons folder found; icons disabled");
        }

        Self {
            files,
            cache: HashMap::new(),
        }
    }

    fn load(&mut self, ctx: &egui::Context, key: &str) -> Option<egui::load::SizedTexture> {
        if !self.cache.contains_key(key) {
            if !self.files.contains_key(key) {
                eprintln!("[icons] no file named '{key}' (falling back)");
            }
            let tex = self.files.get(key).and_then(|path| {
                // Sniff the real format from the file contents; a ".png" that is
                // really a WebP/JPEG (common with downloaded icons) still loads.
                let img = image::ImageReader::open(path)
                    .and_then(|r| r.with_guessed_format())
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.decode().map_err(|e| e.to_string()))
                    .map_err(|e| eprintln!("[icons] FAILED to decode {}: {e}", path.display()))
                    .ok()?;
                eprintln!("[icons] loaded {key} ({}x{})", img.width(), img.height());
                // Icons come in all sizes; keep them reasonably small on the GPU.
                let img = if img.width() > 192 || img.height() > 192 {
                    img.resize(192, 192, image::imageops::FilterType::Lanczos3)
                } else {
                    img
                };
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let ci = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                Some(ctx.load_texture(key, ci, egui::TextureOptions::LINEAR))
            });
            self.cache.insert(key.to_string(), tex);
        }
        self.cache
            .get(key)
            .and_then(|t| t.as_ref())
            .map(|t| egui::load::SizedTexture::new(t.id(), t.size_vec2()))
    }

    /// Icon for a rank, or the fallback if that rank has no file.
    fn texture_for(
        &mut self,
        ctx: &egui::Context,
        key: &str,
    ) -> Option<egui::load::SizedTexture> {
        self.load(ctx, key).or_else(|| self.load(ctx, FALLBACK_ICON))
    }
}

/// Scale `size` to fit inside `max`, keeping aspect ratio.
fn fit_size(size: egui::Vec2, max: egui::Vec2) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return max;
    }
    let s = (max.x / size.x).min(max.y / size.y);
    size * s
}

// ---------------------------------------------------------------------------
// Fonts (CJK support)
// ---------------------------------------------------------------------------

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msjh.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\malgun.ttf",
        r"C:\Windows\Fonts\msgothic.ttc",
    ];

    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            break;
        }
    }

    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------------------
// INI auto-setup
// ---------------------------------------------------------------------------

fn ensure_ini_configured() -> std::io::Result<bool> {
    let docs = dirs::document_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no Documents dir"))?;

    let config_dir = docs
        .join("My Games")
        .join("Rocket League")
        .join("TAGame")
        .join("Config");
    let ini_path = config_dir.join("TAStatsAPI.ini");

    let desired = "[TAGame.MatchStatsExporter_TA]\nPort=49123\nPacketSendRate=30\n";

    if !config_dir.exists() {
        return Ok(false);
    }

    match std::fs::read_to_string(&ini_path) {
        Ok(existing) => {
            let has_rate = existing.lines().any(|l| {
                l.trim_start().to_lowercase().starts_with("packetsendrate")
                    && l.split('=')
                        .nth(1)
                        .map(|v| v.trim() != "0")
                        .unwrap_or(false)
            });
            if has_rate {
                return Ok(false);
            }
            std::fs::write(&ini_path, desired)?;
            Ok(true)
        }
        Err(_) => {
            std::fs::write(&ini_path, desired)?;
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// MMR parsing helpers
// ---------------------------------------------------------------------------

fn point_from_history(item: &serde_json::Value) -> Option<MmrPoint> {
    let mmr_value = item
        .get("value")
        .or_else(|| item.get("mmr"))
        .or_else(|| item.get("rating"))
        .or_else(|| item.get("y"))?;

    let mmr = mmr_value
        .as_i64()
        .or_else(|| mmr_value.as_f64().map(|f| f as i64))?;

    let timestamp = item
        .get("timestamp")
        .or_else(|| item.get("date"))
        .or_else(|| item.get("time"))
        .or_else(|| item.get("x"))
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(chrono_lite_parse)))
        .unwrap_or(0);

    Some(MmrPoint { timestamp, mmr })
}

fn chrono_lite_parse(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;

    let y = year - 1970;
    let leap_days = (year - 1969) / 4 - (year - 1901) / 100 + (year - 1601) / 400;
    let month_days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let days = y * 365 + leap_days + month_days[(month - 1) as usize] + (day - 1);
    Some(days * 86_400)
}

/// Make sure points run oldest -> newest.
fn normalize(mut pts: Vec<MmrPoint>) -> Vec<MmrPoint> {
    if let (Some(f), Some(l)) = (pts.first(), pts.last()) {
        if f.timestamp != 0 && l.timestamp != 0 && f.timestamp > l.timestamp {
            pts.reverse();
        }
    }
    pts
}

/// Which ranked playlist does a key / name / value refer to?
/// 10 = 1v1, 11 = 2v2, 12 = 3v3.
fn playlist_from_text(s: &str) -> Option<u32> {
    let t = s.trim().to_lowercase();
    match t.as_str() {
        "10" | "playlist10" | "playlist_10" => return Some(10),
        "11" | "playlist11" | "playlist_11" => return Some(11),
        "12" | "playlist12" | "playlist_12" => return Some(12),
        _ => {}
    }
    if t.contains("1v1") || t.contains("duel") {
        Some(10)
    } else if t.contains("2v2") || t.contains("doubles") {
        Some(11)
    } else if t.contains("3v3") || t.contains("triples") || t.contains("ranked standard") {
        Some(12)
    } else {
        None
    }
}

/// Look at an object's own fields (and attributes/metadata one level down)
/// for a playlist id or name.
fn playlist_from_object(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u32> {
    for k in ["playlistId", "playlist_id", "playlist", "playlistName", "name"] {
        if let Some(v) = obj.get(k) {
            if let Some(n) = v.as_u64() {
                if (10..=12).contains(&n) {
                    return Some(n as u32);
                }
            } else if let Some(st) = v.as_str() {
                if let Some(p) = playlist_from_text(st) {
                    return Some(p);
                }
            }
        }
    }
    for k in ["attributes", "metadata"] {
        if let Some(sub) = obj.get(k).and_then(|v| v.as_object()) {
            if let Some(p) = playlist_from_object(sub) {
                return Some(p);
            }
        }
    }
    None
}

/// One candidate MMR series found somewhere in the JSON.
struct Series {
    path: String,
    /// Playlist this series belongs to, if the surrounding data says so.
    hint: Option<u32>,
    points: Vec<MmrPoint>,
}

fn collect_series(
    v: &serde_json::Value,
    hint: Option<u32>,
    path: &str,
    out: &mut Vec<Series>,
) {
    match v {
        serde_json::Value::Array(arr) => {
            let pts: Vec<MmrPoint> = arr.iter().filter_map(point_from_history).collect();
            if pts.len() >= 2 {
                out.push(Series {
                    path: path.to_string(),
                    hint,
                    points: pts,
                });
            }
            for (i, item) in arr.iter().enumerate() {
                if item.is_array() || (item.is_object() && point_from_history(item).is_none()) {
                    collect_series(item, hint, &format!("{path}[{i}]"), out);
                }
            }
        }
        serde_json::Value::Object(obj) => {
            let here = playlist_from_object(obj).or(hint);
            for (k, child) in obj {
                let child_hint = playlist_from_text(k).or(here);
                collect_series(child, child_hint, &format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

/// Pick the series for the playlist that was asked for.
///  1. a series the data itself labels with this playlist (longest wins)
///  2. otherwise a series with no label at all (can't tell, so longest wins)
///  3. otherwise None: everything found belongs to OTHER playlists
fn choose_series(series: Vec<Series>, playlist: u32) -> Option<Vec<MmrPoint>> {
    if DEBUG {
        for s in &series {
            eprintln!(
                "[series] {} hint={:?} len={}",
                s.path,
                s.hint,
                s.points.len()
            );
        }
    }
    let longest = |it: Vec<Series>| it.into_iter().max_by_key(|s| s.points.len());

    let (mine, rest): (Vec<Series>, Vec<Series>) =
        series.into_iter().partition(|s| s.hint == Some(playlist));
    if let Some(best) = longest(mine) {
        return Some(best.points);
    }
    let unlabeled: Vec<Series> = rest.into_iter().filter(|s| s.hint.is_none()).collect();
    longest(unlabeled).map(|s| s.points)
}

fn extract_points(v: &serde_json::Value, playlist: u32) -> Option<Vec<MmrPoint>> {
    let mut series = Vec::new();
    collect_series(v, None, "$", &mut series);
    let pts = choose_series(series, playlist)?;
    if pts.len() >= 2 {
        Some(normalize(pts))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Browser process handling (never force-kills)
// ---------------------------------------------------------------------------

/// PowerShell filter for browser processes using OUR dedicated profile dir.
/// `[p]` keeps the script's own command line from matching itself, and your
/// normal Chrome/Edge (different profile dir) is never matched.
#[cfg(windows)]
const PROFILE_FILTER: &str = "Get-CimInstance Win32_Process | Where-Object { \
    $_.ProcessId -ne $PID -and $_.Name -match 'chrome|msedge' -and \
    $_.CommandLine -like '*RLTracker*browser_[p]rofile*' }";

/// Is a Chrome/Edge already running on our profile? (read-only check)
#[cfg(windows)]
fn profile_browser_running() -> bool {
    use std::os::windows::process::CommandExt;
    let script = format!("{PROFILE_FILTER} | ForEach-Object {{ $_.ProcessId }}");
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
    {
        Ok(out) => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// Ask our Chrome to close its window like a user would (so it flushes
/// cookies to disk), then wait a few seconds. Does NOT force-kill anything;
/// if it refuses, the next launch will ask the user to close it.
#[cfg(windows)]
fn close_profile_browsers_gracefully() {
    use std::os::windows::process::CommandExt;
    let script = format!(
        "$ids = @({PROFILE_FILTER} | ForEach-Object {{ $_.ProcessId }}); \
         foreach ($id in $ids) {{ Get-Process -Id $id -ErrorAction SilentlyContinue | \
            ForEach-Object {{ [void]$_.CloseMainWindow() }} }}; \
         for ($i = 0; $i -lt 25; $i++) {{ \
            Start-Sleep -Milliseconds 300; \
            $alive = @($ids | Where-Object {{ Get-Process -Id $_ -ErrorAction SilentlyContinue }}); \
            if ($alive.Count -eq 0) {{ break }} }}"
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000)
        .status();
}

#[cfg(not(windows))]
fn profile_browser_running() -> bool {
    false
}

#[cfg(not(windows))]
fn close_profile_browsers_gracefully() {}

/// Block (without killing anything) until no Chrome is using our profile,
/// telling the user what to do in the status bar.
async fn wait_for_profile_free(tx: &std_mpsc::Sender<WorkerMsg>) {
    let mut told = false;
    while profile_browser_running() {
        if !told {
            eprintln!("[browser] profile is in use by another Chrome; waiting for it to close");
            told = true;
        }
        let _ = tx.send(WorkerMsg::Status(
            "Browser: tracker's Chrome is already open - CLOSE its window (or end it in Task Manager). Waiting...".into(),
        ));
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// MMR fetch worker (one long-lived stygian-browser session)
// ---------------------------------------------------------------------------

/// Evaluate a JS snippet that returns a string.
macro_rules! js {
    ($page:expr, $script:expr) => {
        $page
            .eval::<String>($script)
            .await
            .map_err(|e| e.to_string())
    };
}

/// Block images, fonts and media on profile pages (not scripts/CSS, so the
/// page and any Cloudflare check still work). Set to false if it causes trouble.
const BLOCK_HEAVY_RESOURCES: bool = true;

/// Minimum pause between two player fetches (plus up to ~0.7s random jitter).
const FETCH_GAP_MS: u64 = 2000;
/// No background fetching: only the playlist on screen is ever fetched.
/// Requests for other playlists just wait in the queue until you switch to them.
fn is_allowed(current: u32, pl: u32) -> bool {
    pl == current
}

fn has_work(pending: &[FetchReq], current: u32) -> bool {
    pending.iter().any(|r| is_allowed(current, r.key.1))
}

/// Sentinel result: the fetch was stopped because Manual mode was switched on.
const PAUSED_MSG: &str = "__paused__";

/// Extra cool-down after a Cloudflare challenge was seen.
const CHALLENGE_PENALTY_SECS: u64 = 20;

const HIST_SCRIPT: &str = r#"JSON.stringify(
    (window.__INITIAL_STATE__ && window.__INITIAL_STATE__.stats
        && window.__INITIAL_STATE__.stats.standardProfilesHistory) || null
)"#;

const XHR_SCRIPT: &str = r#"JSON.stringify(
    performance.getEntriesByType('resource')
        .filter(e => e.initiatorType === 'fetch' || e.initiatorType === 'xmlhttprequest')
        .map(e => e.name)
)"#;

/// Does this URL look like it could carry MMR / rating history?
fn looks_like_mmr_api(url: &str) -> bool {
    let u = url.to_lowercase();
    if [".js", ".css", ".png", ".jpg", ".svg", ".woff2"]
        .iter()
        .any(|ext| u.ends_with(ext))
    {
        return false;
    }
    let is_api = u.contains("api.tracker.gg") || u.contains("/api/");
    let keyword = ["mmr", "history", "rating", "playlist", "segments", "profile"]
        .iter()
        .any(|k| u.contains(k));
    is_api && keyword
}

/// `...playlist=11...` in an API url -> Some(11)
fn url_playlist_hint(url: &str) -> Option<u32> {
    let u = url.to_lowercase();
    for key in ["playlistid=", "playlist_id=", "playlist="] {
        if let Some(i) = u.find(key) {
            let digits: String = u[i + key.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

async fn run_mmr_worker(
    mut rx: tokio_mpsc::UnboundedReceiver<WorkerCmd>,
    tx: std_mpsc::Sender<WorkerMsg>,
    manual: Arc<AtomicBool>,
) {
    use std::collections::HashSet;
    use std::time::Duration;
    use stygian_browser::{BrowserConfig, BrowserPool, WaitUntil};

    // Opens the browser + one page on our persistent profile.
    macro_rules! open_session {
        () => {
            async {
                let profile_dir = dirs::data_local_dir()
                    .ok_or_else(|| "Could not find local data directory".to_string())?
                    .join("RLTracker")
                    .join("browser_profile");
                std::fs::create_dir_all(&profile_dir)
                    .map_err(|e| format!("Failed to create profile dir: {e}"))?;

                let config = BrowserConfig::builder()
                    .headless(false)
                    .window_size(1920, 1080)
                    .user_data_dir(profile_dir)
                    .build();

                let pool = BrowserPool::new(config)
                    .await
                    .map_err(|e| format!("BrowserPool init: {e}"))?;
                let handle = pool
                    .acquire()
                    .await
                    .map_err(|e| format!("Pool acquire: {e}"))?;
                let browser = handle
                    .browser()
                    .ok_or_else(|| "Browser handle already released".to_string())?;
                let page = browser
                    .new_page()
                    .await
                    .map_err(|e| format!("new_page: {e}"))?;
                Ok::<_, String>((pool, handle, page))
            }
            .await
        };
    }

    // ---- start the browser immediately, not on first request -------------
    let _ = tx.send(WorkerMsg::Status("Browser: starting...".into()));
    let mut session = None;
    let mut failures = 0u32;
    let mut filter_applied = false;

    // If a Chrome from a previous run still holds the profile, wait for the
    // user to close it (we never force-kill, so cookies get written to disk).
    wait_for_profile_free(&tx).await;
    let _ = tx.send(WorkerMsg::Status("Browser: starting...".into()));
    for attempt in 1..=3 {
        match open_session!() {
            Ok(s) => {
                session = Some(s);
                break;
            }
            Err(e) => {
                eprintln!("[browser] start attempt {attempt}/3 failed: {e}");
                let _ = tx.send(WorkerMsg::Status(format!("Browser: retrying ({attempt}/3)...")));
                tokio::time::sleep(Duration::from_secs(2)).await;
                wait_for_profile_free(&tx).await;
            }
        }
    }

    match session.as_mut() {
        Some((_pool, _handle, page)) => {
            if manual.load(Ordering::Relaxed) {
                // Manual mode: hands off. The user does the first search themselves.
                let _ = tx.send(WorkerMsg::Status("Browser: ready (manual mode)".into()));
            } else {
                // Warm up: loads the site once so Cloudflare cookies are ready.
                let _ = tx.send(WorkerMsg::Status("Browser: warming up...".into()));
                if let Err(e) = page
                    .navigate(
                        "https://rocketleague.tracker.network/",
                        WaitUntil::Selector("body".to_string()),
                        Duration::from_secs(30),
                    )
                    .await
                {
                    eprintln!("[browser] warm-up navigation warning: {e}");
                }
                let _ = tx.send(WorkerMsg::Status("Browser: ready".into()));
            }
        }
        None => {
            let _ = tx.send(WorkerMsg::Status(
                "Browser: FAILED to start (will retry on next fetch)".into(),
            ));
        }
    }

    let mut next_allowed = std::time::Instant::now();
    let mut current_playlist: u32 = 11;
    let mut pending: Vec<FetchReq> = Vec::new();

    macro_rules! apply_cmd {
        ($cmd:expr) => {
            match $cmd {
                WorkerCmd::Fetch(r) => pending.push(r),
                WorkerCmd::SetPlaylist(p) => current_playlist = p,
                WorkerCmd::Wake => {}
            }
        };
    }

    'outer: loop {
        // Sleep until there is work AND we're allowed to touch the browser.
        while !has_work(&pending, current_playlist) || manual.load(Ordering::Relaxed) {
            match rx.recv().await {
                Some(cmd) => apply_cmd!(cmd),
                None => break 'outer,
            }
        }

        // Throttle first, so anything that arrives meanwhile can still jump the queue.
        let now = std::time::Instant::now();
        if now < next_allowed {
            tokio::time::sleep(next_allowed - now).await;
        }
        while let Ok(cmd) = rx.try_recv() {
            apply_cmd!(cmd);
        }
        if !has_work(&pending, current_playlist) || manual.load(Ordering::Relaxed) {
            continue;
        }

        // Only the playlist on screen is fetched; anything else waits.
        let Some(idx) = pending
            .iter()
            .position(|r| is_allowed(current_playlist, r.key.1))
        else {
            continue;
        };
        let req = pending.remove(idx);
        let playlist = req.key.1;

        let _ = tx.send(WorkerMsg::Started(req.key.clone()));
        eprintln!("[mmr] >>> {}", req.url);

        // ---- (re)open the browser if it isn't running --------------------
        if session.is_none() {
            wait_for_profile_free(&tx).await;
            match open_session!() {
                Ok(s) => {
                    session = Some(s);
                    filter_applied = false;
                    let _ = tx.send(WorkerMsg::Status("Browser: ready".into()));
                }
                Err(e) => {
                    let _ = tx.send(WorkerMsg::Status(format!("Browser: FAILED ({e})")));
                    let _ = tx.send(WorkerMsg::Finished(req.key, Err(e)));
                    continue;
                }
            }
        }

        // ---- fetch one player on the shared page -------------------------
        let result: Result<Vec<MmrPoint>, String> = async {
            let (_pool, _handle, page) = session.as_mut().expect("session exists");

            // Manual mode switched on: don't touch the page at all.
            if manual.load(Ordering::Relaxed) {
                return Err(PAUSED_MSG.to_string());
            }

            // Applied after the warm-up page, so the first Cloudflare check
            // loads exactly like a normal browser.
            if BLOCK_HEAVY_RESOURCES && !filter_applied {
                use stygian_browser::page::{ResourceFilter, ResourceType};
                let filter = ResourceFilter::block_images_and_fonts().block(ResourceType::Media);
                match page.set_resource_filter(filter).await {
                    Ok(()) => eprintln!("[browser] blocking images/fonts/media"),
                    Err(e) => eprintln!("[browser] resource filter failed: {e}"),
                }
                filter_applied = true;
            }

            if let Err(e) = page
                .navigate(
                    &req.url,
                    WaitUntil::Selector("body".to_string()),
                    Duration::from_secs(30),
                )
                .await
            {
                eprintln!("[mmr] navigation warning: {e}");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;

            let mut tried: HashSet<String> = HashSet::new();
            let mut last_urls = String::new();
            let mut challenged = false;
            let mut name_sent = false;
            let mut any_eval_ok = false;

            for attempt in 0..30 {
                // A normal page with no data (never played this playlist) shouldn't
                // keep us here for a minute. Only a Cloudflare wait gets the long budget.
                if manual.load(Ordering::Relaxed) {
                    return Err(PAUSED_MSG.to_string());
                }
                if !challenged && attempt >= 10 {
                    break;
                }

                // 0) Cloudflare interstitial? Wait it out instead of burning attempts.
                if let Ok(t) = page.title().await {
                    any_eval_ok = true;
                    if !name_sent {
                        if let Some(n) = name_from_title(&t) {
                            let _ = tx.send(WorkerMsg::DisplayName(req.key.clone(), n));
                            name_sent = true;
                        }
                    }
                    let tl = t.to_lowercase();
                    if tl.contains("just a moment")
                        || tl.contains("attention required")
                        || tl.contains("security verification")
                    {
                        if !challenged {
                            eprintln!("[mmr] Cloudflare challenge detected, waiting...");
                        }
                        challenged = true;
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        continue;
                    }
                }

                // Tracker's 404 page: stop right away instead of retrying for a minute.
                if let Ok(body) =
                    js!(page, "document.body ? document.body.innerText.slice(0, 800) : ''")
                {
                    let b = body.to_lowercase();
                    if b.contains("could not find the player") || b.contains("player not found") {
                        return Err("Tracker: player not found for this name/ID".to_string());
                    }
                }

                // 1) The server-rendered history key.
                if let Ok(hist) = js!(page, HIST_SCRIPT) {
                    if hist != "null" {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&hist) {
                            if let Some(p) = extract_points(&v, playlist) {
                                eprintln!("[mmr] {} pts via standardProfilesHistory", p.len());
                                return Ok(p);
                            }
                        }
                    }
                }

                // 2) Replay any new MMR-looking API call from inside the page.
                let urls_json = match js!(page, XHR_SCRIPT) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[mmr] attempt {attempt}: xhr probe failed: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                last_urls = urls_json.clone();
                let urls: Vec<String> = serde_json::from_str(&urls_json).unwrap_or_default();

                // Skip calls that are explicitly for a different playlist.
                for api in urls.iter().filter(|u| {
                    looks_like_mmr_api(u)
                        && url_playlist_hint(u).map_or(true, |h| h == playlist)
                }) {
                    if !tried.insert(api.clone()) {
                        continue;
                    }
                    let lit = serde_json::to_string(api).map_err(|e| e.to_string())?;
                    let kick = format!(
                        r#"(() => {{
                            window.__mmr = null;
                            fetch({lit}, {{ credentials: "include" }})
                                .then(r => r.text())
                                .then(t => {{ window.__mmr = t; }})
                                .catch(e => {{ window.__mmr = "ERR:" + e.message; }});
                            return "started";
                        }})()"#
                    );
                    if js!(page, &kick).is_err() {
                        continue;
                    }

                    let mut body = String::new();
                    for _ in 0..16 {
                        let b = js!(page, "window.__mmr || ''").unwrap_or_default();
                        if !b.is_empty() {
                            body = b;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }

                    if body.is_empty() || body.starts_with("ERR:") {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                        if let Some(p) = extract_points(&v, playlist) {
                            eprintln!("[mmr] {} pts via {api}", p.len());
                            return Ok(p);
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(2)).await;
            }

            debug_write("xhr_urls.json", &last_urls);
            if challenged {
                Err("Cloudflare challenge didn't clear (if the browser window shows a checkbox, click it, then Refetch)."
                    .to_string())
            } else if !any_eval_ok {
                Err("Browser not responding (was its window closed?)".to_string())
            } else {
                Err("No MMR history for this playlist (no ranked games, or private profile)"
                    .to_string())
            }
        }
        .await;

        // If the browser keeps failing (window closed, crashed), start fresh.
        // Stopped by Manual mode: put it back at the front and show it as queued.
        if matches!(&result, Err(e) if e == PAUSED_MSG) {
            let _ = tx.send(WorkerMsg::Finished(req.key.clone(), result));
            pending.insert(0, req);
            continue;
        }

        // Only count genuine browser faults. "No ranked games" is a normal answer.
        match &result {
            Err(e) if e.contains("Browser not responding") => failures += 1,
            _ => failures = 0,
        }
        if failures >= 3 {
            if let Some((_pool, handle, _page)) = session.take() {
                let _ = handle.release().await;
            }
            close_profile_browsers_gracefully();
            filter_applied = false;
            let _ = tx.send(WorkerMsg::Status("Browser: restarting after errors...".into()));
            failures = 0;
        }

        // Next fetch no sooner than FETCH_GAP_MS (+jitter); longer after a challenge.
        let jitter = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_millis() as u64 % 700)
            .unwrap_or(0);
        let mut gap = Duration::from_millis(FETCH_GAP_MS + jitter);
        if matches!(&result, Err(e) if e.contains("Cloudflare")) {
            gap += Duration::from_secs(CHALLENGE_PENALTY_SECS);
        }
        next_allowed = std::time::Instant::now() + gap;

        let _ = tx.send(WorkerMsg::Finished(req.key, result));
    }

    if let Some((_pool, handle, _page)) = session.take() {
        let _ = handle.release().await;
    }
}

// ---------------------------------------------------------------------------
// Background stats client
// ---------------------------------------------------------------------------

async fn run_stats_client(tx: std_mpsc::Sender<Vec<PlayerInfo>>) {
    let options = ClientOptions::default();

    let mut client = loop {
        match RocketLeagueStatsClient::connect(options.clone()).await {
            Ok(c) => break c,
            Err(e) => {
                eprintln!("Connection failed: {e}. Retrying in 3s...");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    };

    loop {
        match client.next_event().await {
            Ok(Some(event)) => {
                if let StatsEvent::UpdateState(data) = event {
                    let players: Vec<PlayerInfo> = data
                        .players
                        .iter()
                        .map(|p| PlayerInfo {
                            name: p.name.clone().unwrap_or_else(|| "Unknown".into()),
                            primary_id: p.primary_id.clone().unwrap_or_default(),
                        })
                        .collect();
                    let _ = tx.send(players);
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("Event error: {e}. Reconnecting...");
                let _ = client.reconnect().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Chart helpers
// ---------------------------------------------------------------------------

/// Standardised Y window. Always >= 600 MMR tall, centred near the player's
/// average, never outside 0..2500, and always contains all their data.
fn y_bounds(points: &[MmrPoint], full_scale: bool) -> (f64, f64) {
    if full_scale || points.is_empty() {
        return (0.0, 2500.0);
    }
    let n = points.len() as f64;
    let avg = points.iter().map(|p| p.mmr as f64).sum::<f64>() / n;
    let min = points.iter().map(|p| p.mmr).min().unwrap_or(0) as f64;
    let max = points.iter().map(|p| p.mmr).max().unwrap_or(0) as f64;

    let mut lo = (avg - 300.0).min(min - 40.0);
    let mut hi = (avg + 300.0).max(max + 40.0);
    if lo < 0.0 {
        hi -= lo;
        lo = 0.0;
    }
    if hi > 2500.0 {
        lo = (lo - (hi - 2500.0)).max(0.0);
        hi = 2500.0;
    }
    (lo, hi)
}

// ---------------------------------------------------------------------------
// Player card
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum CardAction {
    Open,
    Refetch,
}

fn draw_card(
    ui: &mut egui::Ui,
    idx: usize,
    _player: &PlayerInfo,
    entry: Option<&Entry>,
    full_scale: bool,
    card_h: f32,
    icons: &mut RankIcons,
    display_name: &str,
    paused: bool,
) -> Option<CardAction> {
    let mut action = None;

    // Current rank (if we have data) so the header can show its icon.
    let cur_rank = match entry {
        Some(Entry::Done(p)) if !p.is_empty() => p.last().map(|pt| rank_label(pt.mmr)),
        _ => None,
    };

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height((card_h - 24.0).max(60.0));

        ui.horizontal(|ui| {
            if let Some(r) = &cur_rank {
                if let Some(tex) = icons.texture_for(ui.ctx(), &r.icon_key) {
                    let size = fit_size(tex.size, egui::vec2(52.0, 44.0));
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id, size)));
                }
            }
            ui.label(egui::RichText::new(display_name).strong().size(17.0));
            let busy = matches!(entry, None | Some(Entry::Queued) | Some(Entry::Loading));
            if ui.small_button("Open").clicked() {
                action = Some(CardAction::Open);
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Refetch").small())
                .clicked()
            {
                action = Some(CardAction::Refetch);
            }
        });

        match entry {
            None | Some(Entry::Queued) => {
                if paused {
                    ui.label(egui::RichText::new("Waiting (manual mode is on)").weak());
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Queued...");
                    });
                }
            }
            Some(Entry::Loading) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Fetching from Tracker...");
                });
            }
            Some(Entry::Failed(e)) => {
                ui.colored_label(egui::Color32::LIGHT_RED, e);
            }
            Some(Entry::Done(points)) if points.is_empty() => {
                ui.label("No MMR history found.");
            }
            Some(Entry::Done(points)) => {
                let current = points.last().map(|p| p.mmr).unwrap_or(0);
                let peak = points.iter().map(|p| p.mmr).max().unwrap_or(0);
                let cur = rank_label(current);
                let pk = rank_label(peak);

                // Current on the left, peak on the right. Plain text, normal size.
                ui.columns(2, |cols| {
                    cols[0].horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Current").weak());
                        ui.label(&cur.text);
                        ui.label(egui::RichText::new(format!("({current})")).weak());
                    });
                    cols[1].horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Peak").weak());
                        if let Some(tex) = icons.texture_for(ui.ctx(), &pk.icon_key) {
                            let size = fit_size(tex.size, egui::vec2(26.0, 22.0));
                            ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id, size)));
                        }
                        ui.label(&pk.text);
                        ui.label(egui::RichText::new(format!("({peak})")).weak());
                    });
                });

                let (lo, hi) = y_bounds(points, full_scale);
                let x_max = (points.len().saturating_sub(1)).max(1) as f64;
                let chart_h = (card_h - 150.0).max(90.0);

                let line_points: PlotPoints = points
                    .iter()
                    .enumerate()
                    .map(|(i, p)| [i as f64, p.mmr as f64])
                    .collect();
                let line = Line::new(line_points).color(cur.color).width(2.0_f32);

                Plot::new(format!("mmr_plot_{idx}"))
                    .height(chart_h)
                    .allow_zoom(false)
                    .allow_drag(false)
                    .allow_scroll(false)
                    .show(ui, |plot_ui| {
                        plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                            [-0.5, lo],
                            [x_max + 0.5, hi],
                        ));
                        plot_ui.line(line);
                    });
            }
        }
    });

    action
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl TrackerApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        rx: std_mpsc::Receiver<Vec<PlayerInfo>>,
        req_tx: tokio_mpsc::UnboundedSender<WorkerCmd>,
        res_rx: std_mpsc::Receiver<WorkerMsg>,
        manual_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            players: Vec::new(),
            rx,
            status: "Waiting for Rocket League...".to_string(),
            playlist: 11,
            full_scale: false,
            entries: HashMap::new(),
            req_tx,
            sent_playlist: 11,
            manual_flag,
            res_rx,
            icons: RankIcons::new(),
            browser_status: "Browser: starting...".to_string(),
            names: HashMap::new(),
        }
    }

    fn poll_events(&mut self) {
        while let Ok(players) = self.rx.try_recv() {
            // Censored names ("*****", any length) are skipped entirely.
            self.players = players
                .into_iter()
                .filter(|p| !is_filtered_name(&p.name))
                .collect();
            self.status = format!("Tracking {} players", self.players.len());
        }

        while let Ok(msg) = self.res_rx.try_recv() {
            match msg {
                WorkerMsg::Status(st) => {
                    self.browser_status = st;
                }
                WorkerMsg::DisplayName(key, n) => {
                    self.names.insert(key, n);
                }
                WorkerMsg::Started(key) => {
                    self.entries.insert(key, Entry::Loading);
                }
                WorkerMsg::Finished(key, Ok(points)) => {
                    self.entries.insert(key, Entry::Done(points));
                }
                WorkerMsg::Finished(key, Err(e)) if e == PAUSED_MSG => {
                    self.entries.insert(key, Entry::Queued);
                }
                WorkerMsg::Finished(key, Err(e)) => {
                    self.entries.insert(key, Entry::Failed(e));
                }
            }
        }

        // Tell the worker which playlist is on screen so it can prioritise it.
        if self.sent_playlist != self.playlist {
            let _ = self.req_tx.send(WorkerCmd::SetPlaylist(self.playlist));
            self.sent_playlist = self.playlist;
        }

        self.enqueue_missing();
    }

    /// Queue a fetch for every player in the lobby for the playlist on screen
    /// (if we don't have it yet). Nothing is fetched in the background: switch
    /// tabs and that tab gets fetched.
    fn enqueue_missing(&mut self) {
        let pl = self.playlist;
        for i in 0..self.players.len() {
            let key = (self.players[i].primary_id.clone(), pl);
            if self.entries.contains_key(&key) {
                continue;
            }
            self.queue_player(i, pl, false);
        }
    }

    fn queue_player(&mut self, idx: usize, playlist: u32, force: bool) {
        let Some(p) = self.players.get(idx) else {
            return;
        };
        let key = (p.primary_id.clone(), playlist);
        if !force && self.entries.contains_key(&key) {
            return;
        }
        match tracker_url(&p.name, &p.primary_id, playlist) {
            Some(url) => {
                self.entries.insert(key.clone(), Entry::Queued);
                let _ = self.req_tx.send(WorkerCmd::Fetch(FetchReq { key, url }));
            }
            None => {
                self.entries.insert(
                    key,
                    Entry::Failed("No profile (bot or unknown platform)".into()),
                );
            }
        }
    }
}

impl eframe::App for TrackerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        let mut refetch_all = false;
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let mut manual = self.manual_flag.load(Ordering::Relaxed);
                let label = if manual { "Manual mode: ON" } else { "Manual mode: OFF" };
                if ui
                    .toggle_value(&mut manual, label)
                    .on_hover_text(
                        "ON: the app never touches the browser. Do a search by hand \
                         there, then turn this OFF to start fetching.",
                    )
                    .changed()
                {
                    self.manual_flag.store(manual, Ordering::Relaxed);
                    let _ = self.req_tx.send(WorkerCmd::Wake);
                }
                ui.separator();
                ui.label("Playlist:");
                ui.selectable_value(&mut self.playlist, 10, "1v1");
                ui.selectable_value(&mut self.playlist, 11, "2v2");
                ui.selectable_value(&mut self.playlist, 12, "3v3");
                ui.separator();
                ui.checkbox(&mut self.full_scale, "Full scale (0-2500)");
                ui.separator();
                if ui.button("Refetch all").clicked() {
                    refetch_all = true;
                }
                ui.separator();
                ui.label(&self.status);
                ui.separator();
                let st = &self.browser_status;
                if st.contains("CLOSE") || st.contains("FAILED") {
                    ui.colored_label(egui::Color32::LIGHT_RED, st);
                } else {
                    ui.label(st);
                }
            });
        });

        if refetch_all {
            let pl = self.playlist;
            for i in 0..self.players.len() {
                self.queue_player(i, pl, true);
            }
        }

        let mut pending: Option<(usize, CardAction)> = None;

        let manual_now = self.manual_flag.load(Ordering::Relaxed);

        egui::CentralPanel::default().show(ctx, |ui| {
            if manual_now {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 80),
                    "Manual mode is ON: the app is not touching the browser. Do one search by \
                     hand in the browser window to get past Cloudflare, then switch Manual mode OFF.",
                );
                ui.add_space(4.0);
            }

            if self.players.is_empty() {
                ui.heading("Rocket League Tracker");
                ui.label("Waiting for a match...");
                let st = &self.browser_status;
                if st.contains("CLOSE") || st.contains("FAILED") {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::LIGHT_RED, st);
                }
                return;
            }

            // 1v1: 2 side by side. 2v2: 2x2. 3v3: 3x2.
            let cols: usize = if self.playlist == 12 { 3 } else { 2 };
            let shown = self.players.len().min(8);
            let rows = shown.div_ceil(cols).max(1);

            let gap = ui.spacing().item_spacing.y;
            let row_h =
                ((ui.available_height() - gap * (rows as f32 - 1.0)) / rows as f32).max(120.0);

            let playlist = self.playlist;
            let full_scale = self.full_scale;
            let players = &self.players;
            let entries = &self.entries;
            let names = &self.names;
            let icons = &mut self.icons;

            for r in 0..rows {
                ui.columns(cols, |uis| {
                    for c in 0..cols {
                        let idx = r * cols + c;
                        if idx >= shown {
                            continue;
                        }
                        let p = &players[idx];
                        let key = (p.primary_id.clone(), playlist);
                        let entry = entries.get(&key);
                        // Censored in-game name -> use the real one from the tracker page.
                        let shown = if is_filtered_name(&p.name) {
                            names
                                .get(&key)
                                .cloned()
                                .unwrap_or_else(|| "(censored name)".to_string())
                        } else {
                            p.name.clone()
                        };
                        if let Some(a) =
                            draw_card(&mut uis[c], idx, p, entry, full_scale, row_h, icons, &shown, manual_now)
                        {
                            pending = Some((idx, a));
                        }
                    }
                });
            }
        });

        match pending {
            Some((i, CardAction::Open)) => {
                if let Some(p) = self.players.get(i) {
                    open_tracker(&p.name.clone(), &p.primary_id.clone(), self.playlist);
                }
            }
            Some((i, CardAction::Refetch)) => {
                let pl = self.playlist;
                self.queue_player(i, pl, true);
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    match ensure_ini_configured() {
        Ok(true) => {
            eprintln!("TAStatsAPI.ini was updated. Restart Rocket League for it to take effect.");
        }
        Ok(false) => {}
        Err(e) => eprintln!("INI check failed: {e}"),
    }

    // Rocket League stats stream -> UI
    let (tx, rx) = std_mpsc::channel::<Vec<PlayerInfo>>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        rt.block_on(run_stats_client(tx));
    });

    // UI -> MMR worker (requests) and worker -> UI (results)
    let (req_tx, req_rx) = tokio_mpsc::unbounded_channel::<WorkerCmd>();
    let (res_tx, res_rx) = std_mpsc::channel::<WorkerMsg>();
    // Manual mode starts ON: the app won't touch the browser until you say so.
    let manual = Arc::new(AtomicBool::new(true));
    let manual_worker = manual.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        rt.block_on(run_mmr_worker(req_rx, res_tx, manual_worker));
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "RL Tracker",
        native_options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(TrackerApp::new(cc, rx, req_tx, res_rx, manual)))
        }),
    );

    // Close our Chrome politely so it saves cookies (no force-kill).
    close_profile_browsers_gracefully();
    result
}