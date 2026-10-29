use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persistent user settings, stored as TOML in the user's config directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Discogs personal access token
    pub token: String,
    /// Discogs username for collection actions
    pub username: String,
    /// Discogs API user agent
    pub user_agent: String,
    /// Discogs marketplace currency
    pub currency: String,
    /// Name of the barcode scanner input device (substring match, case-insensitive)
    pub device_name: String,
    /// Fallback device path, used when no device matches `device_name`
    pub device_path: String,
    /// UI zoom (Ctrl +/- in the window); remembered between runs
    pub zoom_factor: f32,
    /// Discogs collection folder that new releases are added to (0 = not chosen yet)
    pub folder_id: u32,
    /// Label printer (Zebra, raw ZPL on TCP): host name or IP address
    pub printer_host: String,
    pub printer_port: u16,
    /// Distance of the label's top edge from the printer's print origin, in mm
    pub label_top_mm: f32,
    /// Darkness adjustment per label, relative to the printer's own setting
    pub printer_darkness: i8,
    /// Print speed in inches per second; 0 keeps the printer's own setting
    pub printer_speed_ips: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            token: String::new(),
            username: String::new(),
            user_agent: "RecordShelf Barcode Client".to_string(),
            currency: "EUR".to_string(),
            device_name: String::new(),
            device_path: String::new(),
            zoom_factor: 1.0,
            folder_id: 0,
            printer_host: String::new(),
            printer_port: crate::printer::DEFAULT_PORT,
            label_top_mm: 1.5,
            printer_darkness: 9,
            printer_speed_ips: 0,
        }
    }
}

/// Zoom limits, kept sane in case the file is edited by hand.
pub const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.5..=4.0;

impl Settings {
    /// The stored zoom, clamped to a usable range (NaN becomes 1.0).
    pub fn zoom(&self) -> f32 {
        if self.zoom_factor.is_finite() {
            self.zoom_factor
                .clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end())
        } else {
            1.0
        }
    }

    /// Changes some settings in the file on disk, leaving everything else as
    /// it is there (so command line overrides are not persisted).
    pub fn update_on_disk(path: &Path, change: impl FnOnce(&mut Self)) -> Result<()> {
        let mut on_disk = Self::load(path)?;
        change(&mut on_disk);
        on_disk.save(path)
    }

    pub fn save_zoom(path: &Path, zoom: f32) -> Result<()> {
        Self::update_on_disk(path, |s| s.zoom_factor = zoom)
    }

    /// Default location: `$XDG_CONFIG_HOME/recordshelf/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("recordshelf").join("config.toml"))
    }

    /// Loads settings from `path`. A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parse settings file {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read settings file {}", path.display())),
        }
    }

    /// Writes settings to `path`, creating parent directories. The file is
    /// only readable by the current user since it contains the API token.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialize settings")?;
        fs::write(path, text).with_context(|| format!("write settings file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .context("restrict settings file permissions")?;
        }
        Ok(())
    }

    /// Whether enough is configured to talk to Discogs.
    pub fn has_token(&self) -> bool {
        !self.token.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(!settings.has_token());
    }

    #[test]
    fn save_and_load_roundtrip_creating_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let settings = Settings {
            token: "secret".into(),
            username: "someone".into(),
            device_name: "Barcode Reader".into(),
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
        assert!(settings.has_token());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Settings::default().save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "username = \"me\"\n").unwrap();
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.username, "me");
        assert_eq!(settings.currency, "EUR");
    }

    #[test]
    fn invalid_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is = not [valid").unwrap();
        assert!(Settings::load(&path).is_err());
    }

    #[test]
    fn blank_token_counts_as_missing() {
        let settings = Settings {
            token: "   ".into(),
            ..Settings::default()
        };
        assert!(!settings.has_token());
    }

    #[test]
    fn zoom_defaults_to_one_and_is_clamped() {
        let mut s = Settings::default();
        assert_eq!(s.zoom(), 1.0);
        s.zoom_factor = 10.0;
        assert_eq!(s.zoom(), 4.0);
        s.zoom_factor = 0.0;
        assert_eq!(s.zoom(), 0.5);
        s.zoom_factor = f32::NAN;
        assert_eq!(s.zoom(), 1.0);
    }

    #[test]
    fn save_zoom_keeps_other_settings_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let on_disk = Settings {
            username: "me".into(),
            token: "disk-token".into(),
            ..Settings::default()
        };
        on_disk.save(&path).unwrap();
        Settings::save_zoom(&path, 1.5).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.zoom_factor, 1.5);
        assert_eq!(loaded.token, "disk-token");
        assert_eq!(loaded.username, "me");
    }

    #[test]
    fn save_zoom_creates_the_file_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        Settings::save_zoom(&path, 2.0).unwrap();
        assert_eq!(Settings::load(&path).unwrap().zoom_factor, 2.0);
    }

    #[test]
    fn update_on_disk_changes_only_what_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Settings {
            username: "me".into(),
            ..Settings::default()
        }
        .save(&path)
        .unwrap();
        Settings::update_on_disk(&path, |s| s.folder_id = 7).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.folder_id, 7);
        assert_eq!(loaded.username, "me");
        assert_eq!(loaded.zoom_factor, 1.0);
    }
}

#[cfg(test)]
mod printer_tests {
    use super::*;

    #[test]
    fn printer_defaults() {
        let s = Settings::default();
        assert_eq!(s.printer_port, 9100);
        assert_eq!(s.printer_darkness, 9);
        assert_eq!(s.printer_speed_ips, 0);
        assert_eq!(s.label_top_mm, 1.5);
    }

    #[test]
    fn printer_settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let s = Settings {
            printer_host: "192.168.4.55".into(),
            printer_darkness: -3,
            printer_speed_ips: 4,
            ..Settings::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
    }
}
