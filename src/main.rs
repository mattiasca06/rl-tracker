#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use rlstatsapi::{ClientOptions, RocketLeagueStatsClient, StatsEvent};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PlayerInfo {
    name: String,
    primary_id: String,
}

/// One playlist's skill, as returned by the rlsidecar (PsyNet) bridge.
#[derive(Clone, Debug, serde::Deserialize)]
struct Skill {
    playlist: u32,
    mmr: f64,
    tier: u32,
    division: u32,
    matches: u32,
    #[allow(dead_code)]
    placement_matches: u32,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct PlayerSkills {
    player_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    skills: Vec<Skill>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct SidecarStatus {
    #[serde(default)]
    state: String,
    #[serde(default)]
    verification_uri: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    error: String,
}

/// UI -> worker.
enum WorkerCmd {
    Fetch(Vec<String>),
}

/// worker -> UI.
enum WorkerMsg {
    Status(SidecarStatus),
    Result(Vec<String>, Result<Vec<PlayerSkills>, String>),
}

enum Entry {
    Loading,
    Done(PlayerSkills),
    Failed(String),
}

struct TrackerApp {
    players: Vec<PlayerInfo>,
    rx: std_mpsc::Receiver<Vec<PlayerInfo>>,
    status: String,

    playlist: u32,

    /// Keyed by primary_id (one request returns every playlist).
    entries: HashMap<String, Entry>,
    req_tx: std_mpsc::Sender<WorkerCmd>,
    res_rx: std_mpsc::Receiver<WorkerMsg>,
    icons: RankIcons,
    sidecar: SidecarStatus,
}

// ---------------------------------------------------------------------------
// Player helpers
// ---------------------------------------------------------------------------

/// The game censors some names as "*****".
fn is_filtered_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && n.chars().all(|c| c == '*')
}

/// Can PsyNet look this player up? Needs "<Platform>|<id>|<n>" with a real id (bots have 0).
fn is_lookupable(primary_id: &str) -> bool {
    let parts: Vec<&str> = primary_id.split('|').collect();
    parts.len() == 3
        && ["Steam", "Epic", "PS4", "XboxOne", "Switch"].contains(&parts[0])
        && !parts[1].is_empty()
        && parts[1] != "0"
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

/// Rank from the game's own tier (0 = unranked, 1..=21 Bronze I..GC III, 22 = SSL) and division (0..=3).
fn rank_from_tier(tier: u32, division: u32) -> Rank {
    if tier == 0 {
        return Rank {
            text: "Unranked".to_string(),
            color: family_color(""),
            icon_key: FALLBACK_ICON.to_string(),
        };
    }
    if tier as usize > RANK_TABLE.len() {
        return Rank {
            text: "Supersonic Legend".to_string(),
            color: family_color("Supersonic Legend"),
            icon_key: icon_key("Supersonic Legend", 0),
        };
    }
    let (family, t, _) = RANK_TABLE[tier as usize - 1];
    Rank {
        text: format!(
            "{} {} Div {}",
            family,
            TIER_NAMES[(t - 1) as usize],
            DIV_NAMES[(division as usize).min(3)]
        ),
        color: family_color(family),
        icon_key: icon_key(family, t),
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
// Sidecar (rlsidecar.exe: Go bridge to PsyNet via github.com/dank/rlapi)
// ---------------------------------------------------------------------------

const SIDECAR_ADDR: &str = "127.0.0.1:49200";

fn sidecar_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "rlsidecar.exe" } else { "rlsidecar" };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.to_path_buf());
            dirs.push(d.join("sidecar"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("sidecar"));
    }
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar"));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn spawn_sidecar() -> Result<Child, String> {
    let path = sidecar_path().ok_or("rlsidecar.exe not found (build it: cd sidecar && go build)")?;
    let mut cmd = Command::new(&path);
    cmd.args(["-addr", SIDECAR_ADDR])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map_err(|e| format!("could not start {}: {e}", path.display()))
}

fn fetch_status() -> Result<SidecarStatus, String> {
    ureq::get(&format!("http://{SIDECAR_ADDR}/status"))
        .timeout(Duration::from_secs(3))
        .call()
        .map_err(|e| e.to_string())?
        .into_json::<SidecarStatus>()
        .map_err(|e| e.to_string())
}

fn fetch_skills(ids: &[String]) -> Result<Vec<PlayerSkills>, String> {
    #[derive(serde::Deserialize)]
    struct Resp {
        players: Vec<PlayerSkills>,
    }
    let resp = ureq::post(&format!("http://{SIDECAR_ADDR}/skills"))
        .timeout(Duration::from_secs(25))
        .send_json(serde_json::json!({ "player_ids": ids }))
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                format!("{code}: {}", r.into_string().unwrap_or_default().trim())
            }
            e => e.to_string(),
        })?;
    Ok(resp.into_json::<Resp>().map_err(|e| e.to_string())?.players)
}

/// Worker thread: waits for the sidecar to be logged in, then serves fetch requests.
fn run_worker(rx: std_mpsc::Receiver<WorkerCmd>, tx: std_mpsc::Sender<WorkerMsg>) {
    let mut last = String::new();
    loop {
        let st = fetch_status().unwrap_or(SidecarStatus {
            state: "starting".into(),
            ..Default::default()
        });
        let key = format!("{}{}{}", st.state, st.user_code, st.error);
        if key != last {
            last = key;
            let _ = tx.send(WorkerMsg::Status(st.clone()));
        }
        match st.state.as_str() {
            "ready" => break,
            "error" => std::thread::sleep(Duration::from_secs(5)),
            _ => std::thread::sleep(Duration::from_millis(700)),
        }
    }

    while let Ok(WorkerCmd::Fetch(mut ids)) = rx.recv() {
        // Coalesce anything queued meanwhile into one request.
        while let Ok(WorkerCmd::Fetch(more)) = rx.try_recv() {
            ids.extend(more);
        }
        ids.sort();
        ids.dedup();
        let res = fetch_skills(&ids);
        let _ = tx.send(WorkerMsg::Result(ids, res));
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
// Player card
// ---------------------------------------------------------------------------

fn skill_line(ui: &mut egui::Ui, icons: &mut RankIcons, s: &Skill, big: bool) {
    let rank = rank_from_tier(s.tier, s.division);
    ui.horizontal(|ui| {
        if let Some(tex) = icons.texture_for(ui.ctx(), &rank.icon_key) {
            let max = if big { egui::vec2(72.0, 64.0) } else { egui::vec2(28.0, 24.0) };
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                tex.id,
                fit_size(tex.size, max),
            )));
        }
        ui.vertical(|ui| {
            let size = if big { 20.0 } else { 14.0 };
            ui.label(egui::RichText::new(&rank.text).color(rank.color).size(size).strong());
            let mmr = format!("{} MMR", s.mmr.round() as i64);
            if big {
                ui.label(format!("{mmr}  -  {} games", s.matches));
            } else {
                ui.label(egui::RichText::new(mmr).weak());
            }
        });
    });
}

fn playlist_name(p: u32) -> &'static str {
    match p {
        10 => "1v1",
        11 => "2v2",
        12 => "3v3",
        _ => "?",
    }
}

/// Returns true if "Refetch" was clicked.
fn draw_card(
    ui: &mut egui::Ui,
    entry: Option<&Entry>,
    card_h: f32,
    icons: &mut RankIcons,
    display_name: &str,
    playlist: u32,
    lookupable: bool,
) -> bool {
    let mut refetch = false;

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height((card_h - 24.0).max(60.0));

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(display_name).strong().size(17.0));
            let busy = matches!(entry, Some(Entry::Loading));
            if lookupable
                && ui
                    .add_enabled(!busy, egui::Button::new("Refetch").small())
                    .clicked()
            {
                refetch = true;
            }
        });

        if !lookupable {
            ui.label(egui::RichText::new("No profile (bot or unknown platform)").weak());
            return;
        }

        match entry {
            None | Some(Entry::Loading) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Fetching...");
                });
            }
            Some(Entry::Failed(e)) => {
                ui.colored_label(egui::Color32::LIGHT_RED, e);
            }
            Some(Entry::Done(ps)) => {
                let find = |pl: u32| ps.skills.iter().find(|s| s.playlist == pl);
                match find(playlist) {
                    Some(s) => {
                        ui.label(egui::RichText::new(playlist_name(playlist)).weak());
                        skill_line(ui, icons, s, true);
                    }
                    None => {
                        ui.label(format!("No {} ranked data.", playlist_name(playlist)));
                    }
                }
                ui.separator();
                for pl in [10, 11, 12].into_iter().filter(|p| *p != playlist) {
                    if let Some(s) = find(pl) {
                        ui.label(egui::RichText::new(playlist_name(pl)).weak());
                        skill_line(ui, icons, s, false);
                    }
                }
            }
        }
    });

    refetch
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl TrackerApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        rx: std_mpsc::Receiver<Vec<PlayerInfo>>,
        req_tx: std_mpsc::Sender<WorkerCmd>,
        res_rx: std_mpsc::Receiver<WorkerMsg>,
    ) -> Self {
        Self {
            players: Vec::new(),
            rx,
            status: "Waiting for Rocket League...".to_string(),
            playlist: 11,
            entries: HashMap::new(),
            req_tx,
            res_rx,
            icons: RankIcons::new(),
            sidecar: SidecarStatus {
                state: "starting".into(),
                ..Default::default()
            },
        }
    }

    fn poll_events(&mut self) {
        while let Ok(players) = self.rx.try_recv() {
            self.players = players;
            self.status = format!("Tracking {} players", self.players.len());
        }

        while let Ok(msg) = self.res_rx.try_recv() {
            match msg {
                WorkerMsg::Status(st) => self.sidecar = st,
                WorkerMsg::Result(ids, Ok(found)) => {
                    for id in ids {
                        // A player PsyNet returned nothing for just has no ranked data.
                        let ps = found
                            .iter()
                            .find(|p| p.player_id == id)
                            .cloned()
                            .unwrap_or(PlayerSkills {
                                player_id: id.clone(),
                                name: String::new(),
                                skills: Vec::new(),
                            });
                        self.entries.insert(id, Entry::Done(ps));
                    }
                }
                WorkerMsg::Result(ids, Err(e)) => {
                    for id in ids {
                        self.entries.insert(id, Entry::Failed(e.clone()));
                    }
                }
            }
        }

        self.enqueue_missing();
    }

    /// One batched request for every lobby player we have no entry for yet.
    fn enqueue_missing(&mut self) {
        if self.sidecar.state != "ready" {
            return;
        }
        let ids: Vec<String> = self
            .players
            .iter()
            .filter(|p| is_lookupable(&p.primary_id) && !self.entries.contains_key(&p.primary_id))
            .map(|p| p.primary_id.clone())
            .collect();
        self.send_fetch(ids);
    }

    fn send_fetch(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            self.entries.insert(id.clone(), Entry::Loading);
        }
        let _ = self.req_tx.send(WorkerCmd::Fetch(ids));
    }
}

impl eframe::App for TrackerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        ctx.request_repaint_after(Duration::from_millis(100));

        let mut refetch_all = false;
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Playlist:");
                ui.selectable_value(&mut self.playlist, 10, "1v1");
                ui.selectable_value(&mut self.playlist, 11, "2v2");
                ui.selectable_value(&mut self.playlist, 12, "3v3");
                ui.separator();
                if ui.button("Refetch all").clicked() {
                    refetch_all = true;
                }
                ui.separator();
                ui.label(&self.status);
                ui.separator();
                match self.sidecar.state.as_str() {
                    "ready" => ui.label("Rank API: connected"),
                    "login" => ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 80),
                        "Rank API: login needed",
                    ),
                    "error" => ui.colored_label(egui::Color32::LIGHT_RED, "Rank API: error"),
                    _ => ui.label("Rank API: starting..."),
                };
            });
        });

        if refetch_all {
            let ids = self
                .players
                .iter()
                .filter(|p| is_lookupable(&p.primary_id))
                .map(|p| p.primary_id.clone())
                .collect();
            self.send_fetch(ids);
        }

        let mut refetch_one: Option<String> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.sidecar.state.as_str() {
                "login" => {
                    ui.heading("Epic login required (one time)");
                    ui.label(
                        "Rank lookups use an Epic account (an alt works). \
                         Open the link, sign in, and enter this code:",
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&self.sidecar.user_code)
                            .size(32.0)
                            .strong()
                            .monospace(),
                    );
                    if ui.button("Open Epic login page").clicked() {
                        let _ = webbrowser::open(&self.sidecar.verification_uri);
                    }
                    return;
                }
                "error" => {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        format!("Rank API failed: {}", self.sidecar.error),
                    );
                    ui.label("Restart the tracker to retry.");
                    return;
                }
                _ => {}
            }

            if self.players.is_empty() {
                ui.heading("Rocket League Tracker");
                ui.label("Waiting for a match...");
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
            let players = &self.players;
            let entries = &self.entries;
            let icons = &mut self.icons;

            for r in 0..rows {
                ui.columns(cols, |uis| {
                    for c in 0..cols {
                        let idx = r * cols + c;
                        if idx >= shown {
                            continue;
                        }
                        let p = &players[idx];
                        let entry = entries.get(&p.primary_id);
                        // Censored in-game name -> use the PsyNet profile name if we have it.
                        let name = match entry {
                            Some(Entry::Done(ps))
                                if is_filtered_name(&p.name) && !ps.name.is_empty() =>
                            {
                                ps.name.clone()
                            }
                            _ if is_filtered_name(&p.name) => "(censored name)".to_string(),
                            _ => p.name.clone(),
                        };
                        if draw_card(
                            &mut uis[c],
                            entry,
                            row_h,
                            icons,
                            &name,
                            playlist,
                            is_lookupable(&p.primary_id),
                        ) {
                            refetch_one = Some(p.primary_id.clone());
                        }
                    }
                });
            }
        });

        if let Some(id) = refetch_one {
            self.send_fetch(vec![id]);
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

    // PsyNet bridge process + the worker thread that talks to it
    let mut sidecar = match spawn_sidecar() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[sidecar] {e}");
            None
        }
    };
    let (req_tx, req_rx) = std_mpsc::channel::<WorkerCmd>();
    let (res_tx, res_rx) = std_mpsc::channel::<WorkerMsg>();
    if sidecar.is_none() {
        let _ = res_tx.send(WorkerMsg::Status(SidecarStatus {
            state: "error".into(),
            error: "rlsidecar.exe not found (build it: cd sidecar && go build -o rlsidecar.exe)"
                .into(),
            ..Default::default()
        }));
    } else {
        std::thread::spawn(move || run_worker(req_rx, res_tx));
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "RL Tracker",
        native_options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(TrackerApp::new(cc, rx, req_tx, res_rx)))
        }),
    );

    if let Some(c) = sidecar.as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
    result
}
