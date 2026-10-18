//! SVG markers the way QGIS draws them (QgsSvgCache + QgsSvgMarkerSymbolLayer):
//! `param(fill)`-style substitution, QGIS's SVG search paths, the "?" placeholder for
//! missing files, and images of `int(width)` × `int(width · aspect)` pixels.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use resvg::usvg;
use tiny_skia::{Pixmap, Transform};
use tracing::{debug, warn};

use crate::symbology::Rgba;

/// QgsSvgCache::mMissingSvg (with the SVG namespace usvg requires).
const MISSING_SVG: &str = "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'><text x='5' y='10' font-size='10' text-anchor='middle'>?</text></svg>";

static OPTIONS: LazyLock<usvg::Options<'static>> = LazyLock::new(|| {
    let mut opt = usvg::Options::default();
    // Qt's SVG renderer falls back to the application font, not usvg's Times New Roman.
    // With QGIS Server's offscreen platform that resolves to .AppleSystemUIFont (fontdb:
    // "System Font", SFNS.ttf) on macOS, verified with PyQt6; DejaVu Sans on typical Linux.
    let default = if cfg!(target_os = "macos") {
        "System Font"
    } else {
        "DejaVu Sans"
    };
    opt.font_family = std::env::var("RGIS_SVG_FONT").unwrap_or_else(|_| default.into());
    let started = std::time::Instant::now();
    opt.fontdb_mut().load_system_fonts();
    // QgsServer::initServer loads QGIS's standard test fonts ("QGIS Vera Sans" Roman and Bold).
    opt.fontdb_mut()
        .load_font_data(include_bytes!("../fonts/QGIS-Vera.ttf").to_vec());
    opt.fontdb_mut()
        .load_font_data(include_bytes!("../fonts/QGIS-VeraBd.ttf").to_vec());
    debug!(
        "loaded {} fonts for SVG text in {:?}",
        opt.fontdb.len(),
        started.elapsed()
    );
    opt
});

/// The system font database (shared with label text rendering).
pub fn fontdb() -> &'static usvg::fontdb::Database {
    &OPTIONS.fontdb
}

/// Rasterised markers, keyed like QgsSvgCache entries.
static IMAGES: LazyLock<Mutex<HashMap<String, Option<Arc<Pixmap>>>>> =
    LazyLock::new(Default::default);

/// Directories searched for relative SVG names: `RGIS_SVG_PATHS` (colon-separated, like
/// QGIS's user SVG paths), then the usual QGIS installation locations.
fn search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var("RGIS_SVG_PATHS")
        .ok()
        .map(|s| {
            s.split(':')
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    for p in [
        "/usr/share/qgis/svg",
        "/usr/local/share/qgis/svg",
        "/Applications/QGIS.app/Contents/Resources/svg",
    ] {
        v.push(PathBuf::from(p));
    }
    v
}

/// QgsSymbolLayerUtils::svgSymbolNameToPath: an existing path, then the SVG search paths,
/// then relative to the project.
pub fn resolve(name: &str, project_dir: &Path) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let name = name.strip_prefix("file://").unwrap_or(name);
    let direct = Path::new(name);
    if direct.is_absolute() && direct.exists() {
        return Some(direct.to_path_buf());
    }
    for dir in search_paths() {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    let p = project_dir.join(name);
    p.exists().then_some(p)
}

/// The leading number of an SVG length ("12.5px" → 12.5).
fn leading_number(s: &str) -> Option<f64> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

/// viewBox size, else width/height (QgsSvgCache::calcSizeScaleFactor).
/// Height / width of an SVG file's view box (QgsSvgCache::svgViewboxSize).
pub fn aspect(path: &Path) -> Option<f64> {
    let (w, h) = viewbox_size(&std::fs::read_to_string(path).ok()?)?;
    (w > 0.0).then(|| h / w)
}

fn viewbox_size(svg: &str) -> Option<(f64, f64)> {
    let doc = roxmltree::Document::parse_with_options(
        svg,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .ok()?;
    let root = doc.root_element();
    let svg_el = if root.has_tag_name("svg") {
        root
    } else {
        root.children().find(|c| c.has_tag_name("svg"))?
    };
    if let Some(vb) = svg_el.attribute("viewBox").or(svg_el.attribute("viewbox")) {
        let v: Vec<f64> = vb
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect();
        if v.len() == 4 && v[2] > 0.0 {
            return Some((v[2], v[3]));
        }
    }
    Some((
        leading_number(svg_el.attribute("width")?)?,
        leading_number(svg_el.attribute("height")?)?,
    ))
}

/// QColor::name(): "#rrggbb".
fn color_name(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// QString::number(alphaF) style output.
fn number(v: f64) -> String {
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s.to_string()
    }
}

/// QgsSvgCache::replaceElemParams: a value starting with `param(name)` is replaced as a
/// whole (any default after it is dropped), in attributes and in `style` entries.
fn replace_params(svg: &str, fill: Rgba, stroke: Rgba, stroke_width: f64) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(pos) = rest.find("param(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 6..];
        let Some(close) = after.find(')') else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let value = match after[..close].trim().to_ascii_lowercase().as_str() {
            "fill" => Some(color_name(fill)),
            "fill-opacity" => Some(number(fill.3 as f64 / 255.0)),
            "outline" => Some(color_name(stroke)),
            "outline-opacity" => Some(number(stroke.3 as f64 / 255.0)),
            "outline-width" => Some(number(stroke_width)),
            _ => None,
        };
        match value {
            Some(v) => {
                out.push_str(&v);
                // Drop the rest of the value (e.g. a default colour) up to its end.
                let tail = &after[close + 1..];
                let end = tail.find([';', '"', '\'']).unwrap_or(tail.len());
                rest = &tail[end..];
            }
            None => {
                out.push_str("param(");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parsed, parameter-substituted SVG plus its viewBox size.
fn load(
    path: Option<&Path>,
    size_px: f64,
    fill: Rgba,
    stroke: Rgba,
    stroke_px: f64,
) -> Option<(usvg::Tree, (f64, f64))> {
    let content = match path.map(std::fs::read_to_string) {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            warn!("cannot read SVG {}: {e}", path.unwrap().display());
            MISSING_SVG.to_string()
        }
        None => MISSING_SVG.to_string(),
    };
    let vb = viewbox_size(&content).unwrap_or((size_px, size_px));
    // Stroke width in SVG user units: pixels × viewBox width / marker width.
    let factor = if size_px > 0.0 { vb.0 / size_px } else { 1.0 };
    let svg = replace_params(&content, fill, stroke, stroke_px * factor);
    match usvg::Tree::from_str(&svg, &OPTIONS) {
        Ok(t) => Some((t, vb)),
        Err(e) => {
            warn!("invalid SVG {:?}: {e}", path);
            None
        }
    }
}

/// Render `tree` so that its viewBox fills `rect` (x, y, w, h) under `base`.
fn render_into(
    pm: &mut tiny_skia::PixmapMut,
    tree: &usvg::Tree,
    rect: (f32, f32, f32, f32),
    base: Transform,
) {
    let size = tree.size();
    let t = Transform::from_row(
        rect.2 / size.width(),
        0.0,
        0.0,
        rect.3 / size.height(),
        rect.0,
        rect.1,
    )
    .post_concat(base);
    resvg::render(tree, t, pm);
}

/// The target rectangle inside an image of `img` size: whole image for square viewBoxes,
/// else the viewBox scaled into (w, h) keeping its aspect ratio, centred (QgsSvgCache::cacheImage).
fn fit(
    vb: (f64, f64),
    w: f64,
    h: f64,
    img: (f64, f64),
    fixed_aspect: bool,
) -> (f32, f32, f32, f32) {
    if (vb.0 - vb.1).abs() < 1e-9 * vb.0.max(1.0) {
        return (0.0, 0.0, img.0 as f32, img.1 as f32);
    }
    let (sw, sh) = if fixed_aspect {
        (w, h)
    } else {
        let s = (w / vb.0).min(h / vb.1);
        (vb.0 * s, vb.1 * s)
    };
    (
        ((img.0 - sw) / 2.0) as f32,
        ((img.1 - sh) / 2.0) as f32,
        sw as f32,
        sh as f32,
    )
}

pub struct Marker<'a> {
    pub path: Option<&'a Path>,
    /// Marker width in pixels.
    pub width: f64,
    /// `fixedAspectRatio` (height / width); 0 = from the SVG's viewBox.
    pub fixed_aspect: f64,
    pub fill: Rgba,
    pub stroke: Rgba,
    pub stroke_width: f64,
    /// Clockwise rotation in degrees.
    pub angle: f64,
    pub opacity: f64,
}

/// Draw one SVG marker centred at (x, y): a cached image when unrotated, vectors when
/// rotated (as QgsSvgMarkerSymbolLayer::renderPoint does).
pub fn draw(pm: &mut Pixmap, m: &Marker, x: f32, y: f32) {
    if (m.width as i64) < 1 || m.width > 10_000.0 {
        return;
    }
    if m.angle.abs() > 1e-9 {
        let Some((tree, vb)) = load(m.path, m.width, m.fill, m.stroke, m.stroke_width) else {
            return;
        };
        let hw = if m.fixed_aspect > 0.0 {
            m.fixed_aspect
        } else {
            vb.1 / vb.0
        };
        let (w, h) = (m.width, m.width * hw);
        let rect = fit(vb, w, h, (w, h), m.fixed_aspect > 0.0);
        let base = Transform::from_translate(-(w as f32) / 2.0, -(h as f32) / 2.0)
            .post_rotate(m.angle as f32)
            .post_translate(x, y);
        let mut layer = Pixmap::new(pm.width(), pm.height()).expect("same size");
        render_into(&mut layer.as_mut(), &tree, rect, base);
        pm.draw_pixmap(
            0,
            0,
            layer.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: m.opacity as f32,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
        return;
    }
    let key = format!(
        "{:?}|{:.4}|{:.4}|{:?}|{:?}|{:.4}",
        m.path, m.width, m.fixed_aspect, m.fill, m.stroke, m.stroke_width
    );
    let cached = IMAGES.lock().get(&key).cloned();
    let img = match cached {
        Some(i) => i,
        None => {
            let img =
                load(m.path, m.width, m.fill, m.stroke, m.stroke_width).and_then(|(tree, vb)| {
                    let hw = if m.fixed_aspect > 0.0 {
                        m.fixed_aspect
                    } else {
                        vb.1 / vb.0
                    };
                    let (sw, sh) = (m.width, m.width * hw);
                    let (iw, ih) = ((sw as u32).max(1), (sh as u32).max(1));
                    let mut img = Pixmap::new(iw, ih)?;
                    let rect = fit(vb, sw, sh, (iw as f64, ih as f64), m.fixed_aspect > 0.0);
                    render_into(&mut img.as_mut(), &tree, rect, Transform::identity());
                    Some(Arc::new(img))
                });
            let mut cache = IMAGES.lock();
            if cache.len() > 10_000 {
                cache.clear();
            }
            cache.insert(key, img.clone());
            img
        }
    };
    let Some(img) = img else { return };
    if img.width() <= 1 {
        return;
    }
    // drawImage(-w/2, -h/2) relative to the point, on whole pixels.
    let ix = (x - img.width() as f32 / 2.0).round() as i32;
    let iy = (y - img.height() as f32 / 2.0).round() as i32;
    pm.draw_pixmap(
        ix,
        iy,
        Pixmap::as_ref(&img),
        &tiny_skia::PixmapPaint {
            opacity: m.opacity as f32,
            ..Default::default()
        },
        Transform::identity(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_parameters_like_qgis() {
        let svg = r##"<path fill="param(fill) #000" stroke="param(outline)" style="stroke-width:param(outline-width) 1;fill-opacity:param(fill-opacity)"/>"##;
        let out = replace_params(svg, Rgba(255, 0, 16, 128), Rgba(1, 2, 3, 255), 2.5);
        assert_eq!(
            out,
            r##"<path fill="#ff0010" stroke="#010203" style="stroke-width:2.5;fill-opacity:0.501961"/>"##
        );
    }

    /// cargo test --release -- --ignored --nocapture list_system_fonts
    #[test]
    #[ignore]
    fn list_system_fonts() {
        for f in OPTIONS.fontdb.faces() {
            let fam = f.families.first().map(|(n, _)| n.as_str()).unwrap_or("");
            if fam.contains("SF") || fam.contains("System") || fam.starts_with('.') {
                println!("{fam} | {:?} | {:?}", f.weight, f.source);
            }
        }
    }

    #[test]
    fn reads_viewbox_or_size() {
        assert_eq!(
            viewbox_size(r#"<svg viewBox="0 0 20 10"/>"#),
            Some((20.0, 10.0))
        );
        assert_eq!(
            viewbox_size(r#"<svg width="12px" height="6"/>"#),
            Some((12.0, 6.0))
        );
    }
}
