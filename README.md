# RL Tracker

RL Tracker is a small Windows app that looks up everyone in your Rocket League match while you play and shows you their ranked history on one screen. Queue up, and a few seconds later you can see each player's current rank, their peak, and a chart of how their MMR has moved over the season, without alt-tabbing to a browser and typing names into a search box.

It is written in Rust with egui for the interface.

## What it does

Rocket League has a built-in Stats API that streams match data over a local socket. RL Tracker connects to that socket, reads the list of players in the current match, and looks each of them up on Tracker Network's Rocket League site. For every player you get a card with their name, a rank icon, their current rank and division, their peak rank, and an MMR chart.

You pick a playlist at the top (1v1, 2v2 or 3v3) and the cards tile to fit. A 2v2 match is a 2x2 grid, 3v3 is three columns by two rows, and 1v1 is two cards side by side. Ranks are worked out from the MMR bands for every tier from Bronze I to Supersonic Legend, and each card's chart is colored by that player's current rank.

Charts all use the same size and a fixed window around the player's average, so you can compare players at a glance. If you would rather see everyone on the same absolute axis, there is a full scale toggle that draws every chart from 0 to 2500.

Each card has an Open button that jumps to that player's Tracker profile in your normal browser, and a Refetch button if you want fresh numbers. Results are remembered for the session, so a teammate you keep getting matched with is not looked up again.

## How the lookups work

Tracker Network sits behind Cloudflare, and plain HTTP requests or headless browsers get stopped at the door. So the app launches a real, visible Chrome or Edge window of its own, with its own profile, and drives it to each player's page. The MMR history is pulled out of the page data the site itself loads.

That browser window starts when the app starts and stays open while it runs. You can ignore it, but do not close it by hand. The app fetches one player at a time with a short pause between them, so a full lobby takes a little while to fill in. Images, fonts and media are blocked in that browser to make pages load faster. The chart data does not depend on any of that.

The browser keeps its own profile in `%LOCALAPPDATA%\RLTracker\browser_profile`. That is where the Cloudflare cookies live, so you should only have to solve a challenge once.

## Requirements

- Windows 10 or newer, 64-bit
- Rocket League on Epic or Steam
- Chrome or Edge installed
- Rust 1.85 or newer if you want to build it yourself

## Setup

On startup the app checks `TAStatsAPI.ini` in `Documents\My Games\Rocket League\TAGame\Config` and writes it if it is missing or the Stats API is switched off. If it had to change the file, restart Rocket League once so the game picks it up.

The first time you run it, Cloudflare may show a checkbox in the browser window. Click it once. When you are done, close RL Tracker with the normal window close button rather than killing the process. That lets the app ask its browser to shut down cleanly so the cookies are saved to disk. If a browser from a previous run is still open, the app will tell you in the top bar and wait for you to close it.

## Rank icons

Rank icons are loaded at runtime from a `rankicons` folder. The app looks for it next to the exe, in the working directory, and in the project folder. Files are matched by name without caring about case, in the form `Champion1_rank_icon.png`, `Grand_champion3_rank_icon.png` or `Supersonic_Legend_rank_icon.png`. Any rank that has no icon file gets `yousuck.png` instead, and if that is missing too, no icon is drawn. The images can be any size, they are scaled to fit, and the app reads the actual file contents, so an image with the wrong extension still loads.

If you give the exe to someone, send the `rankicons` folder along with it.

## Building

```
cargo run
```

for a debug build with a console, or

```
cargo build --release
```

for the real thing. Release builds hide the console window. The exe ends up in `target\release`.

There is a `DEBUG` constant at the top of `src/main.rs`. It is false by default, which means the app writes no extra files next to the exe. Set it to true if you want diagnostic dumps while working on the fetching code.

## Quirks worth knowing

Players whose names show up as all asterisks in the game are skipped completely. Tracker cannot look them up by name and they are rare enough that it is not worth special handling.

Peak rank is the highest point in the history the chart returns, which is not always the same as the season best shown on the Tracker profile. Bots and players on platforms the app cannot map to a Tracker URL show an error on their card instead of data.

If Cloudflare decides it does not trust the session, cards will say the challenge did not clear. Solve it in the browser window and press Refetch.

## Credits

Match data comes from Rocket League's own Stats API. Player history comes from Tracker Network. This is a hobby project and is not affiliated with or endorsed by Psyonix, Epic Games or Tracker Network.