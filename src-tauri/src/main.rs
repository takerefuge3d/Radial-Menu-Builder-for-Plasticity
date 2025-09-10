use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt; // v2 dialog plugin

// ---------- Error helpers ----------
fn io_err<T: ToString>(msg: T) -> String {
    msg.to_string()
}
fn fmt_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---------- macOS specific permissions ----------
#[cfg(target_os = "macos")]
fn ensure_file_access() -> Result<(), String> {
    use std::process::Command;
    
    // Try to access a test directory to trigger permission request
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
    let test_path = format!("{}/Documents", home_dir);
    
    // Attempt to read the directory - this should trigger macOS permission dialog
    match std::fs::read_dir(&test_path) {
        Ok(_) => {
            println!("File access permissions already granted");
            Ok(())
        },
        Err(e) => {
            println!("File access not available, error: {}", e);
            
            // Try to trigger permission dialog via AppleScript
            let script = r#"
                tell application "System Events"
                    display dialog "This app needs file system access to save and load radial menu files. Please grant permission in the next dialog." buttons {"OK"} default button "OK"
                end tell
            "#;
            
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(script)
                .output();
                
            // After showing the dialog, test access again
            match std::fs::read_dir(&test_path) {
                Ok(_) => Ok(()),
                Err(_) => Err("File system access required. Please grant permission in System Settings > Privacy & Security > Files and Folders".to_string())
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_file_access() -> Result<(), String> {
    Ok(())
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
    // Only try to ensure file access on macOS and only for user-selected files
    #[cfg(target_os = "macos")]
    {
        // Only check permissions for files outside the app bundle
        if !path.starts_with("/Applications") && !path.to_string_lossy().contains("_MEIPASS") {
            ensure_file_access()?;
        }
    }
    
    let data = fs::read_to_string(path)
        .map_err(|e| io_err(format!("read {} failed: {e}", fmt_path(path))))?;
    serde_json::from_str(&data)
        .map_err(|e| io_err(format!("parse {} failed: {e}", fmt_path(path))))
}

fn write_json_file(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    // Only try to ensure file access on macOS
    #[cfg(target_os = "macos")]
    {
        ensure_file_access()?;
    }
    
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

// ---------- Commands consumed by index.html ----------
#[tauri::command]
fn load_commands(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    // 1. Try user's app data directory first (for user customizations)
    if let Ok(app_data_commands) = app_data_dir(&app).map(|dir| dir.join("commands.json")) {
        if app_data_commands.exists() {
            return read_json_file(&app_data_commands);
        }
    }
    
    // 2. Try alongside the executable (from Gumroad package)
    if let Ok(exe_dir) = app.path().resource_dir() {
        let portable_commands = exe_dir.join("commands.json");
        if portable_commands.exists() {
            return read_json_file(&portable_commands);
        }
    }
    
    // 3. Dev: try local file in current directory
    let fs_path = PathBuf::from("commands.json");
    if fs_path.exists() {
        return read_json_file(&fs_path);
    }
    
    // 4. Final fallback: embedded file (guaranteed to work)
    let data = include_str!("../../dist/commands.json");
    serde_json::from_str(data).map_err(|e| format!("embedded commands.json parse failed: {e}"))
}

#[tauri::command]
fn load_commands_from_file(path: String) -> Result<serde_json::Value, String> {
    read_json_file(Path::new(&path))
}

#[tauri::command]
fn list_json_files(directory: String) -> Result<Vec<String>, String> {
    // Only check permissions on macOS for user-selected directories
    #[cfg(target_os = "macos")]
    {
        ensure_file_access()?;
    }
    
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

// ---------- Dialog commands (blocking but reliable) ----------
#[tauri::command]
fn pick_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    // Request file access before showing dialog on macOS
    #[cfg(target_os = "macos")]
    {
        ensure_file_access()?;
    }
    
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn pick_json_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    // Request file access before showing dialog on macOS
    #[cfg(target_os = "macos")]
    {
        ensure_file_access()?;
    }
    
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
    // Request file access before showing dialog on macOS
    #[cfg(target_os = "macos")]
    {
        ensure_file_access()?;
    }
    
    let mut builder = app.dialog().file().add_filter("JSON", &["json"]);
    if let Some(name) = suggested_name {
        builder = builder.set_file_name(&name);
    }
    let picked = builder
        .set_title("Save radial menu as…")
        .blocking_save_file();
    Ok(picked.map(|p| p.to_string()))
}

// ---------- Main ----------
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
            pick_save_json_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}