#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use eframe::egui;
use rlstatsapi::{ClientOptions, RocketLeagueStatsClient, StatsEvent};
use std::sync::mpsc as std_mpsc;

#[derive(Clone, Debug)]
struct PlayerInfo {
    name: String,
    primary_id: String,
}

struct TrackerApp {
    players: Vec<PlayerInfo>,
    rx: std_mpsc::Receiver<Vec<PlayerInfo>>,
    status: String,
}

impl TrackerApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        rx: std_mpsc::Receiver<Vec<PlayerInfo>>,
    ) -> Self {
        Self {
            players: Vec::new(),
            rx,
            status: "Waiting for Rocket League...".to_string(),
        }
    }

    fn poll_events(&mut self) {
        while let Ok(players) = self.rx.try_recv() {
            self.players = players;
            self.status = format!("Tracking {} players", self.players.len());
        }
    }
}

/// Decide the Tracker.gg slug + identifier for a player.
///
/// - Epic uses the display name (PrimaryId is an opaque account hash).
/// - Steam uses the numeric Steam64 ID from PrimaryId.
/// - PSN / Xbox / Switch: we use the middle part of PrimaryId, falling
///   back to the display name if it's missing.
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
        // Steam is the only platform where we need the numeric ID.
        // PrimaryId format: "Steam|<steam64id>|0"
        "Steam" => {
            if mid.is_empty() || mid == "0" {
                return None;
            }
            mid.to_string()
        }
        // Everyone else: use the display name.
        // Tracker.gg indexes Epic by display name, PSN by PSN ID,
        // Xbox by gamertag, Switch by Nintendo name.
        _ => {
            if name.trim().is_empty() {
                return None;
            }
            name.to_string()
        }
    };

    Some((slug, identifier))
}

fn open_tracker(name: &str, primary_id: &str) {
    if let Some((slug, id)) = parse_player(name, primary_id) {
        let encoded = urlencoding::encode(&id);
        let url = format!(
            "https://rocketleague.tracker.network/rocket-league/profile/{}/{}/overview",
            slug, encoded
        );
        if let Err(e) = webbrowser::open(&url) {
            eprintln!("Failed to open browser: {e}");
        }
    }
}

/// Load a CJK-capable system font so names in Japanese/Chinese/Korean
/// don't render as empty boxes.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Order matters: the first file that exists wins. We only need one
    // CJK font — they all cover Latin too.
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",    // Microsoft YaHei (Simplified Chinese)
        r"C:\Windows\Fonts\msjh.ttc",    // Microsoft JhengHei (Traditional Chinese)
        r"C:\Windows\Fonts\meiryo.ttc",  // Meiryo (Japanese)
        r"C:\Windows\Fonts\malgun.ttf",  // Malgun Gothic (Korean)
        r"C:\Windows\Fonts\msgothic.ttc",// MS Gothic (Japanese, older Windows)
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

impl eframe::App for TrackerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Rocket League Tracker");
            ui.label(&self.status);
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for player in &self.players {
                    ui.horizontal(|ui| {
                        ui.label(&player.name);
                        if ui.button("Open Tracker").clicked() {
                            open_tracker(&player.name, &player.primary_id);
                        }
                    });
                }
            });
        });
    }
}

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

fn main() -> eframe::Result<()> {
    let (tx, rx) = std_mpsc::channel::<Vec<PlayerInfo>>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        rt.block_on(run_stats_client(tx));
    });

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "RL Tracker",
        native_options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(TrackerApp::new(cc, rx)))
        }),
    )
}