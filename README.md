# rinsefm

Stream [Rinse FM](https://www.rinse.fm/) in your terminal, with a live colour
spectrum visualizer and now-playing metadata.

```
 ~ RINSE FM ~ 106.8 ~ LIVE FROM LONDON ~
        ♪ Josey Rebelle - Sunday Morning

  ▂ █ █ ▅ ▃ ▂ ▁     ▂ ▄ █ ▆ ▃ ▁   ▁ ▂ ▁
  █ █ █ █ █ █ ▅ ▃ ▂ █ █ █ █ █ ▅ ▂ █ █ ▄
  █ █ █ █ █ █ █ █ █ █ █ █ █ █ █ █ █ █ █
                                 q: quit
```


## Install

```bash
brew install keenanjohnson/rinsefm/rinsefm
```

This pulls in `ffmpeg` (which provides `ffplay`) automatically. Then just run
`rinsefm`.

```bash
rinsefm                 # play Rinse FM UK
rinsefm --bars 48       # more spectrum bars
rinsefm --no-audio      # visualizer only
rinsefm --url http://…  # any other Icecast stream
```

Press `q` (or Ctrl-C) to quit.

## Build from source

Requires a Rust toolchain (https://rustup.rs) and `ffmpeg` + `ffplay` on your
PATH (`brew install ffmpeg` / `apt install ffmpeg`).

```bash
cargo build --release
./target/release/rinsefm
```

## How it works

One thread opens a raw HTTP/1.0 connection to the Icecast server with the
`Icy-MetaData: 1` header, so track titles arrive interleaved in the stream
itself. It strips that metadata out and tees the compressed audio bytes to two
child processes: `ffplay` for playback and `ffmpeg` for decoding to PCM. A
second thread chunks the PCM into a channel; the main thread runs an FFT
(Hann window, 2048 samples) mapped onto log-spaced frequency bands from
45 Hz to 16 kHz, and renders with [ratatui](https://ratatui.rs).

See [docs/DESIGN.md](docs/DESIGN.md) for architecture details and decision
records, and [CLAUDE.md](CLAUDE.md) for AI-assistant project context.

## Testing without hitting the real stream

```bash
# terminal 1: fake Icecast server on 127.0.0.1:8899 (needs python3 + ffmpeg)
cd tools && python3 fake_icecast.py

# terminal 2:
./target/release/rinsefm --url http://127.0.0.1:8899/stream
```

The fake server serves a generated AAC tone with rotating `StreamTitle`
metadata, exercising the full pipeline: ICY parsing, decode, FFT, UI.

## Status / roadmap

- [x] ICY streaming with metadata + auto-reconnect
- [x] Spectrum visualizer (gradient bars, peak dots, partial-block tips)
- [ ] `https://` stream support (needs `rustls` or `native-tls`)
- [ ] Pluggable decoders behind cargo features (`symphonia`, optional `fdk`)
- [ ] Volume control / mute keybindings
- [ ] Windows testing (should work — ratatui/crossterm are cross-platform —
      but untested)
- [x] Homebrew install (via `keenanjohnson/homebrew-rinsefm` tap)
