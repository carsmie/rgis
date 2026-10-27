//! Raster layers, ported step by step from QGIS's raster pipeline so the output matches
//! QGIS Server pixel for pixel:
//!
//! - QgsRasterLayerRenderer: the viewport (drawn extent, pixel-aligned size)
//! - QgsRasterIterator / QgsRasterDrawer: parts of at most 2000×2000 pixels
//! - QgsRasterProjector: approximate reprojection on a control-point grid
//! - QgsSingleBandGrayRenderer / QgsMultiBandColorRenderer + QgsContrastEnhancement
//! - QgsRasterDataProvider::block + QgsGdalProvider::readBlock: nearest-neighbour reads
//!
//! Data access goes through GDAL, the library QGIS uses (feature `raster`).

// Without the `raster` feature only the style parsing is compiled (and unused).
#![cfg_attr(not(feature = "raster"), allow(dead_code))]

use roxmltree::Node;

use crate::symbology::Rgba;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Algo {
    NoEnhancement,
    Stretch,
    StretchClip,
    Clip,
}

/// QgsContrastEnhancement with its min/max.
#[derive(Debug, Clone, Copy)]
pub struct Contrast {
    pub min: f64,
    pub max: f64,
    pub algo: Algo,
}

impl Contrast {
    /// QgsContrastEnhancementFunction::isValueInDisplayableRange.
    fn displayable(&self, v: f64) -> bool {
        match self.algo {
            Algo::StretchClip | Algo::Clip => !(v < self.min || v > self.max),
            _ => true,
        }
    }

    /// QgsContrastEnhancement::enhanceContrast (the lookup table for integer types holds
    /// the same values as the functions below).
    fn enhance(&self, v: f64, byte: bool) -> i32 {
        let stretch = || ((((v - self.min) / (self.max - self.min)) * 255.0) as i32).clamp(0, 255);
        match self.algo {
            Algo::Stretch => stretch(),
            Algo::StretchClip => {
                if v < self.min || v > self.max {
                    -1
                } else {
                    stretch()
                }
            }
            Algo::Clip => {
                if v < self.min || v > self.max {
                    -1
                } else if byte {
                    v as i32
                } else {
                    stretch()
                }
            }
            Algo::NoEnhancement => {
                if byte {
                    v as i32
                } else {
                    (((v - self.min) / (self.max - self.min)) * 255.0) as i32
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum RasterRenderer {
    Gray {
        band: i32,
        white_to_black: bool,
        contrast: Option<Contrast>,
        alpha_band: i32,
        opacity: f64,
        /// rasterTransparency singleValuePixelList: (min, max, percent transparent).
        transparency: Vec<(f64, f64, f64)>,
        nodata_color: Option<Rgba>,
    },
    MultiBand {
        bands: [i32; 3],
        alpha_band: i32,
        contrast: [Option<Contrast>; 3],
        opacity: f64,
        nodata_color: Option<Rgba>,
    },
    PseudoColor {
        band: i32,
        alpha_band: i32,
        opacity: f64,
        transparency: Vec<(f64, f64, f64)>,
        nodata_color: Option<Rgba>,
        ramp: ColorRamp,
    },
    /// QgsHillshadeRenderer (Horn slope, GDAL-DEM fast formula).
    Hillshade {
        band: i32,
        alpha_band: i32,
        opacity: f64,
        transparency: Vec<(f64, f64, f64)>,
        nodata_color: Option<Rgba>,
        /// lightAngle / lightAzimuth in degrees.
        angle: f64,
        azimuth: f64,
        z_factor: f64,
        multi_directional: bool,
    },
    Unsupported(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RampKind {
    Interpolated,
    Discrete,
    Exact,
}

/// QgsColorRampShader: items in project order (QGIS does not sort them) plus the
/// 256-entry index LUT QGIS builds on first use, here at parse time.
#[derive(Debug, Clone)]
pub struct ColorRamp {
    items: Vec<(f64, [i32; 4])>,
    kind: RampKind,
    clip: bool,
    lut: Vec<usize>,
    lut_factor: f64,
}

impl ColorRamp {
    fn new(items: Vec<(f64, [i32; 4])>, kind: RampKind, clip: bool) -> ColorRamp {
        // DOUBLE_DIFF_THRESHOLD is 0 in QGIS, so it is left out below.
        let mut lut = vec![];
        let mut lut_factor = 1.0;
        let n = items.len();
        if n >= 3 {
            let range = items[n - 2].0 - items[0].0;
            if range > 0.0 {
                lut_factor = (256.0 - 0.0000001) / range;
                let mut idx = 0;
                for i in 0..256 {
                    let v = i as f64 / lut_factor + items[0].0;
                    while idx < n && items[idx].0 < v {
                        idx += 1;
                    }
                    lut.push(idx);
                }
            }
        }
        ColorRamp {
            items,
            kind,
            clip,
            lut,
            lut_factor,
        }
    }

    /// QgsColorRampShader::shade: RGBA (not premultiplied), None = render as no-data.
    fn shade(&self, v: f64) -> Option<[i32; 4]> {
        let items = &self.items;
        let n = items.len();
        if n == 0 || !v.is_finite() {
            return None;
        }
        let offset = items[0].0;
        let lut_index = ((v - offset) * self.lut_factor) as i32;
        let mut overflow = false;
        let idx = if v <= offset {
            0
        } else if lut_index as usize >= self.lut.len() {
            if items[n - 1].0 < v {
                overflow = true;
            }
            n - 1
        } else if lut_index < 0 {
            return None;
        } else {
            let mut idx = self.lut[lut_index as usize];
            while idx < n && items[idx].0 < v {
                idx += 1;
            }
            if idx >= n {
                overflow = true;
                n - 1
            } else {
                idx
            }
        };
        let (cur_v, cur) = items[idx];
        match self.kind {
            RampKind::Interpolated => {
                if idx < 1 || overflow || cur_v <= v {
                    if self.clip && (overflow || cur_v > v) {
                        return None;
                    }
                    return Some(cur);
                }
                let (prev_v, prev) = items[idx - 1];
                // float arithmetic as in QGIS, truncated per channel
                let scale = (v - prev_v) as f32 / (cur_v - prev_v) as f32;
                Some(std::array::from_fn(|k| {
                    prev[k] + ((cur[k] - prev[k]) as f32 * scale) as i32
                }))
            }
            RampKind::Discrete => (!overflow).then_some(cur),
            RampKind::Exact => (!overflow && cur_v <= v).then_some(cur),
        }
    }
}

/// Provider-stage resampling (`<provider><resampling>` with `resamplingStage` provider):
/// GDALRIOResampleAlg codes, 0 = nearest neighbour.
#[derive(Debug, Clone, Copy)]
pub struct Resampling {
    pub zoomed_in: u32,
    pub zoomed_out: u32,
    pub max_oversampling: f64,
}

/// QgsHueSaturationFilter settings (colorize is not ported).
#[derive(Debug, Clone, Copy, Default)]
pub struct HueSaturation {
    pub saturation: i32,
    /// 0 off, 1 lightness, 2 luminosity, 3 average.
    pub grayscale: i32,
    pub invert: bool,
    /// Colorize: the colour's HSV hue and saturation (QColor::hue/saturation) and strength %.
    pub colorize: Option<(i32, i32, i32)>,
}

/// QColor::fromRgb(r, g, b).hue() / .saturation(): Qt's HSV from 16-bit channels.
fn qt_hsv_hue_saturation(r: i32, g: i32, b: i32) -> (i32, i32) {
    let [r, g, b] = [r, g, b].map(|c| (c * 0x101) as f32);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max == min {
        return (-1, 0);
    }
    let delta = max - min;
    let sat = (delta / max * 65535.0).round() as u32;
    let mut hue = if max == r {
        (g - b) / delta
    } else if max == g {
        2.0 + (b - r) / delta
    } else {
        4.0 + (r - g) / delta
    };
    hue *= 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    // qt_div_257 for the saturation
    let x = sat + 128;
    (
        (hue * 100.0).round() as i32 / 100,
        ((x - (x >> 8)) >> 8) as i32,
    )
}

/// A raster layer's pipe: provider resampling, renderer, then the image filters.
#[derive(Debug, Clone)]
pub struct RasterStyle {
    pub renderer: RasterRenderer,
    pub resampling: Option<Resampling>,
    /// QgsBrightnessContrastFilter: brightness, contrast, gamma.
    pub brightness: (i32, i32, f64),
    pub hue_saturation: HueSaturation,
    /// QgsPainting::BlendMode of the layer (`<blendMode>`).
    pub blend_mode: i32,
}

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(tag))
}

fn text<'a>(n: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    child(n, tag).and_then(|c| c.text()).map(str::trim)
}

fn parse_contrast(n: Option<Node>) -> Option<Contrast> {
    let n = n?;
    let algo = match text(n, "algorithm")? {
        "StretchToMinimumMaximum" => Algo::Stretch,
        "StretchAndClipToMinimumMaximum" => Algo::StretchClip,
        "ClipToMinimumMaximum" => Algo::Clip,
        _ => Algo::NoEnhancement,
    };
    Some(Contrast {
        min: text(n, "minValue")?.parse().ok()?,
        max: text(n, "maxValue")?.parse().ok()?,
        algo,
    })
}

/// QgsRasterTransparency singleValuePixelList: (min, max, percent transparent).
fn parse_transparency(r: Node) -> Vec<(f64, f64, f64)> {
    child(r, "rasterTransparency")
        .and_then(|t| child(t, "singleValuePixelList"))
        .map(|l| {
            l.children()
                .filter(|e| e.has_tag_name("pixelListEntry"))
                .filter_map(|e| {
                    Some((
                        e.attribute("min")?.parse().ok()?,
                        e.attribute("max")?.parse().ok()?,
                        e.attribute("percentTransparent")?.parse().ok()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// QColor::setNamedColor for `#rgb`, `#rrggbb` and `#aarrggbb`.
fn hex_color(s: &str) -> Option<[i32; 4]> {
    let h = s.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok()?;
    let c = |shift: u32| ((v >> shift) & 0xff) as i32;
    match h.len() {
        3 => {
            let d = |shift: u32| ((v >> shift) & 0xf) as i32 * 17;
            Some([d(8), d(4), d(0), 255])
        }
        6 => Some([c(16), c(8), c(0), 255]),
        8 => Some([c(16), c(8), c(0), c(24)]),
        _ => None,
    }
}

fn parse_ramp(r: Node) -> Option<ColorRamp> {
    let s = r
        .descendants()
        .find(|n| n.has_tag_name("colorrampshader"))?;
    let kind = match s.attribute("colorRampType").unwrap_or("INTERPOLATED") {
        "DISCRETE" => RampKind::Discrete,
        "EXACT" => RampKind::Exact,
        _ => RampKind::Interpolated,
    };
    let items = s
        .children()
        .filter(|n| n.has_tag_name("item"))
        .map(|n| {
            let v = n
                .attribute("value")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let mut c = n
                .attribute("color")
                .and_then(hex_color)
                .unwrap_or([0, 0, 0, 255]);
            c[3] = n
                .attribute("alpha")
                .and_then(|a| a.parse().ok())
                .unwrap_or(255);
            (v, c)
        })
        .collect();
    Some(ColorRamp::new(
        items,
        kind,
        s.attribute("clip") == Some("1"),
    ))
}

/// QgsRasterDataProvider::resamplingMethodFromString as GDALRIOResampleAlg.
fn resampling_alg(s: Option<&str>) -> u32 {
    match s.unwrap_or("") {
        "bilinear" => 1,
        "cubic" => 2,
        "cubicSpline" => 3,
        "lanczos" => 4,
        "average" => 5,
        "mode" => 6,
        "gauss" => 7,
        _ => 0,
    }
}

/// The pipe of a raster `<maplayer>`; None for layers without one.
pub fn parse_style(ml: Node) -> Option<RasterStyle> {
    let pipe = child(ml, "pipe")?;
    let attr = |n: Option<Node>, k: &str| n.and_then(|n| n.attribute(k).map(str::to_string));
    let bc = child(pipe, "brightnesscontrast");
    let hs = child(pipe, "huesaturation");
    let int = |n, k, d: i32| attr(n, k).and_then(|v| v.parse().ok()).unwrap_or(d);
    // QgsRasterPipe::setResamplingStage: the provider resamples only in the provider stage.
    let resampling = (text(pipe, "resamplingStage") == Some("provider"))
        .then(|| child(pipe, "provider").and_then(|p| child(p, "resampling")))
        .flatten()
        .map(|r| Resampling {
            zoomed_in: resampling_alg(r.attribute("zoomedInResamplingMethod")),
            zoomed_out: resampling_alg(r.attribute("zoomedOutResamplingMethod")),
            max_oversampling: r
                .attribute("maxOversampling")
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.0),
        });
    Some(RasterStyle {
        renderer: parse_renderer(pipe),
        resampling,
        brightness: (
            int(bc, "brightness", 0).clamp(-255, 255),
            int(bc, "contrast", 0).clamp(-100, 100),
            attr(bc, "gamma")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0f64)
                .clamp(0.1, 10.0),
        ),
        hue_saturation: HueSaturation {
            saturation: int(hs, "saturation", 0).clamp(-100, 100),
            grayscale: int(hs, "grayscaleMode", 0),
            invert: int(hs, "invertColors", 0) != 0,
            colorize: (int(hs, "colorizeOn", 0) != 0).then(|| {
                let (h, s) = qt_hsv_hue_saturation(
                    int(hs, "colorizeRed", 255),
                    int(hs, "colorizeGreen", 128),
                    int(hs, "colorizeBlue", 128),
                );
                (h, s, int(hs, "colorizeStrength", 100))
            }),
        },
        blend_mode: text(ml, "blendMode")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    })
}

/// The `<pipe><rasterrenderer>` of a raster layer.
fn parse_renderer(pipe: Node) -> RasterRenderer {
    let Some(r) = child(pipe, "rasterrenderer") else {
        return RasterRenderer::Unsupported("none".into());
    };
    let int = |k: &str, d: i32| r.attribute(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let opacity = r
        .attribute("opacity")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let nodata_color = r
        .attribute("nodataColor")
        .filter(|c| !c.is_empty())
        .and_then(Rgba::parse);
    match r.attribute("type").unwrap_or("") {
        "singlebandgray" => RasterRenderer::Gray {
            band: int("grayBand", 1),
            white_to_black: r.attribute("gradient") == Some("WhiteToBlack"),
            contrast: parse_contrast(child(r, "contrastEnhancement")),
            alpha_band: int("alphaBand", -1),
            opacity,
            transparency: parse_transparency(r),
            nodata_color,
        },
        "singlebandpseudocolor" => match parse_ramp(r) {
            Some(ramp) => RasterRenderer::PseudoColor {
                band: int("band", -1),
                alpha_band: int("alphaBand", -1),
                opacity,
                transparency: parse_transparency(r),
                nodata_color,
                ramp,
            },
            None => RasterRenderer::Unsupported("singlebandpseudocolor without shader".into()),
        },
        "multibandcolor" => RasterRenderer::MultiBand {
            bands: [
                int("redBand", -1),
                int("greenBand", -1),
                int("blueBand", -1),
            ],
            alpha_band: int("alphaBand", -1),
            contrast: [
                parse_contrast(child(r, "redContrastEnhancement")),
                parse_contrast(child(r, "greenContrastEnhancement")),
                parse_contrast(child(r, "blueContrastEnhancement")),
            ],
            opacity,
            nodata_color,
        },
        "hillshade" => {
            let num = |k: &str, d: f64| r.attribute(k).and_then(|v| v.parse().ok()).unwrap_or(d);
            RasterRenderer::Hillshade {
                band: int("band", 1),
                alpha_band: int("alphaBand", -1),
                opacity,
                transparency: parse_transparency(r),
                nodata_color,
                angle: num("angle", 45.0),
                azimuth: num("azimuth", 315.0),
                z_factor: num("zfactor", 1.0),
                multi_directional: int("multidirection", 0) != 0,
            }
        }
        other => RasterRenderer::Unsupported(other.to_string()),
    }
}

#[cfg(feature = "raster")]
pub use imp::{RasterSource, render};

#[cfg(feature = "raster")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Once};

    use anyhow::{Result, anyhow, bail};
    use gdal_sys as g;
    use parking_lot::Mutex;
    use tiny_skia::{Pixmap, PixmapPaint, PremultipliedColorU8, Transform as SkTransform};
    use tracing::{debug, warn};

    use super::{Contrast, HueSaturation, RasterRenderer, RasterStyle, Resampling};
    use crate::crs::{Crs, Transform};
    use crate::geom::Rect;
    use crate::render::{MapSettings, OGC_DPI};

    const GDAL_OF_RASTER: u32 = 0x02;
    const GMF_ALL_VALID: i32 = 0x01;
    /// QgsRasterIterator::DEFAULT_MAXIMUM_TILE_WIDTH / HEIGHT.
    const MAX_TILE: usize = 2000;

    struct Handle(g::GDALDatasetH);
    // GDAL datasets may be used from any thread as long as access is serialised.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { g::GDALClose(self.0) };
        }
    }

    struct Band {
        byte: bool,
        nodata: Option<f64>,
        dtype: g::GDALDataType::Type,
        /// Bytes per value of `dtype`.
        size: usize,
        color_table: bool,
        /// Overview widths, for QgsGdalProvider::canDoResampling.
        overviews: Vec<usize>,
    }

    /// An opened GDAL raster (QgsGdalProvider), north-up (via a warped VRT if needed).
    pub struct RasterSource {
        /// GDAL name the handles are opened from.
        name: String,
        /// Free dataset handles (base, warped VRT). A GDAL dataset must not be read from two
        /// threads at once, and one shared handle made concurrent requests for the same
        /// raster queue behind each other; a handle per concurrent reader instead.
        pool: Mutex<Vec<(Handle, Option<Handle>)>>,
        width: usize,
        height: usize,
        gt: [f64; 6],
        extent: Rect,
        bands: Vec<Band>,
    }

    static SOURCES: LazyLock<Mutex<HashMap<String, Arc<RasterSource>>>> =
        LazyLock::new(Default::default);

    /// Resolve a QGIS gdal datasource (path, `GPKG:path:table`, options after `|`).
    fn gdal_name(datasource: &str, base_dir: &Path) -> String {
        let ds = datasource.split('|').next().unwrap_or("");
        let resolve = |p: &str| -> String {
            let pb = PathBuf::from(p);
            if pb.is_relative() {
                base_dir.join(pb).display().to_string()
            } else {
                p.to_string()
            }
        };
        if let Some(rest) = ds.strip_prefix("GPKG:")
            && let Some((p, table)) = rest.rsplit_once(':')
        {
            return format!("GPKG:{}:{table}", resolve(p));
        }
        if ds.starts_with("/vsi") || ds.contains("://") {
            ds.to_string()
        } else {
            resolve(ds)
        }
    }

    /// GDALOpenEx plus QgsGdalProvider::initBaseDataset's north-up warped VRT when needed.
    fn open_pair(name: &str) -> Result<(Handle, Option<Handle>)> {
        let c = CString::new(name)?;
        let base = unsafe {
            g::GDALOpenEx(
                c.as_ptr(),
                GDAL_OF_RASTER,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if base.is_null() {
            bail!("GDAL cannot open raster '{name}'");
        }
        let base = Handle(base);
        let mut gt = [0f64; 6];
        let has_gt =
            unsafe { g::GDALGetGeoTransform(base.0, gt.as_mut_ptr()) } == g::CPLErr::CE_None;
        // QgsGdalProvider::initBaseDataset: rotated / south-up / GCP / RPC rasters
        // are read through a north-up warped VRT.
        let needs_warp = unsafe {
            (has_gt && (gt[1] < 0.0 || gt[2] != 0.0 || gt[4] != 0.0 || gt[5] > 0.0))
                || g::GDALGetGCPCount(base.0) > 0
                || !g::GDALGetMetadata(base.0 as g::GDALMajorObjectH, c"RPC".as_ptr()).is_null()
                || !g::GDALGetMetadata(base.0 as g::GDALMajorObjectH, c"GEOLOCATION".as_ptr())
                    .is_null()
        };
        let mut warped = None;
        if needs_warp {
            unsafe {
                let opts = g::GDALCreateWarpOptions();
                let south_up_plain = has_gt
                    && gt[1] > 0.0
                    && gt[2] == 0.0
                    && gt[4] == 0.0
                    && gt[5] > 0.0
                    && g::GDALGetGCPCount(base.0) == 0;
                if !south_up_plain
                    && g::GDALGetMaskFlags(g::GDALGetRasterBand(base.0, 1)) == GMF_ALL_VALID
                {
                    (*opts).nDstAlphaBand = g::GDALGetRasterCount(base.0) + 1;
                }
                let vrt = g::GDALAutoCreateWarpedVRT(
                    base.0,
                    std::ptr::null(),
                    std::ptr::null(),
                    g::GDALResampleAlg::GRA_NearestNeighbour,
                    0.2,
                    opts,
                );
                g::GDALDestroyWarpOptions(opts);
                if vrt.is_null() {
                    warn!("warped VRT creation failed for {name}");
                } else {
                    warped = Some(Handle(vrt));
                }
            }
        }
        Ok((base, warped))
    }

    impl RasterSource {
        pub fn open(datasource: &str, base_dir: &Path) -> Result<Arc<RasterSource>> {
            let name = gdal_name(datasource, base_dir);
            if let Some(s) = SOURCES.lock().get(&name) {
                return Ok(s.clone());
            }
            static INIT: Once = Once::new();
            INIT.call_once(|| unsafe { g::GDALAllRegister() });
            let (base, warped) = open_pair(&name)?;
            let mut gt = [0f64; 6];
            let ds = warped.as_ref().map(|h| h.0).unwrap_or(base.0);
            let (width, height, count) = unsafe {
                (
                    g::GDALGetRasterXSize(ds) as usize,
                    g::GDALGetRasterYSize(ds) as usize,
                    g::GDALGetRasterCount(ds),
                )
            };
            unsafe { g::GDALGetGeoTransform(ds, gt.as_mut_ptr()) };
            let mut bands = vec![];
            for i in 1..=count {
                let b = unsafe { g::GDALGetRasterBand(ds, i) };
                let mut ok = 0;
                let nd = unsafe { g::GDALGetRasterNoDataValue(b, &mut ok) };
                let dtype = unsafe { g::GDALGetRasterDataType(b) };
                // 1 = GDT_Byte, renamed GDT_UInt8 in GDAL 3.13; numeric so both GDALs build.
                let byte = dtype == 1;
                let overviews = (0..unsafe { g::GDALGetOverviewCount(b) })
                    .map(
                        |i| unsafe { g::GDALGetRasterBandXSize(g::GDALGetOverview(b, i)) } as usize,
                    )
                    .collect();
                bands.push(Band {
                    byte,
                    nodata: (ok != 0).then_some(nd),
                    dtype,
                    size: unsafe { g::GDALGetDataTypeSizeBytes(dtype) } as usize,
                    color_table: unsafe { !g::GDALGetRasterColorTable(b).is_null() },
                    overviews,
                });
            }
            let extent = Rect::new(
                gt[0],
                gt[3] + gt[5] * height as f64,
                gt[0] + gt[1] * width as f64,
                gt[3],
            );
            debug!("raster {name}: {width}x{height}, {count} band(s), extent {extent:?}");
            let src = Arc::new(RasterSource {
                name: name.clone(),
                pool: Mutex::new(vec![(base, warped)]),
                width,
                height,
                gt,
                extent,
                bands,
            });
            SOURCES.lock().insert(name, src.clone());
            Ok(src)
        }

        /// Run `f` on a dataset handle no other thread is using (taken from the pool, opened
        /// if all are busy) and return it to the pool.
        fn with_ds<R>(&self, f: impl FnOnce(g::GDALDatasetH) -> R) -> Result<R> {
            let pair = match self.pool.lock().pop() {
                Some(p) => p,
                None => open_pair(&self.name)?,
            };
            let r = f(pair.1.as_ref().map(|h| h.0).unwrap_or(pair.0.0));
            self.pool.lock().push(pair);
            Ok(r)
        }

        /// GDALRasterIO of a window into a `bw`×`bh` buffer (nearest neighbour, may use overviews).
        #[allow(clippy::too_many_arguments)] // mirrors GDALRasterIO's window + buffer arguments
        fn read(
            &self,
            band: i32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            bw: usize,
            bh: usize,
        ) -> Result<Vec<f64>> {
            let mut buf = vec![0f64; bw * bh];
            let err = self.with_ds(|ds| unsafe {
                let b = g::GDALGetRasterBand(ds, band);
                g::GDALRasterIO(
                    b,
                    g::GDALRWFlag::GF_Read,
                    x,
                    y,
                    w,
                    h,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    bw as i32,
                    bh as i32,
                    g::GDALDataType::GDT_Float64,
                    0,
                    0,
                )
            })?;
            if err != g::CPLErr::CE_None {
                bail!("GDALRasterIO failed");
            }
            Ok(buf)
        }

        /// GDALRasterIOEx of a window into `buf` in the band's own data type, as
        /// QgsGdalProvider::readBlock reads (resampled values are rounded to that type).
        /// `buf` holds `bh` rows of `line` values.
        #[allow(clippy::too_many_arguments)] // mirrors GDALRasterIOEx
        fn read_native(
            &self,
            band: i32,
            (x, y, w, h): (i32, i32, i32, i32),
            buf: &mut [u8],
            (bw, bh): (i32, i32),
            line: usize,
            extra: Option<&mut g::GDALRasterIOExtraArg>,
        ) -> Result<()> {
            let info = self.band(band)?;
            let need = ((bh.max(1) as usize - 1) * line + bw as usize) * info.size;
            if buf.len() < need {
                bail!("raster buffer too small");
            }
            let err = self.with_ds(|ds| unsafe {
                g::GDALRasterIOEx(
                    g::GDALGetRasterBand(ds, band),
                    g::GDALRWFlag::GF_Read,
                    x,
                    y,
                    w,
                    h,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    bw,
                    bh,
                    info.dtype,
                    info.size as g::GSpacing,
                    (info.size * line) as g::GSpacing,
                    extra.map_or(std::ptr::null_mut(), |e| e as *mut _),
                )
            })?;
            if err != g::CPLErr::CE_None {
                bail!("GDALRasterIOEx failed");
            }
            Ok(())
        }

        fn band(&self, band: i32) -> Result<&Band> {
            self.bands
                .get((band - 1) as usize)
                .ok_or_else(|| anyhow!("raster has no band {band}"))
        }
    }

    /// A band block: values plus a no-data mask (QgsRasterBlock).
    struct Block {
        data: Vec<f64>,
        nodata: Vec<bool>,
    }

    /// QgsRasterBlock::subRect: pixel rows/cols of `sub` inside a `w`×`h` grid on `ext`.
    fn sub_rect(ext: &Rect, w: usize, h: usize, sub: &Rect) -> (i64, i64, i64, i64) {
        let (xres, yres) = (ext.width() / w as f64, ext.height() / h as f64);
        let mut top = 0i64;
        let mut bottom = h as i64 - 1;
        let mut left = 0i64;
        let mut right = w as i64 - 1;
        if sub.ymax < ext.ymax {
            top = ((ext.ymax - sub.ymax) / yres).round() as i64;
        }
        if sub.ymin > ext.ymin {
            bottom = ((ext.ymax - sub.ymin) / yres).round() as i64 - 1;
        }
        if sub.xmin > ext.xmin {
            left = ((sub.xmin - ext.xmin) / xres).round() as i64;
        }
        if sub.xmax < ext.xmax {
            right = ((sub.xmax - ext.xmin) / xres).round() as i64 - 1;
        }
        (left, top, right, bottom)
    }

    fn intersect(a: &Rect, b: &Rect) -> Rect {
        Rect::new(
            a.xmin.max(b.xmin),
            a.ymin.max(b.ymin),
            a.xmax.min(b.xmax),
            a.ymax.min(b.ymax),
        )
    }

    /// QgsGdalProvider::readBlock, nearest-neighbour path: read the source window at about
    /// the target resolution, then pick the nearest pixel per target cell centre.
    /// Writes into `out` (a `w`×`h` grid on `req`); pixels outside the raster are untouched.
    fn read_block(
        src: &RasterSource,
        band: i32,
        req: &Rect,
        w: usize,
        h: usize,
        out: &mut [f64],
        written: &mut [bool],
    ) -> Result<()> {
        let inter = intersect(req, &src.extent);
        if inter.is_empty() || inter.width() <= 0.0 || inter.height() <= 0.0 {
            return Ok(());
        }
        let (req_xres, req_yres) = (req.width() / w as f64, req.height() / h as f64);
        let (src_xres, src_yres) = (src.gt[1], src.gt[5]);
        let (tgt_left, tgt_top, tgt_right, tgt_bottom) = sub_rect(req, w, h, &inter);
        let e = &src.extent;
        let mut src_left = 0i64;
        let mut src_top = 0i64;
        let mut src_bottom = src.height as i64 - 1;
        let mut src_right = src.width as i64 - 1;
        if e.xmin < inter.xmin {
            src_left = ((inter.xmin - e.xmin) / src_xres).floor() as i64;
        }
        if e.xmax > inter.xmax {
            src_right =
                (src.width as i64 - 1).min(((inter.xmax - e.xmin) / src_xres).floor() as i64);
        }
        if e.ymax > inter.ymax {
            src_top = (-(e.ymax - inter.ymax) / src_yres).floor() as i64;
        }
        if e.ymin < inter.ymin {
            src_bottom =
                (src.height as i64 - 1).min((-(e.ymax - inter.ymin) / src_yres).floor() as i64);
        }
        let (src_w, src_h) = (src_right - src_left + 1, src_bottom - src_top + 1);
        let (tgt_w, tgt_h) = (tgt_right - tgt_left + 1, tgt_bottom - tgt_top + 1);
        if src_w <= 0 || src_h <= 0 || tgt_w <= 0 || tgt_h <= 0 {
            return Ok(());
        }
        let mut tmp_w = src_w;
        let mut tmp_h = src_h;
        if req_xres > src_xres {
            tmp_w = ((src_w as f64 * src_xres / req_xres).round() as i64).max(1);
        }
        if req_yres > src_yres.abs() {
            tmp_h = ((-(src_h as f64) * src_yres / req_yres).round() as i64).max(1);
        }
        let tmp_xmin = e.xmin + src_left as f64 * src_xres;
        let tmp_ymax = e.ymax + src_top as f64 * src_yres;
        let tmp = src.read(
            band,
            src_left as i32,
            src_top as i32,
            src_w as i32,
            src_h as i32,
            tmp_w as usize,
            tmp_h as usize,
        )?;
        let tmp_xres = src_w as f64 * src_xres / tmp_w as f64;
        let tmp_yres = src_h as f64 * src_yres / tmp_h as f64; // negative
        let mut y = inter.ymax - 0.5 * req_yres;
        for row in 0..tgt_h {
            let tmp_row = ((-(tmp_ymax - y) / tmp_yres).floor() as i64)
                .min(tmp_h - 1)
                .max(0);
            let mut x = (inter.xmin + 0.5 * req_xres - tmp_xmin) / tmp_xres;
            let inc = req_xres / tmp_xres;
            // copyNearestNeighborRow: truncation, only ever stepping forward.
            let mut last_col = 0i64;
            for col in 0..tgt_w {
                let tmp_col = (x as i64).min(tmp_w - 1);
                if tmp_col > last_col {
                    last_col = tmp_col;
                }
                let idx = ((tgt_top + row) as usize) * w + (tgt_left + col) as usize;
                if idx < out.len() {
                    out[idx] = tmp[tmp_row as usize * tmp_w as usize + last_col as usize];
                    written[idx] = true;
                }
                x += inc;
            }
            y -= req_yres;
        }
        Ok(())
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 4.0 * f64::EPSILON * a.abs().max(b.abs()).max(1.0)
    }

    /// QgsRasterDataProvider::block: reads at the coarser of request and source resolution
    /// and replicates pixels; no-data where outside the raster or equal to the band's no-data.
    fn provider_block(
        src: &RasterSource,
        band: i32,
        bbox: &Rect,
        w: usize,
        h: usize,
    ) -> Result<Block> {
        let info = src.band(band)?;
        let mut data = vec![0f64; w * h];
        let mut written = vec![false; w * h];
        if !bbox.is_empty() {
            let (xres, yres) = (bbox.width() / w as f64, bbox.height() / h as f64);
            let (pxres, pyres) = (
                src.extent.width() / src.width as f64,
                src.extent.height() / src.height as f64,
            );
            let mut tmp_xres = pxres.max(xres);
            let mut tmp_yres = pyres.max(yres);
            if near(tmp_xres, xres) {
                tmp_xres = xres;
            }
            if near(tmp_yres, yres) {
                tmp_yres = yres;
            }
            if tmp_xres > xres || tmp_yres > yres {
                // Read a lower-resolution block aligned to the source grid, then replicate.
                let mut tmp_ext = *bbox;
                if tmp_xres > xres {
                    let col = ((tmp_ext.xmin - src.extent.xmin) / pxres).floor();
                    tmp_ext.xmin = src.extent.xmin + col * pxres;
                    let col = ((tmp_ext.xmax - src.extent.xmin) / pxres).ceil();
                    tmp_ext.xmax = src.extent.xmin + col * pxres;
                }
                if tmp_yres > yres {
                    let row = ((src.extent.ymax - tmp_ext.ymax) / pyres).floor();
                    tmp_ext.ymax = src.extent.ymax - row * pyres;
                    let row = ((src.extent.ymax - tmp_ext.ymin) / pyres).ceil();
                    tmp_ext.ymin = src.extent.ymax - row * pyres;
                }
                let tw = ((tmp_ext.width() / tmp_xres).round() as usize).max(1);
                let th = ((tmp_ext.height() / tmp_yres).round() as usize).max(1);
                let (txres, tyres) = (tmp_ext.width() / tw as f64, tmp_ext.height() / th as f64);
                let mut tmp = vec![0f64; tw * th];
                let mut tmp_written = vec![false; tw * th];
                read_block(src, band, &tmp_ext, tw, th, &mut tmp, &mut tmp_written)?;
                for row in 0..h {
                    let y = bbox.ymax - (row as f64 + 0.5) * yres;
                    let trow = ((tmp_ext.ymax - y) / tyres).floor() as i64;
                    for col in 0..w {
                        let x = bbox.xmin + (col as f64 + 0.5) * xres;
                        let tcol = ((x - tmp_ext.xmin) / txres).floor() as i64;
                        if trow < 0 || trow >= th as i64 || tcol < 0 || tcol >= tw as i64 {
                            continue;
                        }
                        let ti = trow as usize * tw + tcol as usize;
                        data[row * w + col] = tmp[ti];
                        written[row * w + col] = tmp_written[ti];
                    }
                }
            } else {
                read_block(src, band, bbox, w, h, &mut data, &mut written)?;
            }
        }
        let nodata = data
            .iter()
            .zip(&written)
            .map(|(v, ok)| {
                !ok || info
                    .nodata
                    .is_some_and(|nd| *v == nd || (nd.is_nan() && v.is_nan()))
            })
            .collect();
        Ok(Block { data, nodata })
    }

    /// INIT_RASTERIO_EXTRA_ARG with a resampling algorithm.
    fn extra_arg(alg: u32) -> g::GDALRasterIOExtraArg {
        // SAFETY: plain C struct; null progress callback/data are valid.
        let mut e: g::GDALRasterIOExtraArg = unsafe { std::mem::zeroed() };
        e.nVersion = 1;
        e.eResampleAlg = alg as g::GDALRIOResampleAlg::Type;
        e
    }

    /// QgsGdalProvider::canDoResampling.
    fn can_do_resampling(
        src: &RasterSource,
        info: &Band,
        rs: &Resampling,
        req: &Rect,
        w: usize,
        h: usize,
    ) -> bool {
        if info.color_table {
            return false;
        }
        let factor =
            (req.width() / w as f64 / src.gt[1]).max(req.height() / h as f64 / src.gt[5].abs());
        if factor < 1.0 {
            return rs.zoomed_in != 0;
        }
        if factor < 1.1 {
            return rs.zoomed_in != 0 || rs.zoomed_out != 0;
        }
        if rs.zoomed_out == 0 {
            return false;
        }
        let limit = rs.max_oversampling + 0.1;
        factor <= limit
            || info
                .overviews
                .iter()
                .any(|ow| factor <= limit * (src.width as f64 / *ow as f64))
    }

    /// QgsGdalProvider::block with provider resampling: readBlock's GDAL-resampled read
    /// (floating-point source window, 1-pixel edge reads where the request overhangs the
    /// raster), or its two-step fallback when zoomed out beyond maxOversampling.
    /// None when readBlock would take its nearest-neighbour path.
    fn resampled_block(
        src: &RasterSource,
        band: i32,
        rs: &Resampling,
        req: &Rect,
        w: usize,
        h: usize,
    ) -> Result<Option<Block>> {
        let info = src.band(band)?;
        let n = w * h;
        let (req_xres, req_yres) = (req.width() / w as f64, req.height() / h as f64);
        let (src_xres, src_yres) = (src.gt[1], src.gt[5]); // src_yres negative
        // As in readBlock: the y term is negative, so this is the x ratio.
        let factor = (req_xres / src_xres).max(req_yres / src_yres);
        let can = can_do_resampling(src, info, rs, req, w, h);
        if !can && !(rs.zoomed_out != 0 && factor > 1.0) {
            return Ok(None);
        }
        let e = &src.extent;
        let inter = intersect(req, e);
        if inter.is_empty() || inter.width() <= 0.0 || inter.height() <= 0.0 {
            return Ok(Some(Block {
                data: vec![0.0; n],
                nodata: vec![true; n],
            }));
        }
        let mut nodata = vec![false; n];
        if !(e.xmin <= req.xmin && req.xmax <= e.xmax && e.ymin <= req.ymin && req.ymax <= e.ymax) {
            // QgsRasterBlock::setIsNoDataExcept
            let (l, t, r, b) = sub_rect(req, w, h, e);
            for row in 0..h as i64 {
                for col in 0..w as i64 {
                    if row < t || row > b || col < l || col > r {
                        nodata[row as usize * w + col as usize] = true;
                    }
                }
            }
        }
        let (tgt_left, tgt_top, tgt_right, tgt_bottom) = sub_rect(req, w, h, &inter);
        let mut src_left = 0i64;
        let mut src_top = 0i64;
        let mut src_bottom = src.height as i64 - 1;
        let mut src_right = src.width as i64 - 1;
        if e.xmin < inter.xmin {
            src_left = ((inter.xmin - e.xmin) / src_xres).floor() as i64;
        }
        if e.xmax > inter.xmax {
            src_right =
                (src.width as i64 - 1).min(((inter.xmax - e.xmin) / src_xres).floor() as i64);
        }
        if e.ymax > inter.ymax {
            src_top = (-(e.ymax - inter.ymax) / src_yres).floor() as i64;
        }
        if e.ymin < inter.ymin {
            src_bottom =
                (src.height as i64 - 1).min((-(e.ymax - inter.ymin) / src_yres).floor() as i64);
        }
        let (src_w, src_h) = (
            (src_right - src_left + 1) as i32,
            (src_bottom - src_top + 1) as i32,
        );
        let (src_l, src_t) = (src_left as i32, src_top as i32);
        let ds = info.size;
        let mut buf = vec![0u8; n * ds];
        let at = |row: i64, col: i64| (row as usize * w + col as usize) * ds;
        let read = if can {
            let rows = (tgt_bottom - tgt_top + 1) as i32;
            let cols = (tgt_right - tgt_left + 1) as i32;
            let (mut tl, mut tt, mut tr, mut tb) = (tgt_left, tgt_top, tgt_right, tgt_bottom);
            // GDAL rejects windows outside the raster: overhanging edges read 1 pixel.
            let mut edges = || -> Result<()> {
                if req.xmin + tl as f64 * req_xres < e.xmin {
                    src.read_native(
                        band,
                        (0, src_t, 1, src_h),
                        &mut buf[at(tgt_top, tgt_left)..],
                        (1, rows),
                        w,
                        None,
                    )?;
                    tl += 1;
                }
                if req.ymax - tt as f64 * req_yres > e.ymax {
                    src.read_native(
                        band,
                        (src_l, 0, src_w, 1),
                        &mut buf[at(tgt_top, tgt_left)..],
                        (cols, 1),
                        w,
                        None,
                    )?;
                    tt += 1;
                }
                if req.xmin + (tr + 1) as f64 * req_xres > e.xmax {
                    let x = src.width as i32 - 1;
                    src.read_native(
                        band,
                        (x, src_t, 1, src_h),
                        &mut buf[at(tgt_top, tgt_right)..],
                        (1, rows),
                        w,
                        None,
                    )?;
                    tr -= 1;
                }
                if req.ymax - (tb + 1) as f64 * req_yres < e.ymin {
                    let y = src.height as i32 - 1;
                    src.read_native(
                        band,
                        (src_l, y, src_w, 1),
                        &mut buf[at(tgt_bottom, tgt_left)..],
                        (cols, 1),
                        w,
                        None,
                    )?;
                    tb -= 1;
                }
                let (tw, th) = (tr - tl + 1, tb - tt + 1);
                if tw > 0 && th > 0 {
                    // near nominal resolution the zoomed-out method wins unless nearest
                    let method = if factor < 1.0 || (factor < 1.1 && rs.zoomed_out == 0) {
                        rs.zoomed_in
                    } else {
                        rs.zoomed_out
                    };
                    let mut x = extra_arg(method);
                    x.bFloatingPointWindowValidity = 1;
                    x.dfXOff = (req.xmin + tl as f64 * req_xres - e.xmin) / src_xres;
                    x.dfYOff = (e.ymax - (req.ymax - tt as f64 * req_yres)) / -src_yres;
                    x.dfXSize = tw as f64 * req_xres / src_xres;
                    x.dfYSize = th as f64 * req_yres / -src_yres;
                    let win = (
                        x.dfXOff.floor() as i32,
                        x.dfYOff.floor() as i32,
                        (x.dfXSize.floor() as i32).max(1),
                        (x.dfYSize.floor() as i32).max(1),
                    );
                    src.read_native(
                        band,
                        win,
                        &mut buf[at(tt, tl)..],
                        (tw as i32, th as i32),
                        w,
                        Some(&mut x),
                    )?;
                }
                Ok(())
            };
            edges()
        } else {
            // Nearest read at maxOversampling × the target size, then the zoomed-out
            // resampler down to the target size through a MEM dataset.
            let tw = (tgt_right - tgt_left + 1) as usize;
            let th = (tgt_bottom - tgt_top + 1) as usize;
            let tmp_w = (tw as f64 * rs.max_oversampling + 0.5) as usize;
            let tmp_h = (th as f64 * rs.max_oversampling + 0.5) as usize;
            let mut tmp = vec![0u8; tmp_w * tmp_h * ds];
            src.read_native(
                band,
                (src_l, src_t, src_w, src_h),
                &mut tmp,
                (tmp_w as i32, tmp_h as i32),
                tmp_w,
                None,
            )
            .and_then(|_| {
                let opts: Vec<CString> = [
                    format!("PIXELOFFSET={ds}"),
                    format!("LINEOFFSET={}", ds * tmp_w),
                    format!("DATAPOINTER={}", tmp.as_mut_ptr() as usize),
                ]
                .into_iter()
                .map(|o| CString::new(o).expect("no NUL"))
                .collect();
                let mut ptrs: Vec<*const std::ffi::c_char> =
                    opts.iter().map(|o| o.as_ptr()).collect();
                ptrs.push(std::ptr::null());
                let mut x = extra_arg(rs.zoomed_out);
                // SAFETY: the MEM band aliases `tmp`, which outlives the dataset.
                let err = unsafe {
                    let mem = Handle(g::GDALCreate(
                        g::GDALGetDriverByName(c"MEM".as_ptr()),
                        c"".as_ptr(),
                        tmp_w as i32,
                        tmp_h as i32,
                        0,
                        info.dtype,
                        std::ptr::null_mut(),
                    ));
                    if mem.0.is_null() {
                        bail!("cannot create MEM dataset");
                    }
                    g::GDALAddBand(mem.0, info.dtype, ptrs.as_ptr() as _);
                    g::GDALRasterIOEx(
                        g::GDALGetRasterBand(mem.0, 1),
                        g::GDALRWFlag::GF_Read,
                        0,
                        0,
                        tmp_w as i32,
                        tmp_h as i32,
                        buf[at(tgt_top, tgt_left)..].as_mut_ptr() as *mut std::ffi::c_void,
                        tw as i32,
                        th as i32,
                        info.dtype,
                        ds as g::GSpacing,
                        (ds * w) as g::GSpacing,
                        &mut x,
                    )
                };
                if err != g::CPLErr::CE_None {
                    bail!("GDALRasterIOEx failed");
                }
                Ok(())
            })
        };
        if let Err(err) = read {
            // QgsGdalProvider::block: a failed read leaves the whole block no-data.
            debug!("raster read failed: {err:#}");
            return Ok(Some(Block {
                data: vec![0.0; n],
                nodata: vec![true; n],
            }));
        }
        let mut data = vec![0f64; n];
        // SAFETY: `buf` holds n values of info.dtype, `data` n doubles.
        unsafe {
            g::GDALCopyWords(
                buf.as_ptr() as *const std::ffi::c_void,
                info.dtype,
                ds as i32,
                data.as_mut_ptr() as *mut std::ffi::c_void,
                g::GDALDataType::GDT_Float64,
                8,
                n as i32,
            )
        };
        if let Some(nd) = info.nodata {
            for (v, no) in data.iter().zip(nodata.iter_mut()) {
                if *v == nd || (nd.is_nan() && v.is_nan()) {
                    *no = true;
                }
            }
        }
        Ok(Some(Block { data, nodata }))
    }

    /// A band block as the renderer requests it from the provider.
    fn input_block(
        src: &RasterSource,
        rs: Option<&Resampling>,
        band: i32,
        bbox: &Rect,
        w: usize,
        h: usize,
    ) -> Result<Block> {
        if let Some(rs) = rs
            && let Some(b) = resampled_block(src, band, rs, bbox, w, h)?
        {
            return Ok(b);
        }
        provider_block(src, band, bbox, w, h)
    }

    /// Premultiplied RGBA pixels (QgsRasterBlock of type ARGB32_Premultiplied).
    type Image = Vec<PremultipliedColorU8>;

    fn rgba(r: i32, g: i32, b: i32, a: i32) -> PremultipliedColorU8 {
        // qRgba masks each channel to 8 bits.
        let (r, g, b, a) = (
            (r & 0xff) as u8,
            (g & 0xff) as u8,
            (b & 0xff) as u8,
            (a & 0xff) as u8,
        );
        PremultipliedColorU8::from_rgba(r.min(a), g.min(a), b.min(a), a).expect("premultiplied")
    }

    fn nodata_pixel(c: Option<crate::symbology::Rgba>) -> PremultipliedColorU8 {
        match c {
            Some(c) => tiny_skia::ColorU8::from_rgba(c.0, c.1, c.2, c.3).premultiply(),
            None => PremultipliedColorU8::TRANSPARENT,
        }
    }

    /// QgsSingleBandGrayRenderer::block / QgsMultiBandColorRenderer::block.
    // Per-pixel loops over several parallel buffers, kept close to QGIS's C++ for review.
    #[allow(clippy::needless_range_loop)]
    fn renderer_block(
        r: &RasterRenderer,
        rs: Option<&Resampling>,
        src: &RasterSource,
        bbox: &Rect,
        w: usize,
        h: usize,
    ) -> Result<Image> {
        let n = w * h;
        match r {
            RasterRenderer::Gray {
                band,
                white_to_black,
                contrast,
                alpha_band,
                opacity,
                transparency,
                nodata_color,
            } => {
                let input = input_block(src, rs, *band, bbox, w, h)?;
                let byte = src.band(*band)?.byte;
                let alpha = if *alpha_band > 0 && alpha_band != band {
                    Some(input_block(src, rs, *alpha_band, bbox, w, h)?)
                } else {
                    None
                };
                let nd = nodata_pixel(*nodata_color);
                let mut out = vec![PremultipliedColorU8::TRANSPARENT; n];
                for i in 0..n {
                    if input.nodata[i] {
                        out[i] = nd;
                        continue;
                    }
                    let mut gray = input.data[i];
                    let mut a = *opacity;
                    if let Some(&(_, _, pct)) = transparency
                        .iter()
                        .find(|(mn, mx, _)| gray >= *mn && gray <= *mx)
                    {
                        a *= 1.0 - pct / 100.0;
                    }
                    if *alpha_band > 0 {
                        let av = alpha.as_ref().map(|b| b.data[i]).unwrap_or(gray);
                        if av == 0.0 {
                            out[i] = nd;
                            continue;
                        }
                        a *= av / 255.0;
                    }
                    if let Some(c) = contrast {
                        if !c.displayable(gray) {
                            out[i] = nd;
                            continue;
                        }
                        gray = c.enhance(gray, byte) as f64;
                    }
                    if *white_to_black {
                        gray = 255.0 - gray;
                    }
                    out[i] = if (a - 1.0).abs() < 1e-9 {
                        rgba(gray as i32, gray as i32, gray as i32, 255)
                    } else {
                        rgba(
                            (a * gray) as i32,
                            (a * gray) as i32,
                            (a * gray) as i32,
                            (a * 255.0) as i32,
                        )
                    };
                }
                Ok(out)
            }
            RasterRenderer::MultiBand {
                bands,
                alpha_band,
                contrast,
                opacity,
                nodata_color,
            } => {
                let mut blocks: Vec<Option<Block>> = vec![];
                for b in bands {
                    blocks.push(if *b > 0 {
                        Some(input_block(src, rs, *b, bbox, w, h)?)
                    } else {
                        None
                    });
                }
                let alpha = if *alpha_band > 0 {
                    Some(input_block(src, rs, *alpha_band, bbox, w, h)?)
                } else {
                    None
                };
                let byte: Vec<bool> = bands
                    .iter()
                    .map(|b| *b > 0 && src.band(*b).map(|x| x.byte).unwrap_or(false))
                    .collect();
                let all_byte = byte.iter().all(|b| *b);
                let has_enh = if all_byte {
                    contrast
                        .iter()
                        .any(|c| c.is_some_and(|c| c.algo != super::Algo::NoEnhancement))
                } else {
                    contrast.iter().any(Option::is_some)
                };
                let fast = (*opacity - 1.0).abs() < 1e-9
                    && *alpha_band < 1
                    && bands.iter().all(|b| *b > 0)
                    && !has_enh;
                let nd = nodata_pixel(*nodata_color);
                let mut out = vec![PremultipliedColorU8::TRANSPARENT; n];
                for i in 0..n {
                    let mut v = [0f64; 3];
                    let mut no = false;
                    for k in 0..3 {
                        if let Some(b) = &blocks[k] {
                            if b.nodata[i] {
                                no = true;
                                break;
                            }
                            v[k] = b.data[i];
                        }
                    }
                    if no {
                        out[i] = nd;
                        continue;
                    }
                    if fast {
                        out[i] = rgba(v[0] as i32, v[1] as i32, v[2] as i32, 255);
                        continue;
                    }
                    if (0..3).any(|k| contrast[k].is_some_and(|c: Contrast| !c.displayable(v[k]))) {
                        out[i] = nd;
                        continue;
                    }
                    for k in 0..3 {
                        if let Some(c) = contrast[k] {
                            v[k] = c.enhance(v[k], byte[k]) as f64;
                        }
                    }
                    let mut a = *opacity;
                    if let Some(ab) = &alpha {
                        let av = ab.data[i];
                        if av == 0.0 {
                            out[i] = nd;
                            continue;
                        }
                        a *= av / 255.0;
                    }
                    out[i] = if (a - 1.0).abs() < 1e-9 {
                        rgba(v[0] as i32, v[1] as i32, v[2] as i32, 255)
                    } else {
                        rgba(
                            (a * v[0]) as i32,
                            (a * v[1]) as i32,
                            (a * v[2]) as i32,
                            (a * 255.0) as i32,
                        )
                    };
                }
                Ok(out)
            }
            RasterRenderer::PseudoColor {
                band,
                alpha_band,
                opacity,
                transparency,
                nodata_color,
                ramp,
            } => {
                let input = input_block(src, rs, *band, bbox, w, h)?;
                let alpha = if *alpha_band > 0 && alpha_band != band {
                    Some(input_block(src, rs, *alpha_band, bbox, w, h)?)
                } else {
                    None
                };
                // QgsRasterRenderer::usesTransparency
                let has_transparency =
                    *alpha_band > 0 || !transparency.is_empty() || (*opacity - 1.0).abs() > 1e-9;
                let nd = nodata_pixel(*nodata_color);
                let mut out = vec![PremultipliedColorU8::TRANSPARENT; n];
                // ColorRamp::shade per distinct value (byte rasters have at most 256)
                let mut shades = Memo::<u64, Option<[i32; 4]>>::new();
                for i in 0..n {
                    if input.nodata[i] {
                        out[i] = nd;
                        continue;
                    }
                    let v = input.data[i];
                    let shade = match shades.get(v.to_bits()) {
                        Some(c) => c,
                        None => {
                            let c = ramp.shade(v);
                            shades.put(v.to_bits(), c);
                            c
                        }
                    };
                    let Some([mut r, mut g, mut b, a]) = shade else {
                        out[i] = nd;
                        continue;
                    };
                    if a < 255 {
                        // premultiply; int *= double truncates as in C++
                        let f = a as f64 / 255.0;
                        r = (r as f64 * f) as i32;
                        g = (g as f64 * f) as i32;
                        b = (b as f64 * f) as i32;
                    }
                    if !has_transparency {
                        out[i] = rgba(r, g, b, a);
                        continue;
                    }
                    let mut o = *opacity;
                    if let Some(&(_, _, pct)) =
                        transparency.iter().find(|(mn, mx, _)| v >= *mn && v <= *mx)
                    {
                        o *= 1.0 - pct / 100.0;
                    }
                    if *alpha_band > 0 {
                        let av = alpha.as_ref().map(|b| b.data[i]).unwrap_or(v);
                        if av == 0.0 {
                            out[i] = nd;
                            continue;
                        }
                        o *= av / 255.0;
                    }
                    out[i] = rgba(
                        (o * r as f64) as i32,
                        (o * g as f64) as i32,
                        (o * b as f64) as i32,
                        (o * a as f64) as i32,
                    );
                }
                Ok(out)
            }
            RasterRenderer::Hillshade {
                band,
                alpha_band,
                opacity,
                transparency,
                nodata_color,
                angle,
                azimuth,
                z_factor,
                multi_directional,
            } => {
                let input = input_block(src, rs, *band, bbox, w, h)?;
                let alpha = if *alpha_band > 0 && alpha_band != band {
                    Some(input_block(src, rs, *alpha_band, bbox, w, h)?)
                } else {
                    None
                };
                // QGIS precomputes these in float; the per-pixel math then promotes to double.
                let cell_x = (bbox.width() as f32 / w as f32) as f64;
                let cell_y = (bbox.height() as f32 / h as f32) as f64;
                let zenith = ((90.0 - angle).max(0.0) * std::f64::consts::PI / 180.0) as f32;
                let az = (-azimuth * std::f64::consts::PI / 180.0) as f32;
                let cos_alt_z = zenith.cos() * *z_factor as f32;
                let cos_az_254 = (254.0 * (az.cos() * cos_alt_z)) as f64;
                let sin_az_254 = (254.0 * (az.sin() * cos_alt_z)) as f64;
                let square_z = (z_factor * z_factor) as f32;
                let sin_alt_254 = 254.0f32 * zenith.sin();
                let sin_alt_127 = 127.0f32 * zenith.sin();
                let cos225_127 = -32.870_018_728_020_12_f32 * cos_alt_z;
                let cos_alt_127 = 127.0f32 * cos_alt_z;
                let nd = nodata_pixel(*nodata_color);
                let mut out = vec![PremultipliedColorU8::TRANSPARENT; n];
                // Edge rows/columns reuse the centre cell, nodata neighbours take its value.
                for row in 0..h {
                    let up = row.saturating_sub(1);
                    let down = (row + 1).min(h - 1);
                    for col in 0..w {
                        let i = row * w + col;
                        if input.nodata[i] {
                            out[i] = nd;
                            continue;
                        }
                        let x22 = input.data[i];
                        let (l, r) = (col.saturating_sub(1), (col + 1).min(w - 1));
                        let v = |rr: usize, cc: usize| {
                            let k = rr * w + cc;
                            if input.nodata[k] { x22 } else { input.data[k] }
                        };
                        let (x11, x12, x13) = (v(up, l), v(up, col), v(up, r));
                        let (x21, x23) = (v(row, l), v(row, r));
                        let (x31, x32, x33) = (v(down, l), v(down, col), v(down, r));
                        let der_x =
                            ((x13 + x23 + x23 + x33) - (x11 + x21 + x21 + x31)) / (8.0 * cell_x);
                        let der_y =
                            ((x31 + x32 + x32 + x33) - (x11 + x12 + x12 + x13)) / (8.0 * -cell_y);
                        let gray = if !multi_directional {
                            ((sin_alt_254 as f64 - (der_y * cos_az_254 - der_x * sin_az_254))
                                / (1.0 + square_z as f64 * (der_x * der_x + der_y * der_y)).sqrt())
                            .clamp(0.0, 255.0)
                        } else {
                            // USGS weighted multi-direction, in float as in QGIS
                            let (dx, dy) = (der_x as f32, der_y as f32);
                            let (xx, yy) = (dx * dx, dy * dy);
                            let xy = xx + yy;
                            if xy == 0.0 {
                                (1.0 + sin_alt_254).clamp(0.0, 255.0) as f64
                            } else {
                                let v225 = (sin_alt_127 + (dx - dy) * cos225_127).max(0.0);
                                let v270 = (sin_alt_127 - dx * cos_alt_127).max(0.0);
                                let v315 = (sin_alt_127 + (dx + dy) * cos225_127).max(0.0);
                                let v360 = (sin_alt_127 - dy * cos_alt_127).max(0.0);
                                let w225 = 0.5 * xy - dx * dy;
                                let cang =
                                    ((w225 * v225 + xx * v270 + (xy - w225) * v315 + yy * v360)
                                        / xy)
                                        / (1.0 + square_z * xy);
                                (1.0 + cang).clamp(0.0, 255.0) as f64
                            }
                        };
                        let mut a = *opacity;
                        if let Some(&(_, _, pct)) = transparency
                            .iter()
                            .find(|(mn, mx, _)| x22 >= *mn && x22 <= *mx)
                        {
                            a *= 1.0 - pct / 100.0;
                        }
                        if *alpha_band > 0 {
                            a *= alpha.as_ref().map(|b| b.data[i]).unwrap_or(x22) / 255.0;
                        }
                        out[i] = if (a - 1.0).abs() < 1e-9 {
                            rgba(gray as i32, gray as i32, gray as i32, 255)
                        } else {
                            rgba(
                                (a * gray) as i32,
                                (a * gray) as i32,
                                (a * gray) as i32,
                                (a * 255.0) as i32,
                            )
                        };
                    }
                }
                Ok(out)
            }
            RasterRenderer::Unsupported(t) => bail!("raster renderer '{t}' is not supported"),
        }
    }

    /// QgsBrightnessContrastFilter::block, on premultiplied pixels.
    fn brightness_contrast(img: &mut Image, (brightness, contrast, gamma): (i32, i32, f64)) {
        if brightness == 0 && contrast == 0 && gamma == 1.0 {
            return;
        }
        let f = ((contrast + 100) as f64 / 100.0).powi(2);
        let gc = 1.0 / gamma;
        let curve = |c: f64| {
            255.0 * ((((c / 255.0 - 0.5) * f + 0.5) * 255.0 + brightness as f64) / 255.0).powf(gc)
        };
        // Opaque pixels only depend on the 8-bit channel: one powf per value, not per pixel
        // (6 M powf calls for a 1424² HiDPI metatile).
        let opaque: [i32; 256] = std::array::from_fn(|c| (curve(c as f64) as i32).clamp(0, 255));
        // adjustColorComponent
        let adjust = |c: u8, a: u8| -> i32 {
            match a {
                255 => opaque[c as usize],
                0 => 0,
                _ => {
                    let af = a as f64 / 255.0;
                    (af * curve(c as f64 / af).clamp(0.0, 255.0)) as i32
                }
            }
        };
        // Translucent pixels (ramps with alpha, e.g. lag11's shading) take the powf path:
        // memoised per input pixel like hue_saturation.
        let mut memo = Memo::<u32, PremultipliedColorU8>::new();
        for p in img.iter_mut() {
            if *p == PremultipliedColorU8::TRANSPARENT {
                continue;
            }
            let a = p.alpha();
            let key = u32::from_le_bytes([p.red(), p.green(), p.blue(), a]);
            if a != 255
                && let Some(v) = memo.get(key)
            {
                *p = v;
                continue;
            }
            *p = rgba(
                adjust(p.red(), a),
                adjust(p.green(), a),
                adjust(p.blue(), a),
                a as i32,
            );
            if a != 255 {
                memo.put(key, *p);
            }
        }
    }

    /// Qt's qRound(float).
    fn qround(f: f32) -> i32 {
        f.round() as i32
    }

    /// qt_div_257: 16-bit QColor channel to 8 bits.
    fn div257(x: i32) -> i32 {
        let x = x as u32 + 128;
        ((x - (x >> 8)) >> 8) as i32
    }

    fn fuzzy_eq(a: f32, b: f32) -> bool {
        (a - b).abs() * 100000.0 <= a.abs().min(b.abs())
    }

    /// QColor::fromRgb(r, g, b).getHsl(): Qt's 16-bit HSL in float math.
    fn qt_rgb_to_hsl(r: i32, g: i32, b: i32) -> (i32, i32, i32) {
        let [r, g, b] = [r, g, b].map(|c| (c * 0x101) as f32 / 65535.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let (delta, delta2) = (max - min, max + min);
        let l = 0.5 * delta2;
        let lightness = qround(l * 65535.0);
        if delta.abs() <= 0.00001 {
            return (-1, 0, div257(lightness));
        }
        let sat = if l < 0.5 {
            qround(delta / delta2 * 65535.0)
        } else {
            qround(delta / (2.0 - delta2) * 65535.0)
        };
        let mut hue = if fuzzy_eq(r, max) {
            (g - b) / delta
        } else if fuzzy_eq(g, max) {
            2.0 + (b - r) / delta
        } else {
            4.0 + (r - g) / delta
        };
        hue *= 60.0;
        if hue < 0.0 {
            hue += 360.0;
        }
        (qround(hue * 100.0) / 100, div257(sat), div257(lightness))
    }

    /// QColor::fromHsl(h, s, l).getRgb().
    fn qt_hsl_to_rgb(h: i32, s: i32, l: i32) -> (i32, i32, i32) {
        let hue = if h == -1 { 65535 } else { (h % 360) * 100 };
        let (sat, light) = (s * 0x101, l * 0x101);
        if sat == 0 || hue == 65535 {
            let v = div257(light);
            return (v, v, v);
        }
        if light == 0 {
            return (0, 0, 0);
        }
        let h = if hue == 36000 {
            0.0
        } else {
            hue as f32 / 36000.0
        };
        let s = sat as f32 / 65535.0;
        let l = light as f32 / 65535.0;
        let temp2 = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let temp1 = 2.0 * l - temp2;
        let mut out = [h + 1.0 / 3.0, h, h - 1.0 / 3.0].map(|mut t| {
            if t < 0.0 {
                t += 1.0;
            } else if t > 1.0 {
                t -= 1.0;
            }
            let v = if t * 6.0 < 1.0 {
                temp1 + (temp2 - temp1) * (t * 6.0)
            } else if t * 2.0 < 1.0 {
                temp2
            } else if t * 3.0 < 2.0 {
                temp1 + (temp2 - temp1) * (2.0 / 3.0 - t) * 6.0
            } else {
                temp1
            };
            // stored as quint16; 1 is snapped to 0
            let c = qround(v * 65535.0) & 0xffff;
            if c == 1 { 0 } else { c }
        });
        out = out.map(div257);
        (out[0], out[1], out[2])
    }

    /// QgsHueSaturationFilter::block, on premultiplied pixels.
    fn hue_saturation(img: &mut Image, hs: HueSaturation) {
        if !hs.invert && hs.saturation == 0 && hs.grayscale == 0 && hs.colorize.is_none() {
            return;
        }
        let scale = hs.saturation as f64 / 100.0 + 1.0;
        // A pure function of the input pixel, and renderer output repeats colours (ramps,
        // byte rasters): memoised, it skips most of the per-pixel HSL round trips.
        let mut memo = Memo::<u32, PremultipliedColorU8>::new();
        for p in img.iter_mut() {
            let a = p.alpha();
            if a == 0 {
                continue;
            }
            let key = u32::from_le_bytes([p.red(), p.green(), p.blue(), a]);
            if let Some(v) = memo.get(key) {
                *p = v;
                continue;
            }
            let (mut r, mut g, mut b) = (p.red() as i32, p.green() as i32, p.blue() as i32);
            let af = a as f64 / 255.0;
            if a != 255 {
                // un-premultiply; int /= double truncates
                r = (r as f64 / af) as i32;
                g = (g as f64 / af) as i32;
                b = (b as f64 / af) as i32;
            }
            if hs.invert {
                (r, g, b) = (255 - r, 255 - g, 255 - b);
            }
            let (h, mut s, mut l) = qt_rgb_to_hsl(r, g, b);
            // processSaturation
            if hs.grayscale != 0 || scale != 1.0 {
                match hs.grayscale {
                    1 => (r, g, b) = qt_hsl_to_rgb(h, 0, l),
                    2 => {
                        let v = (0.21 * r as f64 + 0.72 * g as f64 + 0.07 * b as f64) as i32;
                        (r, g, b) = (v, v, v);
                        l = qt_rgb_to_hsl(r, g, b).2; // colorize reads the new lightness
                    }
                    3 => {
                        let v = (r + g + b) / 3;
                        (r, g, b) = (v, v, v);
                        l = qt_rgb_to_hsl(r, g, b).2; // colorize reads the new lightness
                    }
                    _ => {
                        s = if scale < 1.0 {
                            ((s as f64 * scale) as i32).min(255)
                        } else {
                            ((255.0 * (1.0 - (1.0 - s as f64 / 255.0).powf(scale * scale))) as i32)
                                .min(255)
                        };
                        (r, g, b) = qt_hsl_to_rgb(h, s, l);
                    }
                }
            }
            // processColorization: the colorize hue and saturation at this lightness,
            // blended linearly below 100 % strength.
            if let Some((ch, cs, strength)) = hs.colorize {
                let (cr, cg, cb) = qt_hsl_to_rgb(ch, cs, l);
                if strength == 100 {
                    (r, g, b) = (cr, cg, cb);
                } else {
                    let p = strength as f64 / 100.0;
                    r = (p * cr as f64 + (1.0 - p) * r as f64) as i32;
                    g = (p * cg as f64 + (1.0 - p) * g as f64) as i32;
                    b = (p * cb as f64 + (1.0 - p) * b as f64) as i32;
                }
            }
            if a != 255 {
                r = (r as f64 * af) as i32;
                g = (g as f64 * af) as i32;
                b = (b as f64 * af) as i32;
            }
            *p = rgba(r, g, b, a as i32);
            memo.put(key, *p);
        }
    }

    /// A small direct-mapped cache for per-pixel functions of a repeating input.
    struct Memo<K, V> {
        slots: Vec<Option<(K, V)>>,
    }

    impl<K: Copy + PartialEq + Into<u64>, V: Copy> Memo<K, V> {
        const BITS: u32 = 12;

        fn new() -> Self {
            Memo {
                slots: vec![None; 1 << Self::BITS],
            }
        }

        fn slot(k: K) -> usize {
            (k.into().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - Self::BITS)) as usize
        }

        fn get(&self, k: K) -> Option<V> {
            match self.slots[Self::slot(k)] {
                Some((kk, v)) if kk == k => Some(v),
                _ => None,
            }
        }

        fn put(&mut self, k: K, v: V) {
            self.slots[Self::slot(k)] = Some((k, v));
        }
    }

    /// The raster pipe below the projector: provider, renderer, brightness/contrast,
    /// hue/saturation (QgsRasterPipe order).
    fn pipe_block(
        style: &RasterStyle,
        src: &RasterSource,
        bbox: &Rect,
        w: usize,
        h: usize,
    ) -> Result<Image> {
        let mut img = renderer_block(&style.renderer, style.resampling.as_ref(), src, bbox, w, h)?;
        brightness_contrast(&mut img, style.brightness);
        hue_saturation(&mut img, style.hue_saturation);
        Ok(img)
    }

    /// QgsPainting::getCompositionMode.
    fn blend_mode(m: i32) -> tiny_skia::BlendMode {
        use tiny_skia::BlendMode as B;
        match m {
            1 => B::Lighten,
            2 => B::Screen,
            3 => B::ColorDodge,
            4 => B::Plus,
            5 => B::Darken,
            6 => B::Multiply,
            7 => B::ColorBurn,
            8 => B::Overlay,
            9 => B::SoftLight,
            10 => B::HardLight,
            11 => B::Difference,
            12 => B::Exclusion,
            _ => B::SourceOver,
        }
    }

    /// QgsRasterProjector's ProjectorData (approximate precision).
    struct Projector {
        dest: Rect,
        dest_rows: usize,
        dest_cols: usize,
        dest_xres: f64,
        dest_yres: f64,
        cp: Vec<Vec<Option<(f64, f64)>>>,
        src_extent: Rect,
        src_rows: usize,
        src_cols: usize,
        src_xres: f64,
        src_yres: f64,
        layer_extent: Rect,
        approximate: bool,
        inverse: Transform,
    }

    impl Projector {
        fn new(
            dest: Rect,
            w: usize,
            h: usize,
            layer_extent: Rect,
            map_crs: &Crs,
            layer_crs: &Crs,
        ) -> Projector {
            let inverse = Transform::new(map_crs, layer_crs);
            let forward = Transform::new(layer_crs, map_crs);
            let (dest_xres, dest_yres) = (dest.width() / w as f64, dest.height() / h as f64);
            let my_res = dest_xres.min(dest_yres);
            let sqr_tol = my_res * my_res;
            let mut p = Projector {
                dest,
                dest_rows: h,
                dest_cols: w,
                dest_xres,
                dest_yres,
                cp: vec![],
                src_extent: Rect::empty(),
                src_rows: 0,
                src_cols: 0,
                src_xres: 0.0,
                src_yres: 0.0,
                layer_extent,
                approximate: true,
                inverse,
            };
            let mut rows = 3;
            let cols = 3;
            if (-dest.ymin - dest.ymax).abs() / h as f64 / 1.0 < 0.5 * dest_yres {
                rows = 5;
            }
            p.cp = vec![vec![None; cols]; rows];
            for r in 0..rows {
                p.calc_row(r);
            }
            loop {
                let cols_ok = p.check_cols(&forward, sqr_tol);
                if !cols_ok {
                    p.insert_rows();
                }
                let rows_ok = p.check_rows(&forward, sqr_tol);
                if !rows_ok {
                    p.insert_cols();
                }
                if cols_ok && rows_ok {
                    break;
                }
                if (p.cp.len() * p.cp[0].len()) as f64 > 0.25 * (w * h) as f64 {
                    p.approximate = false;
                    break;
                }
            }
            p.calc_src_extent();
            p.calc_src_rows_cols();
            p.src_yres = p.src_extent.height() / p.src_rows.max(1) as f64;
            p.src_xres = p.src_extent.width() / p.src_cols.max(1) as f64;
            p
        }

        fn rows(&self) -> usize {
            self.cp.len()
        }
        fn cols(&self) -> usize {
            self.cp[0].len()
        }

        fn dest_point(&self, row: usize, col: usize) -> (f64, f64) {
            (
                self.dest.xmin + col as f64 * self.dest.width() / (self.cols() - 1) as f64,
                self.dest.ymax - row as f64 * self.dest.height() / (self.rows() - 1) as f64,
            )
        }

        fn calc_cp(&mut self, row: usize, col: usize) {
            let (x, y) = self.dest_point(row, col);
            self.cp[row][col] = self.inverse.point(x, y).ok();
        }
        fn calc_row(&mut self, row: usize) {
            for c in 0..self.cols() {
                self.calc_cp(row, c);
            }
        }
        fn calc_col(&mut self, col: usize) {
            for r in 0..self.rows() {
                self.calc_cp(r, col);
            }
        }

        fn insert_rows(&mut self) {
            let cols = self.cols();
            let old = self.rows();
            for r in 0..old - 1 {
                self.cp.insert(1 + r * 2, vec![None; cols]);
            }
            for r in (1..self.rows() - 1).step_by(2) {
                self.calc_row(r);
            }
        }

        fn insert_cols(&mut self) {
            let old = self.cols();
            for row in self.cp.iter_mut() {
                for c in 0..old - 1 {
                    row.insert(1 + c * 2, None);
                }
            }
            for c in (1..self.cols() - 1).step_by(2) {
                self.calc_col(c);
            }
        }

        fn check(
            &self,
            a: Option<(f64, f64)>,
            mid: Option<(f64, f64)>,
            b: Option<(f64, f64)>,
            dest: (f64, f64),
            fwd: &Transform,
            tol: f64,
        ) -> bool {
            let (Some(a), Some(_), Some(b)) = (a, mid, b) else {
                return false;
            };
            let approx = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            match fwd.point(approx.0, approx.1) {
                Ok(d) => (d.0 - dest.0).powi(2) + (d.1 - dest.1).powi(2) <= tol,
                Err(_) => false,
            }
        }

        fn check_cols(&self, fwd: &Transform, tol: f64) -> bool {
            for c in 0..self.cols() {
                for r in (1..self.rows() - 1).step_by(2) {
                    if !self.check(
                        self.cp[r - 1][c],
                        self.cp[r][c],
                        self.cp[r + 1][c],
                        self.dest_point(r, c),
                        fwd,
                        tol,
                    ) {
                        return false;
                    }
                }
            }
            true
        }

        fn check_rows(&self, fwd: &Transform, tol: f64) -> bool {
            for r in 0..self.rows() {
                for c in (1..self.cols() - 1).step_by(2) {
                    if !self.check(
                        self.cp[r][c - 1],
                        self.cp[r][c],
                        self.cp[r][c + 1],
                        self.dest_point(r, c),
                        fwd,
                        tol,
                    ) {
                        return false;
                    }
                }
            }
            true
        }

        fn calc_src_extent(&mut self) {
            let mut e = match self.cp[0][0] {
                Some(p) => Rect::new(p.0, p.1, p.0, p.1),
                None => Rect::empty(),
            };
            for row in &self.cp {
                for p in row.iter().flatten() {
                    e.expand(p.0, p.1);
                }
            }
            self.src_extent = intersect(&e, &self.layer_extent);
        }

        fn calc_src_rows_cols(&mut self) {
            let mut min_size = f64::MAX;
            if self.approximate {
                let mut max_size = 0f64;
                let cols_per = self.dest_cols as f64 / self.cols() as f64;
                let rows_per = self.dest_rows as f64 / self.rows() as f64;
                for i in 0..self.rows() - 1 {
                    for j in 0..self.cols() - 1 {
                        if let (Some(a), Some(b), Some(c)) =
                            (self.cp[i][j], self.cp[i][j + 1], self.cp[i + 1][j])
                        {
                            for s in [
                                ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() / cols_per,
                                ((a.0 - c.0).powi(2) + (a.1 - c.1).powi(2)).sqrt() / rows_per,
                            ] {
                                min_size = min_size.min(s);
                                max_size = max_size.max(s);
                            }
                        }
                    }
                }
                if min_size < 0.1 * max_size {
                    min_size = 0.1 * max_size;
                }
            } else {
                // QgsRasterProjector::extentSize: resolution from the transformed extent.
                if let Ok(se) = self.inverse.rect(&self.dest) {
                    let n = (self.dest_cols * self.dest_rows) as f64;
                    let res = (se.width() * se.height() / n).sqrt();
                    min_size = res;
                }
            }
            min_size *= 0.75;
            let rows = self.src_extent.height() / min_size;
            self.src_rows = if rows > (self.dest_rows * 10) as f64 {
                self.dest_rows * 10
            } else {
                rows.round() as usize
            };
            let cols = self.src_extent.width() / min_size;
            self.src_cols = if cols > (self.dest_cols * 10) as f64 {
                self.dest_cols * 10
            } else {
                cols.round() as usize
            };
        }

        fn matrix_row(&self, dest_row: usize) -> usize {
            let per = self.dest_rows as f64 / (self.rows() - 1) as f64;
            ((dest_row as f64 + 0.5) / per).floor() as usize
        }
        fn matrix_col(&self, dest_col: usize) -> usize {
            let per = self.dest_cols as f64 / (self.cols() - 1) as f64;
            ((dest_col as f64 + 0.5) / per).floor() as usize
        }

        /// calcHelper: interpolated source points along control-point row `mrow` for all columns.
        fn helper(&self, mrow: usize) -> Vec<Option<(f64, f64)>> {
            (0..self.dest_cols)
                .map(|c| {
                    let x = self.dest.xmin + (c as f64 + 0.5) * self.dest_xres;
                    let mc = self.matrix_col(c);
                    let (x0, _) = self.dest_point(mrow, mc);
                    let (x1, _) = self.dest_point(mrow, mc + 1);
                    let f = (x - x0) / (x1 - x0);
                    let (p0, p1) = (
                        self.cp.get(mrow)?.get(mc)?.as_ref()?,
                        self.cp.get(mrow)?.get(mc + 1)?.as_ref()?,
                    );
                    Some((p0.0 + (p1.0 - p0.0) * f, p0.1 + (p1.1 - p0.1) * f))
                })
                .collect()
        }

        fn src_index(&self, x: f64, y: f64) -> Option<(usize, usize)> {
            if !self.layer_extent.contains(x, y) {
                return None;
            }
            let row = ((self.src_extent.ymax - y) / self.src_yres).floor();
            let col = ((x - self.src_extent.xmin) / self.src_xres).floor();
            if row < 0.0 || col < 0.0 || row >= self.src_rows as f64 || col >= self.src_cols as f64
            {
                return None;
            }
            Some((row as usize, col as usize))
        }

        /// QgsRasterProjector::block: map every destination pixel to its source pixel.
        fn project(&self, input: &Image) -> Image {
            let mut out = vec![PremultipliedColorU8::TRANSPARENT; self.dest_cols * self.dest_rows];
            let mut top = self.helper(0);
            let mut bottom = self.helper(1);
            let mut helper_top_row = 0usize;
            for i in 0..self.dest_rows {
                let mrow = self.matrix_row(i);
                if self.approximate && mrow > helper_top_row {
                    std::mem::swap(&mut top, &mut bottom);
                    bottom = self.helper(helper_top_row + 2);
                    helper_top_row += 1;
                }
                let dest_y = self.dest.ymax - (i as f64 + 0.5) * self.dest_yres;
                for j in 0..self.dest_cols {
                    let src = if self.approximate {
                        let mcol = self.matrix_col(j);
                        let (_, ymin) = self.dest_point(mrow + 1, mcol);
                        let (_, ymax) = self.dest_point(mrow, mcol + 1);
                        let yfrac = (dest_y - ymin) / (ymax - ymin);
                        match (top[j], bottom[j]) {
                            (Some(t), Some(b)) => {
                                self.src_index(b.0 + (t.0 - b.0) * yfrac, b.1 + (t.1 - b.1) * yfrac)
                            }
                            _ => None,
                        }
                    } else {
                        let x = self.dest.xmin + (j as f64 + 0.5) * self.dest_xres;
                        self.inverse
                            .point(x, dest_y)
                            .ok()
                            .and_then(|(sx, sy)| self.src_index(sx, sy))
                    };
                    if let Some((r, c)) = src {
                        out[i * self.dest_cols + j] = input[r * self.src_cols + c];
                    }
                }
            }
            out
        }
    }

    /// HiDPI requests (DPI >= 2x OGC) draw rasters at 1x pixel density and upscale them:
    /// sources run out of resolution a zoom earlier on 2x grids, so full density only adds
    /// cost (~4x pixels) and cubic blur. Deliberate QGIS deviation; RGIS_RASTER_HIDPI=full
    /// restores per-pixel rendering (parity tests).
    static HIDPI_DOWNSAMPLE: LazyLock<bool> =
        LazyLock::new(|| std::env::var("RGIS_RASTER_HIDPI").as_deref() != Ok("full"));

    /// Render one raster layer onto the map (QgsRasterLayerRenderer + QgsRasterDrawer).
    pub fn render(
        pm: &mut Pixmap,
        src: &RasterSource,
        style: &RasterStyle,
        layer_crs: &Crs,
        ms: &MapSettings,
    ) -> Result<()> {
        // Nothing below reads ms.dpi: raster detail depends only on extent / pixel size.
        let f = (ms.dpi / OGC_DPI + 0.01).floor();
        if !*HIDPI_DOWNSAMPLE || f < 2.0 {
            return render_at(pm, src, style, layer_crs, ms, blend_mode(style.blend_mode));
        }
        if visible_extent(src, layer_crs, ms)?.is_none() {
            return Ok(());
        }
        let (w, h) = (
            (ms.width as f64 / f).ceil() as u32,
            (ms.height as f64 / f).ceil() as u32,
        );
        // Keep map units per pixel exactly f x the request's; ceil overhang goes right/down.
        let (mx, my) = (
            ms.extent.width() / ms.width as f64 * f,
            ms.extent.height() / ms.height as f64 * f,
        );
        let mut low = ms.clone();
        low.width = w;
        low.height = h;
        low.dpi = ms.dpi / f;
        low.extent = Rect::new(
            ms.extent.xmin,
            ms.extent.ymax - my * h as f64,
            ms.extent.xmin + mx * w as f64,
            ms.extent.ymax,
        );
        let mut tmp = Pixmap::new(w, h).ok_or_else(|| anyhow!("raster pixmap {w}x{h}"))?;
        // Parts tile the transparent pixmap without overlap; the layer's blend mode is
        // applied once, when compositing onto the map.
        render_at(
            &mut tmp,
            src,
            style,
            layer_crs,
            &low,
            tiny_skia::BlendMode::SourceOver,
        )?;
        // tiny-skia's scaled draw_pixmap (bicubic) cost more than rendering at full density.
        let up = upscale(&tmp, f as usize, ms.width, ms.height);
        pm.draw_pixmap(
            0,
            0,
            up.as_ref(),
            &PixmapPaint {
                blend_mode: blend_mode(style.blend_mode),
                ..Default::default()
            },
            SkTransform::identity(),
            None,
        );
        Ok(())
    }

    /// Bilinear upscale by an integer factor (pixel centres aligned, edges clamped) on
    /// premultiplied RGBA, cropped to w x h. Separable, fixed point.
    fn upscale(src: &Pixmap, f: usize, w: u32, h: u32) -> Pixmap {
        let (sw, sh) = (src.width() as usize, src.height() as usize);
        let (w, h) = (w as usize, h as usize);
        // Per output index: two source indices and the 8-bit weight of the second.
        let taps = |n: usize, sn: usize| -> Vec<(usize, usize, u32)> {
            (0..n)
                .map(|i| {
                    let x = ((i as f64 + 0.5) / f as f64 - 0.5).max(0.0);
                    let i0 = (x.floor() as usize).min(sn - 1);
                    let i1 = (i0 + 1).min(sn - 1);
                    (i0, i1, ((x - i0 as f64) * 256.0).round() as u32)
                })
                .collect()
        };
        let (tx, ty) = (taps(w, sw), taps(h, sh));
        let sp = src.data();
        // Horizontal pass into u16 rows (value * 256 fits: 255 * 256 < 65536).
        let mut mid = vec![0u16; w * sh * 4];
        for y in 0..sh {
            let row = &sp[y * sw * 4..(y + 1) * sw * 4];
            let out = &mut mid[y * w * 4..(y + 1) * w * 4];
            for (x, &(a, b, wt)) in tx.iter().enumerate() {
                for c in 0..4 {
                    let v = row[a * 4 + c] as u32 * (256 - wt) + row[b * 4 + c] as u32 * wt;
                    out[x * 4 + c] = v as u16;
                }
            }
        }
        let mut dst = Pixmap::new(w as u32, h as u32).expect("upscale size");
        let dp = dst.data_mut();
        for (y, &(a, b, wt)) in ty.iter().enumerate() {
            let (ra, rb) = (
                &mid[a * w * 4..(a + 1) * w * 4],
                &mid[b * w * 4..(b + 1) * w * 4],
            );
            let out = &mut dp[y * w * 4..(y + 1) * w * 4];
            for i in 0..w * 4 {
                let v = ra[i] as u32 * (256 - wt) + rb[i] as u32 * wt;
                out[i] = ((v + 32768) >> 16) as u8;
            }
        }
        dst
    }

    /// Part of the map extent the raster covers, None if it doesn't intersect.
    fn visible_extent(
        src: &RasterSource,
        layer_crs: &Crs,
        ms: &MapSettings,
    ) -> Result<Option<Rect>> {
        let layer_extent_map = if layer_crs == &ms.crs {
            src.extent
        } else {
            Transform::new(layer_crs, &ms.crs).rect(&src.extent)?
        };
        let visible = intersect(&ms.extent, &layer_extent_map);
        if visible.is_empty() || visible.width() <= 0.0 || visible.height() <= 0.0 {
            return Ok(None);
        }
        Ok(Some(visible))
    }

    fn render_at(
        pm: &mut Pixmap,
        src: &RasterSource,
        style: &RasterStyle,
        layer_crs: &Crs,
        ms: &MapSettings,
        blend: tiny_skia::BlendMode,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let same_crs = layer_crs == &ms.crs;
        let Some(visible) = visible_extent(src, layer_crs, ms)? else {
            return Ok(());
        };
        // Viewport: pixel-aligned size; the drawn extent is the unaligned one (as in QGIS).
        let mupp_x = ms.extent.width() / ms.width as f64;
        let mupp_y = ms.extent.height() / ms.height as f64;
        let tl = (
            (visible.xmin - ms.extent.xmin) / mupp_x,
            (ms.extent.ymax - visible.ymax) / mupp_y,
        );
        let br = (
            (visible.xmax - ms.extent.xmin) / mupp_x,
            (ms.extent.ymax - visible.ymin) / mupp_y,
        );
        let (tlx, tly) = (tl.0.floor(), tl.1.floor());
        let (brx, bry) = (br.0.ceil(), br.1.ceil());
        let (vw, vh) = ((brx - tlx).abs() as usize, (bry - tly).abs() as usize);
        if vw == 0 || vh == 0 {
            return Ok(());
        }
        // QgsRasterIterator: parts of at most MAX_TILE pixels.
        let mut row0 = 0;
        while row0 < vh {
            let rows = MAX_TILE.min(vh - row0);
            let mut col0 = 0;
            while col0 < vw {
                let cols = MAX_TILE.min(vw - col0);
                let e = &visible;
                let xmin = e.xmin + col0 as f64 / vw as f64 * e.width();
                let xmax = if col0 + cols == vw {
                    e.xmax
                } else {
                    e.xmin + (col0 + cols) as f64 / vw as f64 * e.width()
                };
                let ymin = if row0 + rows == vh {
                    e.ymin
                } else {
                    e.ymax - (row0 + rows) as f64 / vh as f64 * e.height()
                };
                let ymax = e.ymax - row0 as f64 / vh as f64 * e.height();
                let part = Rect::new(xmin, ymin, xmax, ymax);
                let img = if same_crs {
                    Some(pipe_block(style, src, &part, cols, rows)?)
                } else {
                    let p = Projector::new(part, cols, rows, src.extent, &ms.crs, layer_crs);
                    if p.src_rows == 0 || p.src_cols == 0 {
                        None
                    } else {
                        let input = pipe_block(style, src, &p.src_extent, p.src_cols, p.src_rows)?;
                        Some(p.project(&input))
                    }
                };
                if let Some(img) = img {
                    let mut part_pm = Pixmap::new(cols as u32, rows as u32).expect("part size");
                    part_pm.pixels_mut().copy_from_slice(&img);
                    pm.draw_pixmap(
                        (tlx + col0 as f64).floor() as i32,
                        (tly + row0 as f64).floor() as i32,
                        part_pm.as_ref(),
                        &PixmapPaint {
                            blend_mode: blend,
                            ..Default::default()
                        },
                        SkTransform::identity(),
                        None,
                    );
                }
                col0 += cols;
            }
            row0 += rows;
        }
        debug!("raster rendered {vw}x{vh} in {:?}", started.elapsed());
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn px(p: &Pixmap, x: u32, y: u32) -> [u8; 4] {
            let i = ((y * p.width() + x) * 4) as usize;
            p.data()[i..i + 4].try_into().unwrap()
        }

        #[test]
        fn upscale_constant_and_ramp() {
            // 2x1 source: opaque black | opaque white (premultiplied == straight here).
            let mut src = Pixmap::new(2, 1).unwrap();
            src.data_mut()
                .copy_from_slice(&[0, 0, 0, 255, 255, 255, 255, 255]);
            let up = upscale(&src, 2, 4, 2);
            let row: Vec<u8> = (0..4).map(|x| px(&up, x, 0)[0]).collect();
            // Centres at -0.25, 0.25, 0.75, 1.25 in source pixels, clamped at the edges.
            assert_eq!(row, vec![0, 64, 191, 255]);
            assert!((0..4).all(|x| px(&up, x, 0)[3] == 255 && px(&up, x, 1) == px(&up, x, 0)));
            // Odd request size: cropped, not stretched.
            assert_eq!(
                (
                    upscale(&src, 2, 3, 1).width(),
                    upscale(&src, 2, 3, 1).height()
                ),
                (3, 1)
            );
        }
    }
}
