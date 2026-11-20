use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::types::AppSettings;

const KEYCHAIN_SERVICE: &str = "stitch-rs";
const KEY_COPILOT_TOKEN: &str = "copilot_token";
const KEY_AZURE_PAT: &str = "azure_pat";
const KEY_GITHUB_TOKEN: &str = "github_token";
const KEY_CONFLUENCE_TOKEN: &str = "confluence_token";

fn normalize_secret(value: Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

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

        settings.copilot_token = normalize_secret(settings.copilot_token.take());
        settings.connectors.azure_dev_ops.pat =
            normalize_secret(settings.connectors.azure_dev_ops.pat.take());
        settings.connectors.github.token =
            normalize_secret(settings.connectors.github.token.take());
        settings.connectors.atlassian.token =
            normalize_secret(settings.connectors.atlassian.token.take());

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

        settings.prompts.story_writer.general =
            settings.prompts.story_writer.general.trim().to_string();
        settings.prompts.story_writer.title =
            settings.prompts.story_writer.title.trim().to_string();
        settings.prompts.story_writer.description =
            settings.prompts.story_writer.description.trim().to_string();
        settings.prompts.story_writer.acceptance_criteria = settings
            .prompts
            .story_writer
            .acceptance_criteria
            .trim()
            .to_string();
        settings.prompts.story_writer.notes =
            settings.prompts.story_writer.notes.trim().to_string();

        settings.prompts.test_case_writer.general =
            settings.prompts.test_case_writer.general.trim().to_string();
        settings.prompts.test_case_writer.description = settings
            .prompts
            .test_case_writer
            .description
            .trim()
            .to_string();
        settings.prompts.test_case_writer.pre_conditions = settings
            .prompts
            .test_case_writer
            .pre_conditions
            .trim()
            .to_string();
        settings.prompts.test_case_writer.steps =
            settings.prompts.test_case_writer.steps.trim().to_string();
        settings.prompts.test_case_writer.expected_result = settings
            .prompts
            .test_case_writer
            .expected_result
            .trim()
            .to_string();

        settings.prompts.story_elaborator.general =
            settings.prompts.story_elaborator.general.trim().to_string();

        for persona in &mut settings.pr_reviewer.personas {
            persona.name = persona.name.trim().to_string();
            persona.guidelines = persona
                .guidelines
                .replace(['\r', '\n'], " ")
                .trim()
                .to_string();
        }
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
            match self.load_keychain_secret(KEY_AZURE_PAT) {
                Ok(pat) => settings.connectors.azure_dev_ops.pat = pat,
                Err(err) => {
                    eprintln!("Warning: failed to read azure_pat from OS Keychain: {err}");
                }
            }
            match self.load_keychain_secret(KEY_GITHUB_TOKEN) {
                Ok(token) => settings.connectors.github.token = token,
                Err(err) => {
                    eprintln!("Warning: failed to read github_token from OS Keychain: {err}");
                }
            }
            match self.load_keychain_secret(KEY_CONFLUENCE_TOKEN) {
                Ok(token) => settings.connectors.atlassian.token = token,
                Err(err) => {
                    eprintln!("Warning: failed to read confluence_token from OS Keychain: {err}");
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
            self.save_keychain_secret(KEY_AZURE_PAT, settings.connectors.azure_dev_ops.pat.as_deref())?;
            self.save_keychain_secret(KEY_GITHUB_TOKEN, settings.connectors.github.token.as_deref())?;
            self.save_keychain_secret(
                KEY_CONFLUENCE_TOKEN,
                settings.connectors.atlassian.token.as_deref(),
            )?;
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
        settings.connectors.github.token = Some("  ghp_abc  ".to_string());
        settings.connectors.azure_dev_ops.pat = Some("   ".to_string());
        settings.connectors.atlassian.token = Some("  atl_tok\n".to_string());
        settings.prompts.story_writer.general = "  Focus on APIs  ".to_string();
        settings.prompts.story_writer.title = "  Concise title\n".to_string();
        settings.prompts.story_writer.description = "  Standard format ".to_string();
        settings.prompts.story_writer.acceptance_criteria = "  Bullets ".to_string();
        settings.prompts.story_writer.notes = "  Risks  ".to_string();
        settings.prompts.test_case_writer.general = "  Edge cases  ".to_string();
        settings.prompts.story_elaborator.general = "  Plan steps  ".to_string();

        SettingsStore::trim_settings(&mut settings);

        assert_eq!(settings.feature_type, "Feature");
        assert_eq!(settings.story_type, "User Story");
        assert_eq!(settings.task_type, "Task");
        assert_eq!(settings.test_task_title, "QA");
        assert_eq!(settings.copilot_model, "auto");
        assert_eq!(settings.copilot_token, None);
        assert_eq!(settings.git_worktree_base_dir, None);
        assert_eq!(settings.connectors.github.owner, "octocat");
        assert_eq!(settings.connectors.github.token.as_deref(), Some("ghp_abc"));
        assert_eq!(settings.connectors.azure_dev_ops.pat, None);
        assert_eq!(
            settings.connectors.atlassian.token.as_deref(),
            Some("atl_tok")
        );
        assert_eq!(settings.prompts.story_writer.general, "Focus on APIs");
        assert_eq!(settings.prompts.story_writer.title, "Concise title");
        assert_eq!(settings.prompts.story_writer.description, "Standard format");
        assert_eq!(settings.prompts.story_writer.acceptance_criteria, "Bullets");
        assert_eq!(settings.prompts.story_writer.notes, "Risks");
        assert_eq!(settings.prompts.test_case_writer.general, "Edge cases");
        assert_eq!(settings.prompts.story_elaborator.general, "Plan steps");
    }

    #[test]
    fn save_and_load_round_trip_skips_all_secrets_in_json() {
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
        settings.connectors.azure_dev_ops.org = "  https://dev.azure.com/myorg  ".to_string();
        settings.connectors.azure_dev_ops.project = "  MyProject  ".to_string();
        settings.connectors.azure_dev_ops.pat = Some("  azdo_secret_pat_456  ".to_string());
        settings.connectors.atlassian.url = "  https://example.atlassian.net/wiki  ".to_string();
        settings.connectors.atlassian.username = "  alice@example.com  ".to_string();
        settings.connectors.atlassian.token = Some("  confluence_secret_789  ".to_string());
        settings.connectors.github.owner = "  my-org  ".to_string();
        settings.connectors.github.token = Some("  ghp_connector_secret_000  ".to_string());
        settings.sources.issues = "  azureDevOps ".to_string();
        settings.sources.code = " github ".to_string();
        settings.sources.docs = " atlassian ".to_string();

        store.save(&mut settings).expect("save settings");

        assert_eq!(
            settings.copilot_token.as_deref(),
            Some("ghp_super_secret_token_123")
        );
        assert_eq!(
            settings.connectors.azure_dev_ops.pat.as_deref(),
            Some("azdo_secret_pat_456")
        );
        assert_eq!(
            settings.connectors.atlassian.token.as_deref(),
            Some("confluence_secret_789")
        );
        assert_eq!(
            settings.connectors.github.token.as_deref(),
            Some("ghp_connector_secret_000")
        );

        let raw_json = std::fs::read_to_string(&path).expect("read settings.json");
        assert!(!raw_json.contains("ghp_super_secret_token_123"));
        assert!(!raw_json.contains("copilotToken"));
        assert!(!raw_json.contains("azdo_secret_pat_456"));
        assert!(!raw_json.contains("confluence_secret_789"));
        assert!(!raw_json.contains("ghp_connector_secret_000"));

        let loaded = store.load();
        assert_eq!(loaded.theme, ThemeMode::Dark);
        assert!(loaded.git_worktree_enabled);
        assert_eq!(
            loaded.git_worktree_base_dir,
            Some(PathBuf::from("/tmp/stitch-worktrees"))
        );
        assert_eq!(loaded.copilot_model, "claude-sonnet-4.5");
        assert_eq!(loaded.copilot_token, None);
        assert_eq!(
            loaded.connectors.azure_dev_ops.org,
            "https://dev.azure.com/myorg"
        );
        assert_eq!(loaded.connectors.azure_dev_ops.project, "MyProject");
        assert_eq!(loaded.connectors.azure_dev_ops.pat, None);
        assert_eq!(
            loaded.connectors.atlassian.url,
            "https://example.atlassian.net/wiki"
        );
        assert_eq!(loaded.connectors.atlassian.username, "alice@example.com");
        assert_eq!(loaded.connectors.atlassian.token, None);
        assert_eq!(loaded.connectors.github.owner, "my-org");
        assert_eq!(loaded.connectors.github.token, None);
        assert_eq!(loaded.sources.issues, "azureDevOps");
        assert_eq!(loaded.sources.code, "github");
        assert_eq!(loaded.sources.docs, "atlassian");

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
