//! Persistent match history ("who have I played before", last games, session record).

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Most matches kept on disk (the history page shows the newest 20).
const MAX_MATCHES: usize = 500;

/// One playlist's skill, as returned by the rlsidecar (PsyNet) bridge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    pub playlist: u32,
    pub mmr: f64,
    pub tier: u32,
    pub division: u32,
    pub matches: u32,
    pub placement_matches: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecPlayer {
    pub name: String,
    pub id: String,
    pub team: i64,
    pub score: i64,
    pub goals: i64,
    pub assists: i64,
    pub saves: i64,
    pub shots: i64,
    /// Ranks at the time of the game (empty if they were never fetched).
    #[serde(default)]
    pub skills: Vec<Skill>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchRecord {
    pub guid: String,
    /// Unix seconds when the match ended.
    pub ts: i64,
    /// 10 / 11 / 13 by lobby size, 0 if unknown.
    pub playlist: u32,
    /// Your team number, if we knew who you were.
    pub my_team: Option<i64>,
    pub winner: i64,
    /// (team_num, goals)
    pub scores: Vec<(i64, i64)>,
    pub players: Vec<RecPlayer>,
}

impl MatchRecord {
    /// Some(true) = you won, Some(false) = you lost, None = unknown.
    pub fn won(&self) -> Option<bool> {
        self.my_team.map(|t| t == self.winner)
    }

    pub fn score_of(&self, team: i64) -> i64 {
        self.scores.iter().find(|(t, _)| *t == team).map_or(0, |(_, s)| *s)
    }
}

/// One earlier meeting with a player.
pub struct Encounter {
    pub ts: i64,
    /// true = same team as you, false = opposing, None = unknown.
    pub teammate: Option<bool>,
    pub won: Option<bool>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    /// Your own primary id ("Steam|765...|0"), set automatically or via "That's me".
    #[serde(default)]
    pub my_id: Option<String>,
    #[serde(default)]
    pub matches: Vec<MatchRecord>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("RLTracker").join("history.json"))
}

impl Store {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("json.tmp");
        let Ok(json) = serde_json::to_string(self) else { return };
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    /// Adds a finished match (ignored if that guid is already stored).
    pub fn push(&mut self, rec: MatchRecord) {
        if self.matches.iter().any(|m| m.guid == rec.guid) {
            return;
        }
        self.matches.push(rec);
        if self.matches.len() > MAX_MATCHES {
            let extra = self.matches.len() - MAX_MATCHES;
            self.matches.drain(..extra);
        }
        self.save();
    }

    pub fn set_me(&mut self, id: &str) {
        if self.my_id.as_deref() != Some(id) {
            self.my_id = Some(id.to_string());
            self.save();
        }
    }

    /// Newest first.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &MatchRecord> {
        self.matches.iter().rev().take(n)
    }

    pub fn by_guid(&self, guid: &str) -> Option<&MatchRecord> {
        self.matches.iter().find(|m| m.guid == guid)
    }

    /// (wins, losses) for matches that ended at or after `since`.
    pub fn record_since(&self, since: i64) -> (u32, u32) {
        let mut w = 0;
        let mut l = 0;
        for m in self.matches.iter().filter(|m| m.ts >= since) {
            match m.won() {
                Some(true) => w += 1,
                Some(false) => l += 1,
                None => {}
            }
        }
        (w, l)
    }

    pub fn games_since(&self, since: i64) -> usize {
        self.matches.iter().filter(|m| m.ts >= since).count()
    }

    /// Earlier matches (newest first) that `id` was in, skipping the match `skip_guid`.
    pub fn encounters(&self, id: &str, skip_guid: &str) -> Vec<Encounter> {
        self.matches
            .iter()
            .rev()
            .filter(|m| m.guid != skip_guid)
            .filter_map(|m| {
                let p = m.players.iter().find(|p| p.id == id)?;
                Some(Encounter {
                    ts: m.ts,
                    teammate: m.my_team.map(|t| t == p.team),
                    won: m.won(),
                })
            })
            .collect()
    }
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "14 Oct 21:04" in local time.
pub fn fmt_ts(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%d %b %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

pub fn playlist_for_size(players: usize) -> u32 {
    match players {
        2 => 10,
        4 => 11,
        6 => 13,
        _ => 0,
    }
}
