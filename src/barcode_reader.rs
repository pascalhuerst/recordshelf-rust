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
    /// The reader thread of this device ended (missing or unplugged); the app may try again later.
    Stopped(PathBuf),
}

/// An input device that could be a barcode scanner (it reports keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub path: PathBuf,
    pub name: String,
    /// It has Enter and digit keys (also as keypad keys), as a scanner has.
    /// Other devices that report keys are listed too, for scanners that
    /// present themselves differently.
    pub keyboard_like: bool,
}

/// Lists keyboard-like input devices. Event node numbers change between
/// boots and USB ports, so devices are identified by name instead.
pub fn list_devices() -> Vec<InputDevice> {
    let mut out: Vec<InputDevice> = evdev::enumerate()
        .filter_map(|(path, dev)| {
            let keys = dev.supported_keys()?;
            let any = |codes: &[KeyCode]| codes.iter().any(|c| keys.contains(*c));
            let enter = any(&[KeyCode::KEY_ENTER, KeyCode::KEY_KPENTER]);
            let digit = any(&[KeyCode::KEY_1, KeyCode::KEY_KP1]);
            // devices without any of these (mice, buttons) are of no use
            (enter || digit || any(&[KeyCode::KEY_A])).then(|| InputDevice {
                path,
                name: dev.name().unwrap_or_default().to_string(),
                keyboard_like: enter && digit,
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The scanner names of the settings: several can be given, separated by `;`.
pub fn split_names(names: &str) -> Vec<String> {
    names
        .split(';')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from)
        .collect()
}

/// `names` with `name` added, or removed when it is in there already
/// (comparing without case); the settings text for the next choice.
pub fn toggle_name(names: &str, name: &str) -> String {
    let mut list = split_names(names);
    let before = list.len();
    list.retain(|n| !n.eq_ignore_ascii_case(name.trim()));
    if list.len() == before && !name.trim().is_empty() {
        list.push(name.trim().to_string());
    }
    list.join("; ")
}

/// Chooses the devices to open: every one whose name contains one of the
/// `names` (case-insensitive; several receivers of one model all count),
/// otherwise `fallback_path` if set.
pub fn pick_devices(devices: &[InputDevice], names: &str, fallback_path: &str) -> Vec<PathBuf> {
    let names: Vec<String> = split_names(names)
        .iter()
        .map(|n| n.to_lowercase())
        .collect();
    let mut found: Vec<PathBuf> = devices
        .iter()
        .filter(|d| {
            let name = d.name.to_lowercase();
            names.iter().any(|n| name.contains(n))
        })
        .map(|d| d.path.clone())
        .collect();
    found.dedup();
    if found.is_empty() {
        let fallback = fallback_path.trim();
        if !fallback.is_empty() {
            found.push(PathBuf::from(fallback));
        }
    }
    found
}

/// Why no device was picked: what to tell the user.
pub fn explain_missing(devices: &[InputDevice], name: &str, fallback_path: &str) -> String {
    let (name, path) = (name.trim(), fallback_path.trim());
    if name.is_empty() && path.is_empty() {
        return "No scanner configured (pick one in Settings)".to_string();
    }
    let wanted = if name.is_empty() {
        path.to_string()
    } else {
        split_names(name)
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let connected = if devices.is_empty() {
        "none".to_string()
    } else {
        devices
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("Scanner {wanted} not found. Connected input devices: {connected}")
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
            emit(&tx, &repaint, ScanEvent::Stopped(device_path.clone()));
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
        emit(&tx, &repaint, ScanEvent::Stopped(device_path.clone()));
        return;
    }

    // The scanner is a keyboard to the system: without the grab the window
    // would also receive every character of a scan (and run the letters as
    // commands). The grab ends when the device is closed.
    let grabbed = device.grab();
    emit(
        &tx,
        &repaint,
        ScanEvent::Info(listening_message(&device_path, grabbed.as_ref().err())),
    );

    let mut decoder = KeyDecoder::default();

    while !stop.load(Ordering::SeqCst) {
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if let EventSummary::Key(_, keycode, value) = ev.destructure()
                        && let Some(code) = decoder.feed(keycode, value)
                    {
                        emit(&tx, &repaint, ScanEvent::Code(code));
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
                    ScanEvent::Info(format!("Scanner {} lost: {err}", device_path.display())),
                );
                // unplugged or otherwise gone: end the thread, the app looks for it again
                emit(&tx, &repaint, ScanEvent::Stopped(device_path.clone()));
                return;
            }
        }
    }
}

/// What to tell the user once a device is open.
fn listening_message(path: &std::path::Path, grab_error: Option<&std::io::Error>) -> String {
    match grab_error {
        None => format!("Listening for scans on {}", path.display()),
        Some(err) => format!(
            "Listening for scans on {}, but could not take the device for us alone ({err}): the characters of a scan may also reach other windows",
            path.display()
        ),
    }
}

fn is_enter(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KEY_ENTER | KeyCode::KEY_KPENTER | KeyCode::KEY_LINEFEED
    )
}

/// Turns the key events of a scanner (a keyboard to the system) into scanned
/// codes. Scanners type the characters of a code and finish with Enter; QR
/// codes with URLs need Shift for characters such as `:` and `_` (US layout).
#[derive(Default)]
pub struct KeyDecoder {
    buffer: String,
    shift: bool,
}

impl KeyDecoder {
    /// Feeds one key event (`value`: 1 press, 0 release, 2 repeat). Returns the
    /// code when Enter ends one.
    pub fn feed(&mut self, key: KeyCode, value: i32) -> Option<String> {
        if matches!(key, KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT) {
            self.shift = value != 0;
            return None;
        }
        if value != 1 {
            return None;
        }
        if is_enter(key) {
            let code = self.buffer.trim().to_string();
            self.buffer.clear();
            return (!code.is_empty()).then_some(code);
        }
        if let Some(ch) = translate_key(key, self.shift) {
            self.buffer.push(ch);
        }
        None
    }
}

/// The character of a key, for the keys that occur in the codes we scan.
/// Letters are always upper case (the codes are case-insensitive).
fn translate_key(code: KeyCode, shift: bool) -> Option<char> {
    if shift {
        return match code {
            KeyCode::KEY_MINUS => Some('_'),
            KeyCode::KEY_SEMICOLON => Some(':'),
            KeyCode::KEY_SLASH => Some('?'),
            KeyCode::KEY_EQUAL => Some('+'),
            KeyCode::KEY_3 => Some('#'),
            KeyCode::KEY_5 => Some('%'),
            KeyCode::KEY_7 => Some('&'),
            _ => translate_key(code, false).filter(char::is_ascii_alphabetic),
        };
    }
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
        KeyCode::KEY_EQUAL => Some('='),
        // Letters are reported upper case regardless of Shift: the codes we
        // read (decks.de labels) are case-insensitive.
        KeyCode::KEY_A => Some('A'),
        KeyCode::KEY_B => Some('B'),
        KeyCode::KEY_C => Some('C'),
        KeyCode::KEY_D => Some('D'),
        KeyCode::KEY_E => Some('E'),
        KeyCode::KEY_F => Some('F'),
        KeyCode::KEY_G => Some('G'),
        KeyCode::KEY_H => Some('H'),
        KeyCode::KEY_I => Some('I'),
        KeyCode::KEY_J => Some('J'),
        KeyCode::KEY_K => Some('K'),
        KeyCode::KEY_L => Some('L'),
        KeyCode::KEY_M => Some('M'),
        KeyCode::KEY_N => Some('N'),
        KeyCode::KEY_O => Some('O'),
        KeyCode::KEY_P => Some('P'),
        KeyCode::KEY_Q => Some('Q'),
        KeyCode::KEY_R => Some('R'),
        KeyCode::KEY_S => Some('S'),
        KeyCode::KEY_T => Some('T'),
        KeyCode::KEY_U => Some('U'),
        KeyCode::KEY_V => Some('V'),
        KeyCode::KEY_W => Some('W'),
        KeyCode::KEY_X => Some('X'),
        KeyCode::KEY_Y => Some('Y'),
        KeyCode::KEY_Z => Some('Z'),
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
            keyboard_like: true,
        }
    }

    fn devices() -> Vec<InputDevice> {
        vec![
            dev("/dev/input/event3", "AT Translated Set 2 keyboard"),
            dev("/dev/input/event17", "Newtologic NT-1228BL Barcode Scanner"),
        ]
    }

    fn paths(list: Vec<PathBuf>) -> Vec<String> {
        list.iter().map(|p| p.display().to_string()).collect()
    }

    #[test]
    fn picks_device_by_name_even_if_renumbered() {
        let mut devs = devices();
        assert_eq!(
            paths(pick_devices(&devs, "barcode scanner", "")),
            ["/dev/input/event17"]
        );
        devs[1].path = PathBuf::from("/dev/input/event5");
        assert_eq!(
            paths(pick_devices(&devs, "Barcode Scanner", "")),
            ["/dev/input/event5"]
        );
    }

    #[test]
    fn name_takes_precedence_over_fallback_path() {
        assert_eq!(
            paths(pick_devices(&devices(), "NT-1228", "/dev/input/event99")),
            ["/dev/input/event17"]
        );
    }

    #[test]
    fn falls_back_to_path_when_name_does_not_match() {
        assert_eq!(
            paths(pick_devices(&devices(), "unplugged", "/dev/input/event9")),
            ["/dev/input/event9"]
        );
        assert_eq!(
            paths(pick_devices(&[], "", "/dev/input/event9")),
            ["/dev/input/event9"]
        );
    }

    #[test]
    fn nothing_configured_picks_nothing() {
        assert!(pick_devices(&devices(), "", "").is_empty());
        assert!(pick_devices(&devices(), "  ;; ", "  ").is_empty());
        assert!(pick_devices(&devices(), "unplugged", "").is_empty());
    }

    #[test]
    fn several_names_and_several_devices_of_one_name_are_all_used() {
        let mut devs = devices();
        devs.push(dev("/dev/input/event16", "HID 0581:011a"));
        devs.push(dev("/dev/input/event20", "HID 0581:011a"));
        assert_eq!(
            paths(pick_devices(&devs, "Barcode Scanner; hid 0581:011a", "")),
            [
                "/dev/input/event17",
                "/dev/input/event16",
                "/dev/input/event20"
            ],
            "every device of every name"
        );
        assert_eq!(
            paths(pick_devices(&devs, "HID 0581:011a", "/dev/input/event99")),
            ["/dev/input/event16", "/dev/input/event20"],
            "the fallback only counts when nothing matches"
        );
    }

    #[test]
    fn names_are_split_and_toggled() {
        assert_eq!(split_names(" A ;B;; C "), ["A", "B", "C"]);
        assert!(split_names("").is_empty());
        assert_eq!(toggle_name("", "HID 0581:011a"), "HID 0581:011a");
        assert_eq!(toggle_name("A", "B"), "A; B");
        assert_eq!(
            toggle_name("A; B", "a"),
            "B",
            "picking it again takes it out"
        );
        assert_eq!(toggle_name("A", " "), "A");
    }

    #[test]
    fn the_explanation_names_every_wanted_scanner() {
        let text = explain_missing(&devices(), "One; Two", "");
        assert!(text.contains("\"One\", \"Two\" not found"), "{text}");
        assert!(text.contains("Barcode Scanner"), "{text}");
        assert!(explain_missing(&[], " ", " ").contains("No scanner configured"));
    }

    #[test]
    fn translates_digits_and_punctuation() {
        assert_eq!(translate_key(KeyCode::KEY_7, false), Some('7'));
        assert_eq!(translate_key(KeyCode::KEY_KP0, false), Some('0'));
        assert_eq!(translate_key(KeyCode::KEY_MINUS, false), Some('-'));
        assert_eq!(translate_key(KeyCode::KEY_A, false), Some('A'));
        assert_eq!(translate_key(KeyCode::KEY_Z, false), Some('Z'));
        assert_eq!(translate_key(KeyCode::KEY_SPACE, false), None);
    }

    #[test]
    fn the_listening_message_tells_when_the_device_could_not_be_grabbed() {
        let path = std::path::Path::new("/dev/input/event9");
        assert_eq!(
            listening_message(path, None),
            "Listening for scans on /dev/input/event9"
        );
        let busy = std::io::Error::from_raw_os_error(16);
        let text = listening_message(path, Some(&busy));
        assert!(
            text.contains("could not take the device") && text.contains("other windows"),
            "{text}"
        );
    }

    #[test]
    fn recognises_enter_keys() {
        assert!(is_enter(KeyCode::KEY_ENTER));
        assert!(is_enter(KeyCode::KEY_KPENTER));
        assert!(!is_enter(KeyCode::KEY_SPACE));
    }

    /// Types a string like a US keyboard (or a scanner) would: Shift around the
    /// characters that need it, Enter at the end.
    fn type_codes(text: &str) -> Vec<(KeyCode, i32)> {
        let mut events = Vec::new();
        for c in text.chars() {
            let (key, shift) = match c {
                '0'..='9' => (digit_key(c), false),
                '/' => (KeyCode::KEY_SLASH, false),
                '.' => (KeyCode::KEY_DOT, false),
                '-' => (KeyCode::KEY_MINUS, false),
                '_' => (KeyCode::KEY_MINUS, true),
                ':' => (KeyCode::KEY_SEMICOLON, true),
                '?' => (KeyCode::KEY_SLASH, true),
                '=' => (KeyCode::KEY_EQUAL, false),
                'a'..='z' | 'A'..='Z' => {
                    (letter_key(c.to_ascii_uppercase()), c.is_ascii_uppercase())
                }
                other => panic!("not typeable in the test: {other}"),
            };
            if shift {
                events.push((KeyCode::KEY_LEFTSHIFT, 1));
            }
            events.push((key, 1));
            events.push((key, 0));
            if shift {
                events.push((KeyCode::KEY_LEFTSHIFT, 0));
            }
        }
        events.push((KeyCode::KEY_ENTER, 1));
        events.push((KeyCode::KEY_ENTER, 0));
        events
    }

    fn digit_key(c: char) -> KeyCode {
        [
            KeyCode::KEY_0,
            KeyCode::KEY_1,
            KeyCode::KEY_2,
            KeyCode::KEY_3,
            KeyCode::KEY_4,
            KeyCode::KEY_5,
            KeyCode::KEY_6,
            KeyCode::KEY_7,
            KeyCode::KEY_8,
            KeyCode::KEY_9,
        ][c.to_digit(10).unwrap() as usize]
    }

    fn letter_key(c: char) -> KeyCode {
        [
            KeyCode::KEY_A,
            KeyCode::KEY_B,
            KeyCode::KEY_C,
            KeyCode::KEY_D,
            KeyCode::KEY_E,
            KeyCode::KEY_F,
            KeyCode::KEY_G,
            KeyCode::KEY_H,
            KeyCode::KEY_I,
            KeyCode::KEY_J,
            KeyCode::KEY_K,
            KeyCode::KEY_L,
            KeyCode::KEY_M,
            KeyCode::KEY_N,
            KeyCode::KEY_O,
            KeyCode::KEY_P,
            KeyCode::KEY_Q,
            KeyCode::KEY_R,
            KeyCode::KEY_S,
            KeyCode::KEY_T,
            KeyCode::KEY_U,
            KeyCode::KEY_V,
            KeyCode::KEY_W,
            KeyCode::KEY_X,
            KeyCode::KEY_Y,
            KeyCode::KEY_Z,
        ][(c as u8 - b'A') as usize]
    }

    fn scan(text: &str) -> Vec<String> {
        let mut decoder = KeyDecoder::default();
        type_codes(text)
            .into_iter()
            .filter_map(|(key, value)| decoder.feed(key, value))
            .collect()
    }

    #[test]
    fn a_url_from_a_qr_code_arrives_complete() {
        // typed with Shift for the colon and the underscores, like the QR reader does
        assert_eq!(
            scan("https://deejay.de/__960305"),
            ["HTTPS://DEEJAY.DE/__960305"]
        );
    }

    #[test]
    fn plain_codes_still_work() {
        assert_eq!(scan("CNF/O1"), ["CNF/O1"]);
        assert_eq!(scan("5012345678900"), ["5012345678900"]);
        assert_eq!(scan("32474433"), ["32474433"]);
        assert_eq!(scan("a-b.c"), ["A-B.C"]);
    }

    #[test]
    fn shift_only_changes_what_it_should() {
        let mut d = KeyDecoder::default();
        // a dash typed after Shift was released is a dash again
        for (k, v) in [
            (KeyCode::KEY_LEFTSHIFT, 1),
            (KeyCode::KEY_MINUS, 1),
            (KeyCode::KEY_MINUS, 0),
            (KeyCode::KEY_LEFTSHIFT, 0),
            (KeyCode::KEY_MINUS, 1),
            (KeyCode::KEY_MINUS, 0),
        ] {
            assert_eq!(d.feed(k, v), None);
        }
        assert_eq!(d.feed(KeyCode::KEY_ENTER, 1).as_deref(), Some("_-"));
        // the right shift key counts too, and repeats keep it held
        for (k, v) in [
            (KeyCode::KEY_RIGHTSHIFT, 1),
            (KeyCode::KEY_RIGHTSHIFT, 2),
            (KeyCode::KEY_SEMICOLON, 1),
            (KeyCode::KEY_RIGHTSHIFT, 0),
        ] {
            d.feed(k, v);
        }
        assert_eq!(d.feed(KeyCode::KEY_ENTER, 1).as_deref(), Some(":"));
    }

    #[test]
    fn releases_and_unknown_keys_are_ignored() {
        let mut d = KeyDecoder::default();
        assert_eq!(d.feed(KeyCode::KEY_1, 0), None);
        assert_eq!(d.feed(KeyCode::KEY_SPACE, 1), None);
        assert_eq!(d.feed(KeyCode::KEY_F5, 1), None);
        assert_eq!(
            d.feed(KeyCode::KEY_ENTER, 1),
            None,
            "nothing was typed: no code"
        );
        d.feed(KeyCode::KEY_2, 1);
        d.feed(KeyCode::KEY_2, 2); // auto repeat is not a second press
        assert_eq!(d.feed(KeyCode::KEY_KPENTER, 1).as_deref(), Some("2"));
    }

    #[test]
    fn two_scans_in_a_row_stay_apart() {
        assert_eq!(scan("123456"), ["123456"]);
        let mut d = KeyDecoder::default();
        let mut codes = Vec::new();
        for text in ["CNF/O1", "525768"] {
            for (k, v) in type_codes(text) {
                codes.extend(d.feed(k, v));
            }
        }
        assert_eq!(codes, ["CNF/O1", "525768"]);
    }

    #[test]
    fn a_missing_scanner_is_explained_with_what_is_connected() {
        let devices = vec![
            InputDevice {
                path: PathBuf::from("/dev/input/event2"),
                name: "AT Translated Set 2 keyboard".into(),
                keyboard_like: true,
            },
            InputDevice {
                path: PathBuf::from("/dev/input/event16"),
                name: "HID 0581:011a".into(),
                keyboard_like: true,
            },
        ];
        let text = explain_missing(&devices, "Belon.cn 2.4G Wireless Device", "");
        assert!(
            text.contains("\"Belon.cn 2.4G Wireless Device\" not found"),
            "{text}"
        );
        assert!(
            text.contains("HID 0581:011a") && text.contains("AT Translated Set 2 keyboard"),
            "{text}"
        );
        assert!(explain_missing(&[], "X", "").contains("Connected input devices: none"));
        assert!(explain_missing(&devices, "", "/dev/input/event99").contains("/dev/input/event99"));
        assert_eq!(
            explain_missing(&devices, "  ", " "),
            "No scanner configured (pick one in Settings)"
        );
    }
}
