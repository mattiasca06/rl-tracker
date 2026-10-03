
### `FORNEXTAGENT.md`

```markdown
# Handoff notes for the next agent

State of the repo as of the last commit on `v2-mmr-chart`.

## TL;DR

v1 is solid. v2 (native MMR chart) is 90% there and blocked on one specific
thing: reliably extracting the MMR data from Tracker.gg's `__INITIAL_STATE__`
inside a CDP-driven Chrome. Everything upstream of that works.

## What works

- `rlstatsapi` client connects to `127.0.0.1:49123`, parses `UpdateState`,
  and streams the player list to egui via `std::sync::mpsc`.
- `parse_player()` correctly maps platform → Tracker.gg slug and identifier
  (see README for the Epic/Steam distinction).
- `open_tracker()` opens the correct URL in the default browser.
- CJK font loading works on every Windows install tested.
- `ensure_ini_configured()` writes `TAStatsAPI.ini` when missing or disabled.
- `stygian-browser` in **headed mode** with a **persistent user data dir**
  successfully navigates past Cloudflare Turnstile. Console confirms:
  - `Current URL: https://rocketleague.tracker.network/rocket-league/profile/...`
  - `Page title: <player>'s Rocket League Stats - Rocket League Tracker`
- The chart renders correctly in the browser window. Data is present.

## What doesn't work

The JS eval that reads `window.__INITIAL_STATE__.stats.segments` never
returns usable data. Observed behaviors:

| Attempt | Result |
|---|---|
| Original (60s poll inside JS) | `Timeout after 30000ms during 'page.evaluate'` — stygian caps eval at 30s |
| One-shot eval, poll from Rust (20 × 2s) | Either `ERR:no-state`, `ERR:no-segments`, or empty string |

The one-shot version was never run against a fully-populated page (the user
reset the session before testing). The failure mode where the chart is
visible but `__INITIAL_STATE__.stats.segments` is empty or missing suggests
one of:

1. **Timing**: `__INITIAL_STATE__` populates *after* the chart renders. We
   may be reading too early or too late.
2. **Shape change**: The stats we saw in a previous dump had keys
   `standardProfiles, standardProfileMatches, standardProfileSummaries,
   standardProfilesHistory, statsOverviews, standardSessions, subscriptions,
   segments, standardTitles, standardLeaderboards,
   standardLeaderboardLeaders`. The `segments` array may be empty until a
   specific API call completes, or the MMR history may live in
   `standardProfilesHistory` instead.
3. **CDP detach on eval**: The "small square" quirk — the automation session
   uses a tiny viewport and Chrome reverts to the real size only after
   `browser.close()` — suggests the CDP session has unusual lifecycle
   behavior that may interfere with long-running evals.

## Things to try next, in order

1. **Dump everything once and inspect.** Replace the eval with a full dump:
   ```rust
   let dump: String = page.eval("JSON.stringify(window.__INITIAL_STATE__ || null)").await?;
   std::fs::write("initial_state_full.json", dump)?;