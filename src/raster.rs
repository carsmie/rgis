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
    Unsupported(String),
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

/// The `<pipe><rasterrenderer>` of a raster layer.
pub fn parse_renderer(pipe: Node) -> RasterRenderer {
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
            transparency: child(r, "rasterTransparency")
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
                .unwrap_or_default(),
            nodata_color,
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

    use super::{Contrast, RasterRenderer};
    use crate::crs::{Crs, Transform};
    use crate::geom::Rect;
    use crate::render::MapSettings;

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
    }

    /// An opened GDAL raster (QgsGdalProvider), north-up (via a warped VRT if needed).
    pub struct RasterSource {
        ds: Mutex<(Handle, Option<Handle>)>,
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

    impl RasterSource {
        pub fn open(datasource: &str, base_dir: &Path) -> Result<Arc<RasterSource>> {
            let name = gdal_name(datasource, base_dir);
            if let Some(s) = SOURCES.lock().get(&name) {
                return Ok(s.clone());
            }
            static INIT: Once = Once::new();
            INIT.call_once(|| unsafe { g::GDALAllRegister() });
            let c = CString::new(name.clone())?;
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
                let byte = unsafe { g::GDALGetRasterDataType(b) } == g::GDALDataType::GDT_UInt8;
                bands.push(Band {
                    byte,
                    nodata: (ok != 0).then_some(nd),
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
                ds: Mutex::new((base, warped)),
                width,
                height,
                gt,
                extent,
                bands,
            });
            SOURCES.lock().insert(name, src.clone());
            Ok(src)
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
            let guard = self.ds.lock();
            let ds = guard.1.as_ref().map(|h| h.0).unwrap_or(guard.0.0);
            let err = unsafe {
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
            };
            if err != g::CPLErr::CE_None {
                bail!("GDALRasterIO failed");
            }
            Ok(buf)
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
                let input = provider_block(src, *band, bbox, w, h)?;
                let byte = src.band(*band)?.byte;
                let alpha = if *alpha_band > 0 && alpha_band != band {
                    Some(provider_block(src, *alpha_band, bbox, w, h)?)
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
                        Some(provider_block(src, *b, bbox, w, h)?)
                    } else {
                        None
                    });
                }
                let alpha = if *alpha_band > 0 {
                    Some(provider_block(src, *alpha_band, bbox, w, h)?)
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
            RasterRenderer::Unsupported(t) => bail!("raster renderer '{t}' is not supported"),
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

    /// Render one raster layer onto the map (QgsRasterLayerRenderer + QgsRasterDrawer).
    pub fn render(
        pm: &mut Pixmap,
        src: &RasterSource,
        renderer: &RasterRenderer,
        layer_crs: &Crs,
        ms: &MapSettings,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let same_crs = layer_crs == &ms.crs;
        let layer_extent_map = if same_crs {
            src.extent
        } else {
            Transform::new(layer_crs, &ms.crs).rect(&src.extent)?
        };
        let visible = intersect(&ms.extent, &layer_extent_map);
        if visible.is_empty() || visible.width() <= 0.0 || visible.height() <= 0.0 {
            return Ok(());
        }
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
                    Some(renderer_block(renderer, src, &part, cols, rows)?)
                } else {
                    let p = Projector::new(part, cols, rows, src.extent, &ms.crs, layer_crs);
                    if p.src_rows == 0 || p.src_cols == 0 {
                        None
                    } else {
                        let input =
                            renderer_block(renderer, src, &p.src_extent, p.src_cols, p.src_rows)?;
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
                        &PixmapPaint::default(),
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
}
