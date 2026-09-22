use std::path::PathBuf;

use crate::util::{data_dir, fnv1a64};

const FORESIGHT_SH: &str = include_str!("../foresight.sh");
const FORESIGHT_ZSH: &str = include_str!("../foresight.zsh");
const BLESH_LAYER: &str = include_str!("../integrations/blesh/foresight.bash");

fn content_stamp() -> String {
    format!(
        "{:016x}\n",
        fnv1a64(FORESIGHT_SH.as_bytes())
            ^ fnv1a64(FORESIGHT_ZSH.as_bytes()).rotate_left(17)
            ^ fnv1a64(BLESH_LAYER.as_bytes()).rotate_left(43)
    )
}

fn stamp_path() -> PathBuf {
    data_dir().join("integrations.stamp")
}

fn blesh_layer_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_DATA_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join("blesh/local/integration");
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/share/blesh/local/integration")
}

pub fn materialize() -> std::io::Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    crate::util::atomic_write(&dir.join("foresight.sh"), |w| {
        use std::io::Write;
        w.write_all(FORESIGHT_SH.as_bytes())?;
        w.flush()
    })?;
    crate::util::atomic_write(&dir.join("foresight.zsh"), |w| {
        use std::io::Write;
        w.write_all(FORESIGHT_ZSH.as_bytes())?;
        w.flush()
    })?;
    let layer_dir = blesh_layer_dir();
    std::fs::create_dir_all(&layer_dir)?;
    crate::util::atomic_write(&layer_dir.join("foresight.bash"), |w| {
        use std::io::Write;
        w.write_all(BLESH_LAYER.as_bytes())?;
        w.flush()
    })?;
    crate::util::atomic_write(&stamp_path(), |w| {
        use std::io::Write;
        w.write_all(content_stamp().as_bytes())?;
        w.flush()
    })?;
    Ok(())
}

pub fn materialize_if_stale() {
    let wanted = content_stamp();
    let stale = std::fs::read_to_string(stamp_path())
        .map(|s| s.trim() != wanted.trim())
        .unwrap_or(true);
    if stale {
        if let Err(e) = materialize() {
            eprintln!("foresight: failed to write integration files: {e}");
        }
    }
}
pub fn run_init(rest: &[String]) -> Result<(), String> {
    let quiet = rest.iter().any(|a| a == "--quiet" || a == "-q");
    materialize().map_err(|e| format!("foresight init: {e}"))?;
    if !quiet {
        println!("foresight: integration files written:");
        println!("  bash:  {}", data_dir().join("foresight.sh").display());
        println!("  zsh:   {}", data_dir().join("foresight.zsh").display());
        println!("  blesh: {}", blesh_layer_dir().join("foresight.bash").display());
        println!("setup:");
        println!("  bash: source \"{}\" in ~/.bashrc", data_dir().join("foresight.sh").display());
        println!("  zsh:  source \"{}\" in ~/.zshrc", data_dir().join("foresight.zsh").display());
        println!("  ble.sh: `ble-import integration/foresight` in ~/.blerc");
    }
    Ok(())
}
