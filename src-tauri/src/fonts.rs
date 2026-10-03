// ---------- Fonts tab ----------
// Plasticity's Text command reads extra fonts from ~/.plasticity/fonts when it starts: one
// `NAME.typeface-json` file per font, in the three.js "typeface" format its own fonts use.
// Here TTF and OTF fonts are converted to that format.
//
// The format: glyph outlines as a command string per character, in units where the font's
// em is scaled by 100000 / (unitsPerEm * 72) (so `resolution` is 1000), y pointing up:
//   m x y            move to
//   l x y            line to
//   q x y cx cy      quadratic curve to (x, y), control point (cx, cy): end point first
//   z                close
// Plasticity's own fonts use only those four, so cubic curves (CFF / PostScript-flavoured
// OTF) are split into quadratics here rather than written as `b`.
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use ttf_parser::{name_id, Face, OutlineBuilder};

const EXT: &str = "typeface-json";
// What the tab draws as a sample, so only these glyphs travel to the page.
const SAMPLE: &str = "AaBbGg 123";
// Characters a font should have for everyday labels and part numbers.
const BASIC: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

// ---------- Conversion ----------
struct Pen {
    out: String,
    scale: f32,
    at: (f32, f32),
}

impl Pen {
    fn n(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }
    fn quad(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let s = format!("q {} {} {} {} ", self.n(x), self.n(y), self.n(cx), self.n(cy));
        self.out.push_str(&s);
        self.at = (x, y);
    }
    // A cubic as quadratics: one quadratic per piece, splitting the cubic in half until the
    // two agree to within a tenth of a font unit (or eight levels deep).
    fn cubic(&mut self, p0: (f32, f32), c1: (f32, f32), c2: (f32, f32), p3: (f32, f32), depth: u32) {
        let err = ((p3.0 - 3.0 * c2.0 + 3.0 * c1.0 - p0.0).powi(2) + (p3.1 - 3.0 * c2.1 + 3.0 * c1.1 - p0.1).powi(2)).sqrt()
            * 3f32.sqrt()
            / 36.0;
        if err * self.scale <= 0.1 || depth >= 8 {
            let q = ((3.0 * (c1.0 + c2.0) - p0.0 - p3.0) / 4.0, (3.0 * (c1.1 + c2.1) - p0.1 - p3.1) / 4.0);
            self.quad(q.0, q.1, p3.0, p3.1);
            return;
        }
        let mid = |a: (f32, f32), b: (f32, f32)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let (a, b, c) = (mid(p0, c1), mid(c1, c2), mid(c2, p3));
        let (d, e) = (mid(a, b), mid(b, c));
        let m = mid(d, e);
        self.cubic(p0, a, d, m, depth + 1);
        self.cubic(m, e, c, p3, depth + 1);
    }
}

impl OutlineBuilder for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        let s = format!("m {} {} ", self.n(x), self.n(y));
        self.out.push_str(&s);
        self.at = (x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let s = format!("l {} {} ", self.n(x), self.n(y));
        self.out.push_str(&s);
        self.at = (x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.quad(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let p0 = self.at;
        self.cubic(p0, (x1, y1), (x2, y2), (x, y), 0);
    }
    fn close(&mut self) {
        self.out.push_str("z ");
    }
}

fn name(face: &Face, id: u16) -> Option<String> {
    let names: Vec<_> = face.names().into_iter().filter(|n| n.name_id == id).collect();
    // English (Windows 0x0409 or Mac English) first, then anything readable
    names
        .iter()
        .filter(|n| n.language_id == 0x0409 || n.language_id == 0)
        .chain(names.iter())
        .find_map(|n| n.to_string())
        .filter(|s| !s.trim().is_empty())
}

pub struct Converted {
    json: String,
    info: FontInfo,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FontInfo {
    family: String,
    style: String,
    full_name: String,
    file_name: String,         // the suggested NAME.typeface-json
    glyphs: usize,
    missing: String,           // basic letters and digits the font doesn't have
    restricted: bool,          // the font's licence flags say it mustn't be embedded
    cubic: bool,               // had cubic curves, now quadratics
    sample: Value,             // { resolution, ascender, descender, glyphs: { char: { ha, o } } } for the page
}

// Same rule as the matcap names: letters, digits, _ and -.
fn clean(s: &str) -> String {
    crate::matcaps::clean_name(s)
}

fn sample_of(glyphs: &Map<String, Value>, resolution: f64, ascender: f64, descender: f64) -> Value {
    let mut pick = Map::new();
    for ch in SAMPLE.chars() {
        let k = ch.to_string();
        if let Some(g) = glyphs.get(&k) {
            pick.insert(k, json!({ "ha": g["ha"], "o": g["o"] }));
        }
    }
    json!({ "resolution": resolution, "ascender": ascender, "descender": descender, "glyphs": pick })
}

pub fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let face = Face::parse(bytes, 0).map_err(|e| format!("can't read this font: {e}"))?;
    let upem = face.units_per_em() as f32;
    if upem <= 0.0 {
        return Err("the font has no size (unitsPerEm is 0)".into());
    }
    let scale = 100_000.0 / (upem * 72.0);
    let n = |v: f32| (v * scale).round() as i32;

    let mut glyphs = Map::new();
    let mut cubic = false;
    let mut seen = std::collections::HashSet::new();
    if let Some(cmap) = face.tables().cmap {
        for sub in cmap.subtables.into_iter().filter(|s| s.is_unicode()) {
            let mut codes = vec![];
            sub.codepoints(|c| codes.push(c));
            for c in codes {
                let Some(ch) = char::from_u32(c) else { continue };
                if ch.is_control() || !seen.insert(ch) {
                    continue;
                }
                let Some(id) = sub.glyph_index(c) else { continue };
                let advance = face.glyph_hor_advance(id).unwrap_or(0) as f32;
                let mut pen = Pen { out: String::new(), scale, at: (0.0, 0.0) };
                let bbox = face.outline_glyph(id, &mut pen);
                cubic |= face.tables().cff.is_some() || face.tables().cff2.is_some();
                let glyph = match bbox {
                    Some(b) => json!({ "ha": n(advance), "x_min": n(b.x_min as f32), "x_max": n(b.x_max as f32), "o": pen.out.trim_end() }),
                    None => json!({ "ha": n(advance), "x_min": null, "x_max": null, "o": "" }),
                };
                glyphs.insert(ch.to_string(), glyph);
            }
        }
    }
    if glyphs.is_empty() {
        return Err("the font has no characters Plasticity could use".into());
    }

    let family = name(&face, name_id::TYPOGRAPHIC_FAMILY).or_else(|| name(&face, name_id::FAMILY)).unwrap_or_else(|| "Font".into());
    let style = name(&face, name_id::TYPOGRAPHIC_SUBFAMILY).or_else(|| name(&face, name_id::SUBFAMILY)).unwrap_or_else(|| "Regular".into());
    let full_name = name(&face, name_id::FULL_NAME).unwrap_or_else(|| format!("{family} {style}"));
    let mut info = Map::new();
    for (key, id) in [
        ("copyright", name_id::COPYRIGHT_NOTICE),
        ("fontFamily", name_id::FAMILY),
        ("fontSubfamily", name_id::SUBFAMILY),
        ("uniqueID", name_id::UNIQUE_ID),
        ("fullName", name_id::FULL_NAME),
        ("version", name_id::VERSION),
        ("postScriptName", name_id::POST_SCRIPT_NAME),
        ("licenseURL", name_id::LICENSE_URL),
    ] {
        if let Some(v) = name(&face, id) {
            info.insert(key.into(), json!({ "en": v }));
        }
    }
    let b = face.global_bounding_box();
    let underline = face.underline_metrics();
    let ascender = n(face.ascender() as f32);
    let descender = n(face.descender() as f32);
    let doc = json!({
        "glyphs": glyphs,
        "ascender": ascender,
        "descender": descender,
        "underlinePosition": underline.map(|u| n(u.position as f32)).unwrap_or(0),
        "underlineThickness": underline.map(|u| n(u.thickness as f32)).unwrap_or(0),
        "boundingBox": { "yMin": n(b.y_min as f32), "xMin": n(b.x_min as f32), "yMax": n(b.y_max as f32), "xMax": n(b.x_max as f32) },
        "resolution": 1000,
        "original_font_information": info,
        "cssFontWeight": face.weight().to_number().to_string(),
        "cssFontStyle": if face.is_italic() { "italic" } else { "normal" },
    });

    let missing: String = BASIC.chars().filter(|c| !glyphs.contains_key(&c.to_string())).collect();
    let restricted = matches!(face.permissions(), Some(ttf_parser::Permissions::Restricted));
    let file_name = format!("{}-{}.{EXT}", clean(&family), clean(&style));
    let sample = sample_of(&glyphs, 1000.0, ascender as f64, descender as f64);
    let glyph_count = glyphs.len();
    Ok(Converted {
        json: serde_json::to_string(&doc).map_err(|e| e.to_string())?,
        info: FontInfo { family, style, full_name, file_name, glyphs: glyph_count, missing, restricted, cubic, sample },
    })
}

// ---------- Staging: fonts dropped or picked, held until installed ----------
#[derive(Default)]
pub struct FontStaging {
    next_id: u64,
    fonts: HashMap<u64, Converted>,
}
pub type FontStagingState = Mutex<FontStaging>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedFont {
    id: u64,
    source: String, // the dropped file's name
    #[serde(flatten)]
    info: FontInfo,
}

fn stage(state: &FontStagingState, source: String, bytes: &[u8]) -> Result<StagedFont, String> {
    let lower = source.to_lowercase();
    if !(lower.ends_with(".ttf") || lower.ends_with(".otf")) {
        return Err(format!("{source}: only TTF and OTF fonts can be used"));
    }
    let converted = convert(bytes).map_err(|e| format!("{source}: {e}"))?;
    let info = converted.info.clone();
    let mut s = state.lock().map_err(|e| e.to_string())?;
    s.next_id += 1;
    let id = s.next_id;
    s.fonts.insert(id, converted);
    Ok(StagedFont { id, source, info })
}

#[tauri::command]
pub async fn stage_font_bytes(
    request: tauri::ipc::Request<'_>,
    state: tauri::State<'_, FontStagingState>,
) -> Result<StagedFont, String> {
    let source = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .map(crate::matcaps::percent_decode)
        .ok_or("missing file name")?;
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => bytes.clone(),
        tauri::ipc::InvokeBody::Json(value) => serde_json::from_value::<Vec<u8>>(value.clone())
            .map_err(|_| "expected the file's bytes".to_string())?,
    };
    stage(&state, source, &bytes)
}

#[tauri::command]
pub async fn stage_font_path(path: String, state: tauri::State<'_, FontStagingState>) -> Result<StagedFont, String> {
    let path = PathBuf::from(path);
    let source = path.file_name().and_then(|n| n.to_str()).ok_or("bad file name")?.to_string();
    let bytes = fs::read(&path).map_err(|e| format!("{source}: {e}"))?;
    stage(&state, source, &bytes)
}

#[tauri::command]
pub fn unstage_fonts(ids: Vec<u64>, state: tauri::State<'_, FontStagingState>) -> Result<(), String> {
    let mut s = state.lock().map_err(|e| e.to_string())?;
    ids.iter().for_each(|id| {
        s.fonts.remove(id);
    });
    Ok(())
}

fn check_file_name(file_name: &str) -> Result<(), String> {
    let stem = file_name.strip_suffix(&format!(".{EXT}")).ok_or_else(|| format!("{file_name} must end in .{EXT}"))?;
    if stem.is_empty() || clean(stem) != stem {
        return Err(format!("\"{file_name}\" isn't a clean font file name"));
    }
    Ok(())
}

#[tauri::command]
pub async fn install_font(
    folder: String,
    id: u64,
    file_name: String,
    replace: bool,
    state: tauri::State<'_, FontStagingState>,
) -> Result<String, String> {
    check_file_name(&file_name)?;
    let folder = PathBuf::from(folder);
    let path = folder.join(&file_name);
    if path.exists() && !replace {
        return Err(format!("{file_name} is already installed"));
    }
    let json = {
        let s = state.lock().map_err(|e| e.to_string())?;
        s.fonts.get(&id).ok_or("the font has gone missing; add it again")?.json.clone()
    };
    fs::create_dir_all(&folder).map_err(|e| format!("create {} failed: {e}", folder.display()))?;
    fs::write(&path, json).map_err(|e| format!("write {file_name} failed: {e}"))?;
    state.lock().map_err(|e| e.to_string())?.fonts.remove(&id);
    Ok(file_name)
}

// ---------- Installed fonts ----------
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledFont {
    file_name: String,
    full_name: String, // what the font calls itself, e.g. "Arial Bold"
    family: String,
    style: String,
    glyphs: usize,
    size: u64,
    error: Option<String>, // the file can't be read as a font
    sample: Value,
}

fn en(info: &Value, key: &str) -> Option<String> {
    let v = &info[key];
    v["en"].as_str().or_else(|| v.as_str()).map(String::from)
}

fn read_installed(path: &Path) -> InstalledFont {
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let parsed = fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| format!("not valid JSON ({e})")));
    match parsed {
        Ok(doc) if doc["glyphs"].is_object() => {
            let glyphs = doc["glyphs"].as_object().unwrap();
            let info = &doc["original_font_information"];
            let num = |k: &str| doc[k].as_f64().unwrap_or(0.0);
            let family = en(info, "fontFamily").unwrap_or_else(|| file_name.clone());
            let style = en(info, "fontSubfamily").unwrap_or_default();
            InstalledFont {
                full_name: en(info, "fullName").unwrap_or_else(|| format!("{family} {style}").trim().to_string()),
                family,
                style,
                glyphs: glyphs.len(),
                size,
                error: None,
                sample: sample_of(glyphs, num("resolution").max(1.0), num("ascender"), num("descender")),
                file_name,
            }
        }
        Ok(_) => InstalledFont { file_name, full_name: String::new(), family: String::new(), style: String::new(), glyphs: 0, size, error: Some("has no glyphs".into()), sample: Value::Null },
        Err(e) => InstalledFont { file_name, full_name: String::new(), family: String::new(), style: String::new(), glyphs: 0, size, error: Some(e), sample: Value::Null },
    }
}

#[tauri::command]
pub async fn list_installed_fonts(folder: String) -> Result<Vec<InstalledFont>, String> {
    let entries = match fs::read_dir(&folder) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("read {folder} failed: {e}")),
    };
    let mut list: Vec<InstalledFont> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case(EXT)) == Some(true))
        .map(|p| read_installed(&p))
        .collect();
    list.sort_by(|a, b| a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()));
    Ok(list)
}

// Removing goes to the Recycle Bin / Trash, so it can be undone.
#[tauri::command]
pub async fn trash_font(folder: String, file_name: String) -> Result<(), String> {
    if file_name.contains(['/', '\\']) || !file_name.to_lowercase().ends_with(&format!(".{EXT}")) {
        return Err(format!("{file_name} isn't a font file"));
    }
    trash::delete(Path::new(&folder).join(&file_name)).map_err(|e| format!("couldn't move {file_name} to the bin: {e}"))
}

// ---------- Folder and file pickers ----------
fn fonts_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().home_dir().ok()?.join(".plasticity").join("fonts"))
}

#[tauri::command]
pub fn default_fonts_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(fonts_dir(&app).map(|d| d.to_string_lossy().into_owned()))
}

#[tauri::command]
pub async fn pick_font_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let picked = app.dialog().file().add_filter("Fonts", &["ttf", "otf"]).set_title("Add fonts").blocking_pick_files();
    Ok(picked.unwrap_or_default().into_iter().map(|p| p.to_string()).collect())
}

#[tauri::command]
pub async fn pick_fonts_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file().set_title("Choose Plasticity's fonts folder (.plasticity/fonts)");
    if let Some(dir) = fonts_dir(&app).filter(|d| d.is_dir()).or_else(|| fonts_dir(&app).and_then(|d| d.parent().map(Path::to_path_buf))) {
        builder = builder.set_directory(dir);
    }
    Ok(builder.blocking_pick_folder().map(|p| p.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A font shipped with Windows, if this machine has one, converted and checked against
    // the rules Plasticity's own fonts follow.
    #[test]
    fn converts_a_system_font() {
        let candidates = ["C:/Windows/Fonts/arial.ttf", "/System/Library/Fonts/Supplemental/Arial.ttf", "/Library/Fonts/Arial.ttf"];
        let Some(bytes) = candidates.iter().find_map(|p| fs::read(p).ok()) else {
            eprintln!("no system font to test with; skipped");
            return;
        };
        let c = convert(&bytes).unwrap();
        assert!(c.info.glyphs > 100);
        assert_eq!(c.info.missing, "");
        let doc: Value = serde_json::from_str(&c.json).unwrap();
        assert_eq!(doc["resolution"], 1000);
        // every outline is m/l/q/z with the right number of whole numbers after each
        let mut checked = 0;
        for (ch, g) in doc["glyphs"].as_object().unwrap() {
            let o = g["o"].as_str().unwrap();
            let parts: Vec<&str> = o.split_whitespace().collect();
            let mut i = 0;
            while i < parts.len() {
                let count = match parts[i] {
                    "m" | "l" => 2,
                    "q" => 4,
                    "z" => 0,
                    other => panic!("{ch}: unexpected command {other}"),
                };
                for k in 1..=count {
                    parts[i + k].parse::<i32>().unwrap_or_else(|_| panic!("{ch}: {o}"));
                }
                i += count + 1;
            }
            checked += 1;
        }
        assert!(checked > 100);
        // a space has an advance but no outline
        assert_eq!(doc["glyphs"][" "]["o"], "");
        assert!(doc["glyphs"][" "]["ha"].as_i64().unwrap() > 0);
    }

    #[test]
    fn cubic_split_keeps_ends() {
        let mut pen = Pen { out: String::new(), scale: 1.0, at: (0.0, 0.0) };
        pen.move_to(0.0, 0.0);
        pen.curve_to(0.0, 100.0, 100.0, 100.0, 100.0, 0.0);
        let last = pen.out.trim_end().rsplit("q ").next().unwrap().to_string();
        assert!(last.starts_with("100 0 "), "{}", pen.out);
        assert!(!pen.out.contains('b'));
        assert!(pen.out.matches("q ").count() >= 2);
    }

    #[test]
    fn file_names() {
        assert!(check_file_name("DIN_Pro-Bold.typeface-json").is_ok());
        assert!(check_file_name("DIN Pro.typeface-json").is_err());
        assert!(check_file_name("x.json").is_err());
    }
}
