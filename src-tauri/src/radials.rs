// ---------- Radial Menus > Manage ----------
// Plasticity loads every radial menu file in ~/.plasticity/radials. Disabling one moves it
// to a sibling folder (radials-disabled) that Plasticity never reads, the same as matcaps;
// enabling moves it back. Deleting only works on disabled menus and goes to the Recycle Bin.
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use tauri::Manager;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadialFile {
    file: String,
    path: String,
    disabled: bool,
    name: String,
    command: String,        // without view:radial:
    items: Vec<String>,     // each item's command, in order
    modified: u64,          // ms since 1970
    error: Option<String>,  // not a radial menu Plasticity can read
}

fn disabled_dir(folder: &Path) -> Result<PathBuf, String> {
    let name = folder.file_name().and_then(|n| n.to_str()).ok_or("bad radials folder")?;
    Ok(folder.with_file_name(format!("{name}-disabled")))
}

fn read_one(path: &Path, disabled: bool) -> RadialFile {
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut out = RadialFile {
        file,
        path: path.to_string_lossy().into_owned(),
        disabled,
        name: String::new(),
        command: String::new(),
        items: vec![],
        modified,
        error: None,
    };
    let parsed = fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| format!("not valid JSON ({e})")));
    match parsed {
        Ok(v) => match v.get("command").and_then(|c| c.as_str()) {
            Some(c) if !c.trim().is_empty() => {
                out.command = c.to_string();
                out.name = v.get("name").and_then(|n| n.as_str()).filter(|n| !n.trim().is_empty()).unwrap_or(c).to_string();
                out.items = v
                    .get("items")
                    .and_then(|i| i.as_array())
                    .map(|a| a.iter().filter_map(|it| it.get("command").and_then(|c| c.as_str()).map(String::from)).collect())
                    .unwrap_or_default();
            }
            _ => out.error = Some("has no \"command\", so Plasticity can't open it".into()),
        },
        Err(e) => out.error = Some(e),
    }
    out
}

fn json_files(dir: &Path, disabled: bool, out: &mut Vec<RadialFile>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {} failed: {e}", dir.display())),
    };
    for p in entries.flatten().map(|e| e.path()) {
        if p.is_file() && p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("json")) == Some(true) {
            out.push(read_one(&p, disabled));
        }
    }
    Ok(())
}

// The menus in the folder, then the disabled ones.
#[tauri::command]
pub async fn list_radial_files(folder: String) -> Result<Vec<RadialFile>, String> {
    let on = PathBuf::from(&folder);
    let mut out = vec![];
    json_files(&on, false, &mut out)?;
    json_files(&disabled_dir(&on)?, true, &mut out)?;
    // by name; files that can't be read have none, so they go by their file name
    let key = |r: &RadialFile| if r.name.is_empty() { r.file.to_lowercase() } else { r.name.to_lowercase() };
    out.sort_by(|a, b| a.disabled.cmp(&b.disabled).then(key(a).cmp(&key(b))));
    Ok(out)
}

fn plain_file(file: &str) -> Result<(), String> {
    if file.contains(['/', '\\']) || !file.to_lowercase().ends_with(".json") {
        return Err(format!("{file} isn't a radial menu file"));
    }
    Ok(())
}

#[tauri::command]
pub async fn set_radial_enabled(folder: String, file: String, enabled: bool) -> Result<(), String> {
    plain_file(&file)?;
    let on = PathBuf::from(&folder);
    let off = disabled_dir(&on)?;
    let (from, to) = if enabled { (&off, &on) } else { (&on, &off) };
    if !from.join(&file).is_file() {
        return Err(format!("{file} is no longer there"));
    }
    if to.join(&file).exists() {
        let other = if enabled { "an enabled" } else { "a disabled" };
        return Err(format!("there is already {other} menu file called {file}"));
    }
    fs::create_dir_all(to).map_err(|e| format!("create {} failed: {e}", to.display()))?;
    fs::rename(from.join(&file), to.join(&file)).map_err(|e| format!("move {file} failed: {e}"))?;
    if enabled {
        let _ = fs::remove_dir(&off); // only goes if it is now empty
    }
    Ok(())
}

#[tauri::command]
pub async fn trash_disabled_radial(folder: String, file: String) -> Result<(), String> {
    plain_file(&file)?;
    let off = disabled_dir(Path::new(&folder))?;
    let path = off.join(&file);
    if !path.is_file() {
        return Err(format!("{file} isn't in the disabled folder"));
    }
    trash::delete(&path).map_err(|e| format!("couldn't move {file} to the bin: {e}"))?;
    let _ = fs::remove_dir(&off);
    Ok(())
}

#[tauri::command]
pub fn default_radials_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(app.path().home_dir().ok().map(|h| h.join(".plasticity").join("radials").to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    #[test]
    fn disable_and_list() {
        let root = std::env::temp_dir().join(format!("radials-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("radials");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("add.json"), r#"{"name":"Add","command":"me:add","items":[{"command":"command:line"},{"command":"view:radial:me:sub"}]}"#).unwrap();
        fs::write(dir.join("sub.json"), r#"{"command":"me:sub","items":[]}"#).unwrap();
        fs::write(dir.join("broken.json"), "{ nope").unwrap();
        let folder = dir.to_string_lossy().into_owned();
        run(set_radial_enabled(folder.clone(), "sub.json".into(), false)).unwrap();
        assert!(root.join("radials-disabled").join("sub.json").is_file());
        let list = run(list_radial_files(folder.clone())).unwrap();
        let names: Vec<(&str, bool)> = list.iter().map(|r| (r.file.as_str(), r.disabled)).collect();
        assert_eq!(names, [("add.json", false), ("broken.json", false), ("sub.json", true)]);
        assert_eq!(list[0].items, ["command:line", "view:radial:me:sub"]);
        assert!(list[1].error.is_some());
        assert_eq!(list[2].name, "me:sub");
        // enabled menus can't be deleted; paths can't escape the folder
        assert!(run(trash_disabled_radial(folder.clone(), "add.json".into())).is_err());
        assert!(run(set_radial_enabled(folder.clone(), "../x.json".into(), true)).is_err());
        run(set_radial_enabled(folder.clone(), "sub.json".into(), true)).unwrap();
        assert!(!root.join("radials-disabled").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
