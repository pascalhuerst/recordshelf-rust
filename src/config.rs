use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persistent user settings, stored as TOML in the user's config directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        }
    }
}

impl Settings {
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
}
