// ---------- Asset Packs tab ----------
// Plasticity lists the asset packs it shows in ~/.plasticity/asset-packs.json:
//   [ { filePath: 'F:\\kitbash\\Pack.plasticitypack', path: [], lastOpened: 1790480198837 }, … ]
// A pack is "on" when it's in that list. The tab keeps its own list of folders to look in,
// finds the .plasticitypack files under them, and adds or removes entries. The list itself
// is read and written by the page (it's in the same loose format as keymap.json); this does
// the folder walking, the Plasticity-running check and the pickers.
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

const EXT: &str = "plasticitypack";
const MAX_DEPTH: usize = 12;
const MAX_PACKS: usize = 5000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackFile {
    path: String,
    folder: String, // which of the chosen folders it was found under
    size: u64,
    modified: u64, // ms since 1970
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PackFile>) {
    if depth > MAX_DEPTH || out.len() >= MAX_PACKS {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut subdirs = vec![];
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            // skip hidden and system folders ($RECYCLE.BIN, .git, node_modules…)
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !(name.starts_with('.') || name.starts_with('$') || name == "node_modules" || name == "system volume information") {
                subdirs.push(path);
            }
        } else if kind.is_file() && path.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case(EXT)) == Some(true) {
            let meta = entry.metadata().ok();
            out.push(PackFile {
                path: path.to_string_lossy().into_owned(),
                folder: root.to_string_lossy().into_owned(),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            });
        }
    }
    subdirs.sort();
    for d in subdirs {
        walk(root, &d, depth + 1, out);
    }
}

// Every .plasticitypack under the given folders (and their subfolders).
#[tauri::command]
pub async fn scan_asset_packs(folders: Vec<String>) -> Result<Vec<PackFile>, String> {
    let mut out = vec![];
    for f in folders {
        let root = PathBuf::from(&f);
        if root.is_dir() {
            walk(&root, &root, 0, &mut out);
        }
    }
    out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    out.dedup_by(|a, b| a.path.eq_ignore_ascii_case(&b.path));
    Ok(out)
}

// Which of these files exist, for packs Plasticity lists that may have moved or gone.
#[tauri::command]
pub fn files_exist(paths: Vec<String>) -> Vec<bool> {
    paths.iter().map(|p| Path::new(p).is_file()).collect()
}

#[tauri::command]
pub fn default_asset_packs_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(app.path().home_dir().ok().map(|h| h.join(".plasticity").join("asset-packs.json").to_string_lossy().into_owned()))
}

// Plasticity saves its pack list itself, so changes made while it's open may be lost.
#[tauri::command]
pub fn plasticity_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("tasklist")
            .args(["/NH", "/FO", "CSV"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| {
                let list = String::from_utf8_lossy(&o.stdout).to_lowercase();
                list.contains("\"plasticity.exe\"") || list.contains("\"plasticity-beta.exe\"")
            })
            .unwrap_or(false)
    }
    // By where it runs from, so the beta (whatever its process is called) counts too
    #[cfg(not(windows))]
    {
        crate::tray::running_app().is_some()
    }
}

// Shows a pack in Explorer / Finder.
#[tauri::command]
pub fn reveal_file(path: String) -> Result<(), String> {
    if !Path::new(&path).exists() {
        return Err(format!("{path} isn't there any more"));
    }
    // Explorer splits its arguments at commas, so the path is quoted by hand: passed as an
    // ordinary argument, a folder like "Cars, Trucks" would open the wrong place.
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        Command::new("explorer").raw_arg(format!("/select,\"{path}\"")).spawn()
    };
    #[cfg(target_os = "macos")]
    let result = Command::new("open").args(["-R", &path]).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(Path::new(&path).parent().unwrap_or(Path::new("/"))).spawn();
    result.map(|_| ()).map_err(|e| format!("couldn't open the folder: {e}"))
}

#[tauri::command]
pub async fn pick_pack_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(app.dialog().file().set_title("Choose a folder with Plasticity asset packs").blocking_pick_folder().map(|p| p.to_string()))
}

// The tab's own list of folders, kept in the app data folder.
fn folders_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("app_data_dir error: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("create {} failed: {e}", dir.display()))?;
    Ok(dir.join("asset_pack_folders.json"))
}

#[tauri::command]
pub fn load_pack_folders(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    match fs::read_to_string(folders_path(&app)?) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("asset_pack_folders.json isn't valid: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn save_pack_folders(app: tauri::AppHandle, folders: Vec<String>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&folders).map_err(|e| e.to_string())?;
    fs::write(folders_path(&app)?, text).map_err(|e| e.to_string())
}

// Collections: named sets of packs to have on, like matcap collections. Kept as plain text
// ({ collections: [{ name, packs: [paths] }] }) in the app data folder.
fn collections_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(folders_path(app)?.with_file_name("asset_pack_collections.json"))
}

#[tauri::command]
pub fn load_pack_collections(app: tauri::AppHandle) -> Result<Option<String>, String> {
    match fs::read_to_string(collections_path(&app)?) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn save_pack_collections(app: tauri::AppHandle, contents: String) -> Result<(), String> {
    let path = collections_path(&app)?;
    crate::write_atomic(&path, contents.as_bytes()).map_err(|e| format!("write {} failed: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_packs() {
        let root = std::env::temp_dir().join(format!("packs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Sci Fi/deeper")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::create_dir_all(root.join("$RECYCLE.BIN")).unwrap();
        for f in ["A.plasticitypack", "Sci Fi/B.PLASTICITYPACK", "Sci Fi/deeper/C.plasticitypack", "Sci Fi/notes.txt", ".hidden/D.plasticitypack", "$RECYCLE.BIN/E.plasticitypack"] {
            fs::write(root.join(f), "x").unwrap();
        }
        let folder = root.to_string_lossy().into_owned();
        let found = tauri::async_runtime::block_on(scan_asset_packs(vec![folder.clone(), folder.clone()])).unwrap();
        let names: Vec<String> = found.iter().map(|p| Path::new(&p.path).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["A.plasticitypack", "B.PLASTICITYPACK", "C.plasticitypack"]);
        assert!(found.iter().all(|p| p.folder == folder && p.size == 1));
        assert_eq!(files_exist(vec![found[0].path.clone(), root.join("gone.plasticitypack").to_string_lossy().into_owned()]), [true, false]);
        let _ = fs::remove_dir_all(&root);
    }
}
