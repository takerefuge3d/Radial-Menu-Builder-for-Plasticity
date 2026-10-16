// ---------- Profiles in the tray (the menu bar on macOS) ----------
// The app keeps an icon in the tray whose menu lists the user's profiles; closing the window
// hides it there, and Quit in the menu closes the app. Picking a profile hands it to the page
// (window.trayProfile), which asks Plasticity to close, applies the profile and starts the same
// Plasticity again, using close_plasticity and launch_plasticity below. The app can also start
// at login, straight into the tray (IN_TRAY_ARG).
use serde::Serialize;
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
use tauri_plugin_autostart::ManagerExt;

const TRAY_ID: &str = "profiles";
const PROFILE: &str = "profile:";
// Passed when the app starts at login, so it stays in the tray instead of opening its window.
pub const IN_TRAY_ARG: &str = "--in-tray";

// Set once the tray icon exists; from then on closing the window only hides it (see main.rs).
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

// Makes the tray icon, or updates its menu. `status` replaces the tooltip while a switch runs.
pub fn update_tray(app: &AppHandle, profiles: &[String], active: Option<&str>, status: Option<String>) -> Result<(), String> {
    let menu = build_menu(app, profiles, active).map_err(|e| e.to_string())?;
    let tip = status.unwrap_or_else(|| match active {
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
        .build(app)
        .map_err(|e| e.to_string())?;
    TRAY_ON.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub fn set_tray(app: AppHandle, profiles: Vec<String>, active: Option<String>, status: Option<String>) -> Result<(), String> {
    update_tray(&app, &profiles, active.as_deref(), status)
}

// ---------- Starting at login ----------
#[tauri::command]
pub fn start_at_login(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_start_at_login(app: AppHandle, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    if enabled { launcher.enable() } else { launcher.disable() }.map_err(|e| e.to_string())
}

// ---------- The Plasticity that's open ----------
// What to start it again with: on Windows its exe, on macOS its .app. None when it isn't open.
// Release and beta are told apart by where they live, so either one is found.
#[cfg(windows)]
pub(crate) fn running_app() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // tasklist doesn't give paths; PowerShell does
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-Process plasticity,plasticity-beta -ErrorAction SilentlyContinue | Where-Object Path | Select-Object -First 1 -ExpandProperty Path",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

// Plasticity's main processes as (pid, .app), from every process's command line. The main
// process runs from <Plasticity*.app>/Contents/MacOS/, its helpers from a second .app inside.
#[cfg(target_os = "macos")]
fn main_processes() -> Vec<(u32, PathBuf)> {
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,args="]).output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout).lines().filter_map(parse_ps_line).collect()
}

// On Linux, Plasticity's main processes as (pid, what to start it with), from /proc.
#[cfg(target_os = "linux")]
fn main_processes() -> Vec<(u32, PathBuf)> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    let mut found: Vec<(u32, PathBuf)> = dir
        .flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse().ok()?;
            let exe = fs::read_link(e.path().join("exe")).ok()?; // only the user's own processes
            let cmdline = fs::read(e.path().join("cmdline")).ok()?;
            linux_main_process(pid, &exe, &cmdline, || fs::read(e.path().join("environ")).ok())
        })
        .collect();
    found.sort_by_key(|(pid, _)| *pid);
    found
}

// The .deb puts Plasticity in /usr/lib/plasticity/plasticity (plasticity-beta for the beta), and
// Electron's helpers run that same file with --type=…, so they're skipped. An AppImage runs from
// a temporary mount, so it's started again from the AppImage file (its APPIMAGE variable). A file
// replaced by an update while running reads as "… (deleted)".
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_main_process(pid: u32, exe: &Path, cmdline: &[u8], environ: impl FnOnce() -> Option<Vec<u8>>) -> Option<(u32, PathBuf)> {
    let exe = PathBuf::from(exe.to_string_lossy().trim_end_matches(" (deleted)"));
    if !exe.file_name()?.to_string_lossy().to_lowercase().starts_with("plasticity") {
        return None;
    }
    if cmdline.split(|b| *b == 0).any(|arg| arg.starts_with(b"--type=")) {
        return None; // a helper
    }
    let appimage = || {
        let env = environ()?;
        let value = env.split(|b| *b == 0).find_map(|v| v.strip_prefix(b"APPIMAGE="))?;
        Some(PathBuf::from(String::from_utf8_lossy(value).into_owned()))
    };
    let from_mount = exe.to_string_lossy().contains("/.mount_");
    Some((pid, from_mount.then(appimage).flatten().unwrap_or(exe)))
}

#[cfg(target_os = "macos")]
fn parse_ps_line(line: &str) -> Option<(u32, PathBuf)> {
    let (pid, args) = line.trim_start().split_once(' ')?;
    let pid = pid.parse().ok()?;
    let args = args.trim_start();
    let at = args.find(".app/Contents/MacOS/")?;
    let bundle = &args[..at];
    if bundle.contains(".app/") {
        return None; // a helper inside the app
    }
    let name = bundle.rsplit('/').next().unwrap_or("").to_lowercase();
    name.starts_with("plasticity").then(|| (pid, PathBuf::from(format!("{bundle}.app"))))
}

#[cfg(not(windows))]
pub(crate) fn running_app() -> Option<PathBuf> {
    main_processes().into_iter().next().map(|(_, app)| app)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Closed {
    closed: bool,
    app: Option<String>, // the Plasticity that was open, to start again
    full_screen: bool,   // it was in full screen (macOS), so launch_plasticity puts that back
}

// Asks Plasticity to close the way its own close button does, so it can ask about unsaved work,
// then waits up to `wait_secs` for it to go. Never forces it.
#[tauri::command]
pub async fn close_plasticity(wait_secs: u64) -> Result<Closed, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let app = running_app();
        let text = app.as_ref().map(|p| p.to_string_lossy().into_owned());
        if app.is_none() && !crate::packs::plasticity_running() {
            return Closed { closed: true, app: None, full_screen: false };
        }
        let full_now = app.as_deref().and_then(full_screen::is_full_screen);
        ask_to_close(app.as_deref());
        for _ in 0..wait_secs * 2 {
            std::thread::sleep(Duration::from_millis(500));
            if !crate::packs::plasticity_running() {
                // Without Accessibility it can't be asked, so what Plasticity saved as it closed is used.
                let full_screen = full_now.unwrap_or_else(|| app.as_deref().map(full_screen::saved_full_screen).unwrap_or(false));
                return Closed { closed: true, app: text, full_screen };
            }
        }
        Closed { closed: false, app: text, full_screen: false }
    })
    .await
    .map_err(|e| e.to_string())
}

// taskkill without /F sends the windows a close message, like clicking their close button.
#[cfg(windows)]
fn ask_to_close(_app: Option<&Path>) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/IM", "plasticity.exe", "/IM", "plasticity-beta.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

// Quit through AppleScript, which lets the app ask about unsaved work like Cmd+Q does. The app
// is named by its bundle id, so the release and the beta can't be mixed up.
#[cfg(target_os = "macos")]
fn ask_to_close(app: Option<&Path>) {
    let Some(app) = app else { return };
    let plist = fs::read_to_string(app.join("Contents/Info.plist")).unwrap_or_default();
    let target = match plist_string(&plist, "CFBundleIdentifier") {
        Some(id) => format!("application id \"{}\"", id.replace('"', "")),
        None => format!("application \"{}\"", app.file_stem().map(|n| n.to_string_lossy().replace('"', "")).unwrap_or_default()),
    };
    let _ = Command::new("osascript").args(["-e", &format!("tell {target} to quit")]).output();
}

// SIGTERM, which Electron handles like its own Quit (the way macOS's quit arrives too), so
// Plasticity can still ask about unsaved work. Never SIGKILL.
#[cfg(target_os = "linux")]
fn ask_to_close(_app: Option<&Path>) {
    for (pid, _) in main_processes() {
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).output();
    }
}

// ---------- Full screen on macOS ----------
// Plasticity comes back as an ordinary window after it's quit and started again, even when it
// was in full screen. So the switch notes whether it was: through System Events, which macOS
// allows once the app is ticked under Privacy & Security > Accessibility, or else from what
// Plasticity saved in its window-state.json as it quit. Before starting it again that file is
// marked full screen, and if it still opens in a window, its main window is put back in full
// screen through System Events.
#[cfg(target_os = "macos")]
mod full_screen {
    use super::*;
    use std::{process::Stdio, time::Instant};

    // osascript, given up after `wait`: it can sit behind a macOS permission dialog.
    fn osascript(script: &str, wait: Duration) -> Result<String, String> {
        let mut child = Command::new("osascript")
            .args(["-e", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        while child.try_wait().map_err(|e| e.to_string())?.is_none() {
            if started.elapsed() > wait {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
        if out.status.success() { Ok(text(&out.stdout)) } else { Err(text(&out.stderr)) }
    }

    // The app isn't allowed Accessibility, or to control System Events. -1719 is also "Invalid
    // index", which a process System Events can't see yet gives; with_main_window rules that out.
    fn no_permission(err: &str) -> bool {
        ["-1719", "-25211", "-1743", "assistive access"].iter().any(|s| err.contains(s))
    }

    fn pid_of(app: &Path) -> Option<u32> {
        main_processes().into_iter().find(|(_, a)| a == app).map(|(pid, _)| pid)
    }

    // AppleScript that finds the process's biggest window as `best` (so a splash screen or a
    // dialog isn't picked), says "none" if System Events can't see the process yet (just
    // started) or it has no proper window yet, then runs `then`.
    fn with_main_window(pid: u32, then: &str) -> String {
        format!(
            r#"tell application "System Events"
	if not (exists (first process whose unix id is {pid})) then return "none"
	tell (first process whose unix id is {pid})
		set best to missing value
		set bestArea to 0
		repeat with w in windows
			set {{ww, hh}} to size of w
			if ww * hh > bestArea then
				set bestArea to ww * hh
				set best to contents of w
			end if
		end repeat
		if best is missing value or bestArea < 400000 then return "none"
		{then}
	end tell
end tell"#
        )
    }
    const CHECK: &str = r#"if value of attribute "AXFullScreen" of best is true then return "yes"
		return "no""#;
    const SET: &str = r#"set value of attribute "AXFullScreen" of best to true
		return "set""#;

    // Some(true or false) when System Events can tell; None when it isn't allowed to.
    pub fn is_full_screen(app: &Path) -> Option<bool> {
        let pid = pid_of(app)?;
        osascript(&with_main_window(pid, CHECK), Duration::from_secs(60)).ok().map(|s| s == "yes")
    }

    // Electron's window-state.json, in ~/Library/Application Support/Plasticity, or
    // plasticity-beta for the beta (the same names as on Windows).
    fn window_state(app: &Path) -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let name = app.file_stem()?.to_string_lossy().to_lowercase();
        let folder = if name.contains("beta") { "plasticity-beta" } else { "Plasticity" };
        Some(PathBuf::from(home).join("Library/Application Support").join(folder).join("window-state.json"))
    }

    fn read_state(path: &Path) -> Option<serde_json::Value> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    pub fn saved_full_screen(app: &Path) -> bool {
        window_state(app).and_then(|p| read_state(&p)).and_then(|v| v.get("isFullScreen")?.as_bool()).unwrap_or(false)
    }

    // Marks window-state.json full screen, for Plasticity to open that way by itself if it can.
    pub fn save_full_screen(app: &Path) {
        let Some(path) = window_state(app) else { return };
        let Some(mut v) = read_state(&path) else { return };
        if let Some(o) = v.as_object_mut() {
            o.insert("isFullScreen".into(), true.into());
            if let Ok(text) = serde_json::to_string_pretty(&v) {
                let _ = crate::write_atomic(&path, text.as_bytes());
            }
        }
    }

    // Waits up to 90 seconds for Plasticity's main window, gives it two seconds to go full
    // screen by itself, then puts it there. Says how it went (see Launched).
    pub fn restore(app: &Path) -> Option<String> {
        let started = Instant::now();
        let timed_out = || started.elapsed() > Duration::from_secs(90);
        let wait = Duration::from_secs(30);
        let pid = loop {
            if let Some(pid) = pid_of(app) {
                break pid;
            }
            if timed_out() {
                return Some("failed".into());
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        let mut seen = false;
        loop {
            match osascript(&with_main_window(pid, CHECK), wait).as_deref() {
                Ok("yes") => return Some(if seen { "restored" } else { "kept" }.into()),
                Ok("no") if seen => break,
                Ok("no") => seen = true,
                Err(e) if no_permission(e) => return Some("no-permission".into()),
                _ => {} // no window yet, or System Events can't see the process yet
            }
            if timed_out() {
                return Some("failed".into());
            }
            std::thread::sleep(Duration::from_secs(if seen { 2 } else { 1 }));
        }
        match osascript(&with_main_window(pid, SET), wait) {
            Err(e) if no_permission(&e) => return Some("no-permission".into()),
            Err(_) => return Some("failed".into()),
            Ok(_) => {}
        }
        std::thread::sleep(Duration::from_millis(1500));
        let done = osascript(&with_main_window(pid, CHECK), wait).as_deref() == Ok("yes");
        Some(if done { "restored" } else { "failed" }.into())
    }

    // Run on GitHub's Mac by the Mac test workflow (`cargo test -- --ignored mac_`). Ignored
    // otherwise, since talking to System Events makes a Mac ask for permission.
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[ignore]
        fn mac_window_script_before_the_process_shows() {
            let raw = r#"tell application "System Events" to tell (first process whose unix id is 999999) to get windows"#;
            println!("mac-test: without the exists check: {:?}", osascript(raw, Duration::from_secs(30)));
            let checked = osascript(&with_main_window(999_999, CHECK), Duration::from_secs(30));
            println!("mac-test: with it: {checked:?}");
            assert_eq!(checked.as_deref(), Ok("none"));
        }
    }
}

// Elsewhere nothing is done: Plasticity on Windows opens maximised again by itself, and Linux
// desktops have no one way to put another app's window in full screen.
#[cfg(not(target_os = "macos"))]
mod full_screen {
    use std::path::Path;
    pub fn is_full_screen(_app: &Path) -> Option<bool> {
        None
    }
    pub fn saved_full_screen(_app: &Path) -> bool {
        false
    }
    pub fn save_full_screen(_app: &Path) {}
    pub fn restore(_app: &Path) -> Option<String> {
        None
    }
}

// ---------- Starting Plasticity ----------
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Launched {
    path: String,
    // Only when it was in full screen: "kept", "restored", "no-permission" or "failed".
    full_screen: Option<String>,
}

// Starts the Plasticity that was open (`app`, from close_plasticity), or else the newest one
// installed, and on macOS puts it back in full screen if it was.
#[tauri::command]
pub async fn launch_plasticity(app: Option<String>, full_screen: Option<bool>) -> Result<Launched, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = match app.map(PathBuf::from).filter(|p| p.exists()) {
            Some(p) => launcher_for(p),
            None => newest_install().map(|(_, p)| p).ok_or("couldn't find Plasticity installed")?,
        };
        let full = full_screen.unwrap_or(false);
        if full {
            full_screen::save_full_screen(&path);
        }
        start(&path).map_err(|e| format!("couldn't start {}: {e}", path.display()))?;
        let full_screen = full.then(|| full_screen::restore(&path)).flatten();
        Ok(Launched { path: path.to_string_lossy().into_owned(), full_screen })
    })
    .await
    .map_err(|e| e.to_string())?
}

// On Windows, Plasticity runs from %LOCALAPPDATA%\plasticity(-beta)\app-<version>\, and the
// launcher of the same name one folder up starts its newest version, so an update downloaded
// meanwhile is picked up.
#[cfg(windows)]
fn launcher_for(exe: PathBuf) -> PathBuf {
    let up = exe.parent().and_then(|d| d.parent()).zip(exe.file_name()).map(|(root, name)| root.join(name));
    up.filter(|l| l.is_file()).unwrap_or(exe)
}
#[cfg(not(windows))]
fn launcher_for(app: PathBuf) -> PathBuf {
    app
}

#[cfg(windows)]
fn start(path: &Path) -> std::io::Result<()> {
    Command::new(path).current_dir(path.parent().unwrap_or(Path::new("."))).spawn().map(|_| ())
}
#[cfg(target_os = "macos")]
fn start(path: &Path) -> std::io::Result<()> {
    Command::new("open").arg(path).spawn().map(|_| ())
}
// Through sh in the background, so Plasticity isn't left a child of this app.
#[cfg(target_os = "linux")]
fn start(path: &Path) -> std::io::Result<()> {
    let status = Command::new("sh").args(["-c", r#""$0" </dev/null >/dev/null 2>&1 &"#]).arg(path).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("sh exited with {status}"))) }
}

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

// Each install as (version, what to start), for when no Plasticity was open to start again.
// On Windows Plasticity installs per user with Squirrel: %LOCALAPPDATA%\plasticity (or
// plasticity-beta) holds app-<version> folders and a launcher that starts the newest of them.
#[cfg(windows)]
fn installs() -> Vec<(String, PathBuf)> {
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return vec![] };
    let mut out = vec![];
    for name in ["plasticity", "plasticity-beta"] {
        let Ok(entries) = fs::read_dir(base.join(name)) else { continue };
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
#[cfg(target_os = "macos")]
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
                out.push((plist_string(&plist, "CFBundleShortVersionString").unwrap_or_default(), e.path()));
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

// On Linux: the .deb's /usr/lib/plasticity (or plasticity-beta), an /opt install, or a
// Plasticity AppImage in ~/Applications. Versions aren't known, so the release comes last and
// wins (see newest_install).
#[cfg(target_os = "linux")]
fn installs() -> Vec<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut out = vec![];
    for name in ["plasticity-beta", "plasticity"] {
        let exe = [Path::new("/usr/lib"), Path::new("/opt")].into_iter().map(|d| d.join(name).join(name)).find(|p| p.is_file());
        let image = fs::read_dir(home.join("Applications")).ok().and_then(|entries| {
            entries.flatten().map(|e| e.path()).find(|p| {
                let file = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                file.starts_with(name) && file.ends_with(".appimage") && (name == "plasticity-beta" || !file.contains("beta"))
            })
        });
        out.extend(exe.or(image).map(|p| (String::new(), p)));
    }
    out
}

fn newest_install() -> Option<(String, PathBuf)> {
    installs().into_iter().max_by(|a, b| version_key(&a.0).cmp(&version_key(&b.0)))
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

    #[test]
    fn linux_processes() {
        let none = || None;
        let exe = Path::new("/usr/lib/plasticity/plasticity");
        assert_eq!(linux_main_process(7, exe, b"/usr/lib/plasticity/plasticity\0--no-sandbox\0", none), Some((7, exe.to_path_buf())));
        assert_eq!(linux_main_process(8, exe, b"/usr/lib/plasticity/plasticity\0--type=renderer\0", none), None);
        let beta = Path::new("/usr/lib/plasticity-beta/plasticity-beta (deleted)");
        assert_eq!(linux_main_process(9, beta, b"x\0", none), Some((9, PathBuf::from("/usr/lib/plasticity-beta/plasticity-beta"))));
        assert_eq!(linux_main_process(10, Path::new("/usr/bin/blender"), b"blender\0", none), None);
        let mount = Path::new("/tmp/.mount_PlastiAb12/plasticity");
        let env = || Some(b"HOME=/home/u\0APPIMAGE=/home/u/Applications/Plasticity-26.2.0.AppImage\0".to_vec());
        assert_eq!(linux_main_process(11, mount, b"x\0", env), Some((11, PathBuf::from("/home/u/Applications/Plasticity-26.2.0.AppImage"))));
        // APPIMAGE left in the environment by something else doesn't count outside a mount
        assert_eq!(linux_main_process(12, exe, b"x\0", env), Some((12, exe.to_path_buf())));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_processes() {
        let main = parse_ps_line("  812 /Applications/Plasticity Beta.app/Contents/MacOS/Plasticity Beta");
        assert_eq!(main, Some((812, PathBuf::from("/Applications/Plasticity Beta.app"))));
        let helper = "  813 /Applications/Plasticity.app/Contents/Frameworks/Plasticity Helper (GPU).app/Contents/MacOS/Plasticity Helper (GPU) --type=gpu-process";
        assert_eq!(parse_ps_line(helper), None);
        assert_eq!(parse_ps_line("  90 /Applications/Safari.app/Contents/MacOS/Safari"), None);
    }

    // A tray switch on a real Mac, run by the Mac test workflow: it starts a copy of this app
    // named Plasticity.app (FAKE_PLASTICITY), and this puts it in full screen, closes it the way
    // a switch does, and starts it again in full screen. It quits the Plasticity it finds, so it
    // only runs on GitHub's Mac.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn mac_switch() {
        let fake = PathBuf::from(std::env::var_os("FAKE_PLASTICITY").expect("FAKE_PLASTICITY"));
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"), "only on GitHub's Mac");
        assert_eq!(running_app(), Some(fake.clone()), "the fake Plasticity is the one open");
        let fake_text = fake.to_string_lossy().into_owned();

        let first = full_screen::restore(&fake);
        println!("mac-test: full screen before the switch: {first:?}");
        let allowed = first.as_deref() != Some("no-permission");
        if allowed {
            assert_eq!(first.as_deref(), Some("restored"));
            assert_eq!(full_screen::is_full_screen(&fake), Some(true));
        }

        let closed = tauri::async_runtime::block_on(close_plasticity(60)).unwrap();
        println!("mac-test: closed: {}, app: {:?}, was full screen: {}", closed.closed, closed.app, closed.full_screen);
        assert!(closed.closed, "it quit when asked");
        assert_eq!(closed.app.as_deref(), Some(fake_text.as_str()));
        assert_eq!(running_app(), None);
        assert_eq!(closed.full_screen, allowed);

        let launched = tauri::async_runtime::block_on(launch_plasticity(closed.app, Some(true))).unwrap();
        println!("mac-test: started again: {}, full screen: {:?}", launched.path, launched.full_screen);
        assert_eq!(launched.path, fake_text);
        assert_eq!(running_app(), Some(fake.clone()));
        if allowed {
            assert_eq!(launched.full_screen.as_deref(), Some("restored"));
            assert_eq!(full_screen::is_full_screen(&fake), Some(true));
        } else {
            assert_eq!(launched.full_screen.as_deref(), Some("no-permission"));
        }
    }

    // A tray switch on Linux, run by the Linux test workflow: it starts a copy of this app named
    // plasticity (STANDIN_PLASTICITY), and this closes it the way a switch does and starts it
    // again. It closes the Plasticity it finds, so it only runs on GitHub's Linux machine.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn linux_switch() {
        let standin = PathBuf::from(std::env::var_os("STANDIN_PLASTICITY").expect("STANDIN_PLASTICITY"));
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"), "only on GitHub's Linux machine");
        assert_eq!(running_app(), Some(standin.clone()), "the stand-in Plasticity is the one open");
        let text = standin.to_string_lossy().into_owned();

        let closed = tauri::async_runtime::block_on(close_plasticity(60)).unwrap();
        println!("linux-test: closed: {}, app: {:?}, full screen: {}", closed.closed, closed.app, closed.full_screen);
        assert!(closed.closed, "it quit when asked");
        assert_eq!(closed.app.as_deref(), Some(text.as_str()));
        assert_eq!(running_app(), None);

        let launched = tauri::async_runtime::block_on(launch_plasticity(closed.app, Some(false))).unwrap();
        println!("linux-test: started again: {}, full screen: {:?}", launched.path, launched.full_screen);
        assert_eq!(launched.path, text);
        let started = std::time::Instant::now();
        while running_app().is_none() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(running_app(), Some(standin));
    }

    #[cfg(windows)]
    #[test]
    fn launcher_is_one_folder_up() {
        let root = std::env::temp_dir().join(format!("launcher-test-{}", std::process::id()));
        let app = root.join("app-26.2.0-beta30");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("plasticity-beta.exe"), "").unwrap();
        // no launcher yet: the exe itself
        assert_eq!(launcher_for(app.join("plasticity-beta.exe")), app.join("plasticity-beta.exe"));
        fs::write(root.join("plasticity-beta.exe"), "").unwrap();
        assert_eq!(launcher_for(app.join("plasticity-beta.exe")), root.join("plasticity-beta.exe"));
        let _ = fs::remove_dir_all(&root);
    }
}
