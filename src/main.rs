use anyhow::Result;
use clap::Parser;
use eframe;
use std::sync::{Arc, atomic::AtomicBool};

mod barcode_reader;
mod discogs;
mod ui;

use crate::barcode_reader::{BarcodeReader, ScanEvent};
use crate::discogs::DiscogsClient;
use crate::ui::RecordShelfApp;

#[derive(Parser, Debug, Clone)]
#[command(name = "recordshelf-rust")]
pub struct Args {
    /// Path to the barcode scanner input device
    #[arg(long, default_value = "/dev/input/event17")]
    pub device: String,

    /// Discogs API user agent
    #[arg(long = "user-agent", default_value = "RecordShelf Barcode Client")]
    pub user_agent: String,

    /// Discogs personal access token
    #[arg(long, default_value = "plQPlhPdTnGxLSfkEKNlAcRqBOxnPQeMlJHQaneg")]
    pub token: String,

    /// Discogs marketplace currency
    #[arg(long, default_value = "EUR")]
    pub currency: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let discogs = DiscogsClient::new(&args.user_agent, &args.token, &args.currency)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<ScanEvent>();

    BarcodeReader::spawn(args.device.clone(), discogs, stop.clone(), tx)?;

    let app = RecordShelfApp::new(args, rx, stop);
    let native_options = eframe::NativeOptions::default();
    eframe::run_native("RecordShelf", native_options, Box::new(|_cc| Box::new(app)))
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}
