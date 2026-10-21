//! Renderers and symbols as stored in `<renderer-v2>` (QgsFeatureRenderer / QgsSymbol).

use std::collections::HashMap;

use roxmltree::Node;
use serde_json::Value;
use tracing::warn;

use crate::expression::{Attrs, Expr, compare, num, to_str};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    /// QGIS colour strings: "r,g,b,a[,rgb:…]" (3.x) or "#rrggbb[aa]".
    pub fn parse(s: &str) -> Option<Rgba> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            let b = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
            return Some(Rgba(
                b(0)?,
                b(2)?,
                b(4)?,
                if hex.len() >= 8 { b(6)? } else { 255 },
            ));
        }
        let v: Vec<u8> = s
            .split(',')
            .take(4)
            .map_while(|p| p.trim().parse().ok())
            .collect();
        match v.len() {
            3 => Some(Rgba(v[0], v[1], v[2], 255)),
            4 => Some(Rgba(v[0], v[1], v[2], v[3])),
            _ => None,
        }
    }
}

/// Size units, named after QGIS's `RenderUnit` values (`MM`, `Pixel`, `MapUnit`, …).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    Mm,
    Pixel,
    Point,
    Inch,
    MapUnit,
    MetersInMapUnits,
}

#[derive(Debug, Clone, Copy)]
pub struct Size {
    pub value: f64,
    pub unit: Unit,
}

impl Size {
    pub fn new(value: Option<&str>, unit: Option<&str>, default: f64) -> Size {
        let unit = match unit.unwrap_or("MM") {
            "Pixel" => Unit::Pixel,
            "Point" => Unit::Point,
            "Inch" => Unit::Inch,
            "MapUnit" => Unit::MapUnit,
            "RenderMetersInMapUnits" => Unit::MetersInMapUnits,
            _ => Unit::Mm,
        };
        Size {
            value: value.and_then(|v| v.parse().ok()).unwrap_or(default),
            unit,
        }
    }

    /// Size in output pixels. `mupp` = map units per pixel, `mpu` = meters per map unit.
    pub fn px(&self, ctx: &RenderUnits) -> f64 {
        match self.unit {
            Unit::Mm => self.value * ctx.dpi / 25.4,
            Unit::Pixel => self.value,
            Unit::Point => self.value * ctx.dpi / 72.0,
            Unit::Inch => self.value * ctx.dpi,
            Unit::MapUnit => self.value / ctx.mupp,
            Unit::MetersInMapUnits => self.value / ctx.mupp / ctx.meters_per_unit,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderUnits {
    pub dpi: f64,
    pub mupp: f64,
    pub meters_per_unit: f64,
}

#[derive(Debug, Clone)]
pub struct Stroke {
    pub color: Rgba,
    pub width: Size,
    /// Dash pattern in multiples of the pen width (Qt semantics); empty = solid.
    pub dash: Vec<f64>,
    pub custom_dash: Option<(Vec<f64>, Unit)>,
    pub visible: bool,
    pub join: String,
    pub cap: String,
}

/// Qt's built-in brush patterns (QgsSymbolLayerUtils::decodeBrushStyle names → Qt::BrushStyle),
/// extracted from Qt 6.11 by painting each style with PyQt6. Row y, bit x = pixel (x, y)
/// painted; the 8×8 tile repeats from the image origin.
pub fn qt_brush_pattern(style: &str) -> Option<[u8; 8]> {
    Some(match style {
        "dense1" => [0xff, 0xbb, 0xff, 0xff, 0xff, 0xbb, 0xff, 0xff],
        "dense2" => [0x77, 0xff, 0xdd, 0xff, 0x77, 0xff, 0xdd, 0xff],
        "dense3" => [0x55, 0xbb, 0x55, 0xee, 0x55, 0xbb, 0x55, 0xee],
        "dense4" => [0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55],
        "dense5" => [0xaa, 0x44, 0xaa, 0x11, 0xaa, 0x44, 0xaa, 0x11],
        "dense6" => [0x88, 0x00, 0x22, 0x00, 0x88, 0x00, 0x22, 0x00],
        "dense7" => [0x00, 0x44, 0x00, 0x00, 0x00, 0x44, 0x00, 0x00],
        "horizontal" => [0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00],
        "vertical" => [0x10; 8],
        "cross" => [0x10, 0x10, 0x10, 0xff, 0x10, 0x10, 0x10, 0x10],
        "b_diagonal" => [0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01],
        "f_diagonal" => [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80],
        "diagonal_x" => [0x81, 0x42, 0x24, 0x18, 0x18, 0x24, 0x42, 0x81],
        _ => return None,
    })
}

/// Symbol layers, named after their QGIS classes (`class="SimpleFill"` etc.).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum SymbolLayer {
    SimpleFill {
        color: Rgba,
        filled: bool,
        /// Qt brush pattern for non-solid styles (8 rows, bit x = pixel x painted).
        pattern: Option<[u8; 8]>,
        stroke: Stroke,
    },
    SimpleLine {
        stroke: Stroke,
    },
    /// Parallel lines drawn with a line sub-symbol (QgsLinePatternFillSymbolLayer).
    LinePatternFill {
        /// Line angle in degrees, counter-clockwise from horizontal.
        angle: f64,
        distance: Size,
        offset: Size,
        /// `coordinate_reference="feature"`: pattern anchored at the feature, else the map.
        feature_ref: bool,
        /// `clip_mode="during_render"` uses QGIS's cached image brush; other modes draw lines.
        image_brush: bool,
        line: Box<Symbol>,
    },
    /// QgsSvgMarkerSymbolLayer.
    SvgMarker {
        /// The SVG name as stored in the project.
        name: String,
        /// Resolved at project load (None = missing → QGIS's "?" placeholder).
        path: Option<std::path::PathBuf>,
        size: Size,
        /// `fixedAspectRatio`; 0 = from the SVG.
        fixed_aspect: f64,
        fill: Rgba,
        stroke_color: Rgba,
        stroke_width: Size,
        angle: f64,
        offset: (Size, Size),
        /// (horizontal, vertical) anchor point: 0 left/top, 1 centre, 2 right/bottom.
        anchor: (u8, u8),
    },
    /// QgsMarkerLineSymbolLayer / QgsHashedLineSymbolLayer with interval placement: a marker
    /// sub-symbol, or a `hash_length` tick drawn with a line sub-symbol, every `interval`.
    TemplatedLine {
        /// Qgis::MarkerLinePlacements this port handles: Interval, FirstVertex, LastVertex.
        on_interval: bool,
        on_first: bool,
        on_last: bool,
        /// place_on_every_part; false = first vertex of the first part, last of the last.
        every_part: bool,
        interval: Size,
        offset_along: Size,
        /// average_angle_length (the angle is averaged over ± half of it).
        average_over: Size,
        rotate: bool,
        /// Some((hash_length, hash_angle)) for HashLine.
        hash: Option<(Size, f64)>,
        sub: Box<Symbol>,
    },
    /// SimpleMarker (width = height = size) and EllipseMarker (symbol_width × symbol_height).
    Marker {
        shape: String,
        width: Size,
        height: Size,
        /// EllipseMarker: data-defined `width`/`height` instead of `size`.
        ellipse: bool,
        color: Rgba,
        stroke: Stroke,
        /// Clockwise rotation in degrees.
        angle: f64,
        offset: (Size, Size),
        /// (horizontal, vertical) anchor point: 0 left/top, 1 centre, 2 right/bottom.
        anchor: (u8, u8),
    },
}

/// One symbol layer with its data-defined property overrides (QgsPropertyCollection),
/// keyed by QGIS property name (`size`, `angle`, `fillColor`, `strokeColor`, …).
#[derive(Debug, Clone)]
pub struct Layer {
    pub kind: SymbolLayer,
    pub dd: HashMap<String, Expr>,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub kind: String,
    pub opacity: f64,
    pub layers: Vec<Layer>,
    /// Any layer has data-defined properties (callers then supply expression context).
    pub data_defined: bool,
}

fn dd_num(dd: &HashMap<String, Expr>, key: &str, ctx: &Attrs) -> Option<f64> {
    dd.get(key).and_then(|e| num(&e.eval(ctx)))
}

fn dd_color(dd: &HashMap<String, Expr>, key: &str, ctx: &Attrs) -> Option<Rgba> {
    dd.get(key).and_then(|e| match e.eval(ctx) {
        Value::String(s) => Rgba::parse(&s),
        _ => None,
    })
}

impl Layer {
    /// Apply data-defined overrides for one feature; `ctx` holds attributes and
    /// `@variables`. Returns None when the layer is data-defined as disabled.
    pub fn resolve(&self, ctx: &Attrs) -> Option<std::borrow::Cow<'_, SymbolLayer>> {
        use std::borrow::Cow;
        if self.dd.is_empty() {
            return Some(Cow::Borrowed(&self.kind));
        }
        let dd = &self.dd;
        if let Some(e) = dd.get("enabled")
            && !crate::expression::truthy(&e.eval(ctx))
            && !e.eval(ctx).is_null()
        {
            return None;
        }
        let mut k = self.kind.clone();
        let stroke_dd = |s: &mut Stroke| {
            if let Some(c) = dd_color(dd, "strokeColor", ctx) {
                s.color = c;
            }
            if let Some(w) = dd_num(dd, "strokeWidth", ctx) {
                s.width.value = w;
            }
            if let Some(Value::String(st)) = dd.get("strokeStyle").map(|e| e.eval(ctx)) {
                s.dash = dash_for(&st);
                s.visible = st != "no";
            }
        };
        match &mut k {
            SymbolLayer::SimpleFill {
                color,
                filled,
                pattern,
                stroke,
            } => {
                if let Some(c) = dd_color(dd, "fillColor", ctx) {
                    *color = c;
                }
                if let Some(Value::String(st)) = dd.get("fillStyle").map(|e| e.eval(ctx)) {
                    *filled = st != "no";
                    *pattern = qt_brush_pattern(&st);
                }
                stroke_dd(stroke);
            }
            SymbolLayer::SimpleLine { stroke } => {
                // Line layers use strokeColor / strokeWidth for their pen.
                stroke_dd(stroke);
            }
            SymbolLayer::SvgMarker {
                size,
                fill,
                stroke_color,
                stroke_width,
                angle,
                ..
            } => {
                if let Some(s) = dd_num(dd, "size", ctx) {
                    size.value = s;
                }
                if let Some(a) = dd_num(dd, "angle", ctx) {
                    *angle = a;
                }
                if let Some(c) = dd_color(dd, "fillColor", ctx) {
                    *fill = c;
                }
                if let Some(c) = dd_color(dd, "strokeColor", ctx) {
                    *stroke_color = c;
                }
                if let Some(w) = dd_num(dd, "strokeWidth", ctx) {
                    stroke_width.value = w;
                }
            }
            SymbolLayer::TemplatedLine { interval, offset_along, .. } => {
                if let Some(i) = dd_num(dd, "interval", ctx) {
                    interval.value = i;
                }
                if let Some(o) = dd_num(dd, "offsetAlongLine", ctx) {
                    offset_along.value = o;
                }
            }
            SymbolLayer::LinePatternFill {
                angle, distance, ..
            } => {
                if let Some(a) = dd_num(dd, "lineAngle", ctx) {
                    *angle = a;
                }
                if let Some(d) = dd_num(dd, "lineDistance", ctx) {
                    distance.value = d;
                }
            }
            SymbolLayer::Marker {
                shape,
                width,
                height,
                ellipse,
                color,
                stroke,
                angle,
                ..
            } => {
                if let Some(Value::String(n)) = dd.get("name").map(|e| e.eval(ctx)) {
                    *shape = n;
                }
                if *ellipse {
                    if let Some(w) = dd_num(dd, "width", ctx) {
                        width.value = w;
                    }
                    if let Some(h) = dd_num(dd, "height", ctx) {
                        height.value = h;
                    }
                } else if let Some(s) = dd_num(dd, "size", ctx) {
                    width.value = s;
                    height.value = s;
                }
                if let Some(a) = dd_num(dd, "angle", ctx) {
                    *angle = a;
                }
                if let Some(c) = dd_color(dd, "fillColor", ctx) {
                    *color = c;
                }
                stroke_dd(stroke);
            }
        }
        Some(Cow::Owned(k))
    }
}

/// `<data_defined_properties>`: active properties of type 1 (static value), 2 (field)
/// or 3 (expression), as expressions.
fn data_defined(layer: Node) -> HashMap<String, Expr> {
    child(layer, "data_defined_properties")
        .map(data_defined_in)
        .unwrap_or_default()
}

/// Active data-defined properties of a QgsPropertyCollection element (`<data_defined_properties>`,
/// labeling's `<dd_properties>`), as expressions keyed by property name.
pub fn data_defined_in(container: Node) -> HashMap<String, Expr> {
    let mut out = HashMap::new();
    let Some(props) = Some(container).and_then(|d| {
        d.descendants()
            .find(|o| o.has_tag_name("Option") && o.attribute("name") == Some("properties"))
    }) else {
        return out;
    };
    for p in props.children().filter(|o| o.has_tag_name("Option")) {
        let Some(key) = p.attribute("name") else {
            continue;
        };
        let opt = |n: &str| {
            p.children()
                .find(|o| o.has_tag_name("Option") && o.attribute("name") == Some(n))
                .and_then(|o| o.attribute("value"))
        };
        if opt("active") != Some("true") {
            continue;
        }
        let expr = match opt("type") {
            Some("1") => Some(Expr::literal(opt("val").unwrap_or(""))),
            Some("2") => opt("field").map(Expr::field),
            Some("3") => opt("expression").and_then(|e| match Expr::parse(e) {
                Ok(x) => Some(x),
                Err(err) => {
                    warn!("data-defined {key} expression '{e}' not supported ({err})");
                    None
                }
            }),
            _ => None,
        };
        if let Some(e) = expr {
            out.insert(key.to_string(), e);
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Category {
    pub values: Vec<String>,
    pub symbol: String,
    pub label: String,
    pub render: bool,
}

#[derive(Debug, Clone)]
pub struct Range {
    pub lower: f64,
    pub upper: f64,
    pub symbol: String,
    pub label: String,
    pub render: bool,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub label: String,
    pub filter: Option<Expr>,
    pub is_else: bool,
    pub symbol: Option<String>,
    pub min_scale: f64,
    pub max_scale: f64,
    pub active: bool,
    pub children: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub enum Renderer {
    Single(Symbol),
    Categorized {
        attr: String,
        categories: Vec<Category>,
        symbols: HashMap<String, Symbol>,
    },
    Graduated {
        attr: String,
        ranges: Vec<Range>,
        symbols: HashMap<String, Symbol>,
    },
    Rules {
        rules: Vec<Rule>,
        symbols: HashMap<String, Symbol>,
    },
    Null,
}

/// A legend entry (one per symbol class) – used by GetLegendGraphic.
pub struct LegendItem<'a> {
    pub label: String,
    pub symbol: &'a Symbol,
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

/// Properties of a symbol layer, from `<Option type="Map">` (QGIS >= 3.26) or `<prop k v>`.
fn props(layer: Node) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for c in layer.children().filter(|c| c.is_element()) {
        if c.has_tag_name("prop") {
            if let (Some(k), Some(v)) = (c.attribute("k"), c.attribute("v")) {
                m.insert(k.to_string(), v.to_string());
            }
        } else if c.has_tag_name("Option") && c.attribute("type") == Some("Map") {
            for o in c.children().filter(|o| o.has_tag_name("Option")) {
                if let (Some(k), Some(v)) = (o.attribute("name"), o.attribute("value")) {
                    m.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    m
}

fn dash_for(style: &str) -> Vec<f64> {
    match style {
        "dash" => vec![4.0, 2.0],
        "dot" => vec![1.0, 2.0],
        "dash dot" => vec![4.0, 2.0, 1.0, 2.0],
        "dash dot dot" => vec![4.0, 2.0, 1.0, 2.0, 1.0, 2.0],
        _ => vec![],
    }
}

fn stroke(
    p: &HashMap<String, String>,
    color_key: &str,
    width_key: &str,
    unit_key: &str,
    style_key: &str,
    default_w: f64,
) -> Stroke {
    let style = p.get(style_key).map(|s| s.as_str()).unwrap_or("solid");
    let custom_dash = (p.get("use_custom_dash").map(|s| s.as_str()) == Some("1"))
        .then(|| p.get("customdash"))
        .flatten()
        .map(|d| {
            let unit = Size::new(Some("1"), p.get("customdash_unit").map(|s| s.as_str()), 1.0).unit;
            (
                d.split(';')
                    .filter_map(|v| v.parse().ok())
                    .collect::<Vec<f64>>(),
                unit,
            )
        });
    Stroke {
        color: p
            .get(color_key)
            .and_then(|c| Rgba::parse(c))
            .unwrap_or(Rgba(35, 35, 35, 255)),
        width: Size::new(
            p.get(width_key).map(|s| s.as_str()),
            p.get(unit_key).map(|s| s.as_str()),
            default_w,
        ),
        dash: dash_for(style),
        custom_dash,
        visible: style != "no",
        join: p
            .get("joinstyle")
            .cloned()
            .unwrap_or_else(|| "bevel".into()),
        cap: p
            .get("capstyle")
            .cloned()
            .unwrap_or_else(|| "square".into()),
    }
}

pub fn parse_symbol(n: Node) -> Symbol {
    let mut layers = vec![];
    for l in n.children().filter(|c| c.has_tag_name("layer")) {
        if l.attribute("enabled") == Some("0") {
            continue;
        }
        let p = props(l);
        let class = l.attribute("class").unwrap_or("");
        let g = |k: &str| p.get(k).map(|s| s.as_str());
        let offset = || {
            let unit = g("offset_unit");
            let mut it = g("offset").unwrap_or("0,0").split(',');
            let (x, y) = (it.next(), it.next());
            (Size::new(x, unit, 0.0), Size::new(y, unit, 0.0))
        };
        let angle = g("angle").and_then(|a| a.parse().ok()).unwrap_or(0.0);
        // QgsMarkerSymbolLayer::decode{Horizontal,Vertical}AnchorPoint (Baseline = Bottom)
        let anchor = (
            g("horizontal_anchor_point").and_then(|v| v.parse().ok()).unwrap_or(1u8).min(2),
            g("vertical_anchor_point").and_then(|v| v.parse().ok()).unwrap_or(1u8).min(2),
        );
        let kind = match class {
            "SimpleFill" => {
                let style = g("style").unwrap_or("solid");
                SymbolLayer::SimpleFill {
                    color: g("color")
                        .and_then(Rgba::parse)
                        .unwrap_or(Rgba(125, 139, 143, 255)),
                    filled: style != "no",
                    pattern: qt_brush_pattern(style),
                    stroke: stroke(
                        &p,
                        "outline_color",
                        "outline_width",
                        "outline_width_unit",
                        "outline_style",
                        0.26,
                    ),
                }
            }
            "SimpleLine" => SymbolLayer::SimpleLine {
                stroke: stroke(
                    &p,
                    "line_color",
                    "line_width",
                    "line_width_unit",
                    "line_style",
                    0.26,
                ),
            },
            "LinePatternFill" => {
                let Some(sub) = l.children().find(|c| c.has_tag_name("symbol")) else {
                    warn!("LinePatternFill without a line sub-symbol ignored");
                    continue;
                };
                SymbolLayer::LinePatternFill {
                    angle: g("lineangle")
                        .or(g("angle"))
                        .and_then(|a| a.parse().ok())
                        .unwrap_or(45.0),
                    distance: Size::new(g("distance"), g("distance_unit"), 5.0),
                    offset: Size::new(g("offset"), g("offset_unit"), 0.0),
                    feature_ref: g("coordinate_reference") == Some("feature"),
                    image_brush: g("clip_mode").is_none_or(|m| m == "during_render"),
                    line: Box::new(parse_symbol(sub)),
                }
            }
            "EllipseMarker" => SymbolLayer::Marker {
                shape: g("symbol_name").unwrap_or("circle").to_string(),
                width: Size::new(g("symbol_width"), g("symbol_width_unit"), 4.0),
                height: Size::new(g("symbol_height"), g("symbol_height_unit"), 3.0),
                ellipse: true,
                color: g("color")
                    .and_then(Rgba::parse)
                    .unwrap_or(Rgba(255, 255, 255, 255)),
                stroke: stroke(
                    &p,
                    "outline_color",
                    "outline_width",
                    "outline_width_unit",
                    "outline_style",
                    0.0,
                ),
                angle,
                offset: offset(),
                anchor,
            },
            "SvgMarker" => SymbolLayer::SvgMarker {
                name: g("name").unwrap_or("").to_string(),
                path: None,
                size: Size::new(g("size"), g("size_unit"), 4.0),
                fixed_aspect: g("fixedAspectRatio")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0),
                fill: g("color")
                    .and_then(Rgba::parse)
                    .unwrap_or(Rgba(255, 0, 0, 255)),
                stroke_color: g("outline_color")
                    .and_then(Rgba::parse)
                    .unwrap_or(Rgba(35, 35, 35, 255)),
                stroke_width: Size::new(g("outline_width"), g("outline_width_unit"), 0.2),
                angle,
                offset: offset(),
                anchor,
            },
            "SimpleMarker" | "FontMarker" => {
                if class != "SimpleMarker" {
                    warn!("symbol layer {class} approximated as SimpleMarker");
                }
                let size = Size::new(g("size"), g("size_unit"), 2.0);
                SymbolLayer::Marker {
                    shape: g("name").unwrap_or("circle").to_string(),
                    width: size,
                    height: size,
                    ellipse: false,
                    color: g("color")
                        .and_then(Rgba::parse)
                        .unwrap_or(Rgba(255, 0, 0, 255)),
                    stroke: stroke(
                        &p,
                        "outline_color",
                        "outline_width",
                        "outline_width_unit",
                        "outline_style",
                        0.0,
                    ),
                    angle,
                    offset: offset(),
                    anchor,
                }
            }
            "MarkerLine" | "HashLine" => {
                // `placements` (flags) since QGIS 3.24, the single `placement` before.
                let placements: Vec<String> = match g("placements") {
                    Some(v) => v.split('|').map(String::from).collect(),
                    None => vec![match g("placement").unwrap_or("interval") {
                        "vertex" => "Vertex",
                        "lastvertex" => "LastVertex",
                        "firstvertex" => "FirstVertex",
                        "centralpoint" => "CentralPoint",
                        "curvepoint" => "CurvePoint",
                        "segmentcenter" => "SegmentCenter",
                        "innervertices" => "InnerVertices",
                        _ => "Interval",
                    }
                    .to_string()],
                };
                let has = |n: &str| placements.iter().any(|p| p == n);
                let offset = g("offset").and_then(|o| o.split(',').next()?.parse::<f64>().ok()).unwrap_or(0.0);
                let along = g("offset_along_line").and_then(|o| o.parse::<f64>().ok()).unwrap_or(0.0);
                let Some(sub) = l.children().find(|c| c.has_tag_name("symbol")) else {
                    warn!("{class} without a sub-symbol ignored");
                    continue;
                };
                let unsupported: Vec<&String> = placements
                    .iter()
                    .filter(|p| !matches!(p.as_str(), "Interval" | "FirstVertex" | "LastVertex"))
                    .collect();
                if !unsupported.is_empty() {
                    warn!("{class}: placement {unsupported:?} not supported; those are left out");
                }
                // A perpendicular offset needs QGIS's GEOS offset curve; the vertex placements'
                // offset along the line its renderOffsetVertexAlongLine — neither ported yet.
                if offset != 0.0 || (along != 0.0 && (has("FirstVertex") || has("LastVertex"))) {
                    warn!("{class} with an offset is not supported; ignored");
                    continue;
                }
                SymbolLayer::TemplatedLine {
                    on_interval: has("Interval"),
                    on_first: has("FirstVertex"),
                    on_last: has("LastVertex"),
                    every_part: g("place_on_every_part").is_none_or(|v| v == "true" || v == "1"),
                    interval: Size::new(g("interval"), g("interval_unit"), 3.0),
                    offset_along: Size::new(g("offset_along_line"), g("offset_along_line_unit"), 0.0),
                    average_over: Size::new(g("average_angle_length"), g("average_angle_unit"), 4.0),
                    rotate: g("rotate").is_none_or(|r| r == "1" || r == "true"),
                    hash: (class == "HashLine").then(|| {
                        (
                            Size::new(g("hash_length"), g("hash_length_unit"), 3.0),
                            g("hash_angle").and_then(|a| a.parse().ok()).unwrap_or(0.0),
                        )
                    }),
                    sub: Box::new(parse_symbol(sub)),
                }
            }
            other => {
                warn!("unsupported symbol layer class '{other}' ignored");
                continue;
            }
        };
        layers.push(Layer {
            kind,
            dd: data_defined(l),
        });
    }
    Symbol {
        kind: n.attribute("type").unwrap_or("fill").to_string(),
        opacity: n
            .attribute("alpha")
            .and_then(|a| a.parse().ok())
            .unwrap_or(1.0),
        data_defined: layers.iter().any(|l| !l.dd.is_empty()),
        layers,
    }
}

fn parse_symbols(r: Node) -> HashMap<String, Symbol> {
    child(r, "symbols")
        .map(|s| {
            s.children()
                .filter(|c| c.has_tag_name("symbol"))
                .map(|c| {
                    (
                        c.attribute("name").unwrap_or("").to_string(),
                        parse_symbol(c),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_rule(n: Node) -> Rule {
    let filter = n.attribute("filter").unwrap_or("").trim();
    let is_else = filter.eq_ignore_ascii_case("ELSE");
    let parsed = if filter.is_empty() || is_else {
        None
    } else {
        match Expr::parse(filter) {
            Ok(e) => Some(e),
            Err(e) => {
                warn!("rule filter '{filter}' not supported ({e}); rule disabled");
                Some(Expr::Lit(Value::Bool(false)))
            }
        }
    };
    let scale = |k: &str| n.attribute(k).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    Rule {
        label: n.attribute("label").unwrap_or("").to_string(),
        filter: parsed,
        is_else,
        symbol: n.attribute("symbol").map(String::from),
        // QGIS: scalemaxdenom = most zoomed out (largest denominator)
        min_scale: scale("scalemaxdenom"),
        max_scale: scale("scalemindenom"),
        active: n.attribute("active") != Some("0"),
        children: n
            .children()
            .filter(|c| c.has_tag_name("rule"))
            .map(parse_rule)
            .collect(),
    }
}

impl Symbol {
    /// Resolve SVG file names once, at project load (QgsSymbolLayerUtils::svgSymbolNameToPath).
    pub fn resolve_paths(&mut self, project_dir: &std::path::Path) {
        for l in &mut self.layers {
            match &mut l.kind {
                SymbolLayer::SvgMarker { name, path, .. } => {
                    *path = crate::svg::resolve(name, project_dir);
                    if path.is_none() {
                        warn!("SVG '{name}' not found; drawn as QGIS's '?' placeholder");
                    }
                }
                SymbolLayer::LinePatternFill { line, .. } => line.resolve_paths(project_dir),
                SymbolLayer::TemplatedLine { sub, .. } => sub.resolve_paths(project_dir),
                _ => {}
            }
        }
    }
}

impl Renderer {
    pub fn resolve_paths(&mut self, project_dir: &std::path::Path) {
        match self {
            Renderer::Single(s) => s.resolve_paths(project_dir),
            Renderer::Categorized { symbols, .. }
            | Renderer::Graduated { symbols, .. }
            | Renderer::Rules { symbols, .. } => symbols
                .values_mut()
                .for_each(|s| s.resolve_paths(project_dir)),
            Renderer::Null => {}
        }
    }

    pub fn parse(r: Node) -> Renderer {
        let ty = r.attribute("type").unwrap_or("");
        match ty {
            "singleSymbol" => match parse_symbols(r).remove("0") {
                Some(s) => Renderer::Single(s),
                None => Renderer::Null,
            },
            "categorizedSymbol" => Renderer::Categorized {
                attr: r
                    .attribute("attr")
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string(),
                categories: child(r, "categories")
                    .map(|cs| {
                        cs.children()
                            .filter(|c| c.has_tag_name("category"))
                            .map(|c| {
                                let mut values: Vec<String> = c
                                    .children()
                                    .filter(|v| v.has_tag_name("val"))
                                    .filter_map(|v| v.attribute("value"))
                                    .map(String::from)
                                    .collect();
                                if values.is_empty() {
                                    values.push(c.attribute("value").unwrap_or("").to_string());
                                }
                                Category {
                                    values,
                                    symbol: c.attribute("symbol").unwrap_or("").to_string(),
                                    label: c.attribute("label").unwrap_or("").to_string(),
                                    render: c.attribute("render") != Some("false"),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                symbols: parse_symbols(r),
            },
            "graduatedSymbol" => Renderer::Graduated {
                attr: r
                    .attribute("attr")
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string(),
                ranges: child(r, "ranges")
                    .map(|rs| {
                        rs.children()
                            .filter(|c| c.has_tag_name("range"))
                            .map(|c| Range {
                                lower: c
                                    .attribute("lower")
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(f64::MIN),
                                upper: c
                                    .attribute("upper")
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(f64::MAX),
                                symbol: c.attribute("symbol").unwrap_or("").to_string(),
                                label: c.attribute("label").unwrap_or("").to_string(),
                                render: c.attribute("render") != Some("false"),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                symbols: parse_symbols(r),
            },
            "RuleRenderer" => Renderer::Rules {
                rules: child(r, "rules")
                    .map(|rs| {
                        rs.children()
                            .filter(|c| c.has_tag_name("rule"))
                            .map(parse_rule)
                            .collect()
                    })
                    .unwrap_or_default(),
                symbols: parse_symbols(r),
            },
            "nullSymbol" => Renderer::Null,
            _ => {
                // Wrapper renderers (inverted polygons, point displacement, …) embed another one.
                if let Some(inner) = r
                    .descendants()
                    .skip(1)
                    .find(|c| c.has_tag_name("renderer-v2"))
                {
                    warn!("renderer '{ty}' rendered via its embedded renderer");
                    return Renderer::parse(inner);
                }
                warn!("renderer type '{ty}' not supported; layer will not be drawn");
                Renderer::Null
            }
        }
    }

    /// Symbols to draw for a feature (QgsFeatureRenderer::symbolsForFeature).
    pub fn symbols_for(&self, a: &Attrs, scale: f64) -> Vec<&Symbol> {
        match self {
            Renderer::Single(s) => vec![s],
            Renderer::Null => vec![],
            Renderer::Categorized {
                attr,
                categories,
                symbols,
            } => {
                let v = a.get(attr).cloned().unwrap_or(Value::Null);
                let key = to_str(&v);
                let hit = categories
                    .iter()
                    .find(|c| {
                        !v.is_null()
                            && c.values.iter().any(|cv| {
                                *cv == key
                                    || (num(&v).is_some()
                                        && compare(&v, &Value::String(cv.clone()))
                                            == Some(std::cmp::Ordering::Equal))
                            })
                    })
                    .or_else(|| {
                        categories
                            .iter()
                            .find(|c| c.values.iter().all(|cv| cv.is_empty()))
                    });
                hit.filter(|c| c.render)
                    .and_then(|c| symbols.get(&c.symbol))
                    .into_iter()
                    .collect()
            }
            Renderer::Graduated {
                attr,
                ranges,
                symbols,
            } => {
                let Some(v) = a.get(attr).and_then(num) else {
                    return vec![];
                };
                ranges
                    .iter()
                    .enumerate()
                    .find(|(i, r)| (v > r.lower || (*i == 0 && v >= r.lower)) && v <= r.upper)
                    .filter(|(_, r)| r.render)
                    .and_then(|(_, r)| symbols.get(&r.symbol))
                    .into_iter()
                    .collect()
            }
            Renderer::Rules { rules, symbols } => {
                let mut out = vec![];
                collect_rules(rules, a, scale, symbols, &mut out);
                out
            }
        }
    }

    pub fn legend(&self) -> Vec<LegendItem<'_>> {
        match self {
            Renderer::Single(s) => vec![LegendItem {
                label: String::new(),
                symbol: s,
            }],
            Renderer::Null => vec![],
            Renderer::Categorized {
                categories,
                symbols,
                ..
            } => categories
                .iter()
                .filter_map(|c| {
                    symbols.get(&c.symbol).map(|s| LegendItem {
                        label: c.label.clone(),
                        symbol: s,
                    })
                })
                .collect(),
            Renderer::Graduated {
                ranges, symbols, ..
            } => ranges
                .iter()
                .filter_map(|r| {
                    symbols.get(&r.symbol).map(|s| LegendItem {
                        label: r.label.clone(),
                        symbol: s,
                    })
                })
                .collect(),
            Renderer::Rules { rules, symbols } => {
                fn walk<'a>(
                    rs: &'a [Rule],
                    symbols: &'a HashMap<String, Symbol>,
                    out: &mut Vec<LegendItem<'a>>,
                ) {
                    for r in rs {
                        if let Some(s) = r.symbol.as_ref().and_then(|k| symbols.get(k)) {
                            out.push(LegendItem {
                                label: r.label.clone(),
                                symbol: s,
                            });
                        }
                        walk(&r.children, symbols, out);
                    }
                }
                let mut out = vec![];
                walk(rules, symbols, &mut out);
                out
            }
        }
    }
}

impl Renderer {
    /// Whether any feature can get a symbol at this scale (QgsRuleBasedRenderer::startRender
    /// leaves no active rule → QGIS fetches nothing). False lets the layer be skipped
    /// before any data is read.
    pub fn active_at(&self, scale: f64) -> bool {
        fn any(rules: &[Rule], scale: f64) -> bool {
            rules.iter().any(|r| r.active && in_scale(r, scale) && (r.symbol.is_some() || any(&r.children, scale)))
        }
        match self {
            Renderer::Rules { rules, .. } => any(rules, scale),
            Renderer::Null => false,
            _ => true,
        }
    }
}

fn in_scale(r: &Rule, scale: f64) -> bool {
    (r.min_scale <= 0.0 || scale <= r.min_scale) && (r.max_scale <= 0.0 || scale > r.max_scale)
}

fn collect_rules<'a>(
    rules: &'a [Rule],
    a: &Attrs,
    scale: f64,
    symbols: &'a HashMap<String, Symbol>,
    out: &mut Vec<&'a Symbol>,
) -> bool {
    let mut any = false;
    for r in rules.iter().filter(|r| !r.is_else) {
        if r.active && in_scale(r, scale) && r.filter.as_ref().is_none_or(|f| f.matches(a)) {
            any = true;
            if let Some(s) = r.symbol.as_ref().and_then(|k| symbols.get(k)) {
                out.push(s);
            }
            collect_rules(&r.children, a, scale, symbols, out);
        }
    }
    if !any {
        for r in rules
            .iter()
            .filter(|r| r.is_else && r.active && in_scale(r, scale))
        {
            any = true;
            if let Some(s) = r.symbol.as_ref().and_then(|k| symbols.get(k)) {
                out.push(s);
            }
            collect_rules(&r.children, a, scale, symbols, out);
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(
            Rgba::parse("242,239,233,255,rgb:0.9,0.9,0.9,1"),
            Some(Rgba(242, 239, 233, 255))
        );
        assert_eq!(Rgba::parse("#ff0000"), Some(Rgba(255, 0, 0, 255)));
        assert_eq!(Rgba::parse("1,2,3"), Some(Rgba(1, 2, 3, 255)));
    }

    #[test]
    fn parses_both_property_encodings() {
        let new = r#"<symbol type="fill" alpha="0.5" name="0"><layer class="SimpleFill" enabled="1"><Option type="Map"><Option type="QString" name="color" value="1,2,3,255,rgb:0,0,0,1"/><Option type="QString" name="outline_width" value="0.5"/></Option></layer></symbol>"#;
        let old = r#"<symbol type="fill" alpha="1" name="0"><layer class="SimpleFill" enabled="1"><prop k="color" v="4,5,6,255"/><prop k="style" v="no"/></layer></symbol>"#;
        let d = roxmltree::Document::parse(new).unwrap();
        let s = parse_symbol(d.root_element());
        assert_eq!(s.opacity, 0.5);
        match &s.layers[0].kind {
            SymbolLayer::SimpleFill {
                color,
                stroke,
                filled,
                ..
            } => {
                assert_eq!(*color, Rgba(1, 2, 3, 255));
                assert_eq!(stroke.width.value, 0.5);
                assert!(*filled);
            }
            _ => panic!(),
        }
        let d = roxmltree::Document::parse(old).unwrap();
        match &parse_symbol(d.root_element()).layers[0].kind {
            SymbolLayer::SimpleFill { color, filled, .. } => {
                assert_eq!(*color, Rgba(4, 5, 6, 255));
                assert!(!*filled);
            }
            _ => panic!(),
        }
    }
}
