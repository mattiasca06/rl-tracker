#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod store;

use eframe::egui;
use rlstatsapi::{ClientOptions, RocketLeagueStatsClient, StatsEvent};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use store::{fmt_ts, now_ts, playlist_for_size, MatchRecord, RecPlayer, Skill, Store};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A player in the current lobby (from the Stats API).
#[derive(Clone, Debug, Default)]
struct LobbyPlayer {
    name: String,
    primary_id: String,
    team: i64,
    score: i64,
    goals: i64,
    assists: i64,
    saves: i64,
    shots: i64,
}

/// Stats API -> UI.
enum StatsMsg {
    State {
        guid: String,
        players: Vec<LobbyPlayer>,
        /// (team_num, goals)
        scores: Vec<(i64, i64)>,
        /// Name of the player the camera is on (that's you while playing).
        target: Option<String>,
    },
    Ended {
        guid: String,
        winner: i64,
    },
    /// A new match is starting: forget the old lobby.
    NewMatch,
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
    Done(PlayerSkills, Instant),
    Failed(String, Instant),
}

/// Ranks older than this get re-fetched when the player shows up in a lobby.
const STALE_AFTER: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Live,
    History,
}

struct TrackerApp {
    rx: std_mpsc::Receiver<StatsMsg>,
    status: String,

    // Current lobby
    players: Vec<LobbyPlayer>,
    scores: Vec<(i64, i64)>,
    cur_guid: String,
    recorded: HashSet<String>,

    // Rank lookups, keyed by primary_id (one request returns every playlist)
    entries: HashMap<String, Entry>,
    inflight: HashSet<String>,
    req_tx: std_mpsc::Sender<WorkerCmd>,
    res_rx: std_mpsc::Receiver<WorkerMsg>,
    sidecar: SidecarStatus,

    store: Store,
    session_start: i64,
    page: Page,
    /// Past games open in their own window (by match guid).
    open_matches: Vec<String>,
    icons: RankIcons,
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

async fn run_stats_client(tx: std_mpsc::Sender<StatsMsg>) {
    let options = ClientOptions::default();

    let mut client = loop {
        match RocketLeagueStatsClient::connect(options.clone()).await {
            Ok(c) => break c,
            Err(e) => {
                eprintln!("Connection failed: {e}. Retrying in 3s...");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    };

    loop {
        match client.next_event().await {
            Ok(Some(event)) => match event {
                StatsEvent::UpdateState(data) => {
                    let players = data
                        .players
                        .iter()
                        .map(|p| LobbyPlayer {
                            name: p.name.clone().unwrap_or_else(|| "Unknown".into()),
                            primary_id: p.primary_id.clone().unwrap_or_default(),
                            team: p.team_num.unwrap_or(0),
                            score: p.score.unwrap_or(0),
                            goals: p.goals.unwrap_or(0),
                            assists: p.assists.unwrap_or(0),
                            saves: p.saves.unwrap_or(0),
                            shots: p.shots.unwrap_or(0),
                        })
                        .collect();
                    let scores = data
                        .game
                        .teams
                        .iter()
                        .map(|t| (t.team_num.unwrap_or(0), t.score.unwrap_or(0)))
                        .collect();
                    let target = match data.game.b_has_target {
                        Some(true) => data.game.target.map(|t| t.name),
                        _ => None,
                    };
                    let _ = tx.send(StatsMsg::State {
                        guid: data.match_guid.unwrap_or_default(),
                        players,
                        scores,
                        target,
                    });
                }
                StatsEvent::MatchEnded(d) => {
                    let _ = tx.send(StatsMsg::Ended {
                        guid: d.match_guid.unwrap_or_default(),
                        winner: d.winner_team_num,
                    });
                }
                StatsEvent::MatchCreated(_) | StatsEvent::MatchInitialized(_) => {
                    let _ = tx.send(StatsMsg::NewMatch);
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => {
                eprintln!("Event error: {e}. Reconnecting...");
                let _ = client.reconnect().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

const RANKED: [u32; 3] = [10, 11, 13];

fn playlist_name(p: u32) -> &'static str {
    match p {
        10 => "1v1",
        11 => "2v2",
        13 => "3v3",
        _ => "?",
    }
}

/// One playlist: icon, rank, MMR and games played.
fn rank_block(ui: &mut egui::Ui, icons: &mut RankIcons, skill: Option<&Skill>, pl: u32, highlight: bool) {
    ui.vertical(|ui| {
        let label = egui::RichText::new(playlist_name(pl)).strong();
        ui.label(if highlight {
            label.color(egui::Color32::from_rgb(255, 200, 80))
        } else {
            label.weak()
        });

        let Some(s) = skill.filter(|s| s.tier > 0 || s.matches > 0) else {
            ui.label(egui::RichText::new("No games").weak());
            return;
        };
        let rank = rank_from_tier(s.tier, s.division);
        let placing = s.placement_matches < 10 && s.matches == s.placement_matches;

        ui.horizontal(|ui| {
            if let Some(tex) = icons.texture_for(ui.ctx(), &rank.icon_key) {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    tex.id,
                    fit_size(tex.size, egui::vec2(44.0, 38.0)),
                )));
            }
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(&rank.text).color(rank.color).strong().size(15.0));
                let mmr = (s.mmr * 20.0 + 100.0).round() as i64;
                let extra = if placing {
                    format!("  (placements {}/10)", s.placement_matches)
                } else {
                    String::new()
                };
                ui.label(
                    egui::RichText::new(format!("{mmr} MMR  -  {} games{extra}", s.matches)).weak(),
                );
            });
        });
    });
}

fn ranks_row(ui: &mut egui::Ui, icons: &mut RankIcons, skills: &[Skill], current: u32) {
    ui.columns(3, |cols| {
        for (c, pl) in RANKED.iter().enumerate() {
            let s = skills.iter().find(|s| s.playlist == *pl);
            rank_block(&mut cols[c], icons, s, *pl, *pl == current);
        }
    });
}

#[derive(Clone, Copy)]
enum CardAction {
    Refetch,
    ThatsMe,
}

struct CardCtx<'a> {
    player: &'a LobbyPlayer,
    display_name: String,
    entry: Option<&'a Entry>,
    inflight: bool,
    is_me: bool,
    current_pl: u32,
    seen: &'a [store::Encounter],
}

fn draw_card(ui: &mut egui::Ui, icons: &mut RankIcons, c: CardCtx) -> Option<CardAction> {
    let mut action = None;
    let lookupable = is_lookupable(&c.player.primary_id);

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&c.display_name).strong().size(17.0));
            if c.is_me {
                ui.label(egui::RichText::new("(you)").weak());
            } else if lookupable && ui.small_button("That's me").clicked() {
                action = Some(CardAction::ThatsMe);
            }
            if lookupable
                && ui
                    .add_enabled(!c.inflight, egui::Button::new("Refetch").small())
                    .clicked()
            {
                action = Some(CardAction::Refetch);
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} pts  {}G {}A {}S",
                    c.player.score, c.player.goals, c.player.assists, c.player.saves
                ))
                .weak(),
            );
        });

        if !lookupable {
            ui.label(egui::RichText::new("No profile (bot or unknown platform)").weak());
            return;
        }

        match c.entry {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Fetching...");
                });
            }
            Some(Entry::Failed(e, _)) => {
                ui.colored_label(egui::Color32::LIGHT_RED, e);
            }
            Some(Entry::Done(ps, _)) => ranks_row(ui, icons, &ps.skills, c.current_pl),
        }

        if !c.is_me {
            draw_seen_before(ui, &c.player.primary_id, c.seen);
        }
    });

    action
}

/// "Played before" summary + the last 10 dates.
fn draw_seen_before(ui: &mut egui::Ui, id: &str, seen: &[store::Encounter]) {
    if seen.is_empty() {
        ui.label(egui::RichText::new("First time you've seen this player").weak());
        return;
    }
    let with = seen.iter().filter(|e| e.teammate == Some(true)).count();
    let against = seen.iter().filter(|e| e.teammate == Some(false)).count();
    let summary = format!(
        "Played before: {}x  ({with} with you, {against} against)  -  last {}",
        seen.len(),
        fmt_ts(seen[0].ts)
    );
    egui::CollapsingHeader::new(egui::RichText::new(summary).color(egui::Color32::from_rgb(120, 200, 255)))
        .id_salt(id)
        .show(ui, |ui| {
            for e in seen.iter().take(10) {
                let side = match e.teammate {
                    Some(true) => "with you",
                    Some(false) => "against you",
                    None => "-",
                };
                let res = match e.won {
                    Some(true) => "you won",
                    Some(false) => "you lost",
                    None => "",
                };
                ui.label(format!("{}   {side}   {res}", fmt_ts(e.ts)));
            }
            if seen.len() > 10 {
                ui.label(egui::RichText::new(format!("...and {} older", seen.len() - 10)).weak());
            }
        });
}

fn result_text(rec: &MatchRecord) -> egui::RichText {
    match rec.won() {
        Some(true) => egui::RichText::new("WIN").color(egui::Color32::from_rgb(90, 200, 120)).strong(),
        Some(false) => egui::RichText::new("LOSS").color(egui::Color32::from_rgb(235, 90, 90)).strong(),
        None => egui::RichText::new("?").weak(),
    }
}

/// "5 - 3" from your point of view (or blue - orange if we never knew your team).
fn score_text(rec: &MatchRecord) -> String {
    match rec.my_team {
        Some(t) => {
            let opp = rec.scores.iter().map(|(n, _)| *n).find(|n| *n != t).unwrap_or(1 - t);
            format!("{} - {}", rec.score_of(t), rec.score_of(opp))
        }
        None => format!("{} - {}", rec.score_of(0), rec.score_of(1)),
    }
}

/// Everything about one stored game (shown in its own window).
fn draw_match_detail(ui: &mut egui::Ui, icons: &mut RankIcons, rec: &MatchRecord) {
    ui.horizontal(|ui| {
        ui.heading(format!("{}  -  {}", fmt_ts(rec.ts), playlist_name(rec.playlist)));
        ui.label(result_text(rec).size(20.0));
        ui.heading(score_text(rec));
    });
    ui.separator();

    egui::ScrollArea::vertical().show(ui, |ui| {
        let mine = rec.my_team.unwrap_or(0);
        for (title, team_pred) in [("Your team", true), ("Opponents", false)] {
            let label = if rec.my_team.is_some() {
                title.to_string()
            } else if team_pred {
                "Blue".to_string()
            } else {
                "Orange".to_string()
            };
            ui.label(egui::RichText::new(label).strong().size(16.0));
            for p in rec.players.iter().filter(|p| (p.team == mine) == team_pred) {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&p.name).strong().size(16.0));
                        ui.label(
                            egui::RichText::new(format!(
                                "{} pts  {}G {}A {}S  {} shots",
                                p.score, p.goals, p.assists, p.saves, p.shots
                            ))
                            .weak(),
                        );
                    });
                    if p.skills.is_empty() {
                        ui.label(egui::RichText::new("No rank data was available").weak());
                    } else {
                        ranks_row(ui, icons, &p.skills, rec.playlist);
                    }
                });
            }
            ui.add_space(6.0);
        }
    });
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl TrackerApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        rx: std_mpsc::Receiver<StatsMsg>,
        req_tx: std_mpsc::Sender<WorkerCmd>,
        res_rx: std_mpsc::Receiver<WorkerMsg>,
    ) -> Self {
        Self {
            rx,
            status: "Waiting for Rocket League...".to_string(),
            players: Vec::new(),
            scores: Vec::new(),
            cur_guid: String::new(),
            recorded: HashSet::new(),
            entries: HashMap::new(),
            inflight: HashSet::new(),
            req_tx,
            res_rx,
            sidecar: SidecarStatus {
                state: "starting".into(),
                ..Default::default()
            },
            store: Store::load(),
            session_start: now_ts(),
            page: Page::Live,
            open_matches: Vec::new(),
            icons: RankIcons::new(),
        }
    }

    fn my_team(&self) -> Option<i64> {
        let me = self.store.my_id.as_deref()?;
        self.players.iter().find(|p| p.primary_id == me).map(|p| p.team)
    }

    fn poll_events(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                StatsMsg::State { guid, players, scores, target } => {
                    // First run: the player the camera is on while you play is you.
                    if self.store.my_id.is_none() {
                        if let Some(t) = target {
                            if let Some(p) = players
                                .iter()
                                .find(|p| p.name == t && is_lookupable(&p.primary_id))
                            {
                                self.store.set_me(&p.primary_id);
                            }
                        }
                    }
                    self.cur_guid = guid;
                    self.players = players;
                    self.scores = scores;
                    self.status = format!("Tracking {} players", self.players.len());
                }
                StatsMsg::Ended { guid, winner } => self.record_match(guid, winner),
                StatsMsg::NewMatch => {
                    self.players.clear();
                    self.scores.clear();
                    self.cur_guid.clear();
                    self.status = "Waiting for Rocket League...".to_string();
                }
            }
        }

        while let Ok(msg) = self.res_rx.try_recv() {
            match msg {
                WorkerMsg::Status(st) => self.sidecar = st,
                WorkerMsg::Result(ids, Ok(found)) => {
                    for id in ids {
                        self.inflight.remove(&id);
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
                        self.entries.insert(id, Entry::Done(ps, Instant::now()));
                    }
                }
                WorkerMsg::Result(ids, Err(e)) => {
                    for id in ids {
                        self.inflight.remove(&id);
                        self.entries.insert(id, Entry::Failed(e.clone(), Instant::now()));
                    }
                }
            }
        }

        self.enqueue_missing();
    }

    /// Store the finished game (once) with everyone's ranks as we knew them.
    fn record_match(&mut self, guid: String, winner: i64) {
        let guid = if !guid.is_empty() {
            guid
        } else if !self.cur_guid.is_empty() {
            self.cur_guid.clone()
        } else {
            format!("t{}", now_ts())
        };
        if self.players.len() < 2 || !self.recorded.insert(guid.clone()) {
            return;
        }
        let players = self
            .players
            .iter()
            .map(|p| RecPlayer {
                name: p.name.clone(),
                id: p.primary_id.clone(),
                team: p.team,
                score: p.score,
                goals: p.goals,
                assists: p.assists,
                saves: p.saves,
                shots: p.shots,
                skills: match self.entries.get(&p.primary_id) {
                    Some(Entry::Done(ps, _)) => ps.skills.clone(),
                    _ => Vec::new(),
                },
            })
            .collect();
        let rec = MatchRecord {
            guid,
            ts: now_ts(),
            playlist: playlist_for_size(self.players.len()),
            my_team: self.my_team(),
            winner,
            scores: self.scores.clone(),
            players,
        };
        self.store.push(rec);
    }

    /// One batched request for every lobby player that is new, stale, or failed a while ago.
    fn enqueue_missing(&mut self) {
        if self.sidecar.state != "ready" {
            return;
        }
        let ids: Vec<String> = self
            .players
            .iter()
            .filter(|p| is_lookupable(&p.primary_id) && !self.inflight.contains(&p.primary_id))
            .filter(|p| match self.entries.get(&p.primary_id) {
                None => true,
                Some(Entry::Done(_, at)) => at.elapsed() > STALE_AFTER,
                Some(Entry::Failed(_, at)) => at.elapsed() > Duration::from_secs(30),
            })
            .map(|p| p.primary_id.clone())
            .collect();
        self.send_fetch(ids);
    }

    fn send_fetch(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        self.inflight.extend(ids.iter().cloned());
        let _ = self.req_tx.send(WorkerCmd::Fetch(ids));
    }

    /// My games per playlist this season, from my own lookup.
    fn season_games(&self) -> Option<Vec<(u32, u32)>> {
        let me = self.store.my_id.as_deref()?;
        match self.entries.get(me) {
            Some(Entry::Done(ps, _)) => Some(
                RANKED
                    .iter()
                    .map(|pl| (*pl, ps.skills.iter().find(|s| s.playlist == *pl).map_or(0, |s| s.matches)))
                    .collect(),
            ),
            _ => None,
        }
    }

    fn live_page(&mut self, ui: &mut egui::Ui) -> Vec<(String, CardAction)> {
        let mut actions = Vec::new();

        if self.players.is_empty() {
            ui.heading("Rocket League Tracker");
            ui.label("Waiting for a match...");
            return actions;
        }

        let my_team = self.my_team();
        let left_team = my_team.unwrap_or(0);
        let current_pl = playlist_for_size(self.players.len());
        let score_of = |t: i64| self.scores.iter().find(|(n, _)| *n == t).map_or(0, |(_, s)| *s);
        let right_team = self.scores.iter().map(|(n, _)| *n).find(|n| *n != left_team).unwrap_or(1 - left_team);
        let (left_name, right_name) = if my_team.is_some() {
            ("Your team", "Opponents")
        } else if left_team == 0 {
            ("Blue", "Orange")
        } else {
            ("Orange", "Blue")
        };

        let groups = [
            (left_name, score_of(left_team), true),
            (right_name, score_of(right_team), false),
        ];

        let Self { players, entries, inflight, icons, store, cur_guid, .. } = self;
        let my_id = store.my_id.clone();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.columns(2, |cols| {
                for (i, (name, score, is_left)) in groups.iter().enumerate() {
                    let ui = &mut cols[i];
                    ui.label(egui::RichText::new(format!("{name}   {score}")).strong().size(18.0));
                    for p in players.iter().filter(|p| (p.team == left_team) == *is_left) {
                        let seen = store.encounters(&p.primary_id, cur_guid);
                        let entry = entries.get(&p.primary_id);
                        // Censored in-game name -> use the PsyNet profile name if we have it.
                        let display_name = match entry {
                            Some(Entry::Done(ps, _)) if is_filtered_name(&p.name) && !ps.name.is_empty() => {
                                ps.name.clone()
                            }
                            _ if is_filtered_name(&p.name) => "(censored name)".to_string(),
                            _ => p.name.clone(),
                        };
                        let ctx = CardCtx {
                            player: p,
                            display_name,
                            entry,
                            inflight: inflight.contains(&p.primary_id),
                            is_me: my_id.as_deref() == Some(p.primary_id.as_str()),
                            current_pl,
                            seen: &seen,
                        };
                        if let Some(a) = draw_card(ui, icons, ctx) {
                            actions.push((p.primary_id.clone(), a));
                        }
                    }
                }
            });
        });
        actions
    }

    fn history_page(&mut self, ui: &mut egui::Ui) {
        if self.store.matches.is_empty() {
            ui.heading("Game history");
            ui.label("No games recorded yet. Finished games show up here (last 20).");
            return;
        }
        ui.heading("Last 20 games  (click one for details)");
        ui.add_space(4.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for rec in self.store.recent(20) {
                let resp = egui::Frame::group(ui.style())
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(fmt_ts(rec.ts));
                            ui.label(egui::RichText::new(playlist_name(rec.playlist)).strong());
                            ui.label(result_text(rec));
                            ui.label(score_text(rec));
                            let others: Vec<&str> = rec
                                .players
                                .iter()
                                .filter(|p| rec.my_team.map_or(true, |t| p.team != t))
                                .map(|p| p.name.as_str())
                                .collect();
                            ui.label(egui::RichText::new(format!("vs {}", others.join(", "))).weak());
                        });
                    })
                    .response
                    .interact(egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() && !self.open_matches.contains(&rec.guid) {
                    self.open_matches.push(rec.guid.clone());
                }
            }
        });
    }

    /// Past games open in their own OS windows.
    fn show_match_windows(&mut self, ctx: &egui::Context) {
        let mut keep = Vec::new();
        for guid in std::mem::take(&mut self.open_matches) {
            let Some(rec) = self.store.by_guid(&guid).cloned() else {
                continue;
            };
            let mut close = false;
            let icons = &mut self.icons;
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of(("match", &guid)),
                egui::ViewportBuilder::default()
                    .with_title(format!("Game {}", fmt_ts(rec.ts)))
                    .with_inner_size([820.0, 620.0]),
                |ctx, class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        close = true;
                    }
                    if class == egui::ViewportClass::Embedded {
                        egui::Window::new("Game").show(ctx, |ui| draw_match_detail(ui, icons, &rec));
                    } else {
                        egui::CentralPanel::default().show(ctx, |ui| draw_match_detail(ui, icons, &rec));
                    }
                },
            );
            if !close {
                keep.push(guid);
            }
        }
        self.open_matches = keep;
    }
}

impl eframe::App for TrackerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        ctx.request_repaint_after(Duration::from_millis(100));

        let mut refetch_all = false;
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.page, Page::Live, "Live");
                ui.selectable_value(&mut self.page, Page::History, "History");
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

        // Session record + season games
        egui::TopBottomPanel::top("summary").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (w, l) = self.store.record_since(self.session_start);
                let games = self.store.games_since(self.session_start);
                ui.label(egui::RichText::new("Session").weak());
                ui.label(
                    egui::RichText::new(format!("{w}W - {l}L"))
                        .strong()
                        .size(16.0)
                        .color(if w >= l {
                            egui::Color32::from_rgb(90, 200, 120)
                        } else {
                            egui::Color32::from_rgb(235, 90, 90)
                        }),
                );
                ui.label(egui::RichText::new(format!("({games} games)")).weak());
                ui.separator();
                ui.label(egui::RichText::new("Season").weak());
                match self.season_games() {
                    Some(g) => {
                        let txt: Vec<String> =
                            g.iter().map(|(pl, n)| format!("{} {n}", playlist_name(*pl))).collect();
                        ui.label(txt.join("   "));
                    }
                    None => {
                        ui.label(
                            egui::RichText::new("(click \"That's me\" on your card)").weak(),
                        );
                    }
                }
            });
        });

        if refetch_all {
            let ids = self
                .players
                .iter()
                .filter(|p| is_lookupable(&p.primary_id) && !self.inflight.contains(&p.primary_id))
                .map(|p| p.primary_id.clone())
                .collect();
            self.send_fetch(ids);
        }

        let mut actions: Vec<(String, CardAction)> = Vec::new();

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

            match self.page {
                Page::Live => actions = self.live_page(ui),
                Page::History => self.history_page(ui),
            }
        });

        for (id, a) in actions {
            match a {
                CardAction::Refetch => self.send_fetch(vec![id]),
                CardAction::ThatsMe => self.store.set_me(&id),
            }
        }

        self.show_match_windows(ctx);
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
    let (tx, rx) = std_mpsc::channel::<StatsMsg>();
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
        viewport: egui::ViewportBuilder::default().with_inner_size([1250.0, 800.0]),
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
