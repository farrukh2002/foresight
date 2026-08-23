use std::path::{Path, PathBuf};
use std::process::Command;

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

fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch_field = it.next()?;
    let patch_digits = patch_field.split(['-', '+']).next().unwrap_or(patch_field);
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
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

pub fn check_only(channel: Option<&str>) -> Option<String> {
    let channel = channel.unwrap_or_else(|| current_channel());
    let tag = latest_tag(channel)?;
    let want = parse_semver(&tag)?;
    let have = parse_semver(CURRENT_VERSION)?;
    if want > have { Some(tag) } else { None }
}

pub fn apply_update(target_tag: Option<&str>, channel: Option<&str>) -> Result<Option<UpdateOutcome>, String> {
    let tag = match target_tag {
        Some(t) => t.to_string(),
        None => {
            let channel = channel.unwrap_or_else(|| current_channel());
            latest_tag(channel).ok_or_else(|| format!("could not reach GitHub releases for channel '{channel}'"))?
        }
    };
    let have = parse_semver(CURRENT_VERSION).ok_or("bad current version")?;
    let want = parse_semver(&tag).ok_or("bad release tag")?;
    if target_tag.is_none() && want <= have {
        return Ok(None);
    }
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
    result.map(|_| Some(UpdateOutcome { from: CURRENT_VERSION.to_string(), to: tag }))
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

fn set_toml_scalar(home: &str, section: &str, key: &str, quoted_value: &str) -> std::io::Result<()> {
    let path = PathBuf::from(home).join(".config/foresight/config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = format!("[{section}]");
    let mut section_start: Option<usize> = None;
    let mut key_line: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t == header {
            section_start = Some(i);
        } else if section_start.is_some() && t.starts_with('[') && t != header {
            break;
        } else if section_start.is_some() && t.starts_with(key) {
            key_line = Some(i);
        }
    }
    let new_line = format!("{key} = {quoted_value}");
    match (section_start, key_line) {
        (Some(_), Some(k)) => lines[k] = new_line,
        (Some(s), None) => lines.insert(s + 1, new_line),
        (None, _) => {
            lines.push(String::new());
            lines.push(header);
            lines.push(new_line);
        }
    }
    std::fs::write(&path, lines.join("\n") + "\n")
}

pub fn set_pinned_version(home: &str, version: &str) -> std::io::Result<()> {
    set_toml_scalar(home, "update", "pinned_version", &format!("\"{version}\""))
}

pub fn set_update_mode(home: &str, mode: &str) -> std::io::Result<()> {
    set_toml_scalar(home, "update", "mode", &format!("\"{mode}\""))
}
