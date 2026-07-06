# CLAUDE.md

Project context for AI-assisted development. Read this before making changes.

## What this is

`rinse` — a Rust CLI that streams Rinse FM (or any Icecast/Shoutcast URL),
plays the audio, and renders a live terminal spectrum visualizer with
now-playing metadata. Single binary, five crate deps (`ratatui`,
`crossterm`, `rustfft`, `native-tls`, `serde_json`), external runtime
dependency on ffmpeg/ffplay.

## Commands

```bash
cargo build --release          # build
cargo clippy                   # lint (not yet clean — see Known debt)
cargo fmt                      # format

# End-to-end test without network access to the real stream:
python3 tools/fake_icecast.py &          # serves 127.0.0.1:8899
./target/release/rinse-rs --url http://127.0.0.1:8899/stream --no-audio
```

There are no unit tests yet. The fake Icecast server is the primary test
harness — it exercises ICY header parsing, metaint metadata extraction,
decode, FFT, and rendering. `tools/fake_icecast.py` requires python3 and
ffmpeg (it generates its own test AAC on first run if test.aac is missing —
see file header).

## Architecture (all in src/main.rs, ~450 lines)

```
[icy_thread] ──raw AAC──> ffplay stdin        (audio playback)
             ──raw AAC──> ffmpeg stdin ──s16le PCM──> [pcm_thread]
                                                          │
                                          sync_channel<Vec<i16>> (bounded 64)
                                                          ▼
[main thread]  ratatui draw loop <── Spectrum (FFT + smoothing)
```

- `icy_thread` / `stream_once`: raw HTTP/1.0 over `TcpStream`. Deliberately
  NOT an HTTP library: ICY servers can reply `ICY 200 OK` which strict
  clients reject. Parses `icy-metaint`, strips interleaved metadata blocks,
  extracts `StreamTitle='…';`. Reconnects forever on error (2s backoff).
- Fanout writes to both child stdins via `buffered_sink` (a per-sink writer
  thread + unbounded queue). Errors ignored per-sink so one dead child
  doesn't kill the other. The buffering is load-bearing: without it, ffplay
  only pulls its pipe at real-time playback pace, back-pressuring the shared
  fanout in bursts that starve ffmpeg between refills — the visualizer then
  spikes every few seconds instead of tracking the audio.
- `pcm_thread`: reads exact 2048-frame chunks and sends ALL of them in order
  (never drops) — the main loop is a realtime-paced delay line that keeps the
  viz in sync (see below).
- Viz/audio sync: ffmpeg decodes Icecast's burst-on-connect far ahead of
  realtime while ffplay plays it out at realtime, so drawing the freshest
  decoded PCM would run the visualizer seconds AHEAD of the audio. The main
  loop instead buffers decoded chunks in a `VecDeque` and releases them to the
  FFT at realtime pace (a wall-clock sample `budget`); the backlog settles to
  the same depth ffplay is behind, so the viz tracks what you hear. Audio stays
  fully decoupled (ffplay pulls AAC at its own pace) so playback never glitches
  — the delay line only governs the visualizer. Frames with no new chunk hold
  the last spectrum; an empty backlog (real stream gap) feeds silence so bars
  decay. `budget` resets to 0 on underrun to resync instead of catching up.
- `Spectrum`: 2048-pt FFT @ 44.1kHz mono, Hann window, log-spaced bands
  45 Hz–16 kHz. Smoothing: attack `0.3*old + 0.7*new`, decay
  `0.82*old + 0.18*new`, peak dots fall 0.02/frame. The `ln_1p` scaling and
  1.0→1.6 high-band tilt are cosmetic tuning — change freely by eye.
- UI: manual `Line`/`Span` rendering (not ratatui's BarChart — it can't do
  per-row gradient colors or partial-block bar tips). 256-color ramp in
  `RAMP`, falls back implicitly via terminal. Bands recompute when the
  terminal is too narrow (`set_bands` on resize).
- Shared state (`show`/`title`/`station`/`status`) via `Arc<Mutex<StreamState>>`.
- `now_playing_thread`: Rinse's audio stream only sends a placeholder
  `StreamTitle`, so for Rinse URLs we poll their schedule API
  (`www.rinse.fm/api/query/v1/schedule`) every 60s and publish the on-air
  *show* name (`show`, preferred over the ICY `title` in the header). The
  stream mount maps to a channel slug (`rinse_uk`→`uk`); non-Rinse URLs skip
  the poller and just show ICY metadata. Episode air time = `episodeDate`
  (broadcast day, London-midnight in UTC) + `episodeTime`'s time-of-day.
- Exit uses `std::process::exit(0)` because icy_thread may be blocked in a
  socket read; children are killed first. If refactoring shutdown, keep this
  in mind or add socket timeouts + joins.

## Decision records

1. **Decoding: shell out to ffmpeg** (vs Symphonia vs fdk-aac bindings).
   Rinse's stream is aacPlus (HE-AAC = AAC-LC + SBR). Symphonia only decodes
   the LC core (duller highs, dead upper visualizer bands); fdk-aac decodes
   HE-AAC fully but its license is GPL-incompatible and distro-unfriendly
   (Debian non-free). Plan: keep ffmpeg-pipe as default, add optional cargo
   features `symphonia` and `fdk` later so users choose their tradeoff.
2. **https via native-tls**: the reliably-live public Rinse mount is HTTPS
   (`https://admin.stream.rinse.fm/proxy/rinse_uk/stream`, the default URL),
   so `connect()` wraps the `TcpStream` in `native_tls` for `https://` URLs.
   native-tls uses the OS trust store (Security.framework on macOS, OpenSSL
   on Linux). The old plain-http admin mount (`:8820/stream`) is a silent
   failover feed. Both http and https URLs still work via `--url`.
3. **No CLI-parsing crate**: hand-rolled `parse_args` to keep deps minimal.
   If flags grow past ~5, switch to `clap` with `derive`.

## Known debt / gotchas

- Built and tested on rustc 1.75 in the original dev sandbox, which required
  pinning `instability` and `unicode-segmentation` (ratatui 0.29 transitive
  deps want 1.85+). **Cargo.lock was intentionally not committed** — on a
  current stable toolchain, fresh resolution works with no pins. Commit a
  fresh Cargo.lock from your machine.
- `cargo clippy` has not been run clean yet.
- Stream URL is a hardcoded const (`RINSE_URL`). Fine for now.
- The metadata regex-free parser assumes `StreamTitle='…';` — titles
  containing `';` will truncate. Icecast escapes rarely; low priority.
- Tested only on Linux. macOS should just work. Windows: ratatui/crossterm
  are compatible but ffplay pipe behavior is unverified.
- When testing under `script`(1) for headless pty runs, set a size first
  (`stty rows 30 cols 110`) — a 0×0 pty makes draw_frame return early and
  nothing renders.

## Style

- Single-file until it hurts. Likely first split: `icy.rs`, `spectrum.rs`,
  `ui.rs`.
- Prefer std over new dependencies unless there's a clear win.
- Visualizer tuning constants are feel-based; when changing them, verify
  against the fake server (steady tones) AND real music.
