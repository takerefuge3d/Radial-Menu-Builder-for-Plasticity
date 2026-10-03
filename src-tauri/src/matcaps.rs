// ---------- Matcaps tab ----------
// Plasticity keeps each matcap in ~/.plasticity/matcaps as a pair with the same name:
// NAME.exr (the texture, linear colour) and NAME.png (the picker thumbnail, sRGB).
// An optional NAME.json holding `{ isTinted: true }` makes the matcap tintable.
// Disabled matcaps live in a sibling folder (matcaps-disabled) that Plasticity never reads.
//
// Images are handled as premultiplied linear RGBA (EXR's own convention). The EXRs we
// write match the ones Plasticity ships: RGBA half floats, ZIP16, scan lines, top to bottom.
// PNGs are written as 8-bit sRGB with straight alpha.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
    Webp,
    Exr,
}

impl Kind {
    fn from_ext(ext: &str) -> Option<Kind> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Kind::Png),
            "jpg" | "jpeg" => Some(Kind::Jpg),
            "webp" => Some(Kind::Webp),
            "exr" => Some(Kind::Exr),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpg => "jpg",
            Kind::Webp => "webp",
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
        Kind::Png | Kind::Jpg | Kind::Webp => {
            let format = match kind {
                Kind::Png => ImageFormat::Png,
                Kind::Jpg => ImageFormat::Jpeg,
                _ => ImageFormat::WebP,
            };
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

// Decode a source and fit it to `size`. A source with no transparency other than a PNG
// (a JPG, or a WebP dragged from a browser) is cut to a circle.
fn convert(kind: Kind, bytes: &[u8], size: u32) -> Result<Rgba32FImage, String> {
    let d = decode(kind, bytes)?;
    let cut = matches!(kind, Kind::Jpg | Kind::Webp) && !d.has_alpha;
    Ok(square(&d.img, size, cut))
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
        .ok_or_else(|| format!("{file_name}: only PNG, JPG, WebP and EXR files can be used"))?;
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
    src_id: Option<u64>, // a JPG or WebP, used when the PNG or EXR is missing
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
    let other = take(req.src_id)?;

    // Files given are copied as they are; only the missing half of the pair is made.
    // The EXR is the better source for a PNG (more range); a PNG beats a JPG for an EXR.
    let png_bytes = match &png {
        Some(f) => f.bytes.clone(),
        None => {
            let src = exr.as_ref().or(other.as_ref()).ok_or("nothing to make the PNG from")?;
            encode_png(&convert(src.kind, &src.bytes, png_size)?)?
        }
    };
    let exr_bytes = match &exr {
        Some(f) => f.bytes.clone(),
        None => {
            let src = png.as_ref().or(other.as_ref()).ok_or("nothing to make the EXR from")?;
            encode_exr(&convert(src.kind, &src.bytes, exr_size)?)?
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
    for id in [req.png_id, req.exr_id, req.src_id].into_iter().flatten() {
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
    disabled: bool,            // in the matcaps-disabled folder
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

// The folder disabled matcaps are moved to: a sibling of the matcaps folder, so Plasticity
// never sees them whether or not it looks inside subfolders.
fn disabled_dir(folder: &Path) -> Result<PathBuf, String> {
    let name = folder.file_name().and_then(|n| n.to_str()).ok_or("bad matcaps folder")?;
    Ok(folder.with_file_name(format!("{name}-disabled")))
}

fn dir_for(folder: &str, disabled: bool) -> Result<PathBuf, String> {
    let folder = PathBuf::from(folder);
    if disabled { disabled_dir(&folder) } else { Ok(folder) }
}

fn find(dir: &Path, name: &str) -> Result<Installed, String> {
    scan(dir)?
        .into_iter()
        .find(|g| g.name == name)
        .ok_or_else(|| format!("{name} is no longer in the folder"))
}

fn group_files(g: &Installed) -> Vec<String> {
    [&g.exr, &g.png, &g.json, &g.jpg].into_iter().flatten().cloned().chain(g.extra.iter().cloned()).collect()
}

#[tauri::command]
pub async fn list_installed_matcaps(folder: String) -> Result<Vec<Installed>, String> {
    let mut list = scan(Path::new(&folder))?;
    let mut off = scan(&disabled_dir(Path::new(&folder))?)?;
    off.iter_mut().for_each(|g| g.disabled = true);
    list.append(&mut off);
    Ok(list)
}

// Thumbnail for the installed list: the PNG if there is one, else the EXR or JPG.
#[tauri::command]
pub async fn installed_matcap_preview(folder: String, file_name: String, disabled: bool) -> Result<String, String> {
    let (_, kind, _) = split_name(&file_name).ok_or("not an image")?;
    let bytes = fs::read(dir_for(&folder, disabled)?.join(&file_name)).map_err(|e| e.to_string())?;
    data_url(&decode(kind, &bytes)?.img)
}

// Only runs on a click. Re-reads the folder rather than trusting the list the tab shows.
#[tauri::command]
pub async fn fix_installed_matcap(
    folder: String,
    name: String,
    disabled: bool,
    png_size: u32,
    exr_size: u32,
) -> Result<Vec<String>, String> {
    let png_size = check_size(png_size, &PNG_SIZES, "PNG")?;
    let exr_size = check_size(exr_size, &EXR_SIZES, "EXR")?;
    let folder = dir_for(&folder, disabled)?;
    let g = find(&folder, &name)?;
    if !g.fixable {
        return Err(format!("{name} can't be fixed automatically"));
    }

    // Make the missing files before renaming anything, so a bad source stops the fix early.
    let make = |src: Option<&String>, size: u32, png: bool| -> Result<Vec<u8>, String> {
        let src = src.ok_or("nothing to make it from")?;
        let (_, kind, _) = split_name(src).unwrap();
        let bytes = fs::read(folder.join(src)).map_err(|e| format!("read {src} failed: {e}"))?;
        let img = convert(kind, &bytes, size)?;
        if png { encode_png(&img) } else { encode_exr(&img) }
    };
    let new_exr = match &g.exr {
        Some(_) => None,
        None => Some(make(g.png.as_ref().or(g.jpg.as_ref()), exr_size, false)?),
    };
    let new_png = match &g.png {
        Some(_) => None,
        None => Some(make(g.exr.as_ref().or(g.jpg.as_ref()), png_size, true)?),
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

// ---------- Tint ----------
// Sets isTinted in a .json's text without disturbing anything else in it. Plasticity's
// files are JSON5-style (`{ isTinted: true }`, no quotes), so the text is edited rather
// than parsed and rewritten. None means the file holds nothing else and can be deleted.
fn set_tint_text(text: &str, tinted: bool) -> Option<String> {
    let flat: String = text.chars().filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'').collect();
    if flat.is_empty() || flat == "{}" {
        return if tinted { Some(TINT_JSON.to_string()) } else { None };
    }
    if !tinted && (flat == "{isTinted:true}" || flat == "{isTinted:true,}") {
        return None;
    }
    if let Some(i) = text.find("isTinted") {
        let after = i + "isTinted".len();
        let rest = &text[after..];
        let skip = rest.len()
            - rest.trim_start_matches(|c: char| c == '"' || c == '\'' || c == ':' || c.is_whitespace()).len();
        let at = after + skip;
        let old = if text[at..].starts_with("true") {
            4
        } else if text[at..].starts_with("false") {
            5
        } else {
            0
        };
        if old > 0 {
            return Some(format!("{}{tinted}{}", &text[..at], &text[at + old..]));
        }
    }
    if !tinted {
        return Some(text.to_string());
    }
    // No isTinted yet: add it as the first entry.
    match text.find('{') {
        Some(b) => Some(format!("{}\n  isTinted: true,{}", &text[..=b], &text[b + 1..])),
        None => Some(TINT_JSON.to_string()),
    }
}

#[tauri::command]
pub async fn set_matcap_tinted(folder: String, name: String, disabled: bool, tinted: bool) -> Result<(), String> {
    let dir = dir_for(&folder, disabled)?;
    let g = find(&dir, &name)?;
    let json = match &g.json {
        Some(j) => j.clone(),
        None => {
            if !tinted {
                return Ok(());
            }
            // Name it after the EXR, which is the file Plasticity pairs it with.
            let stem = [&g.exr, &g.png].into_iter().flatten().next().map(|f| f.rsplit_once('.').unwrap().0.to_string());
            format!("{}.json", stem.unwrap_or(g.name.clone()))
        }
    };
    let path = dir.join(&json);
    let text = fs::read_to_string(&path).unwrap_or_default();
    match set_tint_text(&text, tinted) {
        Some(new) => fs::write(&path, new).map_err(|e| format!("write {json} failed: {e}")),
        None => fs::remove_file(&path).map_err(|e| format!("remove {json} failed: {e}")),
    }
}

// ---------- Enable / disable ----------
#[tauri::command]
pub async fn set_matcap_enabled(folder: String, name: String, enabled: bool) -> Result<(), String> {
    let on = PathBuf::from(&folder);
    let off = disabled_dir(&on)?;
    let (from, to) = if enabled { (&off, &on) } else { (&on, &off) };
    let g = find(from, &name)?;
    if scan(to)?.iter().any(|x| x.name.to_lowercase() == name.to_lowercase()) {
        let other = if enabled { "an enabled" } else { "a disabled" };
        return Err(format!("there is already {other} matcap called {name}"));
    }
    fs::create_dir_all(to).map_err(|e| format!("create {} failed: {e}", to.display()))?;
    for f in group_files(&g) {
        fs::rename(from.join(&f), to.join(&f)).map_err(|e| format!("move {f} failed: {e}"))?;
    }
    if enabled {
        let _ = fs::remove_dir(&off); // only goes if it is now empty
    }
    Ok(())
}

// Deleting only works on disabled matcaps, so it always takes two clicks (Disable, then
// Delete), and the files go to the Recycle Bin / Trash rather than vanishing outright.
#[tauri::command]
pub async fn trash_disabled_matcap(folder: String, name: String) -> Result<Vec<String>, String> {
    let off = disabled_dir(Path::new(&folder))?;
    let g = find(&off, &name)?;
    let files = group_files(&g);
    trash::delete_all(files.iter().map(|f| off.join(f))).map_err(|e| format!("couldn't move {name} to the bin: {e}"))?;
    let _ = fs::remove_dir(&off); // only goes if it is now empty
    Ok(files)
}

// ---------- Order ----------
// "03_Peach" -> "Peach". Leaves names that are only digits, or have no underscore, alone.
fn strip_number(name: &str) -> &str {
    let digits = name.len() - name.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 && name[digits..].starts_with('_') && name.len() > digits + 1 {
        &name[digits + 1..]
    } else {
        name
    }
}

// Renames the given matcaps to 01_Name, 02_Name… in that order, or (numbered = false)
// takes the numbers off again. Files go through temporary names first, so swapping two
// numbers can't collide part-way through.
#[tauri::command]
pub async fn order_matcaps(folder: String, names: Vec<String>, numbered: bool) -> Result<usize, String> {
    let folder = PathBuf::from(folder);
    let groups = scan(&folder)?;
    let width = names.len().to_string().len().max(2);
    let mut moves: Vec<(String, String)> = vec![];
    for (i, n) in names.iter().enumerate() {
        let g = groups.iter().find(|g| &g.name == n).ok_or_else(|| format!("{n} is no longer in the folder"))?;
        if !g.extra.is_empty() {
            return Err(format!("{n} clashes with other files; sort that out first"));
        }
        let base = strip_number(&g.name);
        let target = if numbered { format!("{:0width$}_{base}", i + 1) } else { base.to_string() };
        for f in [&g.exr, &g.png, &g.json, &g.jpg].into_iter().flatten() {
            let to = format!("{target}.{}", f.rsplit_once('.').unwrap().1.to_ascii_lowercase());
            if *f != to {
                moves.push((f.clone(), to));
            }
        }
    }
    let moving: HashSet<String> = moves.iter().map(|(f, _)| f.to_lowercase()).collect();
    let mut seen = HashSet::new();
    for (_, to) in &moves {
        let key = to.to_lowercase();
        if !seen.insert(key.clone()) {
            return Err(format!("two matcaps would both be called {to}"));
        }
        if folder.join(to).exists() && !moving.contains(&key) {
            return Err(format!("{to} already exists"));
        }
    }

    let tmp = |i: usize| folder.join(format!(".reorder-{i}.tmp"));
    for (i, (from, _)) in moves.iter().enumerate() {
        if let Err(e) = fs::rename(folder.join(from), tmp(i)) {
            for (j, (back, _)) in moves.iter().enumerate().take(i) {
                let _ = fs::rename(tmp(j), folder.join(back));
            }
            return Err(format!("rename {from} failed: {e}"));
        }
    }
    for (i, (from, to)) in moves.iter().enumerate() {
        if let Err(e) = fs::rename(tmp(i), folder.join(to)) {
            for (j, (back, _)) in moves.iter().enumerate().skip(i) {
                let _ = fs::rename(tmp(j), folder.join(back));
            }
            return Err(format!("rename {from} to {to} failed: {e}"));
        }
    }
    Ok(moves.len())
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
        .add_filter("Matcap images", &["png", "jpg", "jpeg", "webp", "exr"])
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
        assert_eq!(strip_number("03_Peach"), "Peach");
        assert_eq!(strip_number("Peach_03"), "Peach_03");
        assert_eq!(strip_number("2024"), "2024");
        assert_eq!(strip_number("3D_Clay"), "3D_Clay");
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("My%20Cap%C3%A9.png"), "My Capé.png");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn tint_text() {
        assert_eq!(set_tint_text("", true).unwrap(), TINT_JSON);
        assert_eq!(set_tint_text(TINT_JSON, false), None);
        assert_eq!(set_tint_text("{ \"isTinted\": true }", false), None);
        assert_eq!(set_tint_text("{\n  isTinted: false\n}", true).unwrap(), "{\n  isTinted: true\n}");
        let more = "{\n  isTinted: true,\n  roughness: 0.4\n}";
        assert_eq!(set_tint_text(more, false).unwrap(), "{\n  isTinted: false,\n  roughness: 0.4\n}");
        assert_eq!(set_tint_text("{ roughness: 0.4 }", true).unwrap(), "{\n  isTinted: true, roughness: 0.4 }");
        assert_eq!(set_tint_text("{ roughness: 0.4 }", false).unwrap(), "{ roughness: 0.4 }");
    }

    fn temp_folder(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mc-test-{tag}-{}", std::process::id())).join("matcaps");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }
    fn run<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    #[test]
    fn order_and_disable() {
        let dir = temp_folder("order");
        for f in ["Apple.exr", "Apple.png", "02_Pear.exr", "02_Pear.png", "02_Pear.json", "01_Fig.exr", "01_Fig.png"] {
            fs::write(dir.join(f), f).unwrap();
        }
        let folder = dir.to_string_lossy().into_owned();
        // Pear first, then Apple, then Fig: Pear and Fig swap numbers
        let names = vec!["02_Pear".to_string(), "Apple".to_string(), "01_Fig".to_string()];
        run(order_matcaps(folder.clone(), names, true)).unwrap();
        assert_eq!(files(&dir), ["01_Pear.exr", "01_Pear.json", "01_Pear.png", "02_Apple.exr", "02_Apple.png", "03_Fig.exr", "03_Fig.png"]);
        assert_eq!(fs::read_to_string(dir.join("01_Pear.json")).unwrap(), "02_Pear.json");

        let names = vec!["01_Pear".to_string(), "02_Apple".to_string(), "03_Fig".to_string()];
        run(order_matcaps(folder.clone(), names, false)).unwrap();
        assert_eq!(files(&dir), ["Apple.exr", "Apple.png", "Fig.exr", "Fig.png", "Pear.exr", "Pear.json", "Pear.png"]);

        run(set_matcap_enabled(folder.clone(), "Pear".into(), false)).unwrap();
        let off = disabled_dir(&dir).unwrap();
        assert_eq!(files(&off), ["Pear.exr", "Pear.json", "Pear.png"]);
        let listed = run(list_installed_matcaps(folder.clone())).unwrap();
        assert!(listed.iter().any(|g| g.name == "Pear" && g.disabled && g.tinted == false));
        run(set_matcap_tinted(folder.clone(), "Pear".into(), true, true)).unwrap();
        assert_eq!(fs::read_to_string(off.join("Pear.json")).unwrap(), TINT_JSON);
        run(set_matcap_enabled(folder.clone(), "Pear".into(), true)).unwrap();
        assert!(!off.exists());
        // only disabled matcaps can be deleted
        assert!(run(trash_disabled_matcap(folder.clone(), "Pear".into())).is_err());
        assert!(dir.join("Pear.exr").exists());
        run(set_matcap_tinted(folder.clone(), "Fig".into(), false, true)).unwrap();
        assert!(dir.join("Fig.json").exists());
        run(set_matcap_tinted(folder.clone(), "Fig".into(), false, false)).unwrap();
        assert!(!dir.join("Fig.json").exists());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
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
