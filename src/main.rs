use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

mod barcode_reader;
mod config;
mod controls;
mod decks;
mod discogs;
mod hhv;
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
        ];
        for (value, target) in overrides {
            if let Some(v) = value {
                *target = v.clone();
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args.config.clone().or_else(Settings::default_path);

    let mut settings = match &config_path {
        Some(path) => Settings::load(path)?,
        None => Settings::default(),
    };
    args.apply_to(&mut settings);

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
