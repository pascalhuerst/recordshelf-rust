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
    /// Print speed in inches per second (slower prints darker); 0 keeps the printer's own setting
    pub printer_speed_ips: u8,
    /// Size of the label stock in mm (the test labels are 51 x 38, the real ones 52 x 39)
    pub label_width_mm: f32,
    pub label_height_mm: f32,
    /// Code on the label: "barcode" (Code 128) or "qr"
    pub label_code: String,
    /// Styles as tags on the label: "off", "outline" or "filled"
    pub label_tags: String,
    /// Parts of the label that are left out: "header", "artist", "title", "tracks", "code"
    pub label_hidden: Vec<String>,
    /// How many screen points make an inch, for the label preview at real size (96 is usual)
    pub preview_ppi: f32,
    /// PostgreSQL connection string of our own database (written by `db/db.sh bootstrap`)
    pub database_url: String,
    /// Keep our database in line with the Discogs collection automatically (key U switches)
    pub library_auto_sync: bool,
    /// Put every label that was printed on the active stack (key J switches)
    pub stack_auto_add: bool,
    /// With --shop-bridge: open the shop pages (decks.de, hhv.de) in the browser at the start
    pub shop_open_tabs: bool,
    /// Browser command for those pages (empty: the system's default browser)
    pub shop_browser: String,
    /// Settings this version does not know (hand written, or from a newer
    /// version) are kept when saving instead of being dropped.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, toml::Value>,
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
            printer_speed_ips: 3,
            label_width_mm: 51.0,
            label_height_mm: 38.0,
            label_code: "barcode".to_string(),
            label_tags: "off".to_string(),
            label_hidden: Vec::new(),
            preview_ppi: 96.0,
            database_url: String::new(),
            library_auto_sync: true,
            stack_auto_add: true,
            shop_open_tabs: true,
            shop_browser: String::new(),
            extra: std::collections::BTreeMap::new(),
        }
    }
}

fn clamp_mm(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
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

    /// The same settings with every value brought into its valid range and
    /// trimmed, as typed into the settings dialog or edited by hand.
    pub fn normalized(mut self) -> Self {
        for text in [
            &mut self.token,
            &mut self.username,
            &mut self.user_agent,
            &mut self.currency,
            &mut self.device_name,
            &mut self.device_path,
            &mut self.printer_host,
            &mut self.database_url,
            &mut self.shop_browser,
        ] {
            *text = text.trim().to_string();
        }
        self.zoom_factor = self.zoom();
        if self.printer_port == 0 {
            self.printer_port = crate::printer::DEFAULT_PORT;
        }
        self.printer_darkness = self.printer_darkness.clamp(-30, 30);
        self.printer_speed_ips = self.printer_speed_ips.min(6);
        // the print area is 56 mm wide; labels smaller than 20 mm make no sense
        self.label_width_mm = clamp_mm(self.label_width_mm, 20.0, 56.0, 51.0);
        self.label_height_mm = clamp_mm(self.label_height_mm, 20.0, 150.0, 38.0);
        self.label_top_mm = clamp_mm(self.label_top_mm, 0.0, 10.0, 1.5);
        self.label_code = match self.label_code.trim().to_lowercase().as_str() {
            "qr" => "qr".to_string(),
            _ => "barcode".to_string(),
        };
        self.preview_ppi = clamp_mm(self.preview_ppi, 40.0, 400.0, 96.0);
        self.label_tags = match self.label_tags.trim().to_lowercase().as_str() {
            "outline" => "outline".to_string(),
            "filled" => "filled".to_string(),
            _ => "off".to_string(),
        };
        let mut hidden: Vec<String> = Vec::new();
        for name in &self.label_hidden {
            let name = name.trim().to_lowercase();
            if crate::label::PARTS.iter().any(|(n, _)| *n == name) && !hidden.contains(&name) {
                hidden.push(name);
            }
        }
        self.label_hidden = hidden;
        self
    }

    /// Which parts of the label are drawn.
    pub fn label_parts(&self) -> crate::label::LabelParts {
        crate::label::LabelParts::without(&self.label_hidden)
    }

    /// How the styles are drawn on the label, if at all.
    pub fn tag_style(&self) -> Option<crate::label::TagStyle> {
        match self.label_tags.as_str() {
            "outline" => Some(crate::label::TagStyle::Outline),
            "filled" => Some(crate::label::TagStyle::Filled),
            _ => None,
        }
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
    fn hidden_label_parts_are_cleaned_up() {
        let s = Settings {
            label_hidden: vec![
                " Tracks ".into(),
                "bogus".into(),
                "tracks".into(),
                "code".into(),
            ],
            ..Settings::default()
        }
        .normalized();
        assert_eq!(s.label_hidden, ["tracks", "code"]);
        let parts = s.label_parts();
        assert!(!parts.tracks && !parts.code && parts.header && parts.artist && parts.title);
        assert_eq!(
            Settings::default().label_parts(),
            crate::label::LabelParts::default()
        );
    }

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
        assert_eq!(s.printer_speed_ips, 3);
        assert_eq!((s.label_width_mm, s.label_height_mm), (51.0, 38.0));
        assert_eq!(s.label_code, "barcode");
        assert!(s.library_auto_sync && s.database_url.is_empty());
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

    #[test]
    fn normalized_trims_and_clamps() {
        let s = Settings {
            token: "  abc \n".into(),
            printer_host: " 192.168.4.55 ".into(),
            printer_port: 0,
            printer_darkness: 90,
            printer_speed_ips: 42,
            label_width_mm: 80.0,
            label_height_mm: 5.0,
            label_top_mm: -3.0,
            label_code: " QR ".into(),
            zoom_factor: 10.0,
            ..Settings::default()
        }
        .normalized();
        assert_eq!(s.token, "abc");
        assert_eq!(s.printer_host, "192.168.4.55");
        assert_eq!(s.printer_port, 9100);
        assert_eq!(s.printer_darkness, 30);
        assert_eq!(s.printer_speed_ips, 6);
        assert_eq!(
            (s.label_width_mm, s.label_height_mm, s.label_top_mm),
            (56.0, 20.0, 0.0)
        );
        assert_eq!(s.label_code, "qr");
        assert_eq!(s.zoom_factor, 4.0);
    }

    #[test]
    fn normalized_replaces_nonsense_and_keeps_good_values() {
        let s = Settings {
            label_width_mm: f32::NAN,
            label_height_mm: f32::INFINITY,
            label_code: "hologram".into(),
            printer_darkness: -30,
            ..Settings::default()
        }
        .normalized();
        assert_eq!(s.label_width_mm, 51.0);
        assert_eq!(s.label_height_mm, 38.0);
        assert_eq!(s.label_code, "barcode");
        assert_eq!(s.printer_darkness, -30);
        let good = Settings::default();
        assert_eq!(
            good.clone().normalized(),
            good,
            "valid settings stay as they are"
        );
        let tuned = Settings {
            label_width_mm: 52.0,
            label_height_mm: 39.0,
            ..Settings::default()
        };
        assert_eq!(tuned.clone().normalized(), tuned);
    }

    #[test]
    fn unknown_settings_survive_loading_and_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "username = \"me\"\nfuture_setting = 42\nnotes = \"keep me\"\n",
        )
        .unwrap();
        let mut s = Settings::load(&path).unwrap();
        assert_eq!(s.username, "me");
        s.zoom_factor = 1.5;
        s.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("future_setting = 42"), "{text}");
        assert!(text.contains("notes = \"keep me\""), "{text}");
        assert!(text.contains("zoom_factor = 1.5"), "{text}");
        // and Settings::update_on_disk (used for zoom and folder) keeps them too
        Settings::save_zoom(&path, 2.0).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("future_setting = 42")
        );
    }

    #[test]
    fn database_url_roundtrips_and_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let s = Settings {
            database_url: "postgres://recordshelf:pw@lightdesk.local/recordshelf".into(),
            ..Settings::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().database_url, s.database_url);
        let padded = Settings {
            database_url: "  postgres://x/y \n".into(),
            ..Settings::default()
        }
        .normalized();
        assert_eq!(padded.database_url, "postgres://x/y");
    }

    #[test]
    fn tags_on_the_label_are_off_by_default_and_normalised() {
        use crate::label::TagStyle;
        assert_eq!(Settings::default().label_tags, "off");
        assert_eq!(Settings::default().tag_style(), None);
        for (text, expected) in [
            (" Outline ", Some(TagStyle::Outline)),
            ("FILLED", Some(TagStyle::Filled)),
            ("off", None),
            ("rainbow", None),
            ("", None),
        ] {
            let s = Settings {
                label_tags: text.into(),
                ..Settings::default()
            }
            .normalized();
            assert_eq!(s.tag_style(), expected, "{text:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Settings {
            label_tags: "filled".into(),
            ..Settings::default()
        }
        .save(&path)
        .unwrap();
        assert_eq!(
            Settings::load(&path).unwrap().tag_style(),
            Some(TagStyle::Filled)
        );
    }

    #[test]
    fn the_preview_scale_is_96_by_default_and_kept_sane() {
        assert_eq!(Settings::default().preview_ppi, 96.0);
        for (value, expected) in [
            (120.0, 120.0),
            (1.0, 40.0),
            (5000.0, 400.0),
            (f32::NAN, 96.0),
        ] {
            let s = Settings {
                preview_ppi: value,
                ..Settings::default()
            }
            .normalized();
            assert_eq!(s.preview_ppi, expected, "{value}");
        }
    }
}
