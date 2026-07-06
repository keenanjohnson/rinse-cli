# Design notes

Deeper background than CLAUDE.md — protocol details, DSP reasoning, and the
full decoder decision analysis. Written during initial development
(July 2026).

## The ICY protocol, briefly

Icecast/Shoutcast servers speak almost-HTTP. The relevant quirks:

- Request `Icy-MetaData: 1` and the server includes `icy-metaint: N` in its
  response headers. The body is then: N bytes of audio, 1 length byte L,
  L×16 bytes of metadata, N bytes of audio, … repeating. L is usually 0
  (empty block) except when the title changes.
- Metadata blocks look like `StreamTitle='Artist - Track';StreamUrl='';`
  padded with NULs to the 16-byte boundary.
- Some servers reply with a status line of `ICY 200 OK` instead of
  `HTTP/1.0 200 OK`, which strict HTTP clients reject. This is why we speak
  raw TCP rather than using an HTTP library.
- Rinse FM UK stream (verified July 2026): `http://admin.stream.rinse.fm:8820/stream`,
  aacPlus (HE-AAC). If this URL rots, check https://www.rinse.fm player
  network traffic or a stream directory (fmstream.org, radio-browser.info).

## Single-connection tee

Naive designs open two connections (one to play, one to analyze) or fetch
metadata separately (doubling bandwidth and risking title desync). Instead we
open ONE connection, strip metadata ourselves, and tee the compressed bytes
to both `ffplay` (playback) and `ffmpeg -f s16le -ac 1 -ar 44100` (analysis
PCM). AAC is self-synchronizing, so joining mid-stream is fine — decoders
lock onto the next frame boundary.

Playback and visualizer are decoded independently, so they can drift by a
buffer's worth (~hundreds of ms). In practice it looks synced; if it ever
matters, the fix is decoding once and playing the PCM ourselves (rodio),
at the cost of owning audio-output problems.

## DSP choices

- **2048-sample window @ 44.1 kHz** ≈ 46 ms ≈ 21.5 Hz/bin. Enough low-end
  resolution to separate sub-bass bands (this is Rinse; the sub matters)
  while staying responsive.
- **Log-spaced bands 45 Hz–16 kHz**: matches pitch perception; 16 kHz ceiling
  because aacPlus SBR reconstruction above that is mostly noise anyway.
- **Hann window** to reduce spectral leakage smearing bass energy across bands.
- **Asymmetric smoothing** (fast attack / slow decay) is what makes bars feel
  musical — punch up on transients, fall gracefully. Values were tuned by
  eye against 4/4 material.
- **`ln_1p` amplitude scaling + high-band tilt (1.0→1.6)**: perceptual-ish
  loudness compression so quiet hi-hats still register. Cosmetic; tune freely.

## Decoder decision record (expanded)

The stream is HE-AAC (aacPlus): an AAC-LC core plus Spectral Band
Replication, which reconstructs the top octaves from side-channel data.

| Option | HE-AAC support | Licensing | Packaging |
|---|---|---|---|
| ffmpeg subprocess | full | clean (external tool) | user installs ffmpeg |
| Symphonia (pure Rust) | LC core only — duller highs | MIT/MPL, clean | perfect |
| fdk-aac bindings | full | GPL-incompatible, no patent grant | Debian non-free etc. |

Chose ffmpeg subprocess: full quality, zero licensing questions in this
repo, and ffmpeg is a one-line install everywhere. The planned end state is
cargo features (`ffmpeg-pipe` default, `symphonia`, `fdk` opt-in) so the
choice belongs to whoever builds the binary. Note: the fdk licensing concern
is about GPL compatibility and distro policy, not about whether an
open-source project may use it — the FDK license permits redistribution.
(Not legal advice; recheck before shipping an `fdk` feature.)

## Test harness

`tools/fake_icecast.py` simulates an Icecast server: generates a two-tone
AAC file with ffmpeg (180 Hz + 4 kHz — lights up opposite ends of the
spectrum), serves it with `icy-metaint: 8192` and rotating `StreamTitle`
values. This exercises every layer except the real network and real music.

Headless UI testing (CI-ish) works with `script`(1) providing a pty:

```bash
timeout 8 script -qec "stty rows 30 cols 110; TERM=xterm-256color \
  ./target/release/rinse-rs --url http://127.0.0.1:8899/stream --no-audio" /dev/null > cap.txt
grep -c "█" cap.txt              # bars rendered?
grep -o "Test Show - Live" cap.txt | sort -u   # metadata parsed?
```

Remember the `stty` — a 0×0 pty renders nothing (see CLAUDE.md gotchas).

## Python reference implementation

`reference/rinse.py` is the original prototype: identical architecture
(socket ICY reader → tee to ffplay/ffmpeg → numpy FFT → curses). Useful for
A/B-ing behavior ("did the Rust port change the feel of the smoothing?") and
for quick experiments where Python iteration speed wins.
