//! Label configuration as QGIS stores it: `<labeling type="simple">` / `"rule-based"` with
//! QgsPalLayerSettings `<settings>` (QgsPalLayerSettings::readXml, QgsTextFormat::readXml), and
//! the project-wide labeling engine settings (QgsLabelingEngineSettings::readSettingsFromProject).

use std::collections::HashMap;

use roxmltree::Node;
use tracing::warn;

use crate::expression::Expr;
use crate::symbology::{data_defined_in, Rgba, Size, Unit};
use crate::text::{qt_join, FontRequest};

/// Qgis::LabelPlacement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    AroundPoint,
    OverPoint,
    Line,
    Curved,
    Horizontal,
    Free,
    OrderedPositionsAroundPoint,
    PerimeterCurved,
    OutsidePolygons,
}

impl Placement {
    fn from_int(v: i64) -> Placement {
        match v {
            1 => Placement::OverPoint,
            2 => Placement::Line,
            3 => Placement::Curved,
            4 => Placement::Horizontal,
            5 => Placement::Free,
            6 => Placement::OrderedPositionsAroundPoint,
            7 => Placement::PerimeterCurved,
            8 => Placement::OutsidePolygons,
            _ => Placement::AroundPoint,
        }
    }
}

/// Qgis::LabelPredefinedPointPosition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointPosition {
    TopLeft,
    TopSlightlyLeft,
    TopMiddle,
    TopSlightlyRight,
    TopRight,
    MiddleLeft,
    MiddleRight,
    BottomLeft,
    BottomSlightlyLeft,
    BottomMiddle,
    BottomSlightlyRight,
    BottomRight,
    OverPoint,
}

/// QgsLabelingUtils::decodePredefinedPositionOrder.
pub fn decode_position_order(s: &str) -> Vec<PointPosition> {
    use PointPosition::*;
    s.split(',')
        .filter_map(|p| {
            Some(match p.trim().to_ascii_uppercase().as_str() {
                "TL" => TopLeft,
                "TSL" => TopSlightlyLeft,
                "T" => TopMiddle,
                "TSR" => TopSlightlyRight,
                "TR" => TopRight,
                "L" => MiddleLeft,
                "R" => MiddleRight,
                "BL" => BottomLeft,
                "BSL" => BottomSlightlyLeft,
                "B" => BottomMiddle,
                "BSR" => BottomSlightlyRight,
                "BR" => BottomRight,
                "O" => OverPoint,
                _ => return None,
            })
        })
        .collect()
}

/// QgsPalLayerSettings::DEFAULT_PLACEMENT_ORDER.
pub fn default_position_order() -> Vec<PointPosition> {
    decode_position_order("TR,TL,BR,BL,R,L,TSR,BSR")
}

/// Qgis::LabelOverlapHandling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverlapHandling {
    PreventOverlap,
    AllowOverlapIfRequired,
    AllowOverlapAtNoCost,
}

/// Qgis::LabelMultiLineAlignment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MultiLineAlign {
    Left,
    Center,
    Right,
    FollowPlacement,
    Justify,
}

/// QgsLabelObstacleSettings::ObstacleType.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObstacleType {
    PolygonInterior,
    PolygonBoundary,
    PolygonWhole,
}

/// QgsTextBufferSettings.
#[derive(Debug, Clone)]
pub struct BufferSettings {
    pub size: Size,
    pub color: Rgba,
    pub opacity: f64,
    pub fill_interior: bool,
    pub join: tiny_skia::LineJoin,
}

/// The parts of QgsTextFormat labels use.
#[derive(Debug, Clone)]
pub struct TextFormat {
    pub font: FontRequest,
    pub size: Size,
    pub color: Rgba,
    pub opacity: f64,
    pub line_height: f64,
    pub buffer: Option<BufferSettings>,
    /// Qgis::Capitalization (0 = mixed case).
    pub capitalization: i64,
}

/// QgsPalLayerSettings (all settings are parsed; some serve placements not ported yet).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PalSettings {
    pub field_name: String,
    pub expression: Option<Expr>,
    pub format: TextFormat,
    pub wrap_char: String,
    pub auto_wrap_length: i64,
    pub use_max_line_length: bool,
    pub multiline_align: MultiLineAlign,
    pub placement: Placement,
    /// Qgis::LabelLinePlacementFlags.
    pub line_flags: u32,
    /// Qgis::LabelPolygonPlacementFlags.
    pub polygon_flags: u32,
    pub centroid_whole: bool,
    pub centroid_inside: bool,
    pub position_order: Vec<PointPosition>,
    pub fit_in_polygon_only: bool,
    pub dist: Size,
    /// Qgis::LabelOffsetType: 0 from point, 1 from symbol bounds.
    pub offset_type: i64,
    /// Qgis::LabelQuadrantPosition (0 AboveLeft … 4 Over … 8 BelowRight).
    pub quad_offset: i64,
    pub x_offset: f64,
    pub y_offset: f64,
    pub offset_unit: Unit,
    /// Degrees, clockwise (QgsPalLayerSettings::angleOffset).
    pub angle_offset: f64,
    pub priority: i64,
    pub repeat_distance: Size,
    pub overrun_distance: Size,
    pub line_anchor_percent: f64,
    pub max_curved_char_angle_in: f64,
    pub max_curved_char_angle_out: f64,
    pub max_distance: Size,
    pub draw_labels: bool,
    pub scale_visibility: bool,
    /// Most zoomed-out scale shown (QgsPalLayerSettings::minimumScale, `scaleMax`).
    pub minimum_scale: f64,
    /// Most zoomed-in scale shown (QgsPalLayerSettings::maximumScale, `scaleMin`).
    pub maximum_scale: f64,
    pub font_limit_pixel_size: bool,
    pub font_min_pixel_size: i64,
    pub font_max_pixel_size: i64,
    pub overlap: OverlapHandling,
    pub allow_degraded: bool,
    /// Qgis::LabelPrioritization: true = PreferCloser, false = PreferPositionOrdering.
    pub prefer_closer: bool,
    /// Qgis::UpsideDownLabelHandling (0 flip, 1 allow when rotation defined, 2 always allow).
    pub upside_down: i64,
    pub label_per_part: bool,
    pub merge_lines: bool,
    pub min_feature_size: f64,
    pub limit_num_labels: bool,
    pub max_num_labels: i64,
    pub label_margin_distance: Size,
    pub obstacle: bool,
    pub obstacle_factor: f64,
    pub obstacle_type: ObstacleType,
    pub z_index: f64,
    pub dd: HashMap<String, Expr>,
}

/// A layer's labeling (QgsVectorLayerSimpleLabeling / QgsRuleBasedLabeling, rules flattened).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct LabelingRule {
    pub filter: Option<Expr>,
    /// (minimum scale denominator, maximum scale denominator), 0 = unbounded.
    pub scale_range: Option<(f64, f64)>,
    pub settings: PalSettings,
    /// The provider id QGIS gives the rule's label provider (rule key; layer id for simple).
    pub key: String,
}

fn attr<'a>(n: Option<Node<'a, '_>>, k: &str) -> Option<&'a str> {
    n.and_then(|n| n.attribute(k))
}

fn num(n: Option<Node>, k: &str, default: f64) -> f64 {
    attr(n, k).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn int(n: Option<Node>, k: &str, default: i64) -> i64 {
    attr(n, k)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|v| v as i64)
        .unwrap_or(default)
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

fn unit(s: Option<&str>, default: Unit) -> Unit {
    match s {
        Some("MM") => Unit::Mm,
        Some("Pixel") => Unit::Pixel,
        Some("Point") | Some("Points") => Unit::Point,
        Some("Inch") => Unit::Inch,
        Some("MapUnit") => Unit::MapUnit,
        Some("RenderMetersInMapUnits") => Unit::MetersInMapUnits,
        _ => default,
    }
}

fn sized(value: f64, u: Unit) -> Size {
    let mut s = Size::new(None, None, value);
    s.unit = u;
    s
}

fn text_format(settings: Node) -> TextFormat {
    let ts = child(settings, "text-style");
    let font = FontRequest {
        family: attr(ts, "fontFamily").unwrap_or("Sans Serif").to_string(),
        weight: int(ts, "fontWeight", 400).clamp(1, 1000) as u16,
        italic: int(ts, "fontItalic", 0) != 0,
        style: attr(ts, "namedStyle").unwrap_or("").to_string(),
    };
    let size_unit = match attr(ts, "fontSizeUnit") {
        Some(u) => unit(Some(u), Unit::Point),
        None if int(ts, "fontSizeInMapUnits", 0) != 0 => Unit::MapUnit,
        None => Unit::Point,
    };
    let opacity = match attr(ts, "textOpacity") {
        Some(v) => v.parse().unwrap_or(1.0),
        None => 1.0 - int(ts, "textTransp", 0) as f64 / 100.0,
    };
    let line_height = attr(ts, "multilineHeight")
        .or_else(|| attr(child(settings, "text-format"), "multilineHeight"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let tb = ts.and_then(|t| child(t, "text-buffer")).or_else(|| child(settings, "text-buffer"));
    let buffer = (int(tb, "bufferDraw", 0) != 0).then(|| BufferSettings {
        size: sized(num(tb, "bufferSize", 0.0), unit(attr(tb, "bufferSizeUnits"), Unit::Mm)),
        color: attr(tb, "bufferColor").and_then(Rgba::parse).unwrap_or(Rgba(255, 255, 255, 255)),
        opacity: match attr(tb, "bufferOpacity") {
            Some(v) => v.parse().unwrap_or(1.0),
            None => 1.0 - int(tb, "bufferTransp", 0) as f64 / 100.0,
        },
        fill_interior: int(tb, "bufferNoFill", 0) == 0,
        join: qt_join(int(tb, "bufferJoinStyle", 0x80)),
    });
    let capitalization = match attr(ts, "capitalization") {
        Some(v) => v.parse().unwrap_or(0),
        None => int(ts, "fontCapitals", 0),
    };
    TextFormat {
        font,
        size: sized(num(ts, "fontSize", 10.0), size_unit),
        color: attr(ts, "textColor").and_then(Rgba::parse).unwrap_or(Rgba(0, 0, 0, 255)),
        opacity,
        line_height,
        buffer,
        capitalization,
    }
}

/// QgsPalLayerSettings::readXml.
pub fn parse_settings(settings: Node) -> PalSettings {
    let ts = child(settings, "text-style");
    let tf = child(settings, "text-format");
    let pl = child(settings, "placement");
    let rd = child(settings, "rendering");
    let field_name = attr(ts, "fieldName").unwrap_or("").to_string();
    let is_expression = int(ts, "isExpression", 0) != 0;
    let expression = if is_expression {
        match Expr::parse(&field_name) {
            Ok(e) => Some(e),
            Err(e) => {
                warn!("label expression '{field_name}' not supported ({e})");
                None
            }
        }
    } else {
        Some(Expr::field(&field_name))
    };
    let (overlap, allow_degraded) = match attr(pl, "overlapHandling") {
        Some("AllowOverlapIfRequired") => (OverlapHandling::AllowOverlapIfRequired, int(pl, "allowDegraded", 0) != 0),
        Some("AllowOverlapAtNoCost") => (OverlapHandling::AllowOverlapAtNoCost, int(pl, "allowDegraded", 0) != 0),
        Some(_) => (OverlapHandling::PreventOverlap, int(pl, "allowDegraded", 0) != 0),
        None if int(rd, "displayAll", 0) != 0 => (OverlapHandling::AllowOverlapIfRequired, true),
        None => (OverlapHandling::PreventOverlap, false),
    };
    let angle_offset = match attr(pl, "angleOffset") {
        Some(v) => (360.0 - v.parse::<f64>().unwrap_or(0.0)) % 360.0,
        None => num(pl, "rotationAngle", 0.0),
    };
    let dist_unit = match attr(pl, "distUnits") {
        Some(u) => unit(Some(u), Unit::Mm),
        None if int(pl, "distInMapUnits", 0) != 0 => Unit::MapUnit,
        None => Unit::Mm,
    };
    let offset_unit = match attr(pl, "offsetUnits") {
        Some(u) => unit(Some(u), Unit::Mm),
        None if int(pl, "labelOffsetInMapUnits", 1) != 0 => Unit::MapUnit,
        None => Unit::Mm,
    };
    let repeat_unit = match attr(pl, "repeatDistanceUnits") {
        Some(u) => unit(Some(u), Unit::Mm),
        None => match int(pl, "repeatDistanceUnit", 1) {
            0 => Unit::Point,
            2 => Unit::MapUnit,
            _ => Unit::Mm,
        },
    };
    let mut position_order = decode_position_order(attr(pl, "predefinedPositionOrder").unwrap_or(""));
    if position_order.is_empty() {
        position_order = default_position_order();
    }
    let multiline_align = match int(tf, "multilineAlign", 3) {
        0 => MultiLineAlign::Left,
        1 => MultiLineAlign::Center,
        2 => MultiLineAlign::Right,
        4 => MultiLineAlign::Justify,
        _ => MultiLineAlign::FollowPlacement,
    };
    let label_per_part = match attr(pl, "multipartBehavior") {
        Some(v) => v != "LabelLargestPartOnly",
        None => int(rd, "labelPerPart", 0) != 0,
    };
    PalSettings {
        field_name,
        expression,
        format: text_format(settings),
        wrap_char: attr(tf, "wrapChar").unwrap_or("").to_string(),
        auto_wrap_length: int(tf, "autoWrapLength", 0),
        use_max_line_length: int(tf, "useMaxLineLengthForAutoWrap", 1) != 0,
        multiline_align,
        placement: Placement::from_int(int(pl, "placement", 0)),
        line_flags: int(pl, "placementFlags", 0) as u32,
        polygon_flags: int(pl, "polygonPlacementFlags", 2) as u32,
        centroid_whole: int(pl, "centroidWhole", 0) != 0,
        centroid_inside: int(pl, "centroidInside", 0) != 0,
        position_order,
        fit_in_polygon_only: int(pl, "fitInPolygonOnly", 0) != 0,
        dist: sized(num(pl, "dist", 0.0), dist_unit),
        offset_type: int(pl, "offsetType", 0),
        quad_offset: int(pl, "quadOffset", 4),
        x_offset: num(pl, "xOffset", 0.0),
        y_offset: num(pl, "yOffset", 0.0),
        offset_unit,
        angle_offset,
        priority: int(pl, "priority", 0),
        repeat_distance: sized(num(pl, "repeatDistance", 0.0), repeat_unit),
        overrun_distance: sized(num(pl, "overrunDistance", 0.0), unit(attr(pl, "overrunDistanceUnit"), Unit::Mm)),
        line_anchor_percent: num(pl, "lineAnchorPercent", 0.5),
        max_curved_char_angle_in: num(pl, "maxCurvedCharAngleIn", 25.0),
        max_curved_char_angle_out: num(pl, "maxCurvedCharAngleOut", -25.0),
        max_distance: sized(num(pl, "maximumDistance", 0.0), unit(attr(pl, "maximumDistanceUnit"), Unit::Mm)),
        draw_labels: int(rd, "drawLabels", 1) != 0,
        scale_visibility: int(rd, "scaleVisibility", 0) != 0,
        maximum_scale: num(rd, "scaleMin", 0.0),
        minimum_scale: num(rd, "scaleMax", 0.0),
        font_limit_pixel_size: int(rd, "fontLimitPixelSize", 0) != 0,
        font_min_pixel_size: int(rd, "fontMinPixelSize", 0),
        font_max_pixel_size: int(rd, "fontMaxPixelSize", 10000),
        overlap,
        allow_degraded,
        prefer_closer: attr(pl, "prioritization") != Some("PreferPositionOrdering"),
        upside_down: int(rd, "upsidedownLabels", 0),
        label_per_part,
        merge_lines: int(rd, "mergeLines", 0) != 0,
        min_feature_size: num(rd, "minFeatureSize", 0.0),
        limit_num_labels: int(rd, "limitNumLabels", 0) != 0,
        max_num_labels: int(rd, "maxNumLabels", 2000),
        label_margin_distance: sized(
            num(pl, "labelMarginDistance", 0.0),
            unit(attr(pl, "labelMarginDistanceUnit"), Unit::Mm),
        ),
        obstacle: int(rd, "obstacle", 1) != 0,
        obstacle_factor: num(rd, "obstacleFactor", 1.0),
        obstacle_type: match int(rd, "obstacleType", 0) {
            1 => ObstacleType::PolygonBoundary,
            2 => ObstacleType::PolygonWhole,
            _ => ObstacleType::PolygonInterior,
        },
        z_index: num(rd, "zIndex", 0.0),
        dd: child(settings, "dd_properties").map(data_defined_in).unwrap_or_default(),
    }
}

fn collect_rules(rule: Node, parent_filter: Option<&Expr>, out: &mut Vec<LabelingRule>) {
    if rule.attribute("active") == Some("0") {
        return;
    }
    let filter = match rule.attribute("filter").filter(|f| !f.trim().is_empty()) {
        Some(f) => match Expr::parse(f) {
            Ok(e) => Some(match parent_filter {
                Some(p) => Expr::Binary("AND", Box::new(p.clone()), Box::new(e)),
                None => e,
            }),
            Err(err) => {
                warn!("label rule filter '{f}' not supported ({err})");
                return;
            }
        },
        None => parent_filter.cloned(),
    };
    if let Some(s) = child(rule, "settings") {
        let scale = |k| rule.attribute(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let scale_range = (rule.attribute("scalemindenom").is_some() || rule.attribute("scalemaxdenom").is_some())
            .then(|| (scale("scalemindenom"), scale("scalemaxdenom")));
        out.push(LabelingRule {
            filter: filter.clone(),
            scale_range,
            settings: parse_settings(s),
            key: rule.attribute("key").unwrap_or("").to_string(),
        });
    }
    for r in rule.children().filter(|c| c.has_tag_name("rule")) {
        collect_rules(r, filter.as_ref(), out);
    }
}

/// `<labeling>` of a map layer (only when the layer's `labelsEnabled` is set).
pub fn parse_labeling(maplayer: Node) -> Vec<LabelingRule> {
    if maplayer.attribute("labelsEnabled") != Some("1") {
        return Vec::new();
    }
    let Some(l) = child(maplayer, "labeling") else {
        return Vec::new();
    };
    let id = child(maplayer, "id").and_then(|n| n.text()).unwrap_or("").to_string();
    match l.attribute("type") {
        Some("simple") => child(l, "settings")
            .map(|s| vec![LabelingRule { filter: None, scale_range: None, settings: parse_settings(s), key: id }])
            .unwrap_or_default(),
        Some("rule-based") => {
            let mut out = Vec::new();
            if let Some(root) = child(l, "rules") {
                for r in root.children().filter(|c| c.has_tag_name("rule")) {
                    collect_rules(r, None, &mut out);
                }
            }
            out
        }
        Some(t) => {
            warn!("labeling type '{t}' is not supported");
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// QgsLabelingEngineSettings (project `<properties><PAL>`).
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub candidates_line_per_cm: f64,
    pub candidates_polygon_per_cm2: f64,
    pub use_all_labels: bool,
    pub partial_labels: bool,
    /// Qgis::LabelPlacementEngineVersion 2 (vs 1).
    pub version2: bool,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings {
            candidates_line_per_cm: 5.0,
            candidates_polygon_per_cm2: 2.5,
            use_all_labels: false,
            partial_labels: true,
            version2: true,
        }
    }
}

impl EngineSettings {
    /// From the project's `PAL/…` entries (`get` returns the raw entry text).
    pub fn from_project(get: impl Fn(&str) -> Option<String>) -> EngineSettings {
        let b = |k: &str, d: bool| get(k).map(|v| matches!(v.trim(), "true" | "1")).unwrap_or(d);
        let f = |k: &str, d: f64| get(k).and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        EngineSettings {
            candidates_line_per_cm: f("CandidatesLinePerCM", 5.0),
            candidates_polygon_per_cm2: f("CandidatesPolygonPerCM", 2.5),
            use_all_labels: b("ShowingAllLabels", false),
            partial_labels: b("ShowingPartialsLabels", true),
            version2: f("PlacementEngineVersion", 0.0) as i64 == 1,
        }
    }
}
