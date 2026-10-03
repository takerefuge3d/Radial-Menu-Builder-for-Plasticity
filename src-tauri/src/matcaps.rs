// ---------- Matcaps tab ----------
// Plasticity keeps each matcap in ~/.plasticity/matcaps as a pair with the same name:
// NAME.exr (the texture, linear colour) and NAME.png (the picker thumbnail, sRGB).
// An optional NAME.json holding `{ isTinted: true }` makes the matcap tintable.
//
// Images are handled as premultiplied linear RGBA (EXR's own convention). The EXRs we
// write match the ones Plasticity ships: RGBA half floats, ZIP16, scan lines, top to bottom.
// PNGs are written as 8-bit sRGB with straight alpha.
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Mutex,
};

use base64::Engine;
use exr::prelude::{f16, ReadChannels, ReadLayers};
use image::{imageops, ImageFormat, Rgba, Rgba32FImage, RgbaImage};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

const PREVIEW_SIZE: u32 = 96;
const PNG_SIZES: [u32; 2] = [64, 128];
const EXR_SIZES: [u32; 2] = [512, 1024];
const TINT_JSON: &str = "{\n  isTinted: true\n}\n";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Png,
    Jpg,
    Exr,
}

impl Kind {
    fn from_ext(ext: &str) -> Option<Kind> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Kind::Png),
            "jpg" | "jpeg" => Some(Kind::Jpg),
            "exr" => Some(Kind::Exr),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpg => "jpg",
            Kind::Exr => "exr",
        }
    }
}

// ---------- Names ----------
// Spaces become underscores, anything else that is not a letter, digit, _ or - becomes an
// underscore too, runs of underscores collapse and the ends are trimmed. The same rule
// is in index.html (mcClean) so the tab can show the name before saving.
pub fn clean_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.trim().chars() {
        let c = if c.is_alphanumeric() || c == '-' { c } else { '_' };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches(|c| c == '_' || c == '-').to_string()
}

fn split_name(file_name: &str) -> Option<(String, Kind, String)> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    Some((stem.to_string(), Kind::from_ext(ext)?, ext.to_string()))
}

// ---------- Pixels ----------
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}
fn linear_to_srgb(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 }
}

struct Decoded {
    img: Rgba32FImage, // premultiplied, linear
    has_alpha: bool,
}

fn decode(kind: Kind, bytes: &[u8]) -> Result<Decoded, String> {
    match kind {
        Kind::Png | Kind::Jpg => {
            let format = if kind == Kind::Png { ImageFormat::Png } else { ImageFormat::Jpeg };
            let dynamic = image::load_from_memory_with_format(bytes, format)
                .map_err(|e| format!("can't read this {}: {e}", kind.label().to_uppercase()))?;
            let has_alpha = dynamic.color().has_alpha();
            let mut img = dynamic.to_rgba32f();
            for p in img.pixels_mut() {
                let a = p[3];
                for i in 0..3 {
                    p[i] = srgb_to_linear(p[i]) * a;
                }
            }
            Ok(Decoded { img, has_alpha })
        }
        Kind::Exr => {
            let image = exr::prelude::read()
                .no_deep_data()
                .largest_resolution_level()
                .rgba_channels(
                    |size, _| Rgba32FImage::new(size.width() as u32, size.height() as u32),
                    |img: &mut Rgba32FImage, pos, (r, g, b, a): (f32, f32, f32, f32)| {
                        let fix = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
                        img.put_pixel(pos.x() as u32, pos.y() as u32, Rgba([fix(r), fix(g), fix(b), fix(a).min(1.0)]));
                    },
                )
                .first_valid_layer()
                .all_attributes()
                .from_buffered(Cursor::new(bytes))
                .map_err(|e| format!("can't read this EXR (it needs R, G and B channels): {e}"))?;
            let has_alpha = image
                .layer_data
                .channel_data
                .channels
                .3
                .is_some();
            Ok(Decoded { img: image.layer_data.channel_data.pixels, has_alpha })
        }
    }
}

// Centre square, resized to `size`. Optionally cut to the circle that fills it, with a
// one-pixel soft edge. Works on premultiplied pixels, so the cut scales all four channels.
fn square(src: &Rgba32FImage, size: u32, cut_circle: bool) -> Rgba32FImage {
    let (w, h) = src.dimensions();
    let side = w.min(h);
    let cropped = imageops::crop_imm(src, (w - side) / 2, (h - side) / 2, side, side).to_image();
    let mut out = if side == size {
        cropped
    } else {
        imageops::resize(&cropped, size, size, imageops::FilterType::Lanczos3)
    };
    for p in out.pixels_mut() {
        // Lanczos rings a little past 0..1 at hard edges
        let a = p[3].clamp(0.0, 1.0);
        p[3] = a;
        for i in 0..3 {
            p[i] = p[i].max(0.0);
        }
    }
    if cut_circle {
        let r = size as f32 / 2.0;
        for (x, y, p) in out.enumerate_pixels_mut() {
            let dx = x as f32 + 0.5 - r;
            let dy = y as f32 + 0.5 - r;
            let cover = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if cover < 1.0 {
                for i in 0..4 {
                    p[i] *= cover;
                }
            }
        }
    }
    out
}

fn encode_png(img: &Rgba32FImage) -> Result<Vec<u8>, String> {
    let mut out = RgbaImage::new(img.width(), img.height());
    for (src, dst) in img.pixels().zip(out.pixels_mut()) {
        let a = src[3];
        let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        dst.0 = if a <= 0.0 {
            [0, 0, 0, 0]
        } else {
            [
                to8(linear_to_srgb(src[0] / a)),
                to8(linear_to_srgb(src[1] / a)),
                to8(linear_to_srgb(src[2] / a)),
                to8(a),
            ]
        };
    }
    let mut bytes = Vec::new();
    out.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed: {e}"))?;
    Ok(bytes)
}

fn encode_exr(img: &Rgba32FImage) -> Result<Vec<u8>, String> {
    let (w, h) = img.dimensions();
    let channels = exr::image::SpecificChannels::rgba(|pos: exr::math::Vec2<usize>| {
        let p = img.get_pixel(pos.x() as u32, pos.y() as u32);
        (f16::from_f32(p[0]), f16::from_f32(p[1]), f16::from_f32(p[2]), f16::from_f32(p[3]))
    });
    let image = exr::image::Image::from_encoded_channels(
        (w as usize, h as usize),
        exr::image::Encoding::SMALL_LOSSLESS,
        channels,
    );
    let mut bytes = Vec::new();
    exr::image::write::WritableImage::write(&image)
        .to_buffered(Cursor::new(&mut bytes))
        .map_err(|e| format!("EXR encode failed: {e}"))?;
    Ok(bytes)
}

fn data_url(img: &Rgba32FImage) -> Result<String, String> {
    let small = square(img, PREVIEW_SIZE, false);
    let png = encode_png(&small)?;
    Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

fn check_size(size: u32, allowed: &[u32], what: &str) -> Result<u32, String> {
    if allowed.contains(&size) {
        Ok(size)
    } else {
        Err(format!("{what} size must be one of {allowed:?}"))
    }
}

// ---------- Staging: files dropped or picked, held until saved ----------
struct StagedFile {
    kind: Kind,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct Staging {
    next_id: u64,
    files: HashMap<u64, StagedFile>,
}

pub type StagingState = Mutex<Staging>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedInfo {
    id: u64,
    file_name: String,
    stem: String,
    kind: &'static str,
    width: u32,
    height: u32,
    has_alpha: bool,
    preview: String,
}

fn stage(state: &StagingState, file_name: String, bytes: Vec<u8>) -> Result<StagedInfo, String> {
    let (stem, kind, _) = split_name(&file_name)
        .ok_or_else(|| format!("{file_name}: only PNG, JPG and EXR files can be used"))?;
    let decoded = decode(kind, &bytes).map_err(|e| format!("{file_name}: {e}"))?;
    let preview = data_url(&decoded.img)?;
    let (width, height) = decoded.img.dimensions();
    let mut staging = state.lock().map_err(|e| e.to_string())?;
    staging.next_id += 1;
    let id = staging.next_id;
    staging.files.insert(id, StagedFile { kind, bytes });
    Ok(StagedInfo {
        id,
        file_name,
        stem,
        kind: kind.label(),
        width,
        height,
        has_alpha: decoded.has_alpha,
        preview,
    })
}

// Dropped files arrive as raw bytes, with the file name in a percent-encoded header.
#[tauri::command]
pub async fn stage_matcap_bytes(
    request: tauri::ipc::Request<'_>,
    state: tauri::State<'_, StagingState>,
) -> Result<StagedInfo, String> {
    let file_name = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .ok_or("missing file name")?;
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => bytes.clone(),
        // the postMessage fallback sends a typed array as a list of numbers
        tauri::ipc::InvokeBody::Json(value) => serde_json::from_value::<Vec<u8>>(value.clone())
            .map_err(|_| "expected the file's bytes".to_string())?,
    };
    stage(&state, file_name, bytes)
}

#[tauri::command]
pub async fn stage_matcap_path(path: String, state: tauri::State<'_, StagingState>) -> Result<StagedInfo, String> {
    let path = PathBuf::from(path);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("bad file name")?
        .to_string();
    let bytes = fs::read(&path).map_err(|e| format!("{file_name}: {e}"))?;
    stage(&state, file_name, bytes)
}

#[tauri::command]
pub fn unstage_matcaps(ids: Vec<u64>, state: tauri::State<'_, StagingState>) -> Result<(), String> {
    let mut staging = state.lock().map_err(|e| e.to_string())?;
    for id in ids {
        staging.files.remove(&id);
    }
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- Saving a new matcap ----------
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveRequest {
    folder: String,
    name: String,
    png_id: Option<u64>,
    exr_id: Option<u64>,
    jpg_id: Option<u64>,
    png_size: u32,
    exr_size: u32,
    tinted: bool,
    replace: bool,
}

fn existing_files(folder: &Path, name: &str) -> Vec<PathBuf> {
    ["png", "exr", "json"]
        .iter()
        .map(|ext| folder.join(format!("{name}.{ext}")))
        .filter(|p| p.exists())
        .collect()
}

#[tauri::command]
pub async fn save_matcap(req: SaveRequest, state: tauri::State<'_, StagingState>) -> Result<Vec<String>, String> {
    let png_size = check_size(req.png_size, &PNG_SIZES, "PNG")?;
    let exr_size = check_size(req.exr_size, &EXR_SIZES, "EXR")?;
    if req.name.is_empty() || clean_name(&req.name) != req.name {
        return Err(format!("\"{}\" isn't a clean matcap name", req.name));
    }
    let folder = PathBuf::from(&req.folder);
    let taken = existing_files(&folder, &req.name);
    if !taken.is_empty() && !req.replace {
        return Err(format!("{} is already installed", req.name));
    }

    let take = |id: Option<u64>| -> Result<Option<StagedFile>, String> {
        let Some(id) = id else { return Ok(None) };
        let staging = state.lock().map_err(|e| e.to_string())?;
        let f = staging.files.get(&id).ok_or("a dropped file has gone missing; drop it again")?;
        Ok(Some(StagedFile { kind: f.kind, bytes: f.bytes.clone() }))
    };
    let png = take(req.png_id)?;
    let exr = take(req.exr_id)?;
    let jpg = take(req.jpg_id)?;

    // Files given are copied as they are; only the missing half of the pair is made.
    // The EXR is the better source for a PNG (more range); a PNG beats a JPG for an EXR.
    let png_bytes = match &png {
        Some(f) => f.bytes.clone(),
        None => {
            let (src, cut) = match (&exr, &jpg) {
                (Some(f), _) => (f, false),
                (None, Some(f)) => (f, true),
                _ => return Err("nothing to make the PNG from".into()),
            };
            encode_png(&square(&decode(src.kind, &src.bytes)?.img, png_size, cut))?
        }
    };
    let exr_bytes = match &exr {
        Some(f) => f.bytes.clone(),
        None => {
            let (src, cut) = match (&png, &jpg) {
                (Some(f), _) => (f, false),
                (None, Some(f)) => (f, true),
                _ => return Err("nothing to make the EXR from".into()),
            };
            encode_exr(&square(&decode(src.kind, &src.bytes)?.img, exr_size, cut))?
        }
    };

    fs::create_dir_all(&folder).map_err(|e| format!("create {} failed: {e}", folder.display()))?;
    if req.replace {
        // Replacing means the old matcap goes entirely, including its tint setting.
        for p in &taken {
            fs::remove_file(p).map_err(|e| format!("remove {} failed: {e}", p.display()))?;
        }
    }
    let mut written = vec![];
    let mut write = |ext: &str, bytes: &[u8]| -> Result<(), String> {
        let path = folder.join(format!("{}.{ext}", req.name));
        fs::write(&path, bytes).map_err(|e| format!("write {} failed: {e}", path.display()))?;
        written.push(format!("{}.{ext}", req.name));
        Ok(())
    };
    write("exr", &exr_bytes)?;
    write("png", &png_bytes)?;
    if req.tinted {
        write("json", TINT_JSON.as_bytes())?;
    }

    let mut staging = state.lock().map_err(|e| e.to_string())?;
    for id in [req.png_id, req.exr_id, req.jpg_id].into_iter().flatten() {
        staging.files.remove(&id);
    }
    Ok(written)
}

// ---------- Installed matcaps ----------
#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    name: String,              // the clean name the pair should have
    png: Option<String>,       // file names as found
    exr: Option<String>,
    jpg: Option<String>,
    json: Option<String>,
    extra: Vec<String>,        // further files that clean to the same name
    tinted: bool,
    issues: Vec<String>,
    fixable: bool,
    fix: Vec<String>,          // what Fix would do, in plain words
}

fn is_tinted(path: &Path) -> bool {
    let text: String = fs::read_to_string(path)
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
        .collect();
    text.contains("isTinted:true")
}

fn scan(folder: &Path) -> Result<Vec<Installed>, String> {
    let mut groups: BTreeMap<String, Installed> = BTreeMap::new();
    let entries = match fs::read_dir(folder) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("read {} failed: {e}", folder.display())),
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Some(file_name) = entry.file_name().to_str().map(String::from) else { continue };
        let Some((stem, ext)) = file_name.rsplit_once('.') else { continue };
        let ext_lower = ext.to_ascii_lowercase();
        let slot = match ext_lower.as_str() {
            "png" | "exr" | "jpg" | "jpeg" | "json" => ext_lower.as_str(),
            _ => continue,
        };
        let key = clean_name(stem).to_lowercase();
        let g = groups.entry(key).or_default();
        let field = match slot {
            "png" => &mut g.png,
            "exr" => &mut g.exr,
            "json" => &mut g.json,
            _ => &mut g.jpg,
        };
        if field.is_none() {
            *field = Some(file_name);
        } else {
            g.extra.push(file_name);
        }
    }

    let stem_of = |f: &Option<String>| f.as_ref().map(|n| n.rsplit_once('.').unwrap().0.to_string());
    let mut list = vec![];
    for (_, mut g) in groups {
        let exr_stem = stem_of(&g.exr);
        let png_stem = stem_of(&g.png);
        let any_stem = exr_stem.clone().or(png_stem.clone()).or(stem_of(&g.jpg)).or(stem_of(&g.json)).unwrap();
        g.name = clean_name(&any_stem);
        g.tinted = g.json.as_ref().map(|j| is_tinted(&folder.join(j))).unwrap_or(false);

        let has_image = g.png.is_some() || g.exr.is_some() || g.jpg.is_some();
        if !has_image {
            g.issues.push("Settings file with no matcap".into());
        } else {
            if g.exr.is_none() {
                g.issues.push("No EXR".into());
            }
            if g.png.is_none() {
                g.issues.push("No PNG".into());
            }
            if let (Some(e), Some(p)) = (&exr_stem, &png_stem) {
                if e != p {
                    g.issues.push("PNG and EXR names differ".into());
                }
            }
            let names = [&g.png, &g.exr, &g.json, &g.jpg];
            if names.iter().filter_map(|f| f.as_ref()).any(|f| {
                let (stem, ext) = f.rsplit_once('.').unwrap();
                clean_name(stem) != stem || ext != ext.to_ascii_lowercase()
            }) {
                g.issues.push("Name has spaces or odd characters".into());
            }
        }
        if g.name.is_empty() {
            g.issues.push("Name has no letters or digits".into());
        }
        if !g.extra.is_empty() {
            g.issues.push(format!("Clashes with {}", g.extra.join(", ")));
        }

        // Plan the fix: rename everything to the clean name, then make what's missing.
        g.fixable = has_image && !g.issues.is_empty() && g.extra.is_empty() && !g.name.is_empty();
        if g.fixable {
            for f in [&g.exr, &g.png, &g.json].into_iter().flatten() {
                let ext = f.rsplit_once('.').unwrap().1.to_ascii_lowercase();
                let target = format!("{}.{ext}", g.name);
                if *f != target {
                    g.fix.push(format!("Rename {f} to {target}"));
                }
            }
            if g.exr.is_none() {
                let from = g.png.as_ref().or(g.jpg.as_ref()).unwrap();
                g.fix.push(format!("Make {}.exr from {from}", g.name));
            }
            if g.png.is_none() {
                let from = g.exr.as_ref().or(g.jpg.as_ref()).unwrap();
                g.fix.push(format!("Make {}.png from {from}", g.name));
            }
        }
        list.push(g);
    }
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(list)
}

#[tauri::command]
pub async fn list_installed_matcaps(folder: String) -> Result<Vec<Installed>, String> {
    scan(Path::new(&folder))
}

// Thumbnail for the installed list: the PNG if there is one, else the EXR or JPG.
#[tauri::command]
pub async fn installed_matcap_preview(folder: String, file_name: String) -> Result<String, String> {
    let (_, kind, _) = split_name(&file_name).ok_or("not an image")?;
    let bytes = fs::read(Path::new(&folder).join(&file_name)).map_err(|e| e.to_string())?;
    data_url(&decode(kind, &bytes)?.img)
}

// Only runs on a click. Re-reads the folder rather than trusting the list the tab shows.
#[tauri::command]
pub async fn fix_installed_matcap(folder: String, name: String, png_size: u32, exr_size: u32) -> Result<Vec<String>, String> {
    let png_size = check_size(png_size, &PNG_SIZES, "PNG")?;
    let exr_size = check_size(exr_size, &EXR_SIZES, "EXR")?;
    let folder = PathBuf::from(folder);
    let g = scan(&folder)?
        .into_iter()
        .find(|g| g.name == name)
        .ok_or_else(|| format!("{name} is no longer in the folder"))?;
    if !g.fixable {
        return Err(format!("{name} can't be fixed automatically"));
    }

    // Read the sources before renaming anything, so a bad file stops the fix early.
    let read = |f: &String| -> Result<(Kind, Vec<u8>), String> {
        let (_, kind, _) = split_name(f).unwrap();
        Ok((kind, fs::read(folder.join(f)).map_err(|e| format!("read {f} failed: {e}"))?))
    };
    let new_exr = match &g.exr {
        Some(_) => None,
        None => {
            let (src, cut) = match (&g.png, &g.jpg) {
                (Some(f), _) => (f, false),
                (None, Some(f)) => (f, true),
                _ => unreachable!(),
            };
            let (kind, bytes) = read(src)?;
            Some(encode_exr(&square(&decode(kind, &bytes)?.img, exr_size, cut))?)
        }
    };
    let new_png = match &g.png {
        Some(_) => None,
        None => {
            let (src, cut) = match (&g.exr, &g.jpg) {
                (Some(f), _) => (f, false),
                (None, Some(f)) => (f, true),
                _ => unreachable!(),
            };
            let (kind, bytes) = read(src)?;
            Some(encode_png(&square(&decode(kind, &bytes)?.img, png_size, cut))?)
        }
    };

    let mut done = vec![];
    for f in [&g.exr, &g.png, &g.json].into_iter().flatten() {
        let ext = f.rsplit_once('.').unwrap().1.to_ascii_lowercase();
        let target = format!("{name}.{ext}");
        if *f != target {
            fs::rename(folder.join(f), folder.join(&target)).map_err(|e| format!("rename {f} failed: {e}"))?;
            done.push(format!("Renamed {f} to {target}"));
        }
    }
    for (ext, bytes) in [("exr", new_exr), ("png", new_png)] {
        if let Some(bytes) = bytes {
            let target = format!("{name}.{ext}");
            fs::write(folder.join(&target), bytes).map_err(|e| format!("write {target} failed: {e}"))?;
            done.push(format!("Made {target}"));
        }
    }
    Ok(done)
}

// ---------- Folder and file pickers ----------
fn matcaps_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().home_dir().ok()?.join(".plasticity").join("matcaps"))
}

#[tauri::command]
pub fn default_matcaps_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(matcaps_dir(&app).map(|d| d.to_string_lossy().into_owned()))
}

#[tauri::command]
pub async fn pick_matcap_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter("Matcap images", &["png", "jpg", "jpeg", "exr"])
        .set_title("Add matcap images")
        .blocking_pick_files();
    Ok(picked.unwrap_or_default().into_iter().map(|p| p.to_string()).collect())
}

#[tauri::command]
pub async fn pick_matcaps_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file().set_title("Choose Plasticity's matcaps folder (.plasticity/matcaps)");
    if let Some(dir) = matcaps_dir(&app).filter(|d| d.is_dir()) {
        builder = builder.set_directory(dir);
    }
    Ok(builder.blocking_pick_folder().map(|p| p.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(clean_name("Dusty Courtyard"), "Dusty_Courtyard");
        assert_eq!(clean_name("  my  cap (v2) "), "my_cap_v2");
        assert_eq!(clean_name("a.b-c"), "a_b-c");
        assert_eq!(clean_name("__x__"), "x");
        assert_eq!(clean_name("TRChromeTR"), "TRChromeTR");
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("My%20Cap%C3%A9.png"), "My Capé.png");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn round_trip() {
        // a red disc on a JPG-style black square, cut to a circle
        let mut src = Rgba32FImage::new(200, 200);
        for p in src.pixels_mut() {
            *p = Rgba([0.5, 0.0, 0.0, 1.0]);
        }
        let png = encode_png(&square(&src, 64, true)).unwrap();
        let exr = encode_exr(&square(&src, 512, true)).unwrap();
        let p = decode(Kind::Png, &png).unwrap().img;
        let e = decode(Kind::Exr, &exr).unwrap().img;
        assert_eq!(p.dimensions(), (64, 64));
        assert_eq!(e.dimensions(), (512, 512));
        assert_eq!(e.get_pixel(0, 0)[3], 0.0);
        assert!((e.get_pixel(256, 256)[0] - 0.5).abs() < 0.01);
        assert!((p.get_pixel(32, 32)[0] - 0.5).abs() < 0.01);
        assert_eq!(p.get_pixel(0, 0)[3], 0.0);
    }
}
