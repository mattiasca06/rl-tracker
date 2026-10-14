//! Persistent match history ("who have I played before", last games, session record).

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};

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
    /// Winning team; only meaningful when `ended`.
    pub winner: i64,
    /// false = autosaved while in progress; stays false if you left before the end.
    #[serde(default = "default_true")]
    pub ended: bool,
    /// (team_num, goals)
    pub scores: Vec<(i64, i64)>,
    pub players: Vec<RecPlayer>,
}

impl MatchRecord {
    /// Some(true) = you won, Some(false) = you lost, None = unknown.
    pub fn won(&self) -> Option<bool> {
        if !self.ended {
            return None;
        }
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
    /// Your own accounts (primary ids like "Steam|765...|0"): found by elimination or via "That's me".
    #[serde(default)]
    pub my_ids: Vec<String>,
    /// Players present in every recent game while we still don't know who you are.
    #[serde(default)]
    pub candidates: Vec<String>,
    #[serde(default)]
    pub matches: Vec<MatchRecord>,
}

fn path() -> Option<PathBuf> {
    // Never touch the real history from unit tests.
    if cfg!(test) {
        return None;
    }
    Some(dirs::data_local_dir()?.join("RLTracker").join("history.json"))
}

static SEQ: AtomicU64 = AtomicU64::new(0);
/// Sequence number of the newest snapshot on disk, so an old queued save can't overwrite a newer one.
static WRITTEN: Mutex<u64> = Mutex::new(0);

/// Atomic write: write a temp file, flush it to disk, then rename over the real file.
/// A crash mid-write leaves the previous history.json intact.
fn write_atomic(seq: u64, json: &str) {
    let Some(p) = path() else { return };
    let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if seq < *written {
        return;
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    let ok = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &p)
    })();
    match ok {
        Ok(()) => *written = seq,
        Err(e) => eprintln!("[store] save failed: {e}"),
    }
}

fn writer() -> &'static mpsc::Sender<(u64, String)> {
    static W: OnceLock<mpsc::Sender<(u64, String)>> = OnceLock::new();
    W.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(u64, String)>();
        std::thread::spawn(move || {
            while let Ok(mut latest) = rx.recv() {
                // Only the newest queued snapshot matters.
                while let Ok(newer) = rx.try_recv() {
                    latest = newer;
                }
                write_atomic(latest.0, &latest.1);
            }
        });
        tx
    })
}

impl Store {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Queue a save on the background writer thread (never blocks the UI).
    pub fn save(&self) {
        if path().is_none() {
            return;
        }
        let Ok(json) = serde_json::to_string(self) else { return };
        let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = writer().send((seq, json));
    }

    /// Write right now (used on exit so nothing is lost).
    pub fn save_blocking(&self) {
        let Ok(json) = serde_json::to_string(self) else { return };
        let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
        write_atomic(seq, &json);
    }

    /// Adds a match, or replaces the stored copy of the same game (used for the
    /// once-a-second autosave of the game in progress). Returns true if it was new.
    pub fn upsert(&mut self, rec: MatchRecord) -> bool {
        let is_new = match self.matches.iter_mut().find(|m| m.guid == rec.guid) {
            Some(existing) => {
                *existing = rec;
                false
            }
            None => {
                self.matches.push(rec);
                true
            }
        };
        if self.matches.len() > MAX_MATCHES {
            let extra = self.matches.len() - MAX_MATCHES;
            self.matches.drain(..extra);
        }
        self.save();
        is_new
    }

    pub fn is_me(&self, id: &str) -> bool {
        self.my_ids.iter().any(|m| m == id)
    }

    /// Mark an account as yours (you can have several) and fix up old games.
    pub fn add_me(&mut self, id: &str) {
        if !self.is_me(id) {
            self.my_ids.push(id.to_string());
        }
        self.candidates.clear();
        self.backfill_teams();
        self.save();
    }

    pub fn remove_me(&mut self, id: &str) {
        self.my_ids.retain(|m| m != id);
        self.save();
    }

    /// Elimination: after each finished game keep only the players who were in every game
    /// since we last knew who you were. When one is left, that's you. If nobody is left
    /// (you switched account), start over from this game.
    pub fn narrow_candidates(&mut self, ids: &[String]) {
        if ids.iter().any(|i| self.is_me(i)) {
            // One of your known accounts is here, nothing to work out.
            self.candidates.clear();
            self.save();
            return;
        }
        let inter: Vec<String> = self.candidates.iter().filter(|c| ids.contains(c)).cloned().collect();
        self.candidates = if self.candidates.is_empty() || inter.is_empty() {
            ids.to_vec()
        } else {
            inter
        };
        if self.candidates.len() == 1 {
            let me = self.candidates[0].clone();
            self.add_me(&me);
        } else {
            self.save();
        }
    }

    /// Games stored before we knew who you were get their team (and so a W/L) filled in.
    fn backfill_teams(&mut self) {
        let mine = self.my_ids.clone();
        for m in self.matches.iter_mut().filter(|m| m.my_team.is_none()) {
            m.my_team = m.players.iter().find(|p| mine.contains(&p.id)).map(|p| p.team);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // narrow_candidates saves to disk via add_me; tests only touch the in-memory state.
    #[test]
    fn finds_you_by_elimination() {
        let mut s = Store::default();
        s.narrow_candidates(&ids(&["me", "a", "b", "c"]));
        assert!(s.my_ids.is_empty());
        s.narrow_candidates(&ids(&["me", "a", "x", "y"]));
        assert!(s.my_ids.is_empty());
        s.narrow_candidates(&ids(&["me", "p", "q", "r"]));
        assert_eq!(s.my_ids, ids(&["me"]));
        assert!(s.candidates.is_empty());
    }

    #[test]
    fn second_account_is_found_separately() {
        let mut s = Store::default();
        s.my_ids = ids(&["main"]);
        // Known account present: nothing to work out.
        s.narrow_candidates(&ids(&["main", "a"]));
        assert!(s.candidates.is_empty());
        // Alt account games: main absent, so elimination starts over and finds the alt.
        s.narrow_candidates(&ids(&["alt", "a", "b"]));
        s.narrow_candidates(&ids(&["alt", "c", "d"]));
        assert_eq!(s.my_ids, ids(&["main", "alt"]));
    }
}

fn default_true() -> bool {
    true
}
