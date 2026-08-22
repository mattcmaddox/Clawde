// preset_store.rs — Read/write named animation presets to disk so the
// /rustail editor can manage multiple animation variants.
//
// Each preset lives as `<name>.json` in `$CLAWDE_HOME/rustail-presets/`.  The
// active preset name is tracked in `_active` (a plain-text file containing
// just the name).

use std::fs;
use std::path::PathBuf;

use clawde_core::paths::clawde_home;

use crate::rustail;

/// Filename of the active-preset marker.
const ACTIVE_FILE: &str = "_active";

/// Default preset name used when seeding from rustail.rs.
const DEFAULT_NAME: &str = "default";

/// On-disk shape of a single frame.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct PresetFrame {
    rows: Vec<String>,
    dur_ms: u64,
}

/// On-disk shape of a full preset.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct PresetData {
    frames: Vec<PresetFrame>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Ensure the preset directory and a `default` seed exist.  Should be called
/// once at startup so the editor always has something to work with.
pub fn ensure_seed() {
    let dir = presets_dir();
    if !dir.is_dir() {
        let _ = fs::create_dir_all(&dir);
    }
    let active = active_preset();
    // Seed "default" from the current rustail.rs frames if nothing exists.
    if !preset_exists(&active) {
        let frames = rustail::rustail_frames_owned();
        let _ = write_preset_inner(&active, &frames);
    }
}

/// List all preset names (sorted, without `.json` suffix).
pub fn list_presets() -> Vec<String> {
    let dir = presets_dir();
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                names.push(name.trim_end_matches(".json").to_string());
            }
        }
    }
    names.sort();
    // "default" always first for stability.
    if let Some(pos) = names.iter().position(|n| n == DEFAULT_NAME) {
        if pos != 0 {
            let d = names.remove(pos);
            names.insert(0, d);
        }
    }
    names
}

/// Does a preset file exist for this name?
pub fn preset_exists(name: &str) -> bool {
    preset_path(name).is_file()
}

/// Load the frames for `name`.  Returns `None` if the preset doesn't exist
/// or cannot be parsed.
pub fn load_preset(name: &str) -> Option<Vec<(Vec<String>, u64)>> {
    let data: PresetData =
        serde_json::from_str(&fs::read_to_string(preset_path(name)).ok()?).ok()?;
    Some(
        data.frames
            .into_iter()
            .map(|f| (f.rows, f.dur_ms))
            .collect(),
    )
}

/// Write frames to the named preset file.
pub fn save_preset(name: &str, frames: &[(Vec<String>, u64)]) -> Result<(), String> {
    write_preset_inner(name, frames)?;
    Ok(())
}

/// Delete the named preset file.  Returns `false` if the file didn't exist.
pub fn delete_preset(name: &str) -> bool {
    let path = preset_path(name);
    if path.is_file() {
        match fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("preset_store: cannot delete {name}: {e}");
                false
            }
        }
    } else {
        false
    }
}

/// Read the active preset name.  Defaults to `"default"`.
pub fn active_preset() -> String {
    let path = presets_dir().join(ACTIVE_FILE);
    match fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                DEFAULT_NAME.into()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => DEFAULT_NAME.into(),
    }
}

/// Persist the active preset name.
pub fn set_active(name: &str) {
    let path = presets_dir().join(ACTIVE_FILE);
    let _ = fs::write(&path, format!("{name}\n"));
}

/// Root of the preset store on disk.
fn presets_dir() -> PathBuf {
    clawde_home().join("rustail-presets")
}

fn preset_path(name: &str) -> PathBuf {
    presets_dir().join(format!("{name}.json"))
}

fn write_preset_inner(name: &str, frames: &[(Vec<String>, u64)]) -> Result<(), String> {
    let dir = presets_dir();
    if !dir.is_dir() {
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create preset dir: {e}"))?;
    }
    let data = PresetData {
        frames: frames
            .iter()
            .map(|(rows, dur_ms)| PresetFrame {
                rows: rows.clone(),
                dur_ms: *dur_ms,
            })
            .collect(),
    };
    let json =
        serde_json::to_string_pretty(&data).map_err(|e| format!("cannot serialise preset: {e}"))?;
    fs::write(preset_path(name), json).map_err(|e| format!("cannot write preset {name}: {e}"))
}
