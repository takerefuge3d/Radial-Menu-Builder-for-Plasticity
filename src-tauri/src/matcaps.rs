// ---------- Matcaps and Environments tabs ----------
// Plasticity keeps each matcap in ~/.plasticity/matcaps as a pair with the same name:
// NAME.exr (the texture, linear colour) and NAME.png (the picker thumbnail, sRGB).
// An optional NAME.json holding `{ isTinted: true }` makes the matcap tintable.
// Environments work the same way in ~/.plasticity/environments, except that they are
// 2:1 panoramas with no transparency, come from EXR or HDR (Radiance) files, and have no
// tint. Every command takes a `library` ("matcaps" by default, or "environments").
// Disabled ones live in a sibling folder (matcaps-disabled, environments-disabled) that
// Plasticity never reads.
//
// Images are handled as premultiplied linear RGBA (EXR's own convention). The matcap EXRs
// we write match the ones Plasticity ships: RGBA half floats, ZIP16, scan lines, top to
// bottom; environment EXRs are the same without alpha. PNGs are 8-bit sRGB, with straight
// alpha for matcaps and none for environments.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Mutex,
};

use base64::Engine;
use exr::prelude::{f16, ReadChannels, ReadLayers};
use image::{imageops, ImageFormat, Rgba, Rgba32FImage, RgbImage, RgbaImage};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

const PREVIEW_SIZE: u32 = 96; // matcap previews are 96 square; environment ones 192 × 96
const TINT_JSON: &str = "{\n  isTinted: true\n}\n";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lib {
    Matcaps,
    Environments,
}

impl Lib {
    fn from(library: &Option<String>) -> Lib {
        match library.as_deref() {
            Some("environments") => Lib::Environments,
            _ => Lib::Matcaps,
        }
    }
    fn one(self) -> &'static str {
        match self {
            Lib::Matcaps => "matcap",
            Lib::Environments => "environment",
        }
    }
    fn folder_name(self) -> &'static str {
        match self {
            Lib::Matcaps => "matcaps",
            Lib::Environments => "environments",
        }
    }
    // Matcaps are square: sizes are the side. Environments are 2:1: sizes are the height,
    // and 0 keeps an EXR made from an HDR at the HDR's own size.
    fn png_sizes(self) -> &'static [u32] {
        &[64, 128]
    }
    fn exr_sizes(self) -> &'static [u32] {
        match self {
            Lib::Matcaps => &[512, 1024],
            Lib::Environments => &[0, 512, 1024],
        }
    }
    fn accepts(self, kind: Kind) -> bool {
        match self {
            Lib::Matcaps => matches!(kind, Kind::Png | Kind::Jpg | Kind::Webp | Kind::Exr),
            Lib::Environments => matches!(kind, Kind::Exr | Kind::Hdr),
        }
    }
    fn accepts_text(self) -> &'static str {
        match self {
            Lib::Matcaps => "PNG, JPG, WebP and EXR",
            Lib::Environments => "EXR and HDR",
        }
    }
    fn collections_file(self) -> &'static str {
        match self {
            Lib::Matcaps => "matcap_collections.json",
            Lib::Environments => "environment_collections.json",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Png,
    Jpg,
    Webp,
    Exr,
    Hdr, // Radiance .hdr (also named .hdri)
}

impl Kind {
    fn from_ext(ext: &str) -> Option<Kind> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Kind::Png),
            "jpg" | "jpeg" => Some(Kind::Jpg),
            "webp" => Some(Kind::Webp),
            "exr" => Some(Kind::Exr),
            "hdr" | "hdri" => Some(Kind::Hdr),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpg => "jpg",
            Kind::Webp => "webp",
            Kind::Exr => "exr",
            Kind::Hdr => "hdr",
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
        // Radiance HDR is already linear light, so no sRGB step
        Kind::Hdr => {
            let dynamic = image::load_from_memory_with_format(bytes, ImageFormat::Hdr)
                .map_err(|e| format!("can't read this HDR: {e}"))?;
            let mut img = dynamic.to_rgba32f();
            for p in img.pixels_mut() {
                for i in 0..3 {
                    p[i] = if p[i].is_finite() { p[i].max(0.0) } else { 0.0 };
                }
                p[3] = 1.0;
            }
            Ok(Decoded { img, has_alpha: false })
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

// image's resize clips every value to 0..1, which would flatten an HDR's sun and lights
// to white. The filters are linear, so scaling into range first and back after is exact.
fn resize(src: &Rgba32FImage, w: u32, h: u32, filter: imageops::FilterType) -> Rgba32FImage {
    let peak = src.pixels().flat_map(|p| p.0[..3].iter().copied()).fold(1.0f32, f32::max);
    if peak <= 1.0 {
        return imageops::resize(src, w, h, filter);
    }
    let mut scaled = src.clone();
    for p in scaled.pixels_mut() {
        for i in 0..3 {
            p[i] /= peak;
        }
    }
    let mut out = imageops::resize(&scaled, w, h, filter);
    for p in out.pixels_mut() {
        for i in 0..3 {
            p[i] *= peak;
        }
    }
    out
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
        resize(&cropped, size, size, imageops::FilterType::Lanczos3)
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

// A 2:1 panorama `height` pixels tall (stretched if the source isn't 2:1), or the source's
// own size for 0. Opaque. Triangle filtering rather than Lanczos, which rings into dark
// halos around a bright sun.
fn panorama(src: &Rgba32FImage, height: u32) -> Rgba32FImage {
    let mut out = if height == 0 || src.dimensions() == (height * 2, height) {
        src.clone()
    } else {
        resize(src, height * 2, height, imageops::FilterType::Triangle)
    };
    for p in out.pixels_mut() {
        let a = p[3];
        for i in 0..3 {
            // undo any premultiplication before making it opaque
            p[i] = if a > 0.0 && a < 1.0 { p[i] / a } else { p[i] }.max(0.0);
        }
        p[3] = 1.0;
    }
    out
}

// Decode a source and fit it to `size`. For matcaps, a source with no transparency other
// than a PNG (a JPG, or a WebP dragged from a browser) is cut to a circle.
fn convert(lib: Lib, kind: Kind, bytes: &[u8], size: u32) -> Result<Rgba32FImage, String> {
    let d = decode(kind, bytes)?;
    Ok(match lib {
        Lib::Matcaps => square(&d.img, size, matches!(kind, Kind::Jpg | Kind::Webp) && !d.has_alpha),
        Lib::Environments => panorama(&d.img, size),
    })
}

fn encode_png(img: &Rgba32FImage, alpha: bool) -> Result<Vec<u8>, String> {
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut bytes = Vec::new();
    if alpha {
        let mut out = RgbaImage::new(img.width(), img.height());
        for (src, dst) in img.pixels().zip(out.pixels_mut()) {
            let a = src[3];
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
        out.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
    } else {
        // bright parts of an HDR just clip to white, like a photo
        let mut out = RgbImage::new(img.width(), img.height());
        for (src, dst) in img.pixels().zip(out.pixels_mut()) {
            dst.0 = [to8(linear_to_srgb(src[0])), to8(linear_to_srgb(src[1])), to8(linear_to_srgb(src[2]))];
        }
        out.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
    }
    .map_err(|e| format!("PNG encode failed: {e}"))?;
    Ok(bytes)
}

// Half floats top out at 65504; anything brighter (an unclipped sun in an HDR can be) would
// become infinity, so it's capped there instead.
fn half(v: f32) -> f16 {
    f16::from_f32(v.min(65504.0))
}

fn encode_exr(img: &Rgba32FImage, alpha: bool) -> Result<Vec<u8>, String> {
    let (w, h) = img.dimensions();
    let size = (w as usize, h as usize);
    let mut bytes = Vec::new();
    let result = if alpha {
        let channels = exr::image::SpecificChannels::rgba(|pos: exr::math::Vec2<usize>| {
            let p = img.get_pixel(pos.x() as u32, pos.y() as u32);
            (half(p[0]), half(p[1]), half(p[2]), half(p[3]))
        });
        let image = exr::image::Image::from_encoded_channels(size, exr::image::Encoding::SMALL_LOSSLESS, channels);
        exr::image::write::WritableImage::write(&image).to_buffered(Cursor::new(&mut bytes))
    } else {
        let channels = exr::image::SpecificChannels::rgb(|pos: exr::math::Vec2<usize>| {
            let p = img.get_pixel(pos.x() as u32, pos.y() as u32);
            (half(p[0]), half(p[1]), half(p[2]))
        });
        let image = exr::image::Image::from_encoded_channels(size, exr::image::Encoding::SMALL_LOSSLESS, channels);
        exr::image::write::WritableImage::write(&image).to_buffered(Cursor::new(&mut bytes))
    };
    result.map_err(|e| format!("EXR encode failed: {e}"))?;
    Ok(bytes)
}

// The finished files: matcaps keep their transparency, environments have none.
fn png_file(lib: Lib, img: &Rgba32FImage) -> Result<Vec<u8>, String> {
    encode_png(img, lib == Lib::Matcaps)
}
fn exr_file(lib: Lib, img: &Rgba32FImage) -> Result<Vec<u8>, String> {
    encode_exr(img, lib == Lib::Matcaps)
}

fn data_url(lib: Lib, img: &Rgba32FImage) -> Result<String, String> {
    let png = match lib {
        Lib::Matcaps => encode_png(&square(img, PREVIEW_SIZE, false), true)?,
        Lib::Environments => encode_png(&panorama(img, PREVIEW_SIZE), false)?,
    };
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
    lighting: Option<Vec<f32>>, // environments only: see diffuse_lighting
}

fn stage(state: &StagingState, lib: Lib, file_name: String, bytes: Vec<u8>) -> Result<StagedInfo, String> {
    let (stem, kind, _) = split_name(&file_name)
        .filter(|(_, kind, _)| lib.accepts(*kind))
        .ok_or_else(|| format!("{file_name}: only {} files can be used", lib.accepts_text()))?;
    let decoded = decode(kind, &bytes).map_err(|e| format!("{file_name}: {e}"))?;
    let preview = data_url(lib, &decoded.img)?;
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
        lighting: (lib == Lib::Environments).then(|| diffuse_lighting(&decoded.img)),
    })
}

// Dropped files arrive as raw bytes, with the file name in a percent-encoded header
// and the library ("environments", or matcaps when missing) in another.
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
    let lib = Lib::from(&request.headers().get("x-library").and_then(|v| v.to_str().ok()).map(String::from));
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => bytes.clone(),
        // the postMessage fallback sends a typed array as a list of numbers
        tauri::ipc::InvokeBody::Json(value) => serde_json::from_value::<Vec<u8>>(value.clone())
            .map_err(|_| "expected the file's bytes".to_string())?,
    };
    stage(&state, lib, file_name, bytes)
}

#[tauri::command]
pub async fn stage_matcap_path(
    path: String,
    library: Option<String>,
    state: tauri::State<'_, StagingState>,
) -> Result<StagedInfo, String> {
    let path = PathBuf::from(path);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("bad file name")?
        .to_string();
    let bytes = fs::read(&path).map_err(|e| format!("{file_name}: {e}"))?;
    stage(&state, Lib::from(&library), file_name, bytes)
}

#[tauri::command]
pub fn unstage_matcaps(ids: Vec<u64>, state: tauri::State<'_, StagingState>) -> Result<(), String> {
    let mut staging = state.lock().map_err(|e| e.to_string())?;
    for id in ids {
        staging.files.remove(&id);
    }
    Ok(())
}

pub fn percent_decode(s: &str) -> String {
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

// ---------- Diffuse lighting (environments) ----------
// How an environment lights a matte ball, for the look-dev sphere in the Environments tab.
// The panorama is shrunk to 128 x 64 (keeping values above 1.0) and projected onto the nine
// order-2 spherical harmonics, which is all the detail diffuse light carries. The result is
// 27 numbers, nine per channel (R then G then B), already scaled so that
//   sum(c[k] * Y_k(n))
// is the light a white matte surface facing direction n sends back (irradiance / pi).
// Directions match the page's mirror ball: +y up, and the panorama's centre (u = 0.5) lies
// along -z.
fn sh_basis(x: f32, y: f32, z: f32) -> [f32; 9] {
    [
        0.282_095,
        0.488_603 * y,
        0.488_603 * z,
        0.488_603 * x,
        1.092_548 * x * y,
        1.092_548 * y * z,
        0.315_392 * (3.0 * z * z - 1.0),
        1.092_548 * x * z,
        0.546_274 * (x * x - y * y),
    ]
}

fn diffuse_lighting(img: &Rgba32FImage) -> Vec<f32> {
    let small = panorama(img, 64);
    let (w, h) = small.dimensions();
    let mut l = [[0f32; 9]; 3];
    let cell = (2.0 * std::f32::consts::PI / w as f32) * (std::f32::consts::PI / h as f32);
    for (px, py, p) in small.enumerate_pixels() {
        let lat = (0.5 - (py as f32 + 0.5) / h as f32) * std::f32::consts::PI;
        let lon = ((px as f32 + 0.5) / w as f32 - 0.5) * 2.0 * std::f32::consts::PI;
        let (x, y, z) = (lat.cos() * lon.sin(), lat.sin(), -lat.cos() * lon.cos());
        let d_omega = lat.cos() * cell;
        let basis = sh_basis(x, y, z);
        for ch in 0..3 {
            for k in 0..9 {
                l[ch][k] += p[ch] * basis[k] * d_omega;
            }
        }
    }
    // convolve with the cosine lobe (pi, 2pi/3, pi/4 per band) and divide by pi
    let band = [1.0, 2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 0.25, 0.25, 0.25, 0.25, 0.25];
    l.iter().flat_map(|c| c.iter().zip(band).map(|(v, a)| v * a)).collect()
}

// Diffuse lighting for an installed environment, from its EXR (or the HDR it came from).
#[tauri::command]
pub async fn installed_environment_lighting(folder: String, file_name: String, disabled: bool) -> Result<Vec<f32>, String> {
    let (_, kind, _) = split_name(&file_name).ok_or("not an image")?;
    if !matches!(kind, Kind::Exr | Kind::Hdr) {
        return Err("needs an EXR or HDR".into());
    }
    let bytes = fs::read(dir_for(&folder, disabled)?.join(&file_name)).map_err(|e| e.to_string())?;
    Ok(diffuse_lighting(&decode(kind, &bytes)?.img))
}

// ---------- Saving a new matcap ----------
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveRequest {
    library: Option<String>,
    folder: String,
    name: String,
    png_id: Option<u64>,
    exr_id: Option<u64>,
    src_id: Option<u64>, // a JPG or WebP (matcaps) or an HDR (environments), used when the PNG or EXR is missing
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
    let lib = Lib::from(&req.library);
    let png_size = check_size(req.png_size, lib.png_sizes(), "PNG")?;
    let exr_size = check_size(req.exr_size, lib.exr_sizes(), "EXR")?;
    if req.name.is_empty() || clean_name(&req.name) != req.name {
        return Err(format!("\"{}\" isn't a clean {} name", req.name, lib.one()));
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
    // An environment's EXR only comes from an HDR, never from an 8-bit picture.
    let png_bytes = match &png {
        Some(f) => f.bytes.clone(),
        None => {
            let src = exr.as_ref().or(other.as_ref()).ok_or("nothing to make the PNG from")?;
            png_file(lib, &convert(lib, src.kind, &src.bytes, png_size)?)?
        }
    };
    let exr_bytes = match &exr {
        Some(f) => f.bytes.clone(),
        None => {
            let src = match lib {
                Lib::Matcaps => png.as_ref().or(other.as_ref()),
                Lib::Environments => other.as_ref(),
            };
            let src = src.ok_or("nothing to make the EXR from")?;
            exr_file(lib, &convert(lib, src.kind, &src.bytes, exr_size)?)?
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
    if req.tinted && lib == Lib::Matcaps {
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
    src: Option<String>,       // a JPG (matcaps) or HDR (environments) to make the pair from
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

fn scan(lib: Lib, folder: &Path) -> Result<Vec<Installed>, String> {
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
        let slot = match (lib, ext_lower.as_str()) {
            (_, "png" | "exr") => ext_lower.as_str(),
            (Lib::Matcaps, "jpg" | "jpeg" | "json") => ext_lower.as_str(),
            (Lib::Environments, "hdr" | "hdri") => ext_lower.as_str(),
            _ => continue,
        };
        let key = clean_name(stem).to_lowercase();
        let g = groups.entry(key).or_default();
        let field = match slot {
            "png" => &mut g.png,
            "exr" => &mut g.exr,
            "json" => &mut g.json,
            _ => &mut g.src,
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
        let any_stem = exr_stem.clone().or(png_stem.clone()).or(stem_of(&g.src)).or(stem_of(&g.json)).unwrap();
        g.name = clean_name(&any_stem);
        g.tinted = g.json.as_ref().map(|j| is_tinted(&folder.join(j))).unwrap_or(false);

        let has_image = g.png.is_some() || g.exr.is_some() || g.src.is_some();
        if !has_image {
            g.issues.push(format!("Settings file with no {}", lib.one()));
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
            let names = [&g.png, &g.exr, &g.json, &g.src];
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
        // An environment's EXR can only be made from an HDR.
        let exr_from = match lib {
            Lib::Matcaps => g.png.as_ref().or(g.src.as_ref()),
            Lib::Environments => g.src.as_ref(),
        };
        g.fixable = has_image
            && !g.issues.is_empty()
            && g.extra.is_empty()
            && !g.name.is_empty()
            && (g.exr.is_some() || exr_from.is_some());
        if g.fixable {
            for f in [&g.exr, &g.png, &g.json, &g.src].into_iter().flatten() {
                let ext = f.rsplit_once('.').unwrap().1.to_ascii_lowercase();
                let target = format!("{}.{ext}", g.name);
                if *f != target {
                    g.fix.push(format!("Rename {f} to {target}"));
                }
            }
            if g.exr.is_none() {
                g.fix.push(format!("Make {}.exr from {}", g.name, exr_from.unwrap()));
            }
            if g.png.is_none() {
                let from = g.exr.as_ref().or(g.src.as_ref()).unwrap();
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
    let name = folder.file_name().and_then(|n| n.to_str()).ok_or("bad folder")?;
    Ok(folder.with_file_name(format!("{name}-disabled")))
}

fn dir_for(folder: &str, disabled: bool) -> Result<PathBuf, String> {
    let folder = PathBuf::from(folder);
    if disabled { disabled_dir(&folder) } else { Ok(folder) }
}

fn find(lib: Lib, dir: &Path, name: &str) -> Result<Installed, String> {
    scan(lib, dir)?
        .into_iter()
        .find(|g| g.name == name)
        .ok_or_else(|| format!("{name} is no longer in the folder"))
}

fn group_files(g: &Installed) -> Vec<String> {
    [&g.exr, &g.png, &g.json, &g.src].into_iter().flatten().cloned().chain(g.extra.iter().cloned()).collect()
}

#[tauri::command]
pub async fn list_installed_matcaps(folder: String, library: Option<String>) -> Result<Vec<Installed>, String> {
    let lib = Lib::from(&library);
    let mut list = scan(lib, Path::new(&folder))?;
    let mut off = scan(lib, &disabled_dir(Path::new(&folder))?)?;
    off.iter_mut().for_each(|g| g.disabled = true);
    list.append(&mut off);
    Ok(list)
}

// Thumbnail for the installed list: the PNG if there is one, else the EXR, JPG or HDR.
#[tauri::command]
pub async fn installed_matcap_preview(
    folder: String,
    file_name: String,
    disabled: bool,
    library: Option<String>,
) -> Result<String, String> {
    let (_, kind, _) = split_name(&file_name).ok_or("not an image")?;
    let bytes = fs::read(dir_for(&folder, disabled)?.join(&file_name)).map_err(|e| e.to_string())?;
    data_url(Lib::from(&library), &decode(kind, &bytes)?.img)
}

// Only runs on a click. Re-reads the folder rather than trusting the list the tab shows.
#[tauri::command]
pub async fn fix_installed_matcap(
    folder: String,
    name: String,
    disabled: bool,
    png_size: u32,
    exr_size: u32,
    library: Option<String>,
) -> Result<Vec<String>, String> {
    let lib = Lib::from(&library);
    let png_size = check_size(png_size, lib.png_sizes(), "PNG")?;
    let exr_size = check_size(exr_size, lib.exr_sizes(), "EXR")?;
    let folder = dir_for(&folder, disabled)?;
    let g = find(lib, &folder, &name)?;
    if !g.fixable {
        return Err(format!("{name} can't be fixed automatically"));
    }

    // Make the missing files before renaming anything, so a bad source stops the fix early.
    let make = |src: Option<&String>, size: u32, png: bool| -> Result<Vec<u8>, String> {
        let src = src.ok_or("nothing to make it from")?;
        let (_, kind, _) = split_name(src).unwrap();
        let bytes = fs::read(folder.join(src)).map_err(|e| format!("read {src} failed: {e}"))?;
        let img = convert(lib, kind, &bytes, size)?;
        if png { png_file(lib, &img) } else { exr_file(lib, &img) }
    };
    let exr_from = match lib {
        Lib::Matcaps => g.png.as_ref().or(g.src.as_ref()),
        Lib::Environments => g.src.as_ref(),
    };
    let new_exr = match &g.exr {
        Some(_) => None,
        None => Some(make(exr_from, exr_size, false)?),
    };
    let new_png = match &g.png {
        Some(_) => None,
        None => Some(make(g.exr.as_ref().or(g.src.as_ref()), png_size, true)?),
    };

    let mut done = vec![];
    for f in [&g.exr, &g.png, &g.json, &g.src].into_iter().flatten() {
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
    let lib = Lib::Matcaps;
    let dir = dir_for(&folder, disabled)?;
    let g = find(lib, &dir, &name)?;
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
fn move_group(g: &Installed, from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("create {} failed: {e}", to.display()))?;
    for f in group_files(g) {
        fs::rename(from.join(&f), to.join(&f)).map_err(|e| format!("move {f} failed: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn set_matcap_enabled(
    folder: String,
    name: String,
    enabled: bool,
    library: Option<String>,
) -> Result<(), String> {
    let lib = Lib::from(&library);
    let on = PathBuf::from(&folder);
    let off = disabled_dir(&on)?;
    let (from, to) = if enabled { (&off, &on) } else { (&on, &off) };
    let g = find(lib, from, &name)?;
    if scan(lib, to)?.iter().any(|x| x.name.to_lowercase() == name.to_lowercase()) {
        let other = if enabled { "an enabled" } else { "a disabled" };
        return Err(format!("there is already {other} {} called {name}", lib.one()));
    }
    move_group(&g, from, to)?;
    if enabled {
        let _ = fs::remove_dir(&off); // only goes if it is now empty
    }
    Ok(())
}

// ---------- Collections ----------
// A collection is a saved set of matcaps to have enabled. Members are kept without their
// order number (01_), so reordering doesn't break a collection. Using one moves every other
// matcap to the disabled folder and brings the members back; nothing is deleted.
fn member_key(name: &str) -> String {
    strip_number(name).to_lowercase()
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CollectionResult {
    enabled: Vec<String>,
    disabled: Vec<String>,
    missing: Vec<String>, // members that aren't in either folder any more
    failed: Vec<String>,
}

#[tauri::command]
pub async fn apply_matcap_collection(
    folder: String,
    members: Vec<String>,
    library: Option<String>,
) -> Result<CollectionResult, String> {
    let lib = Lib::from(&library);
    let on = PathBuf::from(&folder);
    let off = disabled_dir(&on)?;
    let want: HashSet<String> = members.iter().map(|m| member_key(m)).collect();
    let images = |dir: &Path| -> Result<Vec<Installed>, String> {
        Ok(scan(lib, dir)?.into_iter().filter(|g| g.png.is_some() || g.exr.is_some() || g.src.is_some()).collect())
    };
    let on_groups = images(&on)?;
    let off_groups = images(&off)?;
    let on_names: HashSet<String> = on_groups.iter().map(|g| g.name.to_lowercase()).collect();
    let off_names: HashSet<String> = off_groups.iter().map(|g| g.name.to_lowercase()).collect();

    let mut r = CollectionResult::default();
    let mut found = HashSet::new();
    for g in &on_groups {
        let key = member_key(&g.name);
        if want.contains(&key) {
            found.insert(key);
        } else if off_names.contains(&g.name.to_lowercase()) {
            r.failed.push(format!("{} (there's one with the same name in the disabled folder)", g.name));
        } else {
            match move_group(g, &on, &off) {
                Ok(()) => r.disabled.push(g.name.clone()),
                Err(e) => r.failed.push(e),
            }
        }
    }
    for g in &off_groups {
        let key = member_key(&g.name);
        if !want.contains(&key) {
            continue;
        }
        found.insert(key);
        if on_names.contains(&g.name.to_lowercase()) {
            r.failed.push(format!("{} (there's one with the same name enabled)", g.name));
            continue;
        }
        match move_group(g, &off, &on) {
            Ok(()) => r.enabled.push(g.name.clone()),
            Err(e) => r.failed.push(e),
        }
    }
    r.missing = members.into_iter().filter(|m| !found.contains(&member_key(m))).collect();
    let _ = fs::remove_dir(&off); // only goes if it is now empty
    Ok(r)
}

// Collections are kept as plain text in the app data folder, like the user's themes,
// one file for matcaps and one for environments.
fn collections_path(app: &tauri::AppHandle, lib: Lib) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("app_data_dir error: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("create {} failed: {e}", dir.display()))?;
    Ok(dir.join(lib.collections_file()))
}

#[tauri::command]
pub fn load_matcap_collections(app: tauri::AppHandle, library: Option<String>) -> Result<Option<String>, String> {
    let path = collections_path(&app, Lib::from(&library))?;
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {} failed: {e}", path.display())),
    }
}

#[tauri::command]
pub fn save_matcap_collections(
    app: tauri::AppHandle,
    contents: String,
    library: Option<String>,
) -> Result<(), String> {
    let path = collections_path(&app, Lib::from(&library))?;
    fs::write(&path, contents).map_err(|e| format!("write {} failed: {e}", path.display()))
}

// Deleting only works on disabled matcaps, so it always takes two clicks (Disable, then
// Delete), and the files go to the Recycle Bin / Trash rather than vanishing outright.
#[tauri::command]
pub async fn trash_disabled_matcap(folder: String, name: String, library: Option<String>) -> Result<Vec<String>, String> {
    let off = disabled_dir(Path::new(&folder))?;
    let g = find(Lib::from(&library), &off, &name)?;
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
pub async fn order_matcaps(
    folder: String,
    names: Vec<String>,
    numbered: bool,
    library: Option<String>,
) -> Result<usize, String> {
    let lib = Lib::from(&library);
    let folder = PathBuf::from(folder);
    let groups = scan(lib, &folder)?;
    let width = names.len().to_string().len().max(2);
    let mut moves: Vec<(String, String)> = vec![];
    for (i, n) in names.iter().enumerate() {
        let g = groups.iter().find(|g| &g.name == n).ok_or_else(|| format!("{n} is no longer in the folder"))?;
        if !g.extra.is_empty() {
            return Err(format!("{n} clashes with other files; sort that out first"));
        }
        let base = strip_number(&g.name);
        let target = if numbered { format!("{:0width$}_{base}", i + 1) } else { base.to_string() };
        for f in [&g.exr, &g.png, &g.json, &g.src].into_iter().flatten() {
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
            return Err(format!("two {}s would both be called {to}", lib.one()));
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
fn library_dir(app: &tauri::AppHandle, lib: Lib) -> Option<PathBuf> {
    Some(app.path().home_dir().ok()?.join(".plasticity").join(lib.folder_name()))
}

#[tauri::command]
pub fn default_matcaps_folder(app: tauri::AppHandle, library: Option<String>) -> Result<Option<String>, String> {
    Ok(library_dir(&app, Lib::from(&library)).map(|d| d.to_string_lossy().into_owned()))
}

#[tauri::command]
pub async fn pick_matcap_files(app: tauri::AppHandle, library: Option<String>) -> Result<Vec<String>, String> {
    let (filter, exts, title): (&str, &[&str], &str) = match Lib::from(&library) {
        Lib::Matcaps => ("Matcap images", &["png", "jpg", "jpeg", "webp", "exr"], "Add matcap images"),
        Lib::Environments => ("Environment images", &["exr", "hdr", "hdri"], "Add environment images"),
    };
    let picked = app.dialog().file().add_filter(filter, exts).set_title(title).blocking_pick_files();
    Ok(picked.unwrap_or_default().into_iter().map(|p| p.to_string()).collect())
}

#[tauri::command]
pub async fn pick_matcaps_folder(app: tauri::AppHandle, library: Option<String>) -> Result<Option<String>, String> {
    let lib = Lib::from(&library);
    let name = lib.folder_name();
    let mut builder = app.dialog().file().set_title(format!("Choose Plasticity's {name} folder (.plasticity/{name})"));
    if let Some(dir) = library_dir(&app, lib).filter(|d| d.is_dir()) {
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
        run(order_matcaps(folder.clone(), names, true, None)).unwrap();
        assert_eq!(files(&dir), ["01_Pear.exr", "01_Pear.json", "01_Pear.png", "02_Apple.exr", "02_Apple.png", "03_Fig.exr", "03_Fig.png"]);
        assert_eq!(fs::read_to_string(dir.join("01_Pear.json")).unwrap(), "02_Pear.json");

        let names = vec!["01_Pear".to_string(), "02_Apple".to_string(), "03_Fig".to_string()];
        run(order_matcaps(folder.clone(), names, false, None)).unwrap();
        assert_eq!(files(&dir), ["Apple.exr", "Apple.png", "Fig.exr", "Fig.png", "Pear.exr", "Pear.json", "Pear.png"]);

        run(set_matcap_enabled(folder.clone(), "Pear".into(), false, None)).unwrap();
        let off = disabled_dir(&dir).unwrap();
        assert_eq!(files(&off), ["Pear.exr", "Pear.json", "Pear.png"]);
        let listed = run(list_installed_matcaps(folder.clone(), None)).unwrap();
        assert!(listed.iter().any(|g| g.name == "Pear" && g.disabled && g.tinted == false));
        run(set_matcap_tinted(folder.clone(), "Pear".into(), true, true)).unwrap();
        assert_eq!(fs::read_to_string(off.join("Pear.json")).unwrap(), TINT_JSON);
        run(set_matcap_enabled(folder.clone(), "Pear".into(), true, None)).unwrap();
        assert!(!off.exists());
        // only disabled matcaps can be deleted
        assert!(run(trash_disabled_matcap(folder.clone(), "Pear".into(), None)).is_err());
        assert!(dir.join("Pear.exr").exists());
        run(set_matcap_tinted(folder.clone(), "Fig".into(), false, true)).unwrap();
        assert!(dir.join("Fig.json").exists());
        run(set_matcap_tinted(folder.clone(), "Fig".into(), false, false)).unwrap();
        assert!(!dir.join("Fig.json").exists());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn collections() {
        let dir = temp_folder("coll");
        for f in ["01_Clay.exr", "01_Clay.png", "02_Chrome.exr", "02_Chrome.png", "03_Sunset.exr", "03_Sunset.png", "03_Sunset.json"] {
            fs::write(dir.join(f), f).unwrap();
        }
        let folder = dir.to_string_lossy().into_owned();
        let off = disabled_dir(&dir).unwrap();
        // members are stored without numbers; "Gone" isn't installed
        let r = run(apply_matcap_collection(folder.clone(), vec!["Clay".into(), "Gone".into()], None)).unwrap();
        assert_eq!(r.disabled.len(), 2);
        assert_eq!(r.missing, ["Gone"]);
        assert_eq!(files(&dir), ["01_Clay.exr", "01_Clay.png"]);
        assert_eq!(files(&off), ["02_Chrome.exr", "02_Chrome.png", "03_Sunset.exr", "03_Sunset.json", "03_Sunset.png"]);
        let r = run(apply_matcap_collection(folder.clone(), vec!["Chrome".into(), "Sunset".into()], None)).unwrap();
        assert_eq!((r.enabled.len(), r.disabled.len()), (2, 1));
        assert_eq!(files(&dir), ["02_Chrome.exr", "02_Chrome.png", "03_Sunset.exr", "03_Sunset.json", "03_Sunset.png"]);
        let r = run(apply_matcap_collection(folder.clone(), vec!["Clay".into(), "Chrome".into(), "Sunset".into()], None)).unwrap();
        assert_eq!(r.enabled, ["01_Clay"]);
        assert!(!off.exists());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn round_trip() {
        // a red disc on a JPG-style black square, cut to a circle
        let mut src = Rgba32FImage::new(200, 200);
        for p in src.pixels_mut() {
            *p = Rgba([0.5, 0.0, 0.0, 1.0]);
        }
        let png = encode_png(&square(&src, 64, true), true).unwrap();
        let exr = encode_exr(&square(&src, 512, true), true).unwrap();
        let p = decode(Kind::Png, &png).unwrap().img;
        let e = decode(Kind::Exr, &exr).unwrap().img;
        assert_eq!(p.dimensions(), (64, 64));
        assert_eq!(e.dimensions(), (512, 512));
        assert_eq!(e.get_pixel(0, 0)[3], 0.0);
        assert!((e.get_pixel(256, 256)[0] - 0.5).abs() < 0.01);
        assert!((p.get_pixel(32, 32)[0] - 0.5).abs() < 0.01);
        assert_eq!(p.get_pixel(0, 0)[3], 0.0);
    }

    #[test]
    fn lighting() {
        let shade = |c: &[f32], n: (f32, f32, f32)| -> f32 {
            sh_basis(n.0, n.1, n.2).iter().zip(&c[..9]).map(|(y, k)| y * k).sum()
        };
        // a uniform white sky lights a white matte ball evenly at 1.0
        let mut even = Rgba32FImage::new(256, 128);
        even.pixels_mut().for_each(|p| *p = Rgba([1.0, 1.0, 1.0, 1.0]));
        let c = diffuse_lighting(&even);
        assert_eq!(c.len(), 27);
        for n in [(0.0, 1.0, 0.0), (0.0, -1.0, 0.0), (1.0, 0.0, 0.0), (0.0, 0.0, 1.0)] {
            assert!((shade(&c, n) - 1.0).abs() < 0.02, "{n:?} {}", shade(&c, n));
        }
        // light from above only: the top is lit, the bottom dark, the sides in between
        let mut top = Rgba32FImage::new(256, 128);
        for (_, y, p) in top.enumerate_pixels_mut() {
            *p = if y < 64 { Rgba([2.0, 2.0, 2.0, 1.0]) } else { Rgba([0.0, 0.0, 0.0, 1.0]) };
        }
        let c = diffuse_lighting(&top);
        let (up, side, down) = (shade(&c, (0.0, 1.0, 0.0)), shade(&c, (1.0, 0.0, 0.0)), shade(&c, (0.0, -1.0, 0.0)));
        assert!(up > 1.8 && (side - 1.0).abs() < 0.1 && down < 0.2, "{up} {side} {down}");
        // a bright sun keeps its strength (no clipping to 1.0 on the way)
        let mut sun = Rgba32FImage::new(512, 256);
        sun.pixels_mut().for_each(|p| *p = Rgba([0.0, 0.0, 0.0, 1.0])); // opaque, like a real environment
        sun.put_pixel(256, 128, Rgba([5000.0, 5000.0, 5000.0, 1.0]));
        let c = diffuse_lighting(&sun);
        // the sun sits at the panorama's centre, along -z. Its light: 5000 x the pixel's
        // solid angle, over pi
        let expected = 5000.0 * (2.0 * std::f32::consts::PI / 512.0) * (std::f32::consts::PI / 256.0) / std::f32::consts::PI;
        let (facing, away) = (shade(&c, (0.0, 0.0, -1.0)), shade(&c, (0.0, 0.0, 1.0)));
        assert!((facing / expected - 1.0).abs() < 0.25 && away.abs() < 0.15 * expected, "{facing} {away} {expected}");
    }

    #[test]
    fn environments() {
        let lib = Lib::Environments;
        // a 300 x 100 HDR (not 2:1) with a sun far brighter than 1.0
        let mut hdr_px = vec![image::Rgb([0.25f32, 0.5, 1.0]); 300 * 100];
        hdr_px[50 * 300 + 150] = image::Rgb([40.0, 38.0, 30.0]);
        let mut hdr = Vec::new();
        image::codecs::hdr::HdrEncoder::new(&mut hdr).encode(&hdr_px, 300, 100).unwrap();
        assert!(lib.accepts(Kind::Hdr) && lib.accepts(Kind::Exr) && !lib.accepts(Kind::Png) && !lib.accepts(Kind::Jpg));

        // EXR at the HDR's own size, and at 512 tall (1024 x 512, stretched to 2:1)
        let same = convert(lib, Kind::Hdr, &hdr, 0).unwrap();
        assert_eq!(same.dimensions(), (300, 100));
        let exr = exr_file(lib, &convert(lib, Kind::Hdr, &hdr, 512).unwrap()).unwrap();
        let back = decode(Kind::Exr, &exr).unwrap();
        assert_eq!(back.img.dimensions(), (1024, 512));
        assert!(!back.has_alpha);
        assert!((back.img.get_pixel(10, 10)[2] - 1.0).abs() < 0.01);
        assert!(back.img.pixels().any(|p| p[0] > 1.0), "the sun keeps its brightness");

        // a sun brighter than a half float can hold is capped, not written as infinity
        let mut hot = Rgba32FImage::from_pixel(4, 2, Rgba([0.5, 0.5, 0.5, 1.0]));
        hot.put_pixel(1, 1, Rgba([200000.0, 90000.0, 1000.0, 1.0]));
        let back = decode(Kind::Exr, &exr_file(lib, &hot).unwrap()).unwrap().img;
        assert_eq!(back.get_pixel(1, 1)[0], 65504.0);
        assert_eq!(back.get_pixel(1, 1)[1], 65504.0);
        assert!(back.pixels().all(|p| p[0].is_finite() && p[1].is_finite() && p[2].is_finite()));

        // PNG thumbnail 128 x 64, RGB
        let png = png_file(lib, &convert(lib, Kind::Exr, &exr, 64).unwrap()).unwrap();
        let thumb = image::load_from_memory(&png).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (128, 64));
        assert!(!thumb.color().has_alpha());

        // a lone HDR in the folder is fixable; a lone PNG isn't (no EXR from 8-bit pictures)
        let dir = temp_folder("env");
        fs::write(dir.join("Aft Lounge 4k.hdr"), &hdr).unwrap();
        fs::write(dir.join("Only_Thumb.png"), &png).unwrap();
        fs::write(dir.join("desktop.ini"), "x").unwrap();
        let list = scan(lib, &dir).unwrap();
        assert_eq!(list.len(), 2);
        let lounge = list.iter().find(|g| g.name == "Aft_Lounge_4k").unwrap();
        assert!(lounge.fixable);
        assert_eq!(lounge.fix, ["Rename Aft Lounge 4k.hdr to Aft_Lounge_4k.hdr", "Make Aft_Lounge_4k.exr from Aft Lounge 4k.hdr", "Make Aft_Lounge_4k.png from Aft Lounge 4k.hdr"]);
        assert!(!list.iter().find(|g| g.name == "Only_Thumb").unwrap().fixable);
        let folder = dir.to_string_lossy().into_owned();
        run(fix_installed_matcap(folder.clone(), "Aft_Lounge_4k".into(), false, 128, 0, Some("environments".into()))).unwrap();
        assert_eq!(files(&dir), ["Aft_Lounge_4k.exr", "Aft_Lounge_4k.hdr", "Aft_Lounge_4k.png", "Only_Thumb.png", "desktop.ini"]);
        let made = decode(Kind::Exr, &fs::read(dir.join("Aft_Lounge_4k.exr")).unwrap()).unwrap();
        assert_eq!(made.img.dimensions(), (300, 100));
        // the matcaps rules don't see the HDR at all
        assert!(scan(Lib::Matcaps, &dir).unwrap().iter().all(|g| g.src.is_none()));
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }
}
