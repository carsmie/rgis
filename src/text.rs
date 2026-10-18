//! Label text the way QGIS Server draws it: QgsTextRenderer with `TextRenderFormat::AlwaysOutlines`
//! (glyph outlines filled as paths, buffers stroked as paths), using the font metrics and glyph
//! advances Qt reports.
//!
//! Qt's numbers come from its platform font engine, so they are modelled per engine:
//!
//! * **CoreText** (macOS): font size in pixels = CoreText point size, so variable fonts get the
//!   optical size axis set to the pixel size. Glyph advances are CoreText's (design advance + the
//!   `trak` table at the pixel size), truncated to Qt's 26.6 fixed point. Qt then shapes with
//!   HarfBuzz on the *default* instance with ptem = pixel size · 72/96, which adds GPOS kerning and
//!   the `trak` adjustment a second time. Verified glyph by glyph against PyQt6.
//! * **FreeType** (Linux): design advances at the pixel size, HarfBuzz kerning.
//!
//! Shaped runs and glyph outlines are cached per font and size, so repeated labels (the same
//! names on every tile) cost no shaping or outline decoding.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use harfrust::{ShapeOptions, ShaperData, UnicodeBuffer};
use parking_lot::Mutex;
use resvg::usvg::fontdb;
use skrifa::instance::{Location, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{FillRule, LineJoin, Paint, PathBuilder, Pixmap, Transform};
use tracing::debug;

use crate::symbology::Rgba;

/// Which Qt font engine's numbers to reproduce.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Engine {
    CoreText,
    FreeType,
}

static ENGINE: LazyLock<Engine> = LazyLock::new(|| match std::env::var("RGIS_TEXT_ENGINE").as_deref() {
    Ok("coretext") => Engine::CoreText,
    Ok("freetype") => Engine::FreeType,
    _ if cfg!(target_os = "macos") => Engine::CoreText,
    _ => Engine::FreeType,
});

/// A font as QgsTextFormat describes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontRequest {
    pub family: String,
    /// Qt 6 weight (1–1000). QGIS passes the project's `fontWeight` to QFont unchanged, so a
    /// QGIS 3 value like 75 ("bold" in Qt 5) asks Qt 6 for an ultra-light face.
    pub weight: u16,
    pub italic: bool,
    /// `namedStyle`, applied only when the family is installed (QgsFontUtils::updateFontViaStyle).
    pub style: String,
}

/// Qt::PenJoinStyle values used in QGIS project files.
pub fn qt_join(v: i64) -> LineJoin {
    match v {
        0 => LineJoin::Miter,
        0x80 => LineJoin::Round,
        _ => LineJoin::Bevel,
    }
}

/// Weight and italic of a style name (QFont::Weight scale).
fn style_weight(style: &str) -> Option<(u16, bool)> {
    let s = style.to_ascii_lowercase();
    let italic = s.contains("italic") || s.contains("oblique");
    let w = s.replace("italic", "").replace("oblique", "");
    let weight = match w.trim() {
        "thin" | "hairline" => 100,
        "extralight" | "extra light" | "ultralight" | "ultra light" => 200,
        "light" => 300,
        "" | "regular" | "normal" | "book" | "roman" => 400,
        "medium" => 500,
        "semibold" | "semi bold" | "demibold" | "demi bold" => 600,
        "bold" => 700,
        "extrabold" | "extra bold" | "ultrabold" | "ultra bold" | "heavy" => 800,
        "black" => 900,
        _ => return None,
    };
    Some((weight, italic))
}

/// Qt weight of a CoreText named instance (QCoreTextFontEngine::qtWeightFromCFWeight on
/// Apple's weight names, where UltraLight is lighter than Thin).
fn coretext_instance_weight(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().replace(' ', "").as_str() {
        "ultralight" => 100,
        "thin" => 200,
        "light" => 300,
        "regular" => 400,
        "medium" => 500,
        "semibold" => 600,
        "bold" => 700,
        "heavy" => 800,
        "black" => 900,
        other => return style_weight(other).map(|w| w.0),
    })
}

struct Face {
    data: Arc<Vec<u8>>,
    index: u32,
    shaper: ShaperData,
    upem: f64,
    axes: Vec<String>,
    /// Named instances at default width/optical size: (Qt weight, `wght` coordinate).
    instances: Vec<(u16, f32)>,
}

impl Face {
    fn font(&self) -> FontRef<'_> {
        FontRef::from_index(&self.data, self.index).expect("font parsed before")
    }
}

static FACES: LazyLock<Mutex<HashMap<fontdb::ID, Arc<Face>>>> = LazyLock::new(Default::default);
static FONTS: LazyLock<Mutex<HashMap<(FontRequest, i32), Arc<SizedFont>>>> = LazyLock::new(Default::default);

fn query(db: &fontdb::Database, family: &str, req: &FontRequest) -> Option<fontdb::ID> {
    let families = [fontdb::Family::Name(family)];
    db.query(&fontdb::Query {
        families: &families,
        weight: fontdb::Weight(req.weight),
        stretch: fontdb::Stretch::Normal,
        style: if req.italic { fontdb::Style::Italic } else { fontdb::Style::Normal },
    })
}

/// The face Qt would pick: the family if installed, otherwise Qt's fallback (on macOS the
/// system UI font, `.AppleSystemUIFont`; fontconfig's sans-serif elsewhere).
fn resolve_face(req: &FontRequest) -> Option<(Arc<Face>, u16)> {
    let db = crate::svg::fontdb();
    let fallback = std::env::var("RGIS_LABEL_FONT").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") { "System Font" } else { "DejaVu Sans" }.to_string()
    });
    let installed = db
        .faces()
        .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(&req.family)));
    let mut req = req.clone();
    if installed {
        if let Some((w, i)) = style_weight(&req.style) {
            req.weight = w;
            req.italic = i;
        }
    }
    let req = &req;
    let id = (if installed { query(db, &req.family, req) } else { None })
        .or_else(|| query(db, &fallback, req))
        .or_else(|| {
            let families = [fontdb::Family::SansSerif];
            db.query(&fontdb::Query { families: &families, ..Default::default() })
        })?;
    if let Some(f) = FACES.lock().get(&id) {
        return Some((f.clone(), req.weight));
    }
    let (data, index) = db.with_face_data(id, |d, i| (d.to_vec(), i))?;
    let data = Arc::new(data);
    let face = {
        let font = FontRef::from_index(&data, index).ok()?;
        let axes: Vec<_> = font.axes().iter().collect();
        let instances = font
            .named_instances()
            .iter()
            .filter_map(|ni| {
                let mut wght = None;
                for (c, ax) in ni.user_coords().zip(&axes) {
                    if ax.tag() == skrifa::Tag::new(b"wght") {
                        wght = Some(c);
                    } else if c != ax.default_value() {
                        return None;
                    }
                }
                let name = font.localized_strings(ni.subfamily_name_id()).english_or_first()?.to_string();
                Some((coretext_instance_weight(&name)?, wght?))
            })
            .collect();
        Face {
            instances,
            shaper: ShaperData::new(&font),
            upem: font.metrics(Size::unscaled(), skrifa::instance::LocationRef::default()).units_per_em as f64,
            axes: font.axes().iter().map(|a| a.tag().to_string()).collect(),
            data: data.clone(),
            index,
        }
    };
    debug!(family = %req.family, face = ?db.face(id).map(|f| f.families.clone()), "label font");
    let face = Arc::new(face);
    FACES.lock().insert(id, face.clone());
    Some((face, req.weight))
}

/// Horizontal tracking of the `trak` table's normal track at `ptem`, in font units
/// (interpolated between the table's sizes like CoreText and HarfBuzz).
fn trak_units(font: &FontRef, ptem: f32) -> f64 {
    use skrifa::raw::TableProvider;
    let Ok(trak) = font.trak() else { return 0.0 };
    let Some(Ok(h)) = trak.horiz() else { return 0.0 };
    let data = trak.offset_data();
    let Ok(sizes) = h.size_table(data) else { return 0.0 };
    let n = sizes.len();
    let Some(entry) = h.track_table().iter().find(|t| t.track().to_f32() == 0.0) else { return 0.0 };
    let Ok(values) = entry.per_size_values(data, n as u16) else { return 0.0 };
    let size = |i: usize| sizes[i].get().to_f32();
    let value = |i: usize| values[i].get() as f32;
    for i in 0..n.min(values.len()) {
        if size(i) >= ptem {
            if i == 0 {
                return value(0) as f64;
            }
            let t = (ptem - size(i - 1)) / (size(i) - size(i - 1));
            return (value(i - 1) + t * (value(i) - value(i - 1))) as f64;
        }
    }
    values.last().map(|v| v.get() as f64).unwrap_or(0.0)
}

/// Qt's QFixed::fromReal: truncation to 1/64.
fn q64(v: f64) -> f64 {
    (v * 64.0).trunc() / 64.0
}

#[derive(Debug, Clone, Copy)]
enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

struct Pen {
    s: f32,
    segs: Vec<Seg>,
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.segs.push(Seg::Move(x * self.s, -y * self.s));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.segs.push(Seg::Line(x * self.s, -y * self.s));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let s = self.s;
        self.segs.push(Seg::Quad(cx * s, -cy * s, x * s, -y * s));
    }
    fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, x: f32, y: f32) {
        let s = self.s;
        self.segs.push(Seg::Cubic(a * s, -b * s, c * s, -d * s, x * s, -y * s));
    }
    fn close(&mut self) {
        self.segs.push(Seg::Close);
    }
}

/// A shaped single line: glyphs with pen positions (pixels, baseline origin).
pub struct Run {
    glyphs: Vec<(u32, f64, f64)>,
    pub width: f64,
}

/// A font at one integer pixel size (QFont::setPixelSize), with Qt's metrics.
pub struct SizedFont {
    face: Arc<Face>,
    px: i32,
    location: Location,
    pub ascent: f64,
    pub descent: f64,
    #[allow(dead_code)]
    pub leading: f64,
    pub cap_height: f64,
    /// CoreText `trak` at the pixel size, in 1/64 px.
    ct_trak64: f64,
    glyphs: Mutex<HashMap<u32, Arc<Vec<Seg>>>>,
    /// Stroked glyph outlines (buffers), keyed by glyph, pen width bits and join.
    strokes: Mutex<HashMap<(u32, u32, u8), Option<Arc<tiny_skia::Path>>>>,
    runs: Mutex<HashMap<String, Arc<Run>>>,
}

/// The font for a request at `px` pixels (cached).
pub fn font(req: &FontRequest, px: i32) -> Option<Arc<SizedFont>> {
    let key = (req.clone(), px);
    if let Some(f) = FONTS.lock().get(&key) {
        return Some(f.clone());
    }
    let (face, weight) = resolve_face(req)?;
    let f = Arc::new(SizedFont::new(face, weight, px));
    FONTS.lock().insert(key, f.clone());
    Some(f)
}

impl SizedFont {
    fn new(face: Arc<Face>, weight: u16, px: i32) -> SizedFont {
        let font = face.font();
        let mut settings: Vec<(&str, f32)> = Vec::new();
        if face.axes.iter().any(|a| a == "wght") {
            // Qt picks the style (named instance) closest in weight.
            let wght = face
                .instances
                .iter()
                .min_by_key(|(w, _)| (*w as i32 - weight as i32).abs())
                .map(|(_, c)| *c)
                .unwrap_or(weight as f32);
            settings.push(("wght", wght));
        }
        if *ENGINE == Engine::CoreText && face.axes.iter().any(|a| a == "opsz") {
            settings.push(("opsz", px as f32));
        }
        let location = font.axes().location(settings);
        let m = font.metrics(Size::new(px as f32), &location);
        let mut f = SizedFont {
            px,
            ascent: q64(m.ascent as f64),
            descent: q64(-m.descent as f64),
            leading: q64(m.leading as f64),
            cap_height: q64(m.cap_height.unwrap_or(m.ascent) as f64),
            ct_trak64: 0.0,
            location,
            glyphs: Default::default(),
            strokes: Default::default(),
            runs: Default::default(),
            face: face.clone(),
        };
        if *ENGINE == Engine::FreeType {
            // QFontEngineFT reports FreeType's size metrics, which round to whole pixels.
            f.ascent = (m.ascent as f64).ceil();
            f.descent = (-m.descent as f64).ceil();
        }
        if *ENGINE == Engine::CoreText {
            // CoreText adds the `trak` table's tracking at its size to every advance, unrounded.
            f.ct_trak64 = trak_units(&font, px as f32) * f.scale64();
        }
        f
    }

    fn scale64(&self) -> f64 {
        self.px as f64 * 64.0 / self.face.upem
    }

    /// Shape on the default instance (as Qt's HarfBuzz font does): glyph ids and 26.6 advances.
    /// Shape on the default instance at 26.6 scale. With `engine`, glyph advances come from
    /// the font engine (Qt's HarfBuzz font functions), else from the font's tables.
    /// Returns (glyph, x advance, x offset).
    fn hb_shape(&self, text: &str, ptem: f64, engine: bool) -> Vec<(u32, f64, f64)> {
        struct EngineAdvances<'a>(&'a SizedFont);
        impl harfrust::font::FontFuncs for EngineAdvances<'_> {
            fn advance_width(&mut self, _: &harfrust::font::BuiltinFontFuncs, glyph: GlyphId) -> i32 {
                self.0.engine_adv64(glyph.to_u32()) as i32
            }
        }
        let font = self.face.font();
        let shaper = self.face.shaper.shaper(&font).build();
        let mut buf = UnicodeBuffer::new();
        buf.push_str(text);
        buf.guess_segment_properties();
        let mut funcs = EngineAdvances(self);
        let mut opts = ShapeOptions::new().point_size(Some(ptem as f32)).scale(Some(self.px * 64));
        if engine {
            opts = opts.font_funcs(Some(&mut funcs));
        }
        let out = shaper.shape(buf, opts);
        out.glyph_infos()
            .iter()
            .zip(out.glyph_positions())
            .map(|(i, p)| (i.glyph_id, p.x_advance as f64, p.x_offset as f64))
            .collect()
    }

    /// Advance of a glyph as Qt's font engine reports it, in 1/64 px.
    fn engine_adv64(&self, gid: u32) -> f64 {
        let font = self.face.font();
        match *ENGINE {
            Engine::CoreText => {
                // CoreText applies HVAR deltas unrounded (skrifa truncates them like FreeType).
                use skrifa::raw::TableProvider;
                let glyph = GlyphId::new(gid);
                let base = font
                    .glyph_metrics(Size::unscaled(), skrifa::instance::LocationRef::default())
                    .advance_width(glyph)
                    .unwrap_or(0.0) as f64;
                let units = match font.hvar().ok().and_then(|h| h.advance_width_delta(glyph, self.location.coords()).ok()) {
                    Some(delta) => base + delta.to_f64(),
                    None => font
                        .glyph_metrics(Size::unscaled(), &self.location)
                        .advance_width(glyph)
                        .unwrap_or(0.0) as f64,
                };
                (units * self.scale64() + self.ct_trak64).trunc()
            }
            Engine::FreeType => {
                let adv = font
                    .glyph_metrics(Size::new(self.px as f32), &self.location)
                    .advance_width(GlyphId::new(gid))
                    .unwrap_or(0.0) as f64;
                (adv * 64.0).round()
            }
        }
    }

    /// Lay out one line like QTextLayout (cached per string).
    pub fn shape(&self, text: &str) -> Arc<Run> {
        if let Some(r) = self.runs.lock().get(text) {
            return r.clone();
        }
        // Qt shapes with HarfBuzz using the font engine's advances; HarfBuzz adds kerning and
        // `trak` tracking (at ptem = pixel size · 72/96).
        let shaped = self.hb_shape(text, self.px as f64 * 72.0 / 96.0, true);
        let mut x64 = 0.0;
        let mut glyphs = Vec::with_capacity(shaped.len());
        for (gid, adv64, off64) in shaped {
            glyphs.push((gid, (x64 + off64) / 64.0, 0.0));
            x64 += adv64;
        }
        let run = Arc::new(Run { glyphs, width: x64 / 64.0 });
        let mut runs = self.runs.lock();
        if runs.len() > 20_000 {
            runs.clear();
        }
        runs.insert(text.to_string(), run.clone());
        run
    }

    /// QFontMetricsF::horizontalAdvance.
    #[allow(dead_code)] // used by tests and upcoming curved-label metrics
    pub fn horizontal_advance(&self, text: &str) -> f64 {
        self.shape(text).width
    }

    fn outline(&self, gid: u32) -> Arc<Vec<Seg>> {
        if let Some(g) = self.glyphs.lock().get(&gid) {
            return g.clone();
        }
        let font = self.face.font();
        let mut pen = Pen { s: (self.px as f64 / self.face.upem) as f32, segs: Vec::new() };
        if let Some(g) = font.outline_glyphs().get(GlyphId::new(gid)) {
            let _ = g.draw(DrawSettings::unhinted(Size::unscaled(), &self.location), &mut pen);
        }
        let g = Arc::new(pen.segs);
        self.glyphs.lock().insert(gid, g.clone());
        g
    }

    /// A glyph's outline stroked with `stroke` (cached). The stroke of a whole text path is
    /// the nonzero union of its contours' strokes, so per-glyph strokes filled together draw
    /// exactly what stroking the label path would, without re-stroking every label.
    fn stroked(&self, gid: u32, stroke: &tiny_skia::Stroke) -> Option<Arc<tiny_skia::Path>> {
        let key = (gid, stroke.width.to_bits(), stroke.line_join as u8);
        if let Some(p) = self.strokes.lock().get(&key) {
            return p.clone();
        }
        let mut pb = PathBuilder::new();
        let run = Run { glyphs: vec![(gid, 0.0, 0.0)], width: 0.0 };
        self.append_run(&mut pb, &run, Transform::identity());
        let p = pb.finish().and_then(|p| p.stroke(stroke, 1.0)).map(Arc::new);
        self.strokes.lock().insert(key, p.clone());
        p
    }

    /// Append a run's stroked glyph outlines (the buffer) mapped through `ts`.
    pub fn append_run_stroked(&self, pb: &mut PathBuilder, run: &Run, ts: Transform, stroke: &tiny_skia::Stroke) {
        use tiny_skia::PathSegment;
        for &(gid, gx, gy) in &run.glyphs {
            let Some(path) = self.stroked(gid, stroke) else { continue };
            let t = ts.pre_translate(gx as f32, gy as f32);
            let map = |p: tiny_skia::Point| {
                let mut q = [p];
                t.map_points(&mut q);
                q[0]
            };
            for seg in path.segments() {
                match seg {
                    PathSegment::MoveTo(p) => {
                        let p = map(p);
                        pb.move_to(p.x, p.y)
                    }
                    PathSegment::LineTo(p) => {
                        let p = map(p);
                        pb.line_to(p.x, p.y)
                    }
                    PathSegment::QuadTo(a, p) => {
                        let (a, p) = (map(a), map(p));
                        pb.quad_to(a.x, a.y, p.x, p.y)
                    }
                    PathSegment::CubicTo(a, b, p) => {
                        let (a, b, p) = (map(a), map(b), map(p));
                        pb.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y)
                    }
                    PathSegment::Close => pb.close(),
                }
            }
        }
    }

    /// Append a run's glyph outlines (QPainterPath::addText) mapped through `ts`.
    pub fn append_run(&self, pb: &mut PathBuilder, run: &Run, ts: Transform) {
        let map = |x: f32, y: f32| {
            let mut p = [tiny_skia::Point::from_xy(x, y)];
            ts.map_points(&mut p);
            (p[0].x, p[0].y)
        };
        for &(gid, gx, gy) in &run.glyphs {
            let (ox, oy) = (gx as f32, gy as f32);
            for s in self.outline(gid).iter() {
                match *s {
                    Seg::Move(x, y) => {
                        let (x, y) = map(x + ox, y + oy);
                        pb.move_to(x, y)
                    }
                    Seg::Line(x, y) => {
                        let (x, y) = map(x + ox, y + oy);
                        pb.line_to(x, y)
                    }
                    Seg::Quad(a, b, x, y) => {
                        let (a, b) = map(a + ox, b + oy);
                        let (x, y) = map(x + ox, y + oy);
                        pb.quad_to(a, b, x, y)
                    }
                    Seg::Cubic(a, b, c, d, x, y) => {
                        let (a, b) = map(a + ox, b + oy);
                        let (c, d) = map(c + ox, d + oy);
                        let (x, y) = map(x + ox, y + oy);
                        pb.cubic_to(a, b, c, d, x, y)
                    }
                    Seg::Close => pb.close(),
                }
            }
        }
    }
}

/// Horizontal alignment of lines within a label (Qgis::TextHorizontalAlignment).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// QgsTextDocumentMetrics for plain multi-line text in `TextLayoutMode::Labeling`.
pub struct LabelDocument {
    pub font: Arc<SizedFont>,
    pub lines: Vec<(Arc<Run>, f64)>,
    pub width: f64,
    pub height: f64,
}

impl LabelDocument {
    /// `line_height` is QgsTextFormat::lineHeight as a fraction (multilineHeight).
    pub fn new(font: Arc<SizedFont>, lines: &[&str], line_height: f64) -> LabelDocument {
        let n = lines.len().max(1);
        let line = font.ascent + font.descent;
        let runs: Vec<Arc<Run>> = lines.iter().map(|l| font.shape(l)).collect();
        let width = runs.iter().map(|r| r.width).fold(0.0, f64::max);
        // First block: ascent + descent; following blocks: lineHeight · (ascent + descent).
        let height = line + (n - 1) as f64 * line_height * line;
        // Baselines: the last line sits 0.25 · ascent above the label's bottom edge.
        let last = -0.25 * font.ascent;
        let lines = runs
            .into_iter()
            .enumerate()
            .map(|(i, r)| (r, last - (n - 1 - i) as f64 * line_height * line))
            .collect();
        LabelDocument { font, lines, width, height }
    }

    /// The text outline for a label whose bottom-left corner is at `origin` (pixels), rotated
    /// by `rotation` radians counter-clockwise (pal::LabelPosition::getAlpha).
    pub fn path(&self, origin: (f64, f64), rotation: f64, align: HAlign) -> Option<tiny_skia::Path> {
        self.build(origin, rotation, align, |f, pb, run, ts| f.append_run(pb, run, ts))
    }

    /// The buffer outline: the text path stroked with `stroke` (as a fillable path).
    pub fn buffer_path(&self, origin: (f64, f64), rotation: f64, align: HAlign, stroke: &tiny_skia::Stroke) -> Option<tiny_skia::Path> {
        self.build(origin, rotation, align, |f, pb, run, ts| f.append_run_stroked(pb, run, ts, stroke))
    }

    fn build(
        &self,
        origin: (f64, f64),
        rotation: f64,
        align: HAlign,
        mut add: impl FnMut(&SizedFont, &mut PathBuilder, &Run, Transform),
    ) -> Option<tiny_skia::Path> {
        let mut pb = PathBuilder::new();
        let base = Transform::from_translate(origin.0 as f32, origin.1 as f32)
            .pre_rotate((-rotation.to_degrees()) as f32);
        for (run, baseline) in &self.lines {
            let x = match align {
                HAlign::Left => 0.0,
                HAlign::Center => (self.width - run.width) / 2.0,
                HAlign::Right => self.width - run.width,
            };
            let ts = base.pre_translate(x as f32, *baseline as f32);
            add(&self.font, &mut pb, run, ts);
        }
        pb.finish()
    }
}

/// QgsTextBufferSettings as drawn by QgsTextRenderer::renderDeferredBuffer.
#[derive(Debug, Clone, Copy)]
pub struct Buffer {
    pub width_px: f64,
    pub color: Rgba,
    pub opacity: f64,
    pub fill_interior: bool,
    pub join: LineJoin,
}

fn color_paint(c: Rgba, alpha: f64) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.0, c.1, c.2, (alpha * 255.0).round().clamp(0.0, 255.0) as u8);
    p.anti_alias = true;
    p
}

/// Buffer pass: the text path filled with the buffer colour (unless "no fill") and stroked
/// with a pen of the buffer width. Colour alpha is replaced by the buffer opacity
/// (QColor::setAlphaF), as QGIS does.
pub fn draw_buffer(pm: &mut Pixmap, doc: &LabelDocument, path: &tiny_skia::Path, origin: (f64, f64), rotation: f64, align: HAlign, b: &Buffer) {
    let paint = color_paint(b.color, b.opacity);
    if b.fill_interior {
        pm.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    if let Some(outline) = doc.buffer_path(origin, rotation, align, &b.stroke()) {
        pm.fill_path(&outline, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

impl Buffer {
    /// The pen QgsTextRenderer::renderDeferredBuffer sets up (QPen default miter limit 2).
    pub fn stroke(&self) -> tiny_skia::Stroke {
        tiny_skia::Stroke { width: self.width_px as f32, line_join: self.join, miter_limit: 2.0, ..Default::default() }
    }
}

/// Text pass: the outline filled (Qt::WindingFill) with the text colour at the format opacity.
pub fn draw_text(pm: &mut Pixmap, path: &tiny_skia::Path, color: Rgba, opacity: f64) {
    let paint = color_paint(color, opacity);
    pm.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Glyph positions Qt 6 reports for "Sans Serif" Bold at 19 px on macOS
    /// (QTextLayout::glyphRuns, PyQt6, offscreen platform).
    #[test]
    #[cfg(target_os = "macos")]
    fn matches_qt_coretext_layout() {
        let req = FontRequest { family: "Sans Serif".into(), weight: 700, italic: false, style: String::new() };
        let f = font(&req, 19).unwrap();
        assert_eq!((f.ascent, f.descent, f.cap_height), (18.359375, 4.0, 13.375));
        let run = f.shape("Velstove");
        let qt = [0.0, 11.6406, 22.2969, 27.125, 37.1094, 43.875, 54.5, 64.4219];
        for (g, q) in run.glyphs.iter().zip(qt) {
            assert!((g.1 - q).abs() < 0.02, "{} vs {q}", g.1);
        }
        assert!((run.width - 75.078125).abs() < 0.02, "{}", run.width);
        assert!((f.horizontal_advance("Hehlingen") - 89.921875).abs() < 0.02);
    }

    /// Print advances for comparison with Qt: RGIS_WIDTHS=family,weight,px,file.
    #[test]
    #[ignore]
    fn print_widths() {
        let spec = std::env::var("RGIS_WIDTHS").unwrap();
        let p: Vec<&str> = spec.split(',').collect();
        let req = FontRequest { family: p[0].into(), weight: p[1].parse().unwrap(), italic: false, style: String::new() };
        let f = font(&req, p[2].parse().unwrap()).unwrap();
        for t in std::fs::read_to_string(p[3]).unwrap().lines() {
            println!("{t}\t{:.6}", f.horizontal_advance(t));
        }
    }

    #[test]
    fn maps_style_names() {
        assert_eq!(style_weight("Bold Italic"), Some((700, true)));
        assert_eq!(coretext_instance_weight("Ultralight"), Some(100));
    }

    /// QFont("Sans Serif", 14, 75): Qt 6 weight 75 → SF UltraLight (PyQt6: 65.90625 px).
    #[test]
    #[cfg(target_os = "macos")]
    fn legacy_weight_is_ultralight_in_qt6() {
        let req = FontRequest { family: "Sans Serif".into(), weight: 75, italic: false, style: "Bold".into() };
        let f = font(&req, 19).unwrap();
        assert!((f.horizontal_advance("Velstove") - 65.90625).abs() < 0.03, "{}", f.horizontal_advance("Velstove"));
    }
}
