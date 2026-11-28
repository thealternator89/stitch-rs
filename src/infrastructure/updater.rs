use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use url::Url;

use crate::infrastructure::constants::{APP_VERSION, UPDATE_MANIFEST_URL};
use crate::types::UpdateSettings;

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(150);
const PROGRESS_EMIT_BYTES: u64 = 256 * 1024;
const ALLOWED_GITHUB_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

const ENV_DEV_MODE: &str = "STITCH_UPDATER_DEV_MODE";
const ENV_DEV_MANIFEST_URL: &str = "STITCH_UPDATER_MANIFEST_URL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdaterMode {
    Disabled { reason: String },
    NotifyOnly,
    FullAutoUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManifest {
    pub schema_version: u32,
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub published_at: Option<String>,
    pub release_url: String,
    #[serde(default)]
    pub platforms: HashMap<String, PlatformAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformAsset {
    pub asset: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReleaseInfo {
    pub version: String,
    pub tag: String,
    pub release_url: String,
    pub asset: Option<PlatformAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    Disabled {
        reason: String,
    },
    Idle,
    Checking,
    UpToDate {
        checked_at: DateTime<Utc>,
    },
    Available {
        info: UpdateReleaseInfo,
        can_auto_install: bool,
    },
    Downloading {
        info: UpdateReleaseInfo,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    ReadyToInstall {
        info: UpdateReleaseInfo,
        installer_path: PathBuf,
    },
    Installing {
        info: UpdateReleaseInfo,
    },
    Error {
        message: String,
        info: Option<UpdateReleaseInfo>,
        checked_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    UpdateAvailable(UpdateReleaseInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationSummary {
    pub version: String,
    pub manifest_url: String,
    pub updates_dir: PathBuf,
}

pub fn current_platform_key() -> Option<&'static str> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some("windows-x86_64")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("macos-aarch64")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some("macos-x86_64")
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
    )))]
    {
        None
    }
}

pub fn parse_semver(raw: &str) -> anyhow::Result<semver::Version> {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    semver::Version::parse(stripped)
        .with_context(|| format!("invalid SemVer version '{raw}'"))
}

#[cfg(test)]
pub fn is_newer_version(current: &str, candidate: &str) -> anyhow::Result<bool> {
    let cur = parse_semver(current)?;
    let cand = parse_semver(candidate)?;
    Ok(cand > cur)
}

fn is_valid_sha256_hex(hex_str: &str) -> bool {
    hex_str.len() == 64 && hex_str.bytes().all(|b| b.is_ascii_hexdigit())
}

fn validate_url(raw: &str, allow_local_http: bool) -> anyhow::Result<Url> {
    let parsed = Url::parse(raw.trim()).with_context(|| format!("invalid URL '{raw}'"))?;
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow::anyhow!("URL '{raw}' is missing a host"))?;

    if allow_local_http
        && parsed.scheme() == "http"
        && matches!(host.as_str(), "localhost" | "127.0.0.1")
    {
        return Ok(parsed);
    }

    if parsed.scheme() != "https" {
        anyhow::bail!("URL '{raw}' must use HTTPS");
    }

    if !ALLOWED_GITHUB_HOSTS.contains(&host.as_str()) {
        anyhow::bail!("URL '{raw}' host '{host}' is not an allowed GitHub host");
    }

    Ok(parsed)
}

fn default_updates_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "stitch", "Stitch")
        .map(|d| d.data_local_dir().join("updates"))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".stitch").join("updates"))
                .unwrap_or_else(|| PathBuf::from(".stitch").join("updates"))
        })
}

fn sanitize_version_for_filename(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(windows)]
fn has_windows_uninstaller_sibling() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("uninstall.exe").is_file()))
        .unwrap_or(false)
}

fn resolve_mode_from_env() -> UpdaterMode {
    if let Ok(raw) = std::env::var(ENV_DEV_MODE) {
        match raw.trim().to_ascii_lowercase().as_str() {
            "notify" => return UpdaterMode::NotifyOnly,
            "full" => return UpdaterMode::FullAutoUpdate,
            "off" | "disabled" | "false" | "0" => {
                return UpdaterMode::Disabled {
                    reason: format!("Disabled via {ENV_DEV_MODE}"),
                };
            }
            other if !other.is_empty() => {
                eprintln!(
                    "[updater] Ignoring unknown {ENV_DEV_MODE}='{other}' (expected 'notify', 'full', or 'off')."
                );
            }
            _ => {}
        }
    }

    if cfg!(debug_assertions) {
        return UpdaterMode::Disabled {
            reason: "Disabled in development mode (debug build)".to_string(),
        };
    }

    if std::env::var_os("CARGO").is_some() {
        return UpdaterMode::Disabled {
            reason: "Disabled in development mode (cargo run)".to_string(),
        };
    }

    #[cfg(target_os = "windows")]
    {
        if !has_windows_uninstaller_sibling() {
            return UpdaterMode::Disabled {
                reason: "Disabled for unpackaged Windows executable (no uninstall.exe)".to_string(),
            };
        }
        #[cfg(target_arch = "x86_64")]
        {
            return UpdaterMode::FullAutoUpdate;
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            return UpdaterMode::NotifyOnly;
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        UpdaterMode::NotifyOnly
    }
}

pub struct UpdaterService {
    mode: UpdaterMode,
    manifest_url: String,
    allow_local_http: bool,
    updates_dir: PathBuf,
    http_client: reqwest::Client,
}

impl UpdaterService {
    pub fn new() -> Self {
        let mode = resolve_mode_from_env();
        let dev_override_active = std::env::var(ENV_DEV_MODE)
            .ok()
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "notify" | "full"));

        let (manifest_url, allow_local_http) = if dev_override_active {
            match std::env::var(ENV_DEV_MANIFEST_URL) {
                Ok(custom) if !custom.trim().is_empty() => (custom.trim().to_string(), true),
                _ => (UPDATE_MANIFEST_URL.to_string(), false),
            }
        } else {
            (UPDATE_MANIFEST_URL.to_string(), false)
        };

        let http_client = reqwest::Client::builder()
            .user_agent(format!("Stitch/{APP_VERSION}"))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            mode,
            manifest_url,
            allow_local_http,
            updates_dir: default_updates_dir(),
            http_client,
        }
    }

    #[cfg(test)]
    pub fn with_test_config(
        mode: UpdaterMode,
        manifest_url: String,
        allow_local_http: bool,
        updates_dir: PathBuf,
    ) -> Self {
        Self {
            mode,
            manifest_url,
            allow_local_http,
            updates_dir,
            http_client: reqwest::Client::new(),
        }
    }

    pub fn mode(&self) -> &UpdaterMode {
        &self.mode
    }

    pub fn is_disabled(&self) -> bool {
        matches!(self.mode, UpdaterMode::Disabled { .. })
    }

    pub fn can_auto_install(&self) -> bool {
        matches!(self.mode, UpdaterMode::FullAutoUpdate)
    }

    pub fn validate_config(&self, _settings: &UpdateSettings) -> Result<ConfigValidationSummary, String> {
        let parsed_ver =
            parse_semver(APP_VERSION).map_err(|e| format!("invalid APP_VERSION: {e}"))?;
        let parsed_url = validate_url(&self.manifest_url, self.allow_local_http)
            .map_err(|e| format!("invalid manifest URL: {e}"))?;

        if self.updates_dir.as_os_str().is_empty() {
            return Err("updates cache directory path is empty".to_string());
        }

        Ok(ConfigValidationSummary {
            version: parsed_ver.to_string(),
            manifest_url: parsed_url.to_string(),
            updates_dir: self.updates_dir.clone(),
        })
    }

    pub fn log_dev_disabled_startup(&self, settings: &UpdateSettings) {
        match self.validate_config(settings) {
            Ok(summary) => {
                eprintln!(
                    "[updater] Config valid, updater disabled in development mode (version={}, manifest={}, auto_check={}, auto_download={}). Set {ENV_DEV_MODE}=notify|full to override.",
                    summary.version,
                    summary.manifest_url,
                    settings.auto_check,
                    settings.auto_download,
                );
            }
            Err(err) => {
                eprintln!(
                    "[updater] Updater disabled in development mode; config validation failed: {err}"
                );
            }
        }
    }

    pub async fn check_for_update(&self) -> anyhow::Result<CheckResult> {
        let url = validate_url(&self.manifest_url, self.allow_local_http)?;
        let resp = self
            .http_client
            .get(url)
            .send()
            .await
            .context("failed to fetch update manifest")?
            .error_for_status()
            .context("update manifest request returned error status")?;

        let manifest: UpdateManifest = resp
            .json()
            .await
            .context("failed to parse update-manifest.json")?;

        self.evaluate_manifest(manifest)
    }

    pub fn evaluate_manifest(&self, manifest: UpdateManifest) -> anyhow::Result<CheckResult> {
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported update manifest schemaVersion {}",
                manifest.schema_version
            );
        }

        let candidate_version = parse_semver(&manifest.version)?;
        let current_version = parse_semver(APP_VERSION)?;
        if candidate_version <= current_version {
            return Ok(CheckResult::UpToDate);
        }

        let release_url = validate_url(&manifest.release_url, self.allow_local_http)?
            .to_string();

        let asset = current_platform_key()
            .and_then(|key| manifest.platforms.get(key))
            .map(|asset| {
                validate_url(&asset.url, self.allow_local_http)?;
                if !is_valid_sha256_hex(&asset.sha256) {
                    anyhow::bail!("invalid sha256 checksum in manifest for {}", asset.asset);
                }
                Ok(PlatformAsset {
                    asset: asset.asset.clone(),
                    url: asset.url.clone(),
                    sha256: asset.sha256.to_ascii_lowercase(),
                    size: asset.size,
                })
            })
            .transpose()?;

        Ok(CheckResult::UpdateAvailable(UpdateReleaseInfo {
            version: candidate_version.to_string(),
            tag: manifest.tag,
            release_url,
            asset,
        }))
    }

    pub async fn download_and_verify(
        &self,
        info: &UpdateReleaseInfo,
        progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    ) -> anyhow::Result<PathBuf> {
        let asset = info
            .asset
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no installer asset available for this platform"))?;

        let asset_url = validate_url(&asset.url, self.allow_local_http)?;
        if !is_valid_sha256_hex(&asset.sha256) {
            anyhow::bail!("manifest sha256 is invalid");
        }

        tokio::fs::create_dir_all(&self.updates_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to create updates directory {}",
                    self.updates_dir.display()
                )
            })?;

        let safe_ver = sanitize_version_for_filename(&info.version);
        let final_path = self
            .updates_dir
            .join(format!("Stitch-Setup-{safe_ver}.exe"));
        let part_path = self
            .updates_dir
            .join(format!("Stitch-Setup-{safe_ver}.exe.part"));

        if final_path.is_file() && verify_file_sha256(&final_path, &asset.sha256).unwrap_or(false) {
            let size = tokio::fs::metadata(&final_path).await.ok().map(|m| m.len());
            let _ = progress_tx.send(DownloadProgress {
                downloaded_bytes: size.or(asset.size).unwrap_or(0),
                total_bytes: size.or(asset.size),
            });
            return Ok(final_path);
        }

        let _ = tokio::fs::remove_file(&part_path).await;

        let mut response = self
            .http_client
            .get(asset_url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .context("failed to start installer download")?
            .error_for_status()
            .context("installer download returned error status")?;

        let total_bytes = response.content_length().or(asset.size);
        let mut file = tokio::fs::File::create(&part_path).await.with_context(|| {
            format!("failed to create partial installer {}", part_path.display())
        })?;

        let mut hasher = Sha256::new();
        let mut downloaded_bytes: u64 = 0;
        let mut last_emit_at = Instant::now();
        let mut last_emit_bytes: u64 = 0;

        let stream_result: anyhow::Result<()> = async {
            while let Some(chunk) = response
                .chunk()
                .await
                .context("failed while downloading installer")?
            {
                file.write_all(&chunk)
                    .await
                    .context("failed to write installer chunk")?;
                hasher.update(&chunk);
                downloaded_bytes += chunk.len() as u64;

                if downloaded_bytes - last_emit_bytes >= PROGRESS_EMIT_BYTES
                    || last_emit_at.elapsed() >= PROGRESS_EMIT_INTERVAL
                {
                    let _ = progress_tx.send(DownloadProgress {
                        downloaded_bytes,
                        total_bytes,
                    });
                    last_emit_at = Instant::now();
                    last_emit_bytes = downloaded_bytes;
                }
            }
            file.flush().await.context("failed to flush installer file")?;
            Ok(())
        }
        .await;

        if let Err(err) = stream_result {
            drop(file);
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(err);
        }
        drop(file);

        let _ = progress_tx.send(DownloadProgress {
            downloaded_bytes,
            total_bytes,
        });

        if let Some(expected_size) = asset.size
            && downloaded_bytes != expected_size
        {
            let _ = tokio::fs::remove_file(&part_path).await;
            anyhow::bail!(
                "downloaded size mismatch: expected {expected_size} bytes, got {downloaded_bytes} bytes"
            );
        }

        let actual_sha256 = hex_encode_lower(&hasher.finalize());
        if !actual_sha256.eq_ignore_ascii_case(&asset.sha256) {
            let _ = tokio::fs::remove_file(&part_path).await;
            anyhow::bail!(
                "installer SHA-256 mismatch: expected {}, got {actual_sha256}",
                asset.sha256
            );
        }

        if final_path.exists() {
            let _ = tokio::fs::remove_file(&final_path).await;
        }
        tokio::fs::rename(&part_path, &final_path)
            .await
            .with_context(|| {
                format!(
                    "failed to finalize downloaded installer {}",
                    final_path.display()
                )
            })?;

        Ok(final_path)
    }

    pub fn cleanup_stale_installers(&self) -> usize {
        cleanup_stale_installers_in(&self.updates_dir, APP_VERSION)
    }

    pub fn spawn_windows_installer(
        &self,
        installer_path: &Path,
        expected_sha256: &str,
    ) -> anyhow::Result<()> {
        if !installer_path.is_file() {
            anyhow::bail!(
                "downloaded installer not found at {}",
                installer_path.display()
            );
        }

        if !verify_file_sha256(installer_path, expected_sha256)? {
            anyhow::bail!("installer SHA-256 verification failed before launch");
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

            let exe_path = std::env::current_exe()
                .context("failed to determine current Stitch executable path")?;
            let install_dir = exe_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?;
            let install_dir_str = install_dir.to_string_lossy();
            if install_dir_str.contains('"')
                || install_dir_str.contains('\r')
                || install_dir_str.contains('\n')
            {
                anyhow::bail!("install directory contains unsupported characters");
            }

            std::process::Command::new(installer_path)
                .arg("/S")
                // NSIS requires /D=<path> as the final unquoted argument.
                .raw_arg(format!("/D={install_dir_str}"))
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .with_context(|| {
                    format!(
                        "failed to launch installer {}",
                        installer_path.display()
                    )
                })?;

            Ok(())
        }

        #[cfg(not(windows))]
        {
            let _ = (installer_path, expected_sha256);
            anyhow::bail!("silent NSIS installer execution is only supported on Windows")
        }
    }
}

pub fn verify_file_sha256(path: &Path, expected_hex: &str) -> anyhow::Result<bool> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open {} for SHA-256 verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("failed reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let actual = hex_encode_lower(&hasher.finalize());
    Ok(actual.eq_ignore_ascii_case(expected_hex.trim()))
}

fn hex_encode_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn cleanup_stale_installers_in(updates_dir: &Path, current_version: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(updates_dir) else {
        return 0;
    };
    let current = parse_semver(current_version).ok();
    let mut removed = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if name.ends_with(".part") {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            continue;
        }

        if let Some(ver_str) = name
            .strip_prefix("Stitch-Setup-")
            .and_then(|rest| rest.strip_suffix(".exe"))
        {
            let should_remove = match (current.as_ref(), parse_semver(ver_str)) {
                (Some(cur), Ok(file_ver)) => file_ver <= *cur,
                _ => false,
            };
            if should_remove && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_updates_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "stitch-rs-updater-test-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn parse_semver_accepts_calver_without_leading_zeros_and_rejects_leading_zeros() {
        assert_eq!(
            parse_semver("2026.11.26").unwrap(),
            semver::Version::new(2026, 11, 26)
        );
        assert_eq!(
            parse_semver("v2027.1.5").unwrap(),
            semver::Version::new(2027, 1, 5)
        );
        assert!(parse_semver("2027.01.05").is_err());

        assert!(is_newer_version("2026.11.26", "2027.1.5").unwrap());
        assert!(!is_newer_version("2027.1.5", "2027.1.5").unwrap());
        assert!(is_newer_version("2027.1.5-beta.1", "2027.1.5").unwrap());
        assert!(!is_newer_version("2027.1.5", "2027.1.5-beta.1").unwrap());
    }

    #[test]
    fn validate_url_enforces_https_and_github_hosts() {
        assert!(
            validate_url(
                "https://github.com/thealternator89/stitch-rs/releases/latest/download/update-manifest.json",
                false
            )
            .is_ok()
        );
        assert!(
            validate_url(
                "https://objects.githubusercontent.com/github-production-release-asset/foo",
                false
            )
            .is_ok()
        );
        assert!(validate_url("http://github.com/thealternator89/stitch-rs", false).is_err());
        assert!(validate_url("https://evil.example.com/Stitch-Setup.exe", false).is_err());
        assert!(validate_url("http://127.0.0.1:8080/update-manifest.json", false).is_err());
        assert!(validate_url("http://127.0.0.1:8080/update-manifest.json", true).is_ok());
    }

    #[test]
    fn evaluate_manifest_checks_schema_version_and_sha256() {
        let dir = temp_updates_dir("manifest");
        let service = UpdaterService::with_test_config(
            UpdaterMode::FullAutoUpdate,
            UPDATE_MANIFEST_URL.to_string(),
            false,
            dir,
        );

        let mut platforms = HashMap::new();
        if let Some(key) = current_platform_key() {
            platforms.insert(
                key.to_string(),
                PlatformAsset {
                    asset: "Stitch-Setup.exe".to_string(),
                    url: "https://github.com/thealternator89/stitch-rs/releases/download/9999.1.1/Stitch-Setup.exe".to_string(),
                    sha256: "a".repeat(64),
                    size: Some(1024),
                },
            );
        }

        let manifest = UpdateManifest {
            schema_version: 1,
            version: "9999.1.1".to_string(),
            tag: "9999.1.1".to_string(),
            published_at: None,
            release_url: "https://github.com/thealternator89/stitch-rs/releases/tag/9999.1.1"
                .to_string(),
            platforms,
        };

        let result = service.evaluate_manifest(manifest).expect("valid manifest");
        match result {
            CheckResult::UpdateAvailable(info) => {
                assert_eq!(info.version, "9999.1.1");
                if current_platform_key().is_some() {
                    assert!(info.asset.is_some());
                }
            }
            CheckResult::UpToDate => panic!("expected update available"),
        }
    }

    #[test]
    fn verify_file_sha256_and_cleanup_stale_installers_work() {
        let dir = temp_updates_dir("cleanup");
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let old_installer = dir.join("Stitch-Setup-0.1.0.exe");
        let newer_installer = dir.join("Stitch-Setup-2027.1.5.exe");
        let part_file = dir.join("Stitch-Setup-2027.1.5.exe.part");

        std::fs::write(&old_installer, b"old-installer").expect("write old");
        std::fs::write(&newer_installer, b"new-installer").expect("write new");
        std::fs::write(&part_file, b"partial").expect("write part");

        let expected_sha = {
            let mut h = Sha256::new();
            h.update(b"new-installer");
            hex_encode_lower(&h.finalize())
        };
        assert!(verify_file_sha256(&newer_installer, &expected_sha).unwrap());
        assert!(!verify_file_sha256(&newer_installer, &"0".repeat(64)).unwrap());

        let removed = cleanup_stale_installers_in(&dir, "0.1.0");
        assert_eq!(removed, 2);
        assert!(!old_installer.exists());
        assert!(!part_file.exists());
        assert!(newer_installer.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_config_succeeds_for_default_service() {
        let service = UpdaterService::new();
        let summary = service
            .validate_config(&UpdateSettings::default())
            .expect("default updater config should be valid");
        assert!(!summary.version.is_empty());
        assert!(summary.manifest_url.starts_with("https://"));
    }
}
