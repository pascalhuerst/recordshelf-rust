use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

mod barcode_reader;
mod config;
mod controls;
mod decks;
mod discogs;
mod hhv;
mod label;
mod printer;
mod shop_bridge;
mod shops;
mod ui;
mod worker;

use crate::config::Settings;
use crate::ui::RecordShelfApp;

/// Command line arguments. Anything given here overrides the settings file
/// for this run only (nothing is written back).
#[derive(Parser, Debug, Clone)]
#[command(name = "recordshelf-rust")]
pub struct Args {
    /// Path to the settings file (default: ~/.config/recordshelf/config.toml)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Path to the barcode scanner input device
    #[arg(long)]
    pub device: Option<String>,

    /// Name of the barcode scanner input device (substring match)
    #[arg(long)]
    pub device_name: Option<String>,

    /// Discogs API user agent
    #[arg(long = "user-agent")]
    pub user_agent: Option<String>,

    /// Discogs personal access token
    #[arg(long, env = "DISCOGS_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Discogs marketplace currency
    #[arg(long)]
    pub currency: Option<String>,

    /// Discogs username for collection actions
    #[arg(long)]
    pub username: Option<String>,

    /// Write label previews (PNG) into this directory and exit
    #[arg(long, value_name = "DIR")]
    pub label_preview: Option<PathBuf>,

    /// Release id for --label-preview (fetched from Discogs); a built-in sample otherwise
    #[arg(long, value_name = "ID", requires = "label_preview")]
    pub label_release: Option<u32>,

    /// Label printer host name or IP address
    #[arg(long)]
    pub printer_host: Option<String>,

    /// Label printer port (raw ZPL)
    #[arg(long)]
    pub printer_port: Option<u16>,

    /// Print one calibration label (frame and ruler) to see how the stock sits, then exit
    #[arg(long)]
    pub print_calibration: bool,

    /// Print test labels with barcodes of these heights in mm (e.g. 5,4,3), then exit
    #[arg(long, value_name = "MM,MM,...", value_delimiter = ',')]
    pub print_barcode_test: Vec<f32>,

    /// Start in a normal window instead of fullscreen (F11 toggles at runtime)
    #[arg(long)]
    pub windowed: bool,

    /// Enable the shop browser bridge for decks.de and hhv.de (localhost server for the userscript)
    #[arg(long, visible_alias = "decks-bridge")]
    pub shop_bridge: bool,

    /// Port of the shop bridge
    #[arg(long, visible_alias = "decks-port", default_value_t = shop_bridge::DEFAULT_PORT)]
    pub shop_port: u16,
}

impl Args {
    /// Applies command line overrides on top of loaded settings.
    pub fn apply_to(&self, settings: &mut Settings) {
        let overrides = [
            (&self.device, &mut settings.device_path),
            (&self.device_name, &mut settings.device_name),
            (&self.user_agent, &mut settings.user_agent),
            (&self.token, &mut settings.token),
            (&self.currency, &mut settings.currency),
            (&self.username, &mut settings.username),
            (&self.printer_host, &mut settings.printer_host),
        ];
        for (value, target) in overrides {
            if let Some(v) = value {
                *target = v.clone();
            }
        }
    }
}

/// Prints calibration / barcode test labels on the configured printer.
fn print_tests(args: &Args, settings: &Settings) -> Result<()> {
    anyhow::ensure!(
        !settings.printer_host.trim().is_empty(),
        "no printer configured: use --printer-host or printer_host in the settings file"
    );
    let printer = printer::Printer::new(&settings.printer_host, settings.printer_port);
    let status = printer.status()?;
    println!(
        "printer {}:{} ready: label length {} dots, {}",
        printer.host,
        printer.port,
        status.label_length,
        if status.thermal_transfer {
            "thermal transfer"
        } else {
            "direct thermal"
        }
    );
    if args.print_calibration {
        // the printer's whole print area: 448 dots wide, as long as its label
        println!("printing the calibration label");
        printer.print(&printer::calibration_zpl(448, status.label_length))?;
    }
    for mm in &args.print_barcode_test {
        // test labels are the 51 x 38 mm ones
        let size = label::LabelSize::from_mm(51.0, 38.0);
        let mut options = label::LabelOptions::new(label::CodeKind::Barcode);
        options.barcode_height = (mm * 8.0).round() as u32;
        options.caption = Some(format!("{mm}mm"));
        let img = label::render_with(&label::sample_content(), size, &options)?;
        println!(
            "printing a label with a {mm} mm barcode ({} dots)",
            options.barcode_height
        );
        printer.print(&printer::bitmap_zpl(&img, 0, 0))?;
        // let the printer finish this label before the next job arrives
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args.config.clone().or_else(Settings::default_path);

    let mut settings = match &config_path {
        Some(path) => Settings::load(path)?,
        None => Settings::default(),
    };
    args.apply_to(&mut settings);

    if let Some(port) = args.printer_port {
        settings.printer_port = port;
    }
    if args.print_calibration || !args.print_barcode_test.is_empty() {
        return print_tests(&args, &settings);
    }

    if let Some(dir) = &args.label_preview {
        let content = match args.label_release {
            Some(id) => {
                let client = discogs::DiscogsClient::new(
                    &settings.user_agent,
                    &settings.token,
                    &settings.currency,
                )?;
                label::LabelContent::from_release(id, &client.release(id)?)
            }
            None => label::sample_content(),
        };
        for file in label::write_previews(dir, &content)? {
            println!("{}", file.display());
        }
        return Ok(());
    }

    let bridge_port = args.shop_bridge.then_some(args.shop_port);
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_fullscreen(!args.windowed),
        ..Default::default()
    };
    eframe::run_native(
        "RecordShelf",
        native_options,
        Box::new(move |cc| {
            Box::new(RecordShelfApp::new(
                cc.egui_ctx.clone(),
                settings,
                config_path,
                bridge_port,
            ))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
    .context("run UI")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::parse_from(std::iter::once("recordshelf-rust").chain(args.iter().copied()))
    }

    #[test]
    fn no_flags_leave_settings_untouched() {
        let mut settings = Settings {
            username: "file-user".into(),
            ..Settings::default()
        };
        let before = settings.clone();
        parse(&[]).apply_to(&mut settings);
        assert_eq!(settings, before);
    }

    #[test]
    fn flags_override_settings() {
        let mut settings = Settings {
            username: "file-user".into(),
            currency: "EUR".into(),
            ..Settings::default()
        };
        parse(&[
            "--username",
            "cli-user",
            "--device",
            "/dev/input/event3",
            "--device-name",
            "Scanner",
            "--token",
            "abc",
            "--currency",
            "USD",
            "--user-agent",
            "UA",
        ])
        .apply_to(&mut settings);
        assert_eq!(settings.username, "cli-user");
        assert_eq!(settings.device_path, "/dev/input/event3");
        assert_eq!(settings.device_name, "Scanner");
        assert_eq!(settings.token, "abc");
        assert_eq!(settings.currency, "USD");
        assert_eq!(settings.user_agent, "UA");
    }

    #[test]
    fn starts_fullscreen_unless_windowed_is_given() {
        assert!(!parse(&[]).windowed);
        assert!(parse(&["--windowed"]).windowed);
    }
}
