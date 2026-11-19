use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::types::AppSettings;

const KEYCHAIN_SERVICE: &str = "stitch-rs";
const KEY_COPILOT_TOKEN: &str = "copilot_token";

/// Persists non-secret preferences to `settings.json` and secrets to the OS Keychain.
pub struct SettingsStore {
    config_path: PathBuf,
    use_keychain: bool,
}

impl SettingsStore {
    pub fn new() -> Self {
        let dir = directories::ProjectDirs::from("com", "stitch", "Stitch")
            .map(|d| d.data_local_dir().to_path_buf())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".stitch"))
                    .unwrap_or_else(|| PathBuf::from(".stitch"))
            });

        Self {
            config_path: dir.join("settings.json"),
            use_keychain: true,
        }
    }

    #[cfg(test)]
    pub fn with_path_no_keychain(config_path: PathBuf) -> Self {
        Self {
            config_path,
            use_keychain: false,
        }
    }

    #[allow(dead_code)]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Normalizes string fields in-place before persistence or runtime use.
    pub fn trim_settings(settings: &mut AppSettings) {
        settings.feature_type = settings.feature_type.trim().to_string();
        settings.story_type = settings.story_type.trim().to_string();
        settings.task_type = settings.task_type.trim().to_string();
        settings.test_task_title = settings.test_task_title.trim().to_string();

        let trimmed_model = settings.copilot_model.trim();
        settings.copilot_model = if trimmed_model.is_empty() {
            "auto".to_string()
        } else {
            trimmed_model.to_string()
        };

        settings.connectors.atlassian.url = settings.connectors.atlassian.url.trim().to_string();
        settings.connectors.atlassian.username =
            settings.connectors.atlassian.username.trim().to_string();
        settings.connectors.azure_dev_ops.org =
            settings.connectors.azure_dev_ops.org.trim().to_string();
        settings.connectors.azure_dev_ops.project =
            settings.connectors.azure_dev_ops.project.trim().to_string();
        settings.connectors.github.owner = settings.connectors.github.owner.trim().to_string();

        settings.sources.issues = settings.sources.issues.trim().to_string();
        settings.sources.code = settings.sources.code.trim().to_string();
        settings.sources.docs = settings.sources.docs.trim().to_string();

        settings.copilot_token = settings
            .copilot_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        settings.git_worktree_base_dir = settings
            .git_worktree_base_dir
            .as_ref()
            .and_then(|path| {
                let s = path.to_string_lossy();
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(trimmed))
                }
            });
    }

    /// Loads `AppSettings` from disk and reads `copilot_token` from the OS Keychain.
    pub fn load(&self) -> AppSettings {
        let mut settings = match std::fs::read_to_string(&self.config_path) {
            Ok(raw) => match serde_json::from_str::<AppSettings>(&raw) {
                Ok(parsed) => parsed,
                Err(err) => {
                    eprintln!(
                        "Warning: failed to parse settings at {}: {err}. Using defaults.",
                        self.config_path.display()
                    );
                    AppSettings::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => AppSettings::default(),
            Err(err) => {
                eprintln!(
                    "Warning: failed to read settings at {}: {err}. Using defaults.",
                    self.config_path.display()
                );
                AppSettings::default()
            }
        };

        if self.use_keychain {
            match self.load_keychain_secret(KEY_COPILOT_TOKEN) {
                Ok(token) => settings.copilot_token = token,
                Err(err) => {
                    eprintln!("Warning: failed to read copilot_token from OS Keychain: {err}");
                }
            }
        }

        Self::trim_settings(&mut settings);
        settings
    }

    /// Trims fields, writes `settings.json` atomically, and updates `copilot_token` in OS Keychain.
    pub fn save(&self, settings: &mut AppSettings) -> anyhow::Result<()> {
        Self::trim_settings(settings);

        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create settings directory {}", parent.display())
            })?;
        }

        let json =
            serde_json::to_string_pretty(settings).context("failed to serialize AppSettings")?;
        let tmp_path = self.config_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json).with_context(|| {
            format!(
                "failed to write temporary settings file {}",
                tmp_path.display()
            )
        })?;
        std::fs::rename(&tmp_path, &self.config_path).with_context(|| {
            format!(
                "failed to persist settings file {}",
                self.config_path.display()
            )
        })?;

        if self.use_keychain {
            self.save_keychain_secret(KEY_COPILOT_TOKEN, settings.copilot_token.as_deref())?;
        }

        Ok(())
    }

    fn load_keychain_secret(&self, key: &str) -> anyhow::Result<Option<String>> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, key)
            .with_context(|| format!("failed to open keychain entry for {key}"))?;
        match entry.get_password() {
            Ok(secret) => {
                let trimmed = secret.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err).with_context(|| format!("failed to read {key} from keychain")),
        }
    }

    fn save_keychain_secret(&self, key: &str, value: Option<&str>) -> anyhow::Result<()> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, key)
            .with_context(|| format!("failed to open keychain entry for {key}"))?;
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(secret) => entry
                .set_password(secret)
                .with_context(|| format!("failed to store {key} in OS Keychain")),
            None => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(err) => {
                    Err(err).with_context(|| format!("failed to delete {key} from OS Keychain"))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ThemeMode;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_settings_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("stitch-rs-test-{name}-{nanos}"))
            .join("settings.json")
    }

    #[test]
    fn trim_settings_normalizes_whitespace_and_empty_defaults() {
        let mut settings = AppSettings {
            feature_type: "  Feature  ".to_string(),
            story_type: "  User Story ".to_string(),
            task_type: " Task\n".to_string(),
            test_task_title: "  QA  ".to_string(),
            copilot_model: "   ".to_string(),
            copilot_token: Some("   \t  ".to_string()),
            git_worktree_base_dir: Some(PathBuf::from("   ")),
            ..Default::default()
        };
        settings.connectors.github.owner = "  octocat  ".to_string();

        SettingsStore::trim_settings(&mut settings);

        assert_eq!(settings.feature_type, "Feature");
        assert_eq!(settings.story_type, "User Story");
        assert_eq!(settings.task_type, "Task");
        assert_eq!(settings.test_task_title, "QA");
        assert_eq!(settings.copilot_model, "auto");
        assert_eq!(settings.copilot_token, None);
        assert_eq!(settings.git_worktree_base_dir, None);
        assert_eq!(settings.connectors.github.owner, "octocat");
    }

    #[test]
    fn save_and_load_round_trip_skips_copilot_token_in_json() {
        let path = temp_settings_path("roundtrip");
        let store = SettingsStore::with_path_no_keychain(path.clone());

        let mut settings = AppSettings {
            theme: ThemeMode::Dark,
            git_worktree_enabled: true,
            git_worktree_base_dir: Some(PathBuf::from("  /tmp/stitch-worktrees  ")),
            copilot_model: "  claude-sonnet-4.5  ".to_string(),
            copilot_token: Some("  ghp_super_secret_token_123  ".to_string()),
            ..Default::default()
        };

        store.save(&mut settings).expect("save settings");

        assert_eq!(
            settings.copilot_token.as_deref(),
            Some("ghp_super_secret_token_123")
        );

        let raw_json = std::fs::read_to_string(&path).expect("read settings.json");
        assert!(!raw_json.contains("ghp_super_secret_token_123"));
        assert!(!raw_json.contains("copilotToken"));

        let loaded = store.load();
        assert_eq!(loaded.theme, ThemeMode::Dark);
        assert!(loaded.git_worktree_enabled);
        assert_eq!(
            loaded.git_worktree_base_dir,
            Some(PathBuf::from("/tmp/stitch-worktrees"))
        );
        assert_eq!(loaded.copilot_model, "claude-sonnet-4.5");
        assert_eq!(loaded.copilot_token, None);

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
