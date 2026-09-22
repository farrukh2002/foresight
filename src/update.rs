use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;

pub const OWNER: &str = "farrukh2002";
pub const REPO: &str = "foresight";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CHANNELS: [&str; 3] = ["stable", "beta", "dev"];
const BIN_NAME: &str = "foresight";

pub fn current_channel() -> &'static str {
    match CURRENT_VERSION.rsplit_once('-') {
        Some((_, ch)) if CHANNELS.contains(&ch) => ch,
        _ => "stable",
    }
}

pub fn valid_channel(s: &str) -> bool {
    CHANNELS.contains(&s)
}

pub struct UpdateOutcome {
    pub from: String,
    pub to: String,
}

fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("foresight-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("foresight-linux-aarch64.tar.gz"),
        _ => None,
    }
}

pub fn parse_version(s: &str) -> Result<semver::Version, semver::Error> {
    let clean = s.trim().trim_start_matches('v');
    semver::Version::parse(clean)
}

fn curl_text(url: &str) -> Option<String> {
    let out = Command::new("curl").args(["-fsSL", "--max-time", "10", url]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Fetches the version pointed at by a channel's floating release (tagged
/// exactly "stable", "beta", or "dev" — its assets get replaced by every
/// release published to that channel, so this is always that channel's
/// current head, independent of GitHub's single global "latest" release.
pub fn latest_tag(channel: &str) -> Option<String> {
    curl_text(&format!("https://github.com/{OWNER}/{REPO}/releases/download/{channel}/VERSION")).map(|s| s.trim().to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedUpdate {
    UpToDate,
    AlreadyInstalled {
        tag: String,
        persist_pin: Option<String>,
        persist_channel: Option<String>,
    },
    Install {
        tag: String,
        persist_pin: Option<String>,
        persist_channel: Option<String>,
    },
}

pub fn resolve_update<F>(
    cfg: &Config,
    current_version: &str,
    target_tag: Option<&str>,
    channel: Option<&str>,
    fetch_latest: F,
) -> Result<ResolvedUpdate, String>
where
    F: FnOnce(&str) -> Option<String>,
{
    let have = parse_version(current_version)
        .map_err(|e| format!("bad current version '{current_version}': {e}"))?;

    // 1. Explicit target tag (pin) — exact identity, build metadata included
    if let Some(pin) = target_tag {
        let want = parse_version(pin)
            .map_err(|e| format!("bad target tag '{pin}': {e}"))?;
        let is_same = want == have
            || pin.trim().trim_start_matches('v') == current_version.trim().trim_start_matches('v');
        if is_same {
            return Ok(ResolvedUpdate::AlreadyInstalled {
                tag: pin.to_string(),
                persist_pin: Some(pin.to_string()),
                persist_channel: None,
            });
        } else {
            return Ok(ResolvedUpdate::Install {
                tag: pin.to_string(),
                persist_pin: Some(pin.to_string()),
                persist_channel: None,
            });
        }
    }

    // 2. Channel determination
    let explicit_channel = channel.is_some();
    let target_channel = channel.unwrap_or_else(|| {
        if !cfg.update.channel.is_empty() {
            &cfg.update.channel
        } else {
            current_channel()
        }
    });

    // 3. Persisted pin (only honored when no explicit channel override) — exact identity
    if !explicit_channel && !cfg.update.pinned_version.is_empty() {
        let pin = &cfg.update.pinned_version;
        let want = parse_version(pin)
            .map_err(|e| format!("bad pinned version '{pin}': {e}"))?;
        let is_same = want == have
            || pin.trim().trim_start_matches('v') == current_version.trim().trim_start_matches('v');
        if is_same {
            return Ok(ResolvedUpdate::UpToDate);
        } else {
            return Ok(ResolvedUpdate::Install {
                tag: pin.clone(),
                persist_pin: None,
                persist_channel: None,
            });
        }
    }

    // 4. Latest on target channel — semantic precedence (build metadata ignored)
    let tag = fetch_latest(target_channel).ok_or_else(|| {
        format!("could not reach GitHub releases for channel '{target_channel}'")
    })?;
    let want = parse_version(&tag)
        .map_err(|e| format!("bad release tag '{tag}': {e}"))?;
    // Precedence excludes build metadata per SemVer; semver's derived
    // Ord/PartialEq include it, so compare precedence explicitly.
    let precedence = (&want.major, &want.minor, &want.patch, &want.pre)
        .cmp(&(&have.major, &have.minor, &have.patch, &have.pre));
    let is_newer = precedence == std::cmp::Ordering::Greater;
    let is_older = precedence == std::cmp::Ordering::Less;

    if explicit_channel {
        // Channel no-op is judged by precedence (build metadata ignored);
        // an explicit channel switch to a different precedence downgrades or upgrades.
        if is_newer || is_older {
            return Ok(ResolvedUpdate::Install {
                tag,
                persist_pin: None,
                persist_channel: Some(target_channel.to_string()),
            });
        } else {
            return Ok(ResolvedUpdate::AlreadyInstalled {
                tag,
                persist_pin: None,
                persist_channel: Some(target_channel.to_string()),
            });
        }
    }

    if is_newer {
        Ok(ResolvedUpdate::Install {
            tag,
            persist_pin: None,
            persist_channel: None,
        })
    } else {
        Ok(ResolvedUpdate::UpToDate)
    }
}

pub fn check_only_policy<F>(
    cfg: &Config,
    current_version: &str,
    channel: Option<&str>,
    fetch_latest: F,
) -> Result<Option<String>, String>
where
    F: FnOnce(&str) -> Option<String>,
{
    match resolve_update(cfg, current_version, None, channel, fetch_latest)? {
        ResolvedUpdate::UpToDate => Ok(None),
        ResolvedUpdate::AlreadyInstalled { .. } => Ok(None),
        ResolvedUpdate::Install { tag, .. } => Ok(Some(tag)),
    }
}

pub fn check_only(channel: Option<&str>) -> Result<Option<String>, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = Config::load_readonly(&home)?;
    check_only_policy(&cfg, CURRENT_VERSION, channel, |ch| latest_tag(ch))
}

pub fn apply_update(target_tag: Option<&str>, channel: Option<&str>) -> Result<Option<UpdateOutcome>, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = Config::load_readonly(&home)?;
    let resolved = resolve_update(&cfg, CURRENT_VERSION, target_tag, channel, |ch| latest_tag(ch))?;

    match resolved {
        ResolvedUpdate::UpToDate => Ok(None),
        ResolvedUpdate::AlreadyInstalled { persist_pin, persist_channel, .. } => {
            if let Some(pin) = persist_pin {
                set_pinned_version(&home, &pin).map_err(|e| format!("failed to persist pin: {e}"))?;
            }
            if let Some(ch) = persist_channel {
                set_channel(&home, &ch).map_err(|e| format!("failed to persist channel: {e}"))?;
            }
            Ok(None)
        }
        ResolvedUpdate::Install { tag, persist_pin, persist_channel } => {
            let asset = asset_name().ok_or("no prebuilt binary for this platform")?;
            let base = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}");

            let tmp_dir = std::env::temp_dir().join(format!("bp-update-{}", std::process::id()));
            std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
            let result = (|| {
                let archive = tmp_dir.join(asset);
                download(&format!("{base}/{asset}"), &archive)?;
                let sums = tmp_dir.join("SHA256SUMS");
                download(&format!("{base}/SHA256SUMS"), &sums)?;
                verify_checksum(&archive, &sums, asset)?;
                extract(&archive, &tmp_dir)?;
                let new_bin = tmp_dir.join(BIN_NAME);
                if !new_bin.exists() {
                    return Err("downloaded archive missing binary".to_string());
                }
                swap_in(&new_bin)
            })();
            let _ = std::fs::remove_dir_all(&tmp_dir);
            result?;

            if let Some(pin) = persist_pin {
                set_pinned_version(&home, &pin).map_err(|e| format!("update installed but failed to persist pin: {e}"))?;
            }
            if let Some(ch) = persist_channel {
                set_channel(&home, &ch).map_err(|e| format!("update installed but failed to persist channel: {e}"))?;
            }

            Ok(Some(UpdateOutcome { from: CURRENT_VERSION.to_string(), to: tag }))
        }
    }
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let status = Command::new("curl").args(["-fsSL", "--max-time", "60", "-o"]).arg(dest).arg(url).status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("download failed: {url}"));
    }
    Ok(())
}

fn verify_checksum(archive: &Path, sums: &Path, asset: &str) -> Result<(), String> {
    let sums_text = std::fs::read_to_string(sums).map_err(|e| e.to_string())?;
    let expected = sums_text
        .lines()
        .find_map(|l| {
            let mut parts = l.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            if name == asset { Some(hash.to_string()) } else { None }
        })
        .ok_or("checksum not found for asset")?;
    let out = Command::new("sha256sum").arg(archive).output().map_err(|e| e.to_string())?;
    let actual = String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_string();
    if actual != expected {
        return Err(format!("checksum mismatch: expected {expected}, got {actual}"));
    }
    Ok(())
}

fn extract(archive: &Path, dest: &Path) -> Result<(), String> {
    let status = Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(dest).status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("extract failed".to_string());
    }
    Ok(())
}

fn swap_in(new_bin: &Path) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = current.with_extension("update-tmp");
    std::fs::copy(new_bin, &tmp).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    let result = std::fs::rename(&tmp, &current);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| e.to_string())
}

fn set_table_entry(table: &mut dyn toml_edit::TableLike, key: &str, new_val: &str) {
    if let Some(item) = table.get_mut(key) {
        if let Some(val) = item.as_value_mut() {
            let decor = val.decor().clone();
            let mut new_v = toml_edit::Value::from(new_val);
            *new_v.decor_mut() = decor;
            *val = new_v;
        } else {
            *item = toml_edit::value(new_val);
        }
    } else {
        table.insert(key, toml_edit::value(new_val));
    }
}

pub fn update_config_toml_at<F>(path: &Path, mutate: F) -> std::io::Result<()>
where
    F: FnOnce(&mut dyn toml_edit::TableLike) -> std::io::Result<()>,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        text.parse::<toml_edit::DocumentMut>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refusing to overwrite malformed config: {e}"),
            )
        })?
    } else {
        crate::config::TEMPLATE.parse::<toml_edit::DocumentMut>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("template parse error: {e}"),
            )
        })?
    };

    match doc.get("update") {
        None => {
            doc["update"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        Some(item) if item.is_table_like() => {}
        Some(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "refusing to overwrite non-table update setting",
            ));
        }
    }

    let tbl = doc["update"].as_table_like_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "refusing to overwrite non-table update setting",
        )
    })?;

    mutate(tbl)?;

    let output = doc.to_string();
    crate::util::atomic_write(path, |writer| {
        use std::io::Write;
        writer.write_all(output.as_bytes())?;
        writer.flush()
    })
}

pub fn update_config_toml<F>(home: &str, mutate: F) -> std::io::Result<()>
where
    F: FnOnce(&mut dyn toml_edit::TableLike) -> std::io::Result<()>,
{
    let path = PathBuf::from(home).join(".config/foresight/config.toml");
    update_config_toml_at(&path, mutate)
}

pub fn set_pinned_version(home: &str, version: &str) -> std::io::Result<()> {
    update_config_toml(home, |tbl| {
        set_table_entry(tbl, "pinned_version", version);
        Ok(())
    })
}

pub fn set_update_mode(home: &str, mode: &str) -> std::io::Result<()> {
    update_config_toml(home, |tbl| {
        set_table_entry(tbl, "mode", mode);
        Ok(())
    })
}

pub fn set_channel(home: &str, channel: &str) -> std::io::Result<()> {
    update_config_toml(home, |tbl| {
        set_table_entry(tbl, "channel", channel);
        set_table_entry(tbl, "pinned_version", "");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_handles_prefixes_and_prereleases() {
        let v1 = parse_version("0.6.0-beta").unwrap();
        let v2 = parse_version("v0.6.0-stable").unwrap();
        let v3 = parse_version("0.6.0").unwrap();
        let v4 = parse_version("v0.6.1-beta").unwrap();

        assert_eq!(v1.to_string(), "0.6.0-beta");
        assert_eq!(v2.to_string(), "0.6.0-stable");
        assert_eq!(v3.to_string(), "0.6.0");
        assert_eq!(v4.to_string(), "0.6.1-beta");

        assert!(parse_version("invalid-semver").is_err());
        assert!(parse_version("").is_err());
    }

    #[test]
    fn semver_precedence_orders_channels_and_prereleases() {
        let beta = parse_version("0.6.0-beta").unwrap();
        let stable = parse_version("0.6.0-stable").unwrap();
        let bare_release = parse_version("0.6.0").unwrap();
        let next_beta = parse_version("0.6.1-beta").unwrap();

        // 0.6.0-beta < 0.6.0-stable because "beta" < "stable" lexically
        assert!(beta < stable);
        // 0.6.0-beta < 0.6.0
        assert!(beta < bare_release);
        // 0.6.0-beta < 0.6.1-beta
        assert!(beta < next_beta);
    }

    #[test]
    fn resolve_update_beta_to_stable_is_discoverable() {
        let cfg = Config::default();
        let action = resolve_update(
            &cfg,
            "0.6.0-beta",
            None,
            Some("stable"),
            |_| Some("v0.6.0-stable".to_string()),
        ).unwrap();

        assert_eq!(
            action,
            ResolvedUpdate::Install {
                tag: "v0.6.0-stable".to_string(),
                persist_pin: None,
                persist_channel: Some("stable".to_string()),
            }
        );

        let check = check_only_policy(
            &cfg,
            "0.6.0-beta",
            Some("stable"),
            |_| Some("v0.6.0-stable".to_string()),
        ).unwrap();
        assert_eq!(check, Some("v0.6.0-stable".to_string()));
    }

    #[test]
    fn resolve_update_explicit_channel_allows_downgrade() {
        let cfg = Config::default();
        // Currently on dev 0.7.0-dev, user explicitly requests stable channel (at 0.6.0-stable)
        let action = resolve_update(
            &cfg,
            "0.7.0-dev",
            None,
            Some("stable"),
            |_| Some("v0.6.0-stable".to_string()),
        ).unwrap();

        assert_eq!(
            action,
            ResolvedUpdate::Install {
                tag: "v0.6.0-stable".to_string(),
                persist_pin: None,
                persist_channel: Some("stable".to_string()),
            }
        );
    }

    #[test]
    fn resolve_update_persisted_pin_respected() {
        let mut cfg = Config::default();
        cfg.update.pinned_version = "v0.5.0-stable".to_string();

        let action = resolve_update(
            &cfg,
            "0.6.0-beta",
            None,
            None,
            |_| panic!("should not fetch latest when pinned"),
        ).unwrap();

        assert_eq!(
            action,
            ResolvedUpdate::Install {
                tag: "v0.5.0-stable".to_string(),
                persist_pin: None,
                persist_channel: None,
            }
        );

        let check = check_only_policy(
            &cfg,
            "0.6.0-beta",
            None,
            |_| panic!("should not fetch latest when pinned"),
        ).unwrap();
        assert_eq!(check, Some("v0.5.0-stable".to_string()));
    }

    #[test]
    fn resolve_update_exact_installed_pin_noop() {
        let cfg = Config::default();
        let action = resolve_update(
            &cfg,
            "0.6.0-beta",
            Some("v0.6.0-beta"),
            None,
            |_| panic!("should not fetch latest"),
        ).unwrap();

        assert_eq!(
            action,
            ResolvedUpdate::AlreadyInstalled {
                tag: "v0.6.0-beta".to_string(),
                persist_pin: Some("v0.6.0-beta".to_string()),
                persist_channel: None,
            }
        );
    }

    #[test]
    fn check_only_offline_or_parse_failure_is_not_up_to_date() {
        let cfg = Config::default();

        // Offline / curl failure
        let res = check_only_policy(&cfg, "0.6.0-beta", None, |_| None);
        assert!(res.is_err(), "offline fetch must return Err");

        // Invalid release tag
        let res2 = check_only_policy(&cfg, "0.6.0-beta", None, |_| Some("not-valid-semver".to_string()));
        assert!(res2.is_err(), "invalid tag must return Err");
    }

    #[test]
    fn structural_toml_edit_preserves_comments_and_scan_settings() {
        let tmp_dir = std::env::temp_dir().join(format!("foresight_cfg_test_{}", std::process::id()));
        let cfg_path = tmp_dir.join("config.toml");
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let initial_toml = r#"# Top level comment
[scan]
# scan mode comment
mode = "deep"
max_entries_normal = 12345
roots = ["/custom/root"]

[update]
# update mode comment
mode = "manual"
# pinned version comment
pinned_version = ""
# check interval comment
check_interval_hours = 48
"#;
        std::fs::write(&cfg_path, initial_toml).unwrap();

        // Modify update mode
        update_config_toml_at(&cfg_path, |tbl| {
            set_table_entry(tbl, "mode", "silent");
            set_table_entry(tbl, "channel", "beta");
            Ok(())
        }).unwrap();

        let updated = std::fs::read_to_string(&cfg_path).unwrap();

        // Verify comments are preserved
        assert!(updated.contains("# Top level comment"));
        assert!(updated.contains("# scan mode comment"));
        assert!(updated.contains("# update mode comment"));
        assert!(updated.contains("# pinned version comment"));
        assert!(updated.contains("# check interval comment"));

        // Read back Config values (scan settings intact, update settings changed)
        let cfg: Config = toml::from_str(&updated).unwrap();
        assert_eq!(cfg.scan.mode, "deep");
        assert_eq!(cfg.scan.max_entries_normal, 12345);
        assert_eq!(cfg.update.mode, "silent");
        assert_eq!(cfg.update.channel, "beta");
        assert_eq!(cfg.update.check_interval_hours, 48);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_exact_update_hash_comment_reproduction() {
        let tmp_dir = std::env::temp_dir().join(format!("foresight_comment_{}", std::process::id()));
        let cfg_path = tmp_dir.join("config.toml");
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let initial = r#"[scan]
mode = "deep"
max_entries_normal = 50000

[update] # update comment
mode = "manual" # trailing mode
pinned_version = "v0.4.0"
check_interval_hours = 12
"#;
        std::fs::write(&cfg_path, initial).unwrap();

        update_config_toml_at(&cfg_path, |tbl| {
            set_table_entry(tbl, "mode", "silent");
            Ok(())
        }).unwrap();

        let updated = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(updated.contains("[update] # update comment"));
        assert!(updated.contains("# trailing mode"));

        let cfg: Config = toml::from_str(&updated).unwrap();
        assert_eq!(cfg.scan.mode, "deep");
        assert_eq!(cfg.scan.max_entries_normal, 50000);
        assert_eq!(cfg.update.mode, "silent");
        assert_eq!(cfg.update.pinned_version, "v0.4.0");
        assert_eq!(cfg.update.check_interval_hours, 12);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_inline_table_policy_preservation() {
        let tmp_dir = std::env::temp_dir().join(format!("foresight_inline_{}", std::process::id()));
        let cfg_path = tmp_dir.join("config.toml");
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let initial = r#"update = { mode = "manual", pinned_version = "v0.5.0", check_interval_hours = 48 }

[scan]
mode = "deep"
"#;
        std::fs::write(&cfg_path, initial).unwrap();

        update_config_toml_at(&cfg_path, |tbl| {
            set_table_entry(tbl, "mode", "silent");
            Ok(())
        }).unwrap();

        let updated = std::fs::read_to_string(&cfg_path).unwrap();
        let cfg: Config = toml::from_str(&updated).unwrap();
        assert_eq!(cfg.scan.mode, "deep");
        assert_eq!(cfg.update.mode, "silent");
        assert_eq!(cfg.update.pinned_version, "v0.5.0");
        assert_eq!(cfg.update.check_interval_hours, 48);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_non_table_update_rejected() {
        let tmp_dir = std::env::temp_dir().join(format!("foresight_nontable_{}", std::process::id()));
        let cfg_path = tmp_dir.join("config.toml");
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let initial = "update = \"not a table\"\n";
        std::fs::write(&cfg_path, initial).unwrap();

        let res = update_config_toml_at(&cfg_path, |tbl| {
            set_table_entry(tbl, "mode", "silent");
            Ok(())
        });
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), initial);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn structural_toml_edit_refuses_to_overwrite_malformed() {
        let tmp_dir = std::env::temp_dir().join(format!("foresight_cfg_malformed_{}", std::process::id()));
        let cfg_path = tmp_dir.join("config.toml");
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let malformed = "[scan\nmode = ";
        std::fs::write(&cfg_path, malformed).unwrap();

        let res = update_config_toml_at(&cfg_path, |tbl| {
            set_table_entry(tbl, "mode", "silent");
            Ok(())
        });
        assert!(res.is_err());

        // Content must be unchanged
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), malformed);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn structural_toml_edit_creates_missing_parents() {
        let tmp_dir = std::env::temp_dir().join(format!("foresight_cfg_parents_{}/sub/dir", std::process::id()));
        let cfg_path = tmp_dir.join("config.toml");

        update_config_toml_at(&cfg_path, |tbl| {
            set_table_entry(tbl, "mode", "silent");
            Ok(())
        }).unwrap();

        assert!(cfg_path.exists());
        let content = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(content.contains(r#"mode = "silent""#));

        let base_dir = std::env::temp_dir().join(format!("foresight_cfg_parents_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn resolve_update_ignores_build_metadata_for_channel_precedence() {
        // Installed 1.2.3+build9 vs channel release 1.2.3+build1: same precedence
        // (build metadata ignored) => no update, no downgrade to older build.
        let action = resolve_update(
            &Config::default(),
            "1.2.3+build9",
            None,
            Some("stable"),
            |_| Some("1.2.3+build1".to_string()),
        ).unwrap();
        assert_eq!(
            action,
            ResolvedUpdate::AlreadyInstalled {
                tag: "1.2.3+build1".to_string(),
                persist_pin: None,
                persist_channel: Some("stable".to_string()),
            }
        );

        // Normal (non-explicit-channel) update: same precedence => up to date
        let action2 = resolve_update(
            &Config::default(),
            "1.2.3+build9",
            None,
            None,
            |_| Some("1.2.3+build1".to_string()),
        ).unwrap();
        assert_eq!(action2, ResolvedUpdate::UpToDate);
    }

    #[test]
    fn resolve_update_exact_pin_identity_includes_build_metadata() {
        let cfg = Config::default();
        // Exact pin 1.2.3+build2 while installed 1.2.3+build1: identity differs => install
        let action = resolve_update(
            &cfg,
            "1.2.3+build1",
            Some("1.2.3+build2"),
            None,
            |_| panic!("should not fetch latest when explicit pin given"),
        ).unwrap();
        assert_eq!(
            action,
            ResolvedUpdate::Install {
                tag: "1.2.3+build2".to_string(),
                persist_pin: Some("1.2.3+build2".to_string()),
                persist_channel: None,
            }
        );
    }
}
