// Release builds on Windows run without a console window (debug builds keep it for logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

mod fonts;
mod matcaps;
mod packs;
mod radials;
mod shortcuts;

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

// ---------- Writing files safely ----------
// Writes a temporary file next to the target and renames it over the target, so a crash or
// power cut part-way through never leaves a half-written file. Falls back to a plain write
// when that isn't allowed (a folder we can't create files in, or a target another program
// has locked against renames).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    if fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path)).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(&tmp);
    fs::write(path, bytes)
}

// For Plasticity's own files (keymap.json, theme.json, asset-packs.json): the file being
// replaced is copied to NAME.bak first, and the very first time, to NAME.original as well,
// so the file as it was before this app ever changed it can always be got back.
fn backup_and_write(path: &Path, contents: &str) -> Result<(), String> {
    if path.is_file() {
        let with = |ext: &str| PathBuf::from(format!("{}.{ext}", path.display()));
        let original = with("original");
        if !original.exists() {
            fs::copy(path, &original).map_err(|e| format!("backup to {} failed: {e}", fmt_path(&original)))?;
        }
        let bak = with("bak");
        fs::copy(path, &bak).map_err(|e| format!("backup to {} failed: {e}", fmt_path(&bak)))?;
    }
    write_atomic(path, contents.as_bytes()).map_err(|e| format!("write {} failed: {e}", fmt_path(path)))
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
    write_atomic(path, pretty.as_bytes())
        .map_err(|e| io_err(format!("write {} failed: {e}", fmt_path(path))))
}

// ---------- Commands ----------
// One command list per Plasticity version, embedded at build time. The ids here
// match PLASTICITY_VERSIONS in index.html.
#[tauri::command]
fn load_commands(version: Option<String>) -> Result<serde_json::Value, String> {
    let (name, data) = match version.as_deref().unwrap_or("26.1") {
        "26.1" => ("26.1.json", include_str!("../../dist/commands/26.1.json")),
        "26.2-beta" => ("26.2-beta.json", include_str!("../../dist/commands/26.2-beta.json")),
        other => return Err(format!("unknown Plasticity version {other}")),
    };
    serde_json::from_str(data).map_err(|e| format!("embedded {name} parse failed: {e}"))
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
// Async so they run off the main thread: a blocking dialog called from the
// main thread deadlocks on macOS.
#[tauri::command]
async fn pick_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
async fn pick_json_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter("JSON", &["json"])
        .set_title("Select a commands JSON")
        .blocking_pick_file();
    Ok(picked.map(|p| p.to_string()))
}

// `directory`: where the dialog opens (the opened menu's folder, or the radials folder).
#[tauri::command]
async fn pick_save_json_path(
    app: tauri::AppHandle,
    suggested_name: Option<String>,
    directory: Option<String>,
) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file().add_filter("JSON", &["json"]);
    if let Some(dir) = directory.map(PathBuf::from).filter(|d| d.is_dir()) {
        builder = builder.set_directory(dir);
    }
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
// Plasticity reads theme.json (and settings.json, keymap.json) from
// ~/.plasticity/config/v2, not from ~/.plasticity itself.
fn plasticity_theme_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().home_dir().ok()?.join(".plasticity").join("config").join("v2"))
}

// Where the file pickers open: the theme folder if it exists, else ~/.plasticity.
fn plasticity_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let theme_dir = plasticity_theme_dir(app)?;
    if theme_dir.is_dir() {
        return Some(theme_dir);
    }
    let dir = app.path().home_dir().ok()?.join(".plasticity");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

#[tauri::command]
async fn pick_theme_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
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
async fn pick_save_theme_path(app: tauri::AppHandle, suggested_name: Option<String>) -> Result<Option<String>, String> {
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

// The Quit button on the first-launch terms.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

// F11 full screen. macOS has its own full-screen button; Windows has none.
#[tauri::command]
async fn toggle_fullscreen(window: tauri::WebviewWindow) -> Result<bool, String> {
    let full = !window.is_fullscreen().map_err(|e| e.to_string())?;
    window.set_fullscreen(full).map_err(|e| e.to_string())?;
    Ok(full)
}

// The user's named themes, kept as plain text in the app data folder.
fn user_themes_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("user_themes.json"))
}

#[tauri::command]
fn load_user_themes(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = user_themes_path(&app)?;
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(format!("read {} failed: {e}", fmt_path(&path)))),
    }
}

#[tauri::command]
fn save_user_themes(app: tauri::AppHandle, contents: String) -> Result<(), String> {
    let path = user_themes_path(&app)?;
    write_atomic(&path, contents.as_bytes()).map_err(|e| io_err(format!("write {} failed: {e}", fmt_path(&path))))
}

// The Profiles tab's saved setups, kept as plain text in the app data folder too.
fn profiles_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("profiles.json"))
}

#[tauri::command]
fn load_profiles(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = profiles_path(&app)?;
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(format!("read {} failed: {e}", fmt_path(&path)))),
    }
}

#[tauri::command]
fn save_profiles(app: tauri::AppHandle, contents: String) -> Result<(), String> {
    let path = profiles_path(&app)?;
    write_atomic(&path, contents.as_bytes()).map_err(|e| io_err(format!("write {} failed: {e}", fmt_path(&path))))
}

// The folder Plasticity reads theme.json from, whether or not it exists yet.
#[tauri::command]
fn default_plasticity_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(plasticity_theme_dir(&app).map(|dir| fmt_path(&dir)))
}

#[tauri::command]
async fn pick_plasticity_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let mut builder = app
        .dialog()
        .file()
        .set_title("Choose the folder Plasticity keeps theme.json in (.plasticity/config/v2)");
    if let Some(dir) = plasticity_dir(&app).or_else(|| app.path().home_dir().ok()) {
        builder = builder.set_directory(dir);
    }
    let picked = builder.blocking_pick_folder();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|e| io_err(format!("read {} failed: {e}", path)))
}

#[tauri::command]
fn write_text_file(path: String, contents: String) -> Result<(), String> {
    write_atomic(Path::new(&path), contents.as_bytes()).map_err(|e| io_err(format!("write {} failed: {e}", path)))
}

// Replaces one of Plasticity's own files, keeping NAME.bak and NAME.original (see above).
#[tauri::command]
fn write_plasticity_file(path: String, contents: String) -> Result<(), String> {
    backup_and_write(Path::new(&path), &contents)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(matcaps::StagingState::default())
        .manage(fonts::FontStagingState::default())
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
            default_plasticity_folder,
            pick_plasticity_folder,
            load_user_themes,
            save_user_themes,
            load_profiles,
            save_profiles,
            toggle_fullscreen,
            quit_app,
            read_text_file,
            write_text_file,
            write_plasticity_file,
            matcaps::stage_matcap_bytes,
            matcaps::stage_matcap_path,
            matcaps::unstage_matcaps,
            matcaps::save_matcap,
            matcaps::list_installed_matcaps,
            matcaps::installed_matcap_preview,
            matcaps::fix_installed_matcap,
            matcaps::set_matcap_tinted,
            matcaps::set_matcap_enabled,
            matcaps::trash_disabled_matcap,
            matcaps::apply_matcap_collection,
            matcaps::load_matcap_collections,
            matcaps::save_matcap_collections,
            matcaps::order_matcaps,
            matcaps::default_matcaps_folder,
            matcaps::pick_matcap_files,
            matcaps::pick_matcaps_folder,
            matcaps::installed_environment_lighting,
            fonts::stage_font_bytes,
            fonts::stage_font_path,
            fonts::unstage_fonts,
            fonts::install_font,
            fonts::list_installed_fonts,
            fonts::trash_font,
            fonts::default_fonts_folder,
            fonts::pick_font_files,
            fonts::pick_fonts_folder,
            packs::scan_asset_packs,
            packs::files_exist,
            packs::default_asset_packs_file,
            packs::plasticity_running,
            packs::reveal_file,
            packs::pick_pack_folder,
            packs::load_pack_folders,
            packs::save_pack_folders,
            packs::load_pack_collections,
            packs::save_pack_collections,
            radials::list_radial_files,
            radials::set_radial_enabled,
            radials::trash_disabled_radial,
            radials::default_radials_folder,
            shortcuts::load_default_shortcuts,
            shortcuts::read_text_if_exists,
            shortcuts::list_plasticity_radials,
            shortcuts::pick_radial_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backups() {
        let dir = std::env::temp_dir().join(format!("backup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keymap.json");
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
        // no file yet: no backups
        backup_and_write(&path, "one").unwrap();
        assert!(!dir.join("keymap.json.bak").exists() && !dir.join("keymap.json.original").exists());
        backup_and_write(&path, "two").unwrap();
        backup_and_write(&path, "three").unwrap();
        assert_eq!(read("keymap.json"), "three");
        assert_eq!(read("keymap.json.bak"), "two");
        assert_eq!(read("keymap.json.original"), "one");
        // nothing left over from the temporary files
        let mut names: Vec<String> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["keymap.json", "keymap.json.bak", "keymap.json.original"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
