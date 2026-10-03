// Keyboard Shortcuts tab: Plasticity's built-in shortcuts, the user's keymap.json and
// their radial menus. keymap.json itself is parsed and written by the page, as text, so
// it keeps the loose format Plasticity writes (bare keys, single quotes).
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

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
#[serde(rename_all = "camelCase")]
pub struct RadialInfo {
    command: String, // without the view:radial: prefix
    name: String,
    file: String,
    in_plasticity: bool, // in ~/.plasticity/radials, where Plasticity loads radial menus from
}

fn plasticity_radials_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .home_dir()
        .map_err(|e| format!("home_dir error: {e}"))?
        .join(".plasticity")
        .join("radials"))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// A radial menu file has a "command" (e.g. "default-menu:modeling") and usually a "name".
fn read_radial(path: &Path, plasticity_dir: &Path) -> Option<RadialInfo> {
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let command = value.get("command")?.as_str()?.trim();
    if command.is_empty() || value.get("items").map(|i| !i.is_array()).unwrap_or(false) {
        return None;
    }
    let name = value
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(command)
        .to_string();
    let in_plasticity = path.parent().map(|d| same_dir(d, plasticity_dir)).unwrap_or(false);
    Some(RadialInfo { command: command.to_string(), name, file: path.to_string_lossy().into_owned(), in_plasticity })
}

fn radials_in(dir: &Path, plasticity_dir: &Path, out: &mut Vec<RadialInfo>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("json")) != Some(true) {
            continue;
        }
        if let Some(r) = read_radial(&path, plasticity_dir) {
            out.push(r);
        }
    }
}

// The user's radial menus, so they can be given shortcuts: the ones in Plasticity's radials
// folder, then any others in the folder the Radial Menus tab saves to.
#[tauri::command]
pub fn list_plasticity_radials(app: tauri::AppHandle) -> Result<Vec<RadialInfo>, String> {
    let plasticity_dir = plasticity_radials_dir(&app)?;
    let mut out = Vec::new();
    radials_in(&plasticity_dir, &plasticity_dir, &mut out);
    let saved = app
        .path()
        .app_data_dir()
        .ok()
        .and_then(|d| fs::read_to_string(d.join("radials_dir.txt")).ok())
        .map(|s| PathBuf::from(s.trim()))
        .filter(|d| !d.as_os_str().is_empty() && !same_dir(d, &plasticity_dir));
    if let Some(dir) = saved {
        let mut more = Vec::new();
        radials_in(&dir, &plasticity_dir, &mut more);
        // the same menu saved in both places is listed once, from Plasticity's folder
        more.retain(|r| !out.iter().any(|o| o.command == r.command));
        out.extend(more);
    }
    out.sort_by(|a, b| b.in_plasticity.cmp(&a.in_plasticity).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

// Browse for any radial menu file. Async so the dialog runs off the main thread.
#[tauri::command]
pub async fn pick_radial_file(app: tauri::AppHandle) -> Result<Option<RadialInfo>, String> {
    let plasticity_dir = plasticity_radials_dir(&app)?;
    let mut builder = app.dialog().file().add_filter("Radial menu", &["json"]).set_title("Choose a radial menu");
    if plasticity_dir.is_dir() {
        builder = builder.set_directory(&plasticity_dir);
    }
    let Some(picked) = builder.blocking_pick_file() else { return Ok(None) };
    let path = picked.into_path().map_err(|e| format!("can't use that file: {e}"))?;
    read_radial(&path, &plasticity_dir)
        .map(Some)
        .ok_or_else(|| format!("{} isn't a radial menu (it has no \"command\")", path.display()))
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

    #[test]
    fn radial_files() {
        let dir = std::env::temp_dir().join(format!("ks-radials-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.json"), r#"{"command":"my:menu","name":"My Menu","items":[]}"#).unwrap();
        fs::write(dir.join("b.json"), r#"{"command":"no-name","items":[]}"#).unwrap();
        fs::write(dir.join("c.json"), r#"{"app:quit":"quit"}"#).unwrap();
        let mut out = Vec::new();
        radials_in(&dir, &dir, &mut out);
        out.sort_by(|a, b| a.command.cmp(&b.command));
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].command.as_str(), out[0].name.as_str(), out[0].in_plasticity), ("my:menu", "My Menu", true));
        assert_eq!(out[1].name, "no-name");
        let _ = fs::remove_dir_all(&dir);
    }
}
