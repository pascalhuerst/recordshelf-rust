use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

mod barcode_reader;
mod config;
mod discogs;
mod ui;

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

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "RecordShelf",
        native_options,
        Box::new(move |cc| {
            Box::new(RecordShelfApp::new(
                cc.egui_ctx.clone(),
                settings,
                config_path,
            ))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
    .context("run UI")
}
