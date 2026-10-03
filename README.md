# RL Tracker

A lightweight Rocket League match tracker for Windows, written in Rust + egui.

The app connects to Rocket League's built-in Stats API (the same local TCP
stream the Overwolf app uses) and shows the current match roster. For each
player it can open their Tracker.gg profile in your browser, and (in progress)
fetch and chart their MMR history natively.

## Status

| Feature | State |
|---|---|
| Live roster from Stats API | ✅ Working |
| Deep-link to Tracker.gg profile | ✅ Working |
| Platform-aware URL construction | ✅ Working |
| UTF-8 / CJK player names | ✅ Working |
| Auto-configure `TAStatsAPI.ini` | ✅ Working |
| Native MMR chart | ⚠️ In progress — see `FORNEXTAGENT.md` |

The MMR chart feature is blocked on Cloudflare. Tracker.gg wraps every
profile page in a Turnstile challenge that headless browsers can't pass.
Running the fetch in a visible browser window works well enough to render
the chart, but reliably extracting the data from the page is unsolved.

## Requirements

- Windows 10 or newer (x64)
- Rocket League (Epic or Steam)
- Chrome or Edge installed
- Rust toolchain 1.85+ (`rustup update`)

