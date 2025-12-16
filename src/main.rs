use anyhow::{Context, Result};
use clap::Parser;
use eframe::egui;
use evdev::{Device, EventSummary, KeyCode};
use std::io::ErrorKind;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::thread;
use std::time::{Duration, Instant};

mod discogs;
use crate::discogs::{DiscogsClient, Release};

#[derive(Parser, Debug, Clone)]
#[command(name = "recordshelf-rust")]
struct Args {
    /// Path to the barcode scanner input device
    #[arg(long, default_value = "/dev/input/event17")] // We use dev 17 for now!
    device: String,

    /// Discogs API user agent
    #[arg(long = "user-agent", default_value = "RecordShelf Barcode Client")]
    user_agent: String,

    /// Discogs personal access token
    #[arg(long, default_value = "plQPlhPdTnGxLSfkEKNlAcRqBOxnPQeMlJHQaneg")]
    token: String,

    /// Discogs marketplace currency
    #[arg(long, default_value = "EUR")]
    currency: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let discogs = DiscogsClient::new(&args.user_agent, &args.token, &args.currency)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<ScanEvent>();

    spawn_scanner_thread(args.clone(), discogs.clone(), stop.clone(), tx)?;

    let app = RecordShelfApp::new(args, rx, stop);
    let native_options = eframe::NativeOptions::default();
    eframe::run_native("RecordShelf", native_options, Box::new(|_cc| Box::new(app)))
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}

fn spawn_scanner_thread(
    args: Args,
    discogs: DiscogsClient,
    stop: Arc<AtomicBool>,
    tx: Sender<ScanEvent>,
) -> Result<()> {
    thread::Builder::new()
        .name("barcode-scanner".to_string())
        .spawn(move || scanner_loop(args, discogs, stop, tx))
        .context("spawn scanner thread")?;
    Ok(())
}

fn scanner_loop(args: Args, discogs: DiscogsClient, stop: Arc<AtomicBool>, tx: Sender<ScanEvent>) {
    let mut device = match Device::open(&args.device) {
        Ok(d) => d,
        Err(err) => {
            let _ = tx.send(ScanEvent::Info(format!(
                "Failed to open device {}: {err}",
                args.device
            )));
            return;
        }
    };

    if let Err(err) = device.set_nonblocking(true) {
        let _ = tx.send(ScanEvent::Info(format!(
            "Failed to set non-blocking mode on {}: {err}",
            args.device
        )));
        return;
    }

    let _ = tx.send(ScanEvent::Info(format!(
        "Listening for scans on {}",
        args.device
    )));

    let mut buffer = String::new();

    while !stop.load(Ordering::SeqCst) {
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if let EventSummary::Key(_, keycode, value) = ev.destructure() {
                        if value != 1 {
                            continue;
                        }
                        if is_enter(keycode) {
                            let code = buffer.trim().to_string();
                            buffer.clear();
                            if code.is_empty() {
                                continue;
                            }
                            let start = Instant::now();
                            match lookup_release(&discogs, &code) {
                                Ok(release) => {
                                    let _ = tx.send(ScanEvent::Result(ScanResult {
                                        barcode: code,
                                        release: Some(release),
                                        error: None,
                                        elapsed: start.elapsed(),
                                    }));
                                }
                                Err(err) => {
                                    let _ = tx.send(ScanEvent::Result(ScanResult {
                                        barcode: code,
                                        release: None,
                                        error: Some(err.to_string()),
                                        elapsed: start.elapsed(),
                                    }));
                                }
                            }
                        } else if let Some(ch) = translate_key(keycode) {
                            buffer.push(ch);
                        }
                    }
                }
            }
            Err(err) => {
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let _ = tx.send(ScanEvent::Info(format!(
                    "Read error on {}: {err}",
                    args.device
                )));
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn lookup_release(discogs: &DiscogsClient, barcode: &str) -> Result<Release> {
    let id: u32 = barcode
        .parse()
        .with_context(|| format!("invalid numeric barcode: {barcode}"))?;
    discogs.release(id)
}

fn is_enter(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KEY_ENTER | KeyCode::KEY_KPENTER | KeyCode::KEY_LINEFEED
    )
}

fn translate_key(code: KeyCode) -> Option<char> {
    match code {
        KeyCode::KEY_0 | KeyCode::KEY_KP0 => Some('0'),
        KeyCode::KEY_1 | KeyCode::KEY_KP1 => Some('1'),
        KeyCode::KEY_2 | KeyCode::KEY_KP2 => Some('2'),
        KeyCode::KEY_3 | KeyCode::KEY_KP3 => Some('3'),
        KeyCode::KEY_4 | KeyCode::KEY_KP4 => Some('4'),
        KeyCode::KEY_5 | KeyCode::KEY_KP5 => Some('5'),
        KeyCode::KEY_6 | KeyCode::KEY_KP6 => Some('6'),
        KeyCode::KEY_7 | KeyCode::KEY_KP7 => Some('7'),
        KeyCode::KEY_8 | KeyCode::KEY_KP8 => Some('8'),
        KeyCode::KEY_9 | KeyCode::KEY_KP9 => Some('9'),
        KeyCode::KEY_MINUS | KeyCode::KEY_KPMINUS => Some('-'),
        KeyCode::KEY_DOT | KeyCode::KEY_KPDOT => Some('.'),
        KeyCode::KEY_SLASH => Some('/'),
        _ => None,
    }
}

#[derive(Debug)]
enum ScanEvent {
    Info(String),
    Result(ScanResult),
}

#[derive(Debug, Clone)]
struct ScanResult {
    barcode: String,
    release: Option<Release>,
    error: Option<String>,
    elapsed: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Scan,
    Search,
}

struct RecordShelfApp {
    args: Args,
    rx: Receiver<ScanEvent>,
    scan_results: Vec<ScanResult>,
    status: String,
    tab: Tab,
    stop: Arc<AtomicBool>,
}

impl RecordShelfApp {
    fn new(args: Args, rx: Receiver<ScanEvent>, stop: Arc<AtomicBool>) -> Self {
        Self {
            args,
            rx,
            scan_results: Vec::new(),
            status: "Waiting for scans...".to_string(),
            tab: Tab::Scan,
            stop,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                ScanEvent::Info(msg) => self.status = msg,
                ScanEvent::Result(res) => {
                    self.status = format!("Last scan {} ({:.0?})", res.barcode, res.elapsed);
                    self.scan_results.insert(0, res);
                    if self.scan_results.len() > 1000 {
                        self.scan_results.pop();
                    }
                }
            }
        }
    }

    fn ui_scan(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("Device: {}", self.args.device));
        ui.label(format!("Status: {}", self.status));
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for result in &self.scan_results {
                    ui.group(|ui| {
                        ui.heading(format!("Barcode: {}", result.barcode));
                        ui.label(format!("Lookup time: {:.0?}", result.elapsed));
                        if let Some(err) = &result.error {
                            ui.colored_label(egui::Color32::RED, format!("Error: {err}"));
                        } else if let Some(release) = &result.release {
                            render_release(ui, release);
                        } else {
                            ui.label("No data");
                        }
                    });
                    ui.add_space(6.0);
                }
            });
    }

    fn ui_search_placeholder(&mut self, ui: &mut egui::Ui) {
        ui.label("Search tab coming soon.");
    }
}

impl eframe::App for RecordShelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("top-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RecordShelf");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Scan, "Scan");
                ui.selectable_value(&mut self.tab, Tab::Search, "Search");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::widgets::global_dark_light_mode_switch(ui);
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Scan => self.ui_scan(ui),
            Tab::Search => self.ui_search_placeholder(ui),
        });
    }
}

impl Drop for RecordShelfApp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn render_release(ui: &mut egui::Ui, release: &Release) {
    if let Some(title) = &release.title {
        ui.label(format!("Title: {title}"));
    }
    if let Some(artists) = &release.artists {
        if !artists.is_empty() {
            let names = artists
                .iter()
                .filter_map(|a| a.name.as_deref())
                .collect::<Vec<_>>()
                .join(", ");
            if !names.is_empty() {
                ui.label(format!("Artist: {names}"));
            }
        }
    }
    if let Some(labels) = &release.labels {
        if !labels.is_empty() {
            let labels_str = labels
                .iter()
                .map(|l| {
                    let name = l.name.as_deref().unwrap_or("");
                    let catno = l.catno.as_deref().unwrap_or("");
                    if catno.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name} ({catno})")
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !labels_str.is_empty() {
                ui.label(format!("Label: {labels_str}"));
            }
        }
    }
    if let Some(year) = release.year {
        if year > 0 {
            ui.label(format!("Year: {year}"));
        }
    }
    if let Some(country) = &release.country {
        if !country.is_empty() {
            ui.label(format!("Country: {country}"));
        }
    }
    if let Some(formats) = &release.formats {
        if !formats.is_empty() {
            let formats_str = formats
                .iter()
                .map(|f| {
                    let name = f.name.as_deref().unwrap_or("").to_string();
                    match f.descriptions.as_ref() {
                        Some(descs) if !descs.is_empty() => {
                            format!("{name} ({})", descs.join(", "))
                        }
                        _ => name,
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !formats_str.is_empty() {
                ui.label(format!("Format: {formats_str}"));
            }
        }
    }
    if let Some(genres) = &release.genres {
        if !genres.is_empty() {
            ui.label(format!("Genres: {}", genres.join(", ")));
        }
    }
    if let Some(styles) = &release.styles {
        if !styles.is_empty() {
            ui.label(format!("Styles: {}", styles.join(", ")));
        }
    }
    if let Some(tracks) = &release.tracklist {
        if !tracks.is_empty() {
            ui.label("Tracklist (first 10):");
            for track in tracks.iter().take(10) {
                let mut title = track.title.clone().unwrap_or_default();
                if let Some(duration) = &track.duration {
                    if !duration.is_empty() {
                        title.push_str(&format!(" [{duration}]"));
                    }
                }
                let pos = track.position.as_deref().unwrap_or("");
                ui.label(format!("  {pos} {title}"));
            }
            if tracks.len() > 10 {
                ui.label(format!("  ... and {} more tracks", tracks.len() - 10));
            }
        }
    }
}
