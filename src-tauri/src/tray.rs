// ---------- Profiles in the tray (the menu bar on macOS) ----------
// When the user turns it on in the Profiles tab, the app keeps an icon in the tray whose menu
// lists their profiles, and closing the window hides it instead of quitting. Picking a profile
// hands it to the page (window.trayProfile), which closes Plasticity politely, applies the
// profile and starts Plasticity again, using close_plasticity and launch_plasticity below.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

const TRAY_ID: &str = "profiles";
const PROFILE: &str = "profile:";

// Whether the tray is on; while it is, closing the window only hides it (see main.rs).
pub static TRAY_ON: AtomicBool = AtomicBool::new(false);

pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[tauri::command]
pub fn show_main_window(app: AppHandle) {
    show_window(&app);
}

fn build_menu(app: &AppHandle, profiles: &[String], active: Option<&str>) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    if profiles.is_empty() {
        menu.append(&MenuItem::with_id(app, "none", "No profiles yet", false, None::<&str>)?)?;
    }
    for name in profiles {
        let item = CheckMenuItem::with_id(app, format!("{PROFILE}{name}"), name, true, active == Some(name.as_str()), None::<&str>)?;
        menu.append(&item)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "open", "Open Radial Menu Builder++", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

// Turns the tray on (or updates its menu) or off. `status` replaces the tooltip while a switch runs.
#[tauri::command]
pub fn set_tray(app: AppHandle, enabled: bool, profiles: Vec<String>, active: Option<String>, status: Option<String>) -> Result<(), String> {
    TRAY_ON.store(enabled, Ordering::SeqCst);
    if !enabled {
        let _ = app.remove_tray_by_id(TRAY_ID);
        return Ok(());
    }
    let menu = build_menu(&app, &profiles, active.as_deref()).map_err(|e| e.to_string())?;
    let tip = status.unwrap_or_else(|| match &active {
        Some(name) => format!("Radial Menu Builder++ · {name}"),
        None => "Radial Menu Builder++".into(),
    });
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
        tray.set_tooltip(Some(tip)).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let icon = app.default_window_icon().cloned().ok_or("the app has no icon")?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip(tip)
        .menu(&menu)
        .show_menu_on_left_click(false) // left click opens the window, right click the menu
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "quit" => app.exit(0),
            id => {
                if let (Some(name), Some(w)) = (id.strip_prefix(PROFILE), app.get_webview_window("main")) {
                    let name = serde_json::to_string(name).unwrap_or_default();
                    let _ = w.eval(&format!("window.trayProfile && window.trayProfile({name})"));
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        })
        .build(&app)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- Plasticity's installs ----------
// "26.2.0-beta30" -> ([26, 2, 0], 30); a release sorts after its betas.
fn version_key(v: &str) -> (Vec<u64>, u64) {
    let (main, pre) = match v.split_once('-') {
        Some((m, p)) => (m, Some(p)),
        None => (v, None),
    };
    let nums = main.split('.').map(|n| n.trim().parse().unwrap_or(0)).collect();
    let pre = match pre {
        None => u64::MAX,
        Some(p) => p.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0),
    };
    (nums, pre)
}

// Each install as (version, what to start). On Windows Plasticity installs per user with
// Squirrel: %LOCALAPPDATA%\plasticity (or plasticity-beta) holds app-<version> folders and a
// small launcher exe that starts the newest of them.
#[cfg(windows)]
fn installs() -> Vec<(String, PathBuf)> {
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return vec![] };
    let mut out = vec![];
    for name in ["plasticity", "plasticity-beta"] {
        let dir = base.join(name);
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut launcher = None;
        let mut newest: Option<String> = None;
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            if path.is_dir() {
                if let Some(v) = file.strip_prefix("app-") {
                    if newest.as_deref().map(|n| version_key(v) > version_key(n)).unwrap_or(true) {
                        newest = Some(v.to_string());
                    }
                }
            } else if file.to_lowercase().ends_with(".exe") && !file.eq_ignore_ascii_case("Update.exe") {
                launcher = Some(path);
            }
        }
        if let (Some(v), Some(l)) = (newest, launcher) {
            out.push((v, l));
        }
    }
    out
}

// On macOS: Plasticity*.app in /Applications or ~/Applications, versioned by their Info.plist.
#[cfg(not(windows))]
fn installs() -> Vec<(String, PathBuf)> {
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Applications"));
    }
    let mut out = vec![];
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.starts_with("plasticity") && name.ends_with(".app") {
                let plist = fs::read_to_string(e.path().join("Contents/Info.plist")).unwrap_or_default();
                out.push((plist_version(&plist).unwrap_or_default(), e.path()));
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn plist_version(plist: &str) -> Option<String> {
    let after = &plist[plist.find("<key>CFBundleShortVersionString</key>")?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

fn newest_install() -> Option<(String, PathBuf)> {
    installs().into_iter().max_by(|a, b| version_key(&a.0).cmp(&version_key(&b.0)))
}

// Starts the newest Plasticity installed. Returns its version.
#[tauri::command]
pub async fn launch_plasticity() -> Result<String, String> {
    let (version, path) = newest_install().ok_or("couldn't find Plasticity installed")?;
    start(&path).map_err(|e| format!("couldn't start Plasticity {version}: {e}"))?;
    Ok(version)
}

#[cfg(windows)]
fn start(path: &Path) -> std::io::Result<()> {
    Command::new(path).current_dir(path.parent().unwrap_or(Path::new("."))).spawn().map(|_| ())
}
#[cfg(not(windows))]
fn start(path: &Path) -> std::io::Result<()> {
    Command::new("open").arg(path).spawn().map(|_| ())
}

// Asks Plasticity to close the way its own close button does, so it can ask about unsaved work,
// then waits up to `wait_secs` for it to go. Never forces it. True once it has closed.
#[tauri::command]
pub async fn close_plasticity(wait_secs: u64) -> Result<bool, String> {
    if !crate::packs::plasticity_running() {
        return Ok(true);
    }
    ask_to_close();
    tauri::async_runtime::spawn_blocking(move || {
        for _ in 0..wait_secs * 2 {
            std::thread::sleep(Duration::from_millis(500));
            if !crate::packs::plasticity_running() {
                return true;
            }
        }
        false
    })
    .await
    .map_err(|e| e.to_string())
}

// taskkill without /F sends the windows a close message, like clicking their close button.
#[cfg(windows)]
fn ask_to_close() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/IM", "plasticity.exe", "/IM", "plasticity-beta.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

// Quit through AppleScript, which lets the app ask about unsaved work like Cmd+Q does.
#[cfg(not(windows))]
fn ask_to_close() {
    for (_, path) in installs() {
        let Some(name) = path.file_stem().map(|n| n.to_string_lossy().replace('"', "")) else { continue };
        let script = format!("if application \"{name}\" is running then tell application \"{name}\" to quit");
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_key("26.2.0-beta30") > version_key("26.1.3"));
        assert!(version_key("26.2.0") > version_key("26.2.0-beta30"));
        assert!(version_key("26.2.0-beta.31") > version_key("26.2.0-beta30"));
        assert!(version_key("26.10.0") > version_key("26.9.9"));
    }
}
