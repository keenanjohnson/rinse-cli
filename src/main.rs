//! rinse — stream Rinse FM in your terminal, with a live spectrum visualizer.
//!
//! Architecture:
//!   [IcyStream thread] --raw AAC--> ffplay stdin (audio out)
//!                      --raw AAC--> ffmpeg stdin --s16le PCM--> [PcmReader thread]
//!                                                                    |
//!                                              mpsc channel of PCM chunks
//!                                                                    v
//!   [main thread] ratatui draw loop <-- Spectrum (FFT + smoothing)

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use rustfft::{num_complex::Complex, FftPlanner};

const RINSE_URL: &str = "https://admin.stream.rinse.fm/proxy/rinse_uk/stream";
const SAMPLE_RATE: usize = 44100;
const CHUNK_FRAMES: usize = 2048; // PCM frames per FFT window (~46 ms)
const LOGO: &str = "~ RINSE FM ~ 106.8 ~ LIVE FROM LONDON ~   ";

// 256-color gradient, low rows green -> high rows red, plus magenta accent.
const RAMP: [u8; 12] = [46, 46, 82, 118, 154, 190, 226, 220, 214, 208, 202, 196];
const ACCENT: Color = Color::Indexed(201);

// ---------------------------------------------------------------- shared state

#[derive(Default)]
struct StreamState {
    status: String,
    station: String,
    title: String,
    show: String, // current on-air show, from Rinse's schedule API
}

type Shared = Arc<Mutex<StreamState>>;

// ---------------------------------------------------------------- ICY streaming

/// Connect to an Icecast server, strip ICY metadata, fan audio bytes out to sinks.
/// Reconnects forever on failure.
fn icy_thread(url: String, mut sinks: Vec<Box<dyn Write + Send>>, state: Shared) {
    loop {
        if let Err(e) = stream_once(&url, &mut sinks, &state) {
            let mut st = state.lock().unwrap();
            st.status = format!("reconnecting… ({e})");
            drop(st);
            thread::sleep(Duration::from_secs(2));
        }
    }
}

fn stream_once(
    url: &str,
    sinks: &mut [Box<dyn Write + Send>],
    state: &Shared,
) -> Result<(), String> {
    let (host, port, path, https) = parse_http_url(url)?;
    let mut stream = connect(&host, port, https)?;

    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nIcy-MetaData: 1\r\n\
         User-Agent: rinse-cli/1.0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);

    // Status line — ICY servers may answer "ICY 200 OK" rather than HTTP/1.x.
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    if !line.contains("200") {
        return Err(format!("bad status: {}", line.trim()));
    }

    // Headers.
    let mut metaint: usize = 0;
    loop {
        let mut hdr = String::new();
        reader.read_line(&mut hdr).map_err(|e| e.to_string())?;
        let hdr = hdr.trim();
        if hdr.is_empty() {
            break;
        }
        if let Some((k, v)) = hdr.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "icy-metaint" => metaint = v.parse().unwrap_or(0),
                "icy-name" if !v.is_empty() => state.lock().unwrap().station = v.into(),
                _ => {}
            }
        }
    }
    state.lock().unwrap().status = "live".into();

    // Body: audio interleaved with metadata blocks every `metaint` bytes.
    let mut buf = vec![0u8; 8192];
    loop {
        if metaint > 0 {
            let mut remaining = metaint;
            while remaining > 0 {
                let n = remaining.min(buf.len());
                reader
                    .read_exact(&mut buf[..n])
                    .map_err(|_| "stream ended".to_string())?;
                fanout(sinks, &buf[..n]);
                remaining -= n;
            }
            let mut lenbyte = [0u8; 1];
            reader
                .read_exact(&mut lenbyte)
                .map_err(|_| "stream ended".to_string())?;
            let mlen = lenbyte[0] as usize * 16;
            if mlen > 0 {
                let mut meta = vec![0u8; mlen];
                reader
                    .read_exact(&mut meta)
                    .map_err(|_| "stream ended".to_string())?;
                parse_meta(&meta, state);
            }
        } else {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("stream ended".into());
            }
            fanout(sinks, &buf[..n]);
        }
    }
}

fn fanout(sinks: &mut [Box<dyn Write + Send>], data: &[u8]) {
    for s in sinks.iter_mut() {
        let _ = s.write_all(data).and_then(|_| s.flush());
    }
}

/// A `Write` sink backed by a background writer thread: bytes are queued and
/// written to the wrapped child stdin off-thread. This decouples the sinks so
/// `icy_thread` reads the socket at line rate regardless of how fast each
/// child drains. Without it, ffplay only pulls its pipe at real-time playback
/// pace, which back-pressures the shared `fanout` in bursts — starving ffmpeg
/// between refills so the visualizer spikes every few seconds instead of
/// tracking the audio smoothly.
struct BufferedSink(std::sync::mpsc::Sender<Vec<u8>>);

impl Write for BufferedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "sink closed"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn buffered_sink(mut w: impl Write + Send + 'static) -> BufferedSink {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        for chunk in rx {
            if w.write_all(&chunk).is_err() {
                break;
            }
        }
    });
    BufferedSink(tx)
}

fn parse_meta(blob: &[u8], state: &Shared) {
    let text = String::from_utf8_lossy(blob);
    let text = text.trim_end_matches('\0');
    if let Some(start) = text.find("StreamTitle='") {
        let rest = &text[start + 13..];
        if let Some(end) = rest.find("';") {
            let title = rest[..end].trim();
            if !title.is_empty() {
                state.lock().unwrap().title = title.to_string();
            }
        }
    }
}

/// Read+Write connection: a plain `TcpStream` or a TLS-wrapped one.
trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

/// Open a TCP connection, wrapping it in TLS for `https`. A read timeout is
/// set on the underlying socket so a dead stream can't wedge icy_thread.
fn connect(host: &str, port: u16, https: bool) -> Result<Box<dyn ReadWrite>, String> {
    let sock = TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    if https {
        let connector = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
        let tls = connector.connect(host, sock).map_err(|e| e.to_string())?;
        Ok(Box::new(tls))
    } else {
        Ok(Box::new(sock))
    }
}

fn parse_http_url(url: &str) -> Result<(String, u16, String, bool), String> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("URL must start with http:// or https://".into());
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "bad port")?),
        None => (hostport.to_string(), if https { 443 } else { 80 }),
    };
    Ok((host, port, path.to_string(), https))
}

// ---------------------------------------------------------- now playing (schedule API)
//
// Rinse's audio stream only carries a placeholder `StreamTitle`; the real
// on-air show lives in their web schedule API. We map the stream mount to a
// channel slug, poll the schedule, and publish the show currently on air.

/// `…/proxy/rinse_uk/stream` -> `uk`, `…/proxy/kool/stream` -> `kool`.
/// None for non-Rinse URLs — then we just show the ICY metadata as before.
fn rinse_channel(url: &str) -> Option<String> {
    let mount = url.split("admin.stream.rinse.fm/proxy/").nth(1)?.split('/').next()?;
    Some(mount.strip_prefix("rinse_").unwrap_or(mount).to_string())
}

/// Poll the schedule every 60s and publish the on-air show name.
fn now_playing_thread(channel: String, state: Shared) {
    loop {
        if let Some(show) = fetch_schedule().and_then(|s| current_show(&s, &channel)) {
            state.lock().unwrap().show = show;
        }
        thread::sleep(Duration::from_secs(60));
    }
}

fn fetch_schedule() -> Option<serde_json::Value> {
    let mut stream = connect("www.rinse.fm", 443, true).ok()?;
    write!(
        stream,
        "GET /api/query/v1/schedule HTTP/1.0\r\nHost: www.rinse.fm\r\n\
         User-Agent: rinse-cli/1.0\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let body = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    serde_json::from_slice(&raw[body..]).ok()
}

/// Find the show on air *now* for `channel`. Each episode carries a broadcast
/// day (`episodeDate`, London midnight as UTC) and a time-of-day
/// (`episodeTime`); combined they give the real air window.
fn current_show(sched: &serde_json::Value, channel: &str) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    for ep in sched.get("episodes")?.as_array()? {
        let slug = ep
            .get("channel")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("slug"))
            .and_then(|s| s.as_str());
        if slug != Some(channel) {
            continue;
        }
        let day = ep.get("episodeDate").and_then(|v| v.as_str()).and_then(iso_to_epoch);
        let tod = ep.get("episodeTime").and_then(|v| v.as_str());
        let (Some(day), Some(tod)) = (day, tod) else { continue };
        let h = tod.get(11..13).and_then(|s| s.parse::<i64>().ok());
        let m = tod.get(14..16).and_then(|s| s.parse::<i64>().ok());
        let (Some(h), Some(m)) = (h, m) else { continue };
        let start = day + (h * 60 + m) * 60;
        let end = start + ep.get("episodeLength").and_then(|v| v.as_i64()).unwrap_or(60) * 60;
        if now < start || now >= end {
            continue;
        }
        // Prefer the explicit artist; else the name before " - " in the title.
        let name = ep
            .get("artistTitle")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                ep.get("title")
                    .and_then(|v| v.as_str())
                    .map(|t| t.split(" - ").next().unwrap_or(t).trim().to_string())
            })
            .filter(|s| !s.is_empty());
        if let Some(name) = name {
            return Some(name);
        }
    }
    None
}

/// Parse `YYYY-MM-DDTHH:MM:SS…` (UTC) into an epoch in seconds.
fn iso_to_epoch(s: &str) -> Option<i64> {
    let f = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d) = (f(0, 4)?, f(5, 7)?, f(8, 10)?);
    let (h, mi, se) = (f(11, 13)?, f(14, 16)?, f(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ---------------------------------------------------------------- PCM reader

fn pcm_thread(mut pipe: impl Read, tx: std::sync::mpsc::SyncSender<Vec<i16>>) {
    let bytes = CHUNK_FRAMES * 2;
    let mut buf = vec![0u8; bytes];
    loop {
        if pipe.read_exact(&mut buf).is_err() {
            return;
        }
        let samples: Vec<i16> = buf
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let _ = tx.try_send(samples); // drop chunks if the UI falls behind
    }
}

// ---------------------------------------------------------------- spectrum

struct Spectrum {
    n: usize,
    max_n: usize, // upper bound on bands; actual count grows/shrinks to fill width
    window: Vec<f32>,
    bins: Vec<(usize, usize)>, // [start, end) FFT bin range per band
    levels: Vec<f32>,
    peaks: Vec<f32>,
    fft: Arc<dyn rustfft::Fft<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Spectrum {
    fn new(n: usize) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(CHUNK_FRAMES);
        let window: Vec<f32> = (0..CHUNK_FRAMES)
            .map(|i| {
                let x = i as f32 / (CHUNK_FRAMES - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect();
        let mut s = Spectrum {
            n: 0,
            max_n: n,
            window,
            bins: vec![],
            levels: vec![],
            peaks: vec![],
            fft,
            scratch: vec![Complex::default(); CHUNK_FRAMES],
        };
        s.set_bands(n);
        s
    }

    fn set_bands(&mut self, n: usize) {
        self.n = n;
        let hz_per_bin = SAMPLE_RATE as f32 / CHUNK_FRAMES as f32;
        // log-spaced edges 45 Hz .. 16 kHz
        let (lo, hi) = (45.0f32, 16000.0f32);
        self.bins = (0..n)
            .map(|i| {
                let f0 = lo * (hi / lo).powf(i as f32 / n as f32);
                let f1 = lo * (hi / lo).powf((i + 1) as f32 / n as f32);
                let b0 = (f0 / hz_per_bin) as usize;
                let b1 = ((f1 / hz_per_bin) as usize).max(b0 + 1);
                (b0, b1.min(CHUNK_FRAMES / 2))
            })
            .collect();
        self.levels = vec![0.0; n];
        self.peaks = vec![0.0; n];
    }

    fn feed(&mut self, samples: &[i16]) {
        for i in 0..CHUNK_FRAMES {
            let s = samples.get(i).copied().unwrap_or(0);
            self.scratch[i] = Complex::new(s as f32 / 32768.0 * self.window[i], 0.0);
        }
        self.fft.process(&mut self.scratch);

        let norm = (8.0f32 * 60.0).ln_1p();
        for (i, &(b0, b1)) in self.bins.iter().enumerate() {
            let mag: f32 = self.scratch[b0..b1].iter().map(|c| c.norm()).sum::<f32>()
                / (b1 - b0) as f32;
            let tilt = 1.0 + 0.6 * i as f32 / self.n as f32; // mild high-end lift
            let raw = ((mag * 8.0).ln_1p() / norm * tilt).clamp(0.0, 1.0);
            // fast attack, slow decay
            self.levels[i] = if raw > self.levels[i] {
                self.levels[i] * 0.3 + raw * 0.7
            } else {
                self.levels[i] * 0.82 + raw * 0.18
            };
            self.peaks[i] = (self.peaks[i] - 0.02).max(self.levels[i]);
        }
    }
}

// ---------------------------------------------------------------- UI

fn ramp_color(row: usize, height: usize) -> Color {
    let idx = row * RAMP.len() / height.max(1);
    Color::Indexed(RAMP[idx.min(RAMP.len() - 1)])
}

fn draw_frame(frame: &mut ratatui::Frame, spec: &mut Spectrum, state: &Shared, tick: usize) {
    let area = frame.area();
    let (w, h) = (area.width as usize, area.height as usize);
    if h < 6 || w < 10 {
        return;
    }

    // Header marquee.
    let logo_len = LOGO.chars().count();
    let off = (tick / 2) % logo_len;
    let marquee: String = LOGO
        .chars()
        .chain(LOGO.chars())
        .chain(LOGO.chars())
        .skip(off)
        .take(w.saturating_sub(2))
        .collect();
    let st = state.lock().unwrap();
    let info = if !st.show.is_empty() {
        format!("♪ {}", st.show)
    } else if !st.title.is_empty() {
        format!("♪ {}", st.title)
    } else if !st.station.is_empty() {
        st.station.clone()
    } else {
        st.status.clone()
    };
    drop(st);

    // Bars geometry: 2-wide bars with a 1-col gap, as many as fill the width
    // (up to spec.max_n). Band count follows the terminal size, growing when
    // it widens and shrinking when it narrows.
    let (bar_w, gap) = (2usize, 1usize);
    let n = ((w - 2) / (bar_w + gap)).clamp(1, spec.max_n);
    if n != spec.n {
        spec.set_bands(n);
    }
    let x0 = (w - n * (bar_w + gap)) / 2;
    let height = h - 5; // rows available for bars

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    lines.push(Line::from(Span::styled(
        format!(" {marquee}"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    let pad = w.saturating_sub(info.chars().count()) / 2;
    lines.push(Line::from(Span::styled(
        format!("{}{}", " ".repeat(pad), info),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    // Build bar rows top-to-bottom.
    const PARTIALS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    for screen_row in 0..height {
        let row = height - 1 - screen_row; // 0 at the bottom
        let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(x0))];
        for i in 0..n {
            let lvl = spec.levels[i] * height as f32;
            let pk = (spec.peaks[i] * height as f32) as usize;
            let full = lvl as usize;
            let frac = lvl - full as f32;
            let (ch, color) = if row < full {
                ('█', ramp_color(row, height))
            } else if row == full && frac > 0.05 {
                (PARTIALS[((frac * 8.0) as usize).clamp(1, 7)], ramp_color(row, height))
            } else if row == pk && spec.peaks[i] > 0.05 {
                ('▔', ACCENT)
            } else {
                (' ', Color::Reset)
            };
            let cell: String = std::iter::repeat(ch).take(bar_w).collect();
            spans.push(Span::styled(cell, Style::default().fg(color)));
            spans.push(Span::raw(" ".repeat(gap)));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(Span::styled(
        format!("{:>width$}", "q: quit ", width = w.saturating_sub(1)),
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

// ---------------------------------------------------------------- main

struct Args {
    url: String,
    bars: usize,
    no_audio: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        url: RINSE_URL.into(),
        bars: 128, // max bands; the visualizer uses as many as fill the terminal
        no_audio: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => args.url = it.next().unwrap_or_else(|| usage()),
            "--bars" => {
                args.bars = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())
            }
            "--no-audio" => args.no_audio = true,
            _ => usage(),
        }
    }
    args
}

fn usage() -> ! {
    eprintln!(
        "rinse — stream Rinse FM in your terminal, with visuals\n\n\
         usage: rinse [--url <http-stream-url>] [--bars N (max bands)] [--no-audio]\n\n\
         requires ffmpeg (and ffplay for audio) on your PATH"
    );
    std::process::exit(2);
}

fn spawn_ffmpeg_decoder() -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args([
            "-loglevel", "quiet", "-i", "pipe:0", "-f", "s16le", "-ac", "1", "-ar",
            "44100", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn spawn_ffplay() -> std::io::Result<Child> {
    Command::new("ffplay")
        .args(["-loglevel", "quiet", "-nodisp", "-autoexit", "-i", "pipe:0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn main() {
    let args = parse_args();

    let mut dec = match spawn_ffmpeg_decoder() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("error: 'ffmpeg' not found — install it first (brew/apt install ffmpeg)");
            std::process::exit(1);
        }
    };
    let dec_stdin = dec.stdin.take().unwrap();
    let dec_stdout = dec.stdout.take().unwrap();

    let mut sinks: Vec<Box<dyn Write + Send>> = vec![Box::new(buffered_sink(dec_stdin))];
    let mut player: Option<Child> = None;
    if !args.no_audio {
        match spawn_ffplay() {
            Ok(mut p) => {
                sinks.push(Box::new(buffered_sink(p.stdin.take().unwrap())));
                player = Some(p);
            }
            Err(_) => {
                eprintln!("error: 'ffplay' not found — install ffmpeg, or run with --no-audio");
                std::process::exit(1);
            }
        }
    }

    let state: Shared = Arc::new(Mutex::new(StreamState {
        status: "connecting…".into(),
        ..Default::default()
    }));

    {
        let (url, state) = (args.url.clone(), state.clone());
        thread::spawn(move || icy_thread(url, sinks, state));
    }

    let (tx, rx): (_, Receiver<Vec<i16>>) = sync_channel(64);
    thread::spawn(move || pcm_thread(dec_stdout, tx));

    // For Rinse streams, show the real on-air show from the schedule API.
    if let Some(channel) = rinse_channel(&args.url) {
        let state = state.clone();
        thread::spawn(move || now_playing_thread(channel, state));
    }

    let mut terminal = ratatui::init();
    let mut spec = Spectrum::new(args.bars);
    let silence = vec![0i16; CHUNK_FRAMES];
    let mut tick = 0usize;

    'outer: loop {
        // Pull one chunk (or feed silence so bars keep decaying), then drain backlog.
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => spec.feed(&chunk),
            Err(_) => spec.feed(&silence),
        }
        loop {
            match rx.try_recv() {
                Ok(chunk) => spec.feed(&chunk),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        let _ = terminal.draw(|f| draw_frame(f, &mut spec, &state, tick));
        tick += 1;

        // Handle input without blocking the render cadence.
        let deadline = Instant::now() + Duration::from_millis(5);
        while event::poll(deadline.saturating_duration_since(Instant::now())).unwrap_or(false)
        {
            if let Ok(Event::Key(k)) = event::read() {
                let ctrl_c = k.code == KeyCode::Char('c')
                    && k.modifiers.contains(KeyModifiers::CONTROL);
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q')) || ctrl_c {
                    break 'outer;
                }
            }
        }
    }

    ratatui::restore();
    let _ = dec.kill();
    if let Some(mut p) = player {
        let _ = p.kill();
    }
    std::process::exit(0); // ICY thread may be blocked in a socket read
}
