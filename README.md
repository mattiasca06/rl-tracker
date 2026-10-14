# RL Tracker

A small Windows app that shows everyone in your Rocket League match, with their current ranked rank and MMR for 1v1, 2v2 and 3v3, on one screen.

## How it works

- Rocket League's built-in Stats API streams the players in your match over a local socket. RL Tracker reads that list (and sets up `TAStatsAPI.ini` for you; restart the game once if it changed).
- Ranks come straight from Rocket League's own PsyNet service through [dank/rlapi](https://github.com/dank/rlapi). Because rlapi is a Go library, a tiny helper (`sidecar/`, built to `rlsidecar.exe`) does the talking. The app starts it on launch, stops it on exit, and asks it for the whole lobby in a single request.
- There is no scraping and no browser. Only current MMR is shown (no history).

## First run: Epic login

PsyNet needs an Epic account. The first time, the app shows a code and a button that opens the Epic login page. Enter the code and sign in (an alt account is fine). The refresh token is saved in `%APPDATA%\RLTracker\eos_refresh_token`, so you should not be asked again.

## Build

```
cd sidecar
go build -o rlsidecar.exe .
cd ..
cargo build --release
```

`rlsidecar.exe` is looked up next to the app exe, in a `sidecar` folder next to it, or in `./sidecar`. Keep the `rankicons` folder next to the exe as well.

## Notes

- Rank is taken from the game's own tier/division values, MMR from PsyNet's skill value.
- Bots and players on unknown platforms show "No profile".
- Players with censored names show the PsyNet profile name when available.
- Match data and ranks belong to Psyonix/Epic. This is a hobby project and is not affiliated with or endorsed by them. rlapi is unofficial and can break when the game updates.
