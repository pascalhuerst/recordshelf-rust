use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::Duration,
};

use anyhow::{Context as AnyhowContext, Result};
use eframe::egui::Context as EguiContext;
use evdev::{Device, EventSummary, KeyCode};

fn emit(tx: &Sender<ScanEvent>, repaint: &Option<EguiContext>, event: ScanEvent) {
    let _ = tx.send(event);
    if let Some(ctx) = repaint {
        ctx.request_repaint();
    }
}

/// Events from the scanner thread. Lookups happen elsewhere; the scanner
/// only reports what it read.
#[derive(Debug)]
pub enum ScanEvent {
    Info(String),
    Code(String),
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
    if !name.is_empty()
        && let Some(dev) = devices
            .iter()
            .find(|d| d.name.to_lowercase().contains(&name))
    {
        return Some(dev.path.clone());
    }
    let fallback = fallback_path.trim();
    (!fallback.is_empty()).then(|| PathBuf::from(fallback))
}

pub struct BarcodeReader;

impl BarcodeReader {
    pub fn spawn(
        device_path: PathBuf,
        stop: Arc<AtomicBool>,
        tx: Sender<ScanEvent>,
        repaint: Option<EguiContext>,
    ) -> Result<()> {
        thread::Builder::new()
            .name("barcode-scanner".to_string())
            .spawn(move || scanner_loop(device_path, stop, tx, repaint))
            .context("spawn barcode scanner thread")?;
        Ok(())
    }
}

fn scanner_loop(
    device_path: PathBuf,
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
                            emit(&tx, &repaint, ScanEvent::Code(code));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(path: &str, name: &str) -> InputDevice {
        InputDevice {
            path: PathBuf::from(path),
            name: name.to_string(),
        }
    }

    fn devices() -> Vec<InputDevice> {
        vec![
            dev("/dev/input/event3", "AT Translated Set 2 keyboard"),
            dev("/dev/input/event17", "Newtologic NT-1228BL Barcode Scanner"),
        ]
    }

    #[test]
    fn picks_device_by_name_even_if_renumbered() {
        let mut devs = devices();
        assert_eq!(
            pick_device(&devs, "barcode scanner", ""),
            Some(PathBuf::from("/dev/input/event17"))
        );
        devs[1].path = PathBuf::from("/dev/input/event5");
        assert_eq!(
            pick_device(&devs, "Barcode Scanner", ""),
            Some(PathBuf::from("/dev/input/event5"))
        );
    }

    #[test]
    fn name_takes_precedence_over_fallback_path() {
        assert_eq!(
            pick_device(&devices(), "NT-1228", "/dev/input/event99"),
            Some(PathBuf::from("/dev/input/event17"))
        );
    }

    #[test]
    fn falls_back_to_path_when_name_does_not_match() {
        assert_eq!(
            pick_device(&devices(), "unplugged", "/dev/input/event9"),
            Some(PathBuf::from("/dev/input/event9"))
        );
        assert_eq!(
            pick_device(&[], "", "/dev/input/event9"),
            Some(PathBuf::from("/dev/input/event9"))
        );
    }

    #[test]
    fn nothing_configured_picks_nothing() {
        assert_eq!(pick_device(&devices(), "", ""), None);
        assert_eq!(pick_device(&devices(), "  ", "  "), None);
        assert_eq!(pick_device(&devices(), "unplugged", ""), None);
    }

    #[test]
    fn translates_digits_and_punctuation() {
        assert_eq!(translate_key(KeyCode::KEY_7), Some('7'));
        assert_eq!(translate_key(KeyCode::KEY_KP0), Some('0'));
        assert_eq!(translate_key(KeyCode::KEY_MINUS), Some('-'));
        assert_eq!(translate_key(KeyCode::KEY_A), None);
    }

    #[test]
    fn recognises_enter_keys() {
        assert!(is_enter(KeyCode::KEY_ENTER));
        assert!(is_enter(KeyCode::KEY_KPENTER));
        assert!(!is_enter(KeyCode::KEY_SPACE));
    }
}
