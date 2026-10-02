use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

// ---------- Error helpers ----------
fn io_err<T: ToString>(msg: T) -> String {
    msg.to_string()
}
fn fmt_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---------- App data helpers ----------
fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| io_err(format!("app_data_dir error: {e}")))?;
    if !base.exists() {
        fs::create_dir_all(&base)
            .map_err(|e| io_err(format!("create app_data_dir {} failed: {e}", fmt_path(&base))))?;
    }
    Ok(base)
}

fn radials_dir_marker_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app_data_dir(app)?;
    Ok(dir.join("radials_dir.txt"))
}

// ---------- JSON file helpers ----------
fn read_json_file(path: &Path) -> Result<serde_json::Value, String> {
    let data = fs::read_to_string(path)
        .map_err(|e| io_err(format!("read {} failed: {e}", fmt_path(path))))?;
    serde_json::from_str(&data)
        .map_err(|e| io_err(format!("parse {} failed: {e}", fmt_path(path))))
}

fn write_json_file(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let pretty = serde_json::to_string_pretty(value)
        .map_err(|e| io_err(format!("serialize json failed: {e}")))?;
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| io_err(format!("create dir {} failed: {e}", fmt_path(parent))))?;
        }
    }
    fs::write(path, pretty)
        .map_err(|e| io_err(format!("write {} failed: {e}", fmt_path(path))))
}

// ---------- Commands ----------
#[tauri::command]
fn load_commands(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    // Just use embedded fallback for now
    let data = include_str!("../../dist/commands.json");
    serde_json::from_str(data).map_err(|e| format!("embedded commands.json parse failed: {e}"))
}

#[tauri::command]
fn load_commands_from_file(path: String) -> Result<serde_json::Value, String> {
    read_json_file(Path::new(&path))
}

#[tauri::command]
fn list_json_files(directory: String) -> Result<Vec<String>, String> {
    let dir = PathBuf::from(&directory);
    if !dir.exists() {
        return Err(io_err(format!("directory {} does not exist", directory)));
    }

    let mut files = vec![];
    for entry in fs::read_dir(&dir)
        .map_err(|e| io_err(format!("read_dir {} failed: {e}", directory)))?
    {
        let entry = entry.map_err(|e| io_err(format!("dir entry error: {e}")))?;
        let path = entry.path();
        if path.extension().map(|x| x == "json").unwrap_or(false) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                files.push(name.to_string());
            }
        }
    }
    files.sort();
    Ok(files)
}

#[tauri::command]
fn load_radial_menu(path: String) -> Result<serde_json::Value, String> {
    read_json_file(Path::new(&path))
}

#[tauri::command]
fn save_radial_menu(menu: serde_json::Value, path: String) -> Result<(), String> {
    write_json_file(Path::new(&path), &menu)
}

#[tauri::command]
fn save_radials_directory(path: String, app: tauri::AppHandle) -> Result<(), String> {
    let marker = radials_dir_marker_path(&app)?;
    fs::write(&marker, &path)
        .map_err(|e| io_err(format!("persist dir to {} failed: {e}", fmt_path(&marker))))
}

#[tauri::command]
fn get_saved_radials_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let marker = radials_dir_marker_path(&app)?;
    match fs::read_to_string(&marker) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(format!("read {} failed: {e}", fmt_path(&marker)))),
    }
}

// ---------- Simple dialog commands ----------
#[tauri::command]
fn pick_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn pick_json_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter("JSON", &["json"])
        .set_title("Select a commands JSON")
        .blocking_pick_file();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn pick_save_json_path(app: tauri::AppHandle, suggested_name: Option<String>) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file().add_filter("JSON", &["json"]);
    if let Some(name) = suggested_name {
        builder = builder.set_file_name(&name);
    }
    let picked = builder
        .set_title("Save radial menu as…")
        .blocking_save_file();
    Ok(picked.map(|p| p.to_string()))
}

// ---------- Theme Preview tab ----------
// Theme files are read and written as plain text so the key order the editor
// produces is kept (serde_json::Value would re-sort the keys).
fn plasticity_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().home_dir().ok()?.join(".plasticity");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

#[tauri::command]
fn pick_theme_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let mut builder = app
        .dialog()
        .file()
        .add_filter("Theme", &["json", "json5", "js", "txt"])
        .set_title("Open a Plasticity theme");
    if let Some(dir) = plasticity_dir(&app) {
        builder = builder.set_directory(dir);
    }
    let picked = builder.blocking_pick_file();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn pick_save_theme_path(app: tauri::AppHandle, suggested_name: Option<String>) -> Result<Option<String>, String> {
    let mut builder = app
        .dialog()
        .file()
        .add_filter("JSON", &["json"])
        .set_title("Save Plasticity theme as…");
    if let Some(dir) = plasticity_dir(&app) {
        builder = builder.set_directory(dir);
    }
    if let Some(name) = suggested_name {
        builder = builder.set_file_name(&name);
    }
    let picked = builder.blocking_save_file();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|e| io_err(format!("read {} failed: {e}", path)))
}

#[tauri::command]
fn write_text_file(path: String, contents: String) -> Result<(), String> {
    fs::write(&path, contents).map_err(|e| io_err(format!("write {} failed: {e}", path)))
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            load_commands,
            load_commands_from_file,
            list_json_files,
            load_radial_menu,
            save_radial_menu,
            save_radials_directory,
            get_saved_radials_directory,
            pick_directory,
            pick_json_file,
            pick_save_json_path,
            pick_theme_file,
            pick_save_theme_path,
            read_text_file,
            write_text_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}