// Keyboard Shortcuts tab: Plasticity's built-in shortcuts, the user's keymap.json and
// their radial menus. keymap.json itself is parsed and written by the page, as text, so
// it keeps the loose format Plasticity writes (bare keys, single quotes).
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tauri::Manager;

// Plasticity's built-in shortcuts, read from Plasticity 26.1.3.
#[tauri::command]
pub fn load_default_shortcuts() -> Result<serde_json::Value, String> {
    serde_json::from_str(include_str!("../../dist/shortcuts/26.1.json"))
        .map_err(|e| format!("shortcuts/26.1.json is not valid JSON: {e}"))
}

// Like read_text_file, but a missing file is None rather than an error.
#[tauri::command]
pub fn read_text_if_exists(path: String) -> Result<Option<String>, String> {
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {path} failed: {e}")),
    }
}

#[derive(Serialize)]
pub struct RadialInfo {
    command: String, // without the view:radial: prefix
    name: String,
}

// The radial menus Plasticity loads from ~/.plasticity/radials, so they can be given shortcuts.
#[tauri::command]
pub fn list_plasticity_radials(app: tauri::AppHandle) -> Result<Vec<RadialInfo>, String> {
    let dir: PathBuf = app
        .path()
        .home_dir()
        .map_err(|e| format!("home_dir error: {e}"))?
        .join(".plasticity")
        .join("radials");
    let entries = match fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {} failed: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("json")) != Some(true) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(command) = value.get("command").and_then(|c| c.as_str()) else { continue };
        let name = value
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(command)
            .to_string();
        out.push(RadialInfo { command: command.to_string(), name });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let v = load_default_shortcuts().unwrap();
        let bindings = v["bindings"].as_array().unwrap();
        let main = bindings
            .iter()
            .find(|b| b["selector"] == "body:not([gizmo])" && b.get("platform").is_none())
            .unwrap();
        assert_eq!(main["keys"]["x"], "command:dissolve");
        assert_eq!(main["keys"]["shift-x"], "command:delete");
    }
}
