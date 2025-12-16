use anyhow::{Context, Result};
use clap::Parser;
use evdev::{Device, EventSummary, KeyCode};
use std::io::ErrorKind;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{fmt::Write as _, thread, time::Duration};

mod discogs;
use crate::discogs::{DiscogsClient, Release};

#[derive(Parser, Debug)]
#[command(name = "recordshelf-rust")]
struct Args {
    /// Path to the barcode scanner input device
    #[arg(long, default_value = "/dev/input/event0")]
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

fn main() {
    if let Err(err) = run() {
        eprintln!("fatal: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let discogs = DiscogsClient::new(&args.user_agent, &args.token, &args.currency)?;

    let mut device = Device::open(&args.device)
        .with_context(|| format!("open input device {}", &args.device))?;
    device
        .set_nonblocking(true)
        .context("enable non-blocking mode")?;

    println!(
        "Listening for barcode scans on {} (press Ctrl+C to quit)",
        args.device
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })
        .context("set Ctrl+C handler")?;
    }

    read_loop(&shutdown, &mut device, &discogs)
}

fn read_loop(
    shutdown: &Arc<AtomicBool>,
    device: &mut Device,
    discogs: &DiscogsClient,
) -> Result<()> {
    let mut buffer = String::new();

    while !shutdown.load(Ordering::SeqCst) {
        match device.fetch_events() {
            Ok(events) => {
                let mut had_events = false;
                for ev in events {
                    had_events = true;
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
                            println!("\nBarcode scanned: {code}");
                            if let Err(err) = lookup_and_print_release(discogs, &code) {
                                eprintln!("lookup error: {err}");
                            }
                        } else if let Some(ch) = translate_key(keycode) {
                            buffer.push(ch);
                        }
                    }
                }
                if !had_events {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Err(err) => {
                if err.kind() == ErrorKind::WouldBlock {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                if err.kind() == ErrorKind::Interrupted {
                    continue;
                }
                if let Some(raw) = err.raw_os_error() {
                    eprintln!("read error (os code {raw}): {err}");
                } else {
                    eprintln!("read error: {err}");
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    }

    Ok(())
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

fn lookup_and_print_release(discogs: &DiscogsClient, barcode: &str) -> Result<()> {
    let id: u32 = barcode
        .parse()
        .with_context(|| format!("invalid numeric barcode: {barcode}"))?;

    let start = std::time::Instant::now();
    let release = discogs.release(id)?;
    print_release(&release, barcode, start.elapsed());
    Ok(())
}

fn print_release(release: &Release, barcode: &str, elapsed: Duration) {
    println!("========================================");
    if let Some(title) = &release.title {
        println!("Title:   {title}");
    }

    if let Some(artists) = &release.artists {
        if !artists.is_empty() {
            let names = artists
                .iter()
                .filter_map(|a| a.name.as_deref())
                .collect::<Vec<_>>()
                .join(", ");
            if !names.is_empty() {
                println!("Artist:  {names}");
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
                println!("Label:   {labels_str}");
            }
        }
    }

    if let Some(year) = release.year {
        if year > 0 {
            println!("Year:    {year}");
        }
    }
    if let Some(country) = &release.country {
        if !country.is_empty() {
            println!("Country: {country}");
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
                println!("Format:  {formats_str}");
            }
        }
    }
    if let Some(genres) = &release.genres {
        if !genres.is_empty() {
            println!("Genres:  {}", genres.join(", "));
        }
    }
    if let Some(styles) = &release.styles {
        if !styles.is_empty() {
            println!("Styles:  {}", styles.join(", "));
        }
    }

    if let Some(tracks) = &release.tracklist {
        if !tracks.is_empty() {
            println!("Tracklist:");
            let limit = tracks.iter().take(10);
            for track in limit {
                let mut title = track.title.clone().unwrap_or_default();
                if let Some(duration) = &track.duration {
                    if !duration.is_empty() {
                        let _ = write!(title, " [{duration}]");
                    }
                }
                println!("  {} {}", track.position.as_deref().unwrap_or(""), title);
            }
            if tracks.len() > 10 {
                println!("  ... and {} more tracks", tracks.len() - 10);
            }
        }
    }

    println!("Barcode: {barcode}");
    if let Some(uri) = &release.uri {
        println!("Discogs: {uri}");
    }
    println!("Lookup time: {} ms", elapsed.as_millis());
    println!("========================================\n");
}
