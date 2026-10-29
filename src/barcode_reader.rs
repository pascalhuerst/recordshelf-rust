use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as AnyhowContext, Result};
use eframe::egui::Context as EguiContext;
use evdev::{Device, EventSummary, KeyCode};

use crate::discogs::{DiscogsClient, Release};

fn emit(tx: &Sender<ScanEvent>, repaint: &Option<EguiContext>, event: ScanEvent) {
    let _ = tx.send(event);
    if let Some(ctx) = repaint {
        ctx.request_repaint();
    }
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub barcode: String,
    pub release: Option<Release>,
    pub error: Option<String>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum ScanEvent {
    Info(String),
    Result(ScanResult),
}

/// An input device that could be a barcode scanner (it reports digit keys and Enter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub path: PathBuf,
    pub name: String,
}

/// Lists keyboard-like input devices. Event node numbers change between
/// boots and USB ports, so devices are identified by name instead.
pub fn list_devices() -> Vec<InputDevice> {
    let mut out: Vec<InputDevice> = evdev::enumerate()
        .filter(|(_, dev)| {
            dev.supported_keys()
                .is_some_and(|k| k.contains(KeyCode::KEY_ENTER) && k.contains(KeyCode::KEY_1))
        })
        .map(|(path, dev)| InputDevice {
            path,
            name: dev.name().unwrap_or_default().to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Chooses the device to open: the first whose name contains `name`
/// (case-insensitive), otherwise `fallback_path` if set.
pub fn pick_device(devices: &[InputDevice], name: &str, fallback_path: &str) -> Option<PathBuf> {
    let name = name.trim().to_lowercase();
    if !name.is_empty() {
        if let Some(dev) = devices
            .iter()
            .find(|d| d.name.to_lowercase().contains(&name))
        {
            return Some(dev.path.clone());
        }
    }
    let fallback = fallback_path.trim();
    (!fallback.is_empty()).then(|| PathBuf::from(fallback))
}

pub struct BarcodeReader;

impl BarcodeReader {
    pub fn spawn(
        device_path: PathBuf,
        discogs: DiscogsClient,
        stop: Arc<AtomicBool>,
        tx: Sender<ScanEvent>,
        repaint: Option<EguiContext>,
    ) -> Result<()> {
        thread::Builder::new()
            .name("barcode-scanner".to_string())
            .spawn(move || scanner_loop(device_path, discogs, stop, tx, repaint))
            .context("spawn barcode scanner thread")?;
        Ok(())
    }
}

fn scanner_loop(
    device_path: PathBuf,
    discogs: DiscogsClient,
    stop: Arc<AtomicBool>,
    tx: Sender<ScanEvent>,
    repaint: Option<EguiContext>,
) {
    let mut device = match Device::open(&device_path) {
        Ok(d) => d,
        Err(err) => {
            emit(
                &tx,
                &repaint,
                ScanEvent::Info(format!(
                    "Failed to open device {}: {err}",
                    device_path.display()
                )),
            );
            return;
        }
    };
    // Non-blocking so the loop can notice `stop` (e.g. when settings change).
    if let Err(err) = device.set_nonblocking(true) {
        emit(
            &tx,
            &repaint,
            ScanEvent::Info(format!("Failed to set device non-blocking: {err}")),
        );
        return;
    }

    emit(
        &tx,
        &repaint,
        ScanEvent::Info(format!("Listening for scans on {}", device_path.display())),
    );

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
                                    emit(
                                        &tx,
                                        &repaint,
                                        ScanEvent::Result(ScanResult {
                                            barcode: code,
                                            release: Some(release),
                                            error: None,
                                            elapsed: start.elapsed(),
                                        }),
                                    );
                                }
                                Err(err) => {
                                    emit(
                                        &tx,
                                        &repaint,
                                        ScanEvent::Result(ScanResult {
                                            barcode: code,
                                            release: None,
                                            error: Some(err.to_string()),
                                            elapsed: start.elapsed(),
                                        }),
                                    );
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
                emit(
                    &tx,
                    &repaint,
                    ScanEvent::Info(format!("Read error on {}: {err}", device_path.display())),
                );
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
