// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Revision tracking and targeted atomic updates for the durable configuration.

use super::Config;
use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use toml_edit::{DocumentMut, Item, Value};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

pub fn revision(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

pub async fn read_with_revision(path: &Path) -> Result<(String, String)> {
    let content = fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let revision = revision(&content);
    Ok((content, revision))
}

pub fn update_output_mode(
    source: &str,
    profile_name: &str,
    output_index: usize,
    mode: &str,
) -> Result<(String, Config)> {
    let mut document = source
        .parse::<DocumentMut>()
        .context("Failed to parse configuration for targeted update")?;
    let profiles = document
        .get_mut("profile")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| anyhow!("Configuration does not contain a profile array"))?;
    let profile = profiles
        .iter_mut()
        .find(|profile| profile.get("name").and_then(Item::as_str) == Some(profile_name))
        .ok_or_else(|| anyhow!("Profile '{}' no longer exists", profile_name))?;
    let outputs = profile
        .get_mut("output")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| anyhow!("Profile '{}' does not contain outputs", profile_name))?;
    let output = outputs.get_mut(output_index).ok_or_else(|| {
        anyhow!(
            "Output index {} no longer exists in profile '{}'",
            output_index,
            profile_name
        )
    })?;
    let decor = output
        .get("mode")
        .and_then(Item::as_value)
        .map(|current| current.decor().clone());
    let mut updated_mode = Value::from(mode);
    if let Some(decor) = decor {
        *updated_mode.decor_mut() = decor;
    }
    output["mode"] = Item::Value(updated_mode);

    let updated = document.to_string();
    let config = Config::from_toml(&updated).context("Updated configuration is invalid")?;
    Ok((updated, config))
}

pub async fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let target = fs::canonicalize(path)
        .await
        .with_context(|| format!("Failed to resolve config file: {}", path.display()))?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("Config path has no parent directory"))?;
    let metadata = fs::metadata(&target)
        .await
        .with_context(|| format!("Failed to inspect config file: {}", target.display()))?;
    let suffix = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".config.toml.nagame-{}-{suffix}.tmp",
        std::process::id()
    ));

    let result = async {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options
            .open(&temp_path)
            .await
            .with_context(|| format!("Failed to create {}", temp_path.display()))?;
        file.set_permissions(metadata.permissions()).await?;
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        fs::rename(&temp_path, &target)
            .await
            .with_context(|| format!("Failed to replace {}", target.display()))?;
        let directory = fs::File::open(parent).await?;
        directory.sync_all().await?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"# leading comment
[[profile]]
name = "desk"
wallpaper = "wall.jpg"

  [[profile.output]]
  name = "DP-*" # selector comment
  mode = "2560x1440@120" # mode comment
  scale = 1.25
  position = [10, 20]
"#;

    #[test]
    fn targeted_update_preserves_unrelated_values_and_comments() {
        let (updated, config) = update_output_mode(SOURCE, "desk", 0, "2560x1440@143.973").unwrap();

        assert!(updated.contains("# leading comment"));
        assert!(updated.contains("# selector comment"));
        assert!(updated.contains("# mode comment"));
        assert!(updated.contains("wallpaper = \"wall.jpg\""));
        assert!(updated.contains("scale = 1.25"));
        assert!(updated.contains("position = [10, 20]"));
        assert_eq!(
            config.profiles[0].outputs[0].mode.as_deref(),
            Some("2560x1440@143.973")
        );
    }

    #[test]
    fn refuses_stale_profile_or_output_targets() {
        assert!(update_output_mode(SOURCE, "missing", 0, "2560x1440@144").is_err());
        assert!(update_output_mode(SOURCE, "desk", 1, "2560x1440@144").is_err());
    }

    #[test]
    fn adds_mode_when_output_has_no_mode_key() {
        let source = r#"[[profile]]
name = "desk"

[[profile.output]]
name = "DP-*"
scale = 1.25
"#;

        let (updated, config) = update_output_mode(source, "desk", 0, "2560x1440@144").unwrap();

        assert!(updated.contains("mode = \"2560x1440@144\""));
        assert_eq!(
            config.profiles[0].outputs[0].mode.as_deref(),
            Some("2560x1440@144")
        );
    }

    #[test]
    fn targets_output_by_position_when_selectors_overlap() {
        let source = r#"[[profile]]
name = "desk"

[[profile.output]]
name = "*"
mode = "1920x1080@60"

[[profile.output]]
name = "DP-*"
mode = "2560x1440@120"
"#;

        let (_, config) = update_output_mode(source, "desk", 1, "2560x1440@144").unwrap();

        assert_eq!(
            config.profiles[0].outputs[0].mode.as_deref(),
            Some("1920x1080@60")
        );
        assert_eq!(
            config.profiles[0].outputs[1].mode.as_deref(),
            Some("2560x1440@144")
        );
    }

    #[tokio::test]
    async fn atomic_write_replaces_content_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, SOURCE).await.unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .await
            .unwrap();

        atomic_write(&path, "replacement\n").await.unwrap();

        assert_eq!(fs::read_to_string(&path).await.unwrap(), "replacement\n");
        assert_eq!(
            fs::metadata(&path).await.unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[tokio::test]
    async fn atomic_write_follows_a_config_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("tracked-config.toml");
        let link = directory.path().join("config.toml");
        fs::write(&target, SOURCE).await.unwrap();
        symlink(&target, &link).unwrap();

        atomic_write(&link, "replacement\n").await.unwrap();

        assert!(fs::symlink_metadata(&link)
            .await
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "replacement\n");
    }
}
