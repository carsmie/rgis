//! Coordinate reference systems and transforms (QgsCoordinateReferenceSystem /
//! QgsCoordinateTransform equivalents), backed by the pure-Rust proj4rs.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use proj4rs::Proj;

use crate::geom::Rect;

#[derive(Clone)]
pub struct Crs {
    /// Normalised identifier, e.g. "EPSG:4326" or "CRS:84".
    pub authid: String,
    proj: Arc<Proj>,
    pub geographic: bool,
    mercator: bool,
}

impl std::fmt::Debug for Crs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Crs({})", self.authid)
    }
}

impl PartialEq for Crs {
    fn eq(&self, other: &Self) -> bool {
        self.same_definition(other)
    }
}

static CACHE: LazyLock<Mutex<HashMap<String, Crs>>> = LazyLock::new(Default::default);

impl Crs {
    /// Parses OGC style identifiers: `EPSG:4326`, `CRS:84`, `OGC:CRS84`,
    /// `urn:ogc:def:crs:EPSG::4326`, `http://www.opengis.net/def/crs/EPSG/0/4326`.
    pub fn from_ogc(id: &str) -> Result<Crs> {
        let authid = normalise(id).ok_or_else(|| anyhow!("unsupported CRS identifier '{id}'"))?;
        if let Some(c) = CACHE.lock().get(&authid) {
            return Ok(c.clone());
        }
        let proj4 = if authid == "CRS:84" {
            "+proj=longlat +datum=WGS84 +no_defs".to_string()
        } else {
            let code: u16 = authid[5..]
                .parse()
                .map_err(|_| anyhow!("unsupported EPSG code in '{id}'"))?;
            crs_definitions::from_code(code)
                .ok_or_else(|| anyhow!("unknown CRS '{authid}'"))?
                .proj4
                .to_string()
        };
        let crs = Crs::build(authid.clone(), &proj4)?;
        CACHE.lock().insert(authid, crs.clone());
        Ok(crs)
    }

    /// Build from the `<spatialrefsys>` of a project: prefer the authid, fall back to proj4.
    pub fn from_authid_or_proj4(authid: &str, proj4: &str) -> Result<Crs> {
        match Crs::from_ogc(authid) {
            Ok(c) => Ok(c),
            Err(e) if !proj4.trim().is_empty() => Crs::build(
                if authid.is_empty() {
                    proj4.to_string()
                } else {
                    authid.to_string()
                },
                proj4,
            )
            .map_err(|e2| anyhow!("{e}; proj4 fallback failed: {e2}")),
            Err(e) => Err(e),
        }
    }

    fn build(authid: String, proj4: &str) -> Result<Crs> {
        let proj = Proj::from_proj_string(proj4)
            .map_err(|e| anyhow!("invalid proj string '{proj4}': {e:?}"))?;
        Ok(Crs {
            geographic: proj.is_latlong(),
            mercator: proj4.contains("+proj=merc"),
            authid,
            proj: Arc::new(proj),
        })
    }

    pub fn wgs84() -> Crs {
        Crs::from_ogc("EPSG:4326").expect("EPSG:4326 is always available")
    }

    /// QGIS `hasAxisInverted()`: for WMS 1.3.0 / WFS 1.1 GML, EPSG geographic CRSs
    /// use latitude, longitude order. CRS:84 is explicitly lon/lat.
    pub fn axis_inverted(&self) -> bool {
        self.geographic && self.authid.starts_with("EPSG:")
    }

    pub fn epsg_code(&self) -> Option<u32> {
        self.authid
            .strip_prefix("EPSG:")
            .and_then(|c| c.parse().ok())
    }

    pub fn urn(&self) -> String {
        match self.epsg_code() {
            Some(c) => format!("urn:ogc:def:crs:EPSG::{c}"),
            None => "urn:ogc:def:crs:OGC:1.3:CRS84".to_string(),
        }
    }

    fn same_definition(&self, other: &Crs) -> bool {
        let canon = |a: &str| {
            if a == "CRS:84" {
                "EPSG:4326".to_string()
            } else {
                a.to_string()
            }
        };
        canon(&self.authid) == canon(&other.authid)
    }

    /// Meters per map unit (approximate for geographic CRSs, evaluated at `lat`).
    pub fn meters_per_unit(&self, lat: f64) -> f64 {
        if self.geographic {
            111_319.490_793 * lat.to_radians().cos().abs().max(1e-6)
        } else {
            1.0
        }
    }
}

fn normalise(id: &str) -> Option<String> {
    let s = id.trim();
    let upper = s.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CRS:84" | "OGC:CRS84" | "URN:OGC:DEF:CRS:OGC:1.3:CRS84" | "URN:OGC:DEF:CRS:OGC::CRS84"
    ) || upper.ends_with("/OGC/1.3/CRS84")
    {
        return Some("CRS:84".into());
    }
    let code = if let Some(c) = upper.strip_prefix("EPSG:") {
        c
    } else if let Some(c) = upper.strip_prefix("URN:OGC:DEF:CRS:EPSG:") {
        c.rsplit(':').next()?
    } else if upper.contains("/DEF/CRS/EPSG/") {
        upper.rsplit('/').next()?
    } else {
        return None;
    };
    let code: u32 = code.trim().parse().ok()?;
    Some(format!("EPSG:{code}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Fast {
    None,
    Identity,
    /// WGS84 → Web Mercator: EPSG:3857 is a sphere of radius 6378137 with `+nadgrids=@null`,
    /// i.e. WGS84 degrees are used directly; the closed form is exact.
    GeoToWebMercator,
    WebMercatorToGeo,
}

const WEB_MERCATOR_R: f64 = 6_378_137.0;

/// A transform between two CRSs, working in degrees for geographic systems.
pub struct Transform {
    src: Crs,
    dst: Crs,
    fast: Fast,
}

impl Transform {
    pub fn new(src: &Crs, dst: &Crs) -> Transform {
        let wgs84 = |c: &Crs| matches!(c.authid.as_str(), "EPSG:4326" | "CRS:84");
        let web_merc = |c: &Crs| matches!(c.authid.as_str(), "EPSG:3857" | "EPSG:900913");
        let fast = if src == dst {
            Fast::Identity
        } else if wgs84(src) && web_merc(dst) {
            Fast::GeoToWebMercator
        } else if web_merc(src) && wgs84(dst) {
            Fast::WebMercatorToGeo
        } else {
            Fast::None
        };
        Transform {
            src: src.clone(),
            dst: dst.clone(),
            fast,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.fast == Fast::Identity
    }

    pub fn point(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        match self.fast {
            Fast::Identity => return Ok((x, y)),
            Fast::GeoToWebMercator => {
                let lat = y.clamp(-85.051_128_78, 85.051_128_78).to_radians();
                let my = WEB_MERCATOR_R * (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln();
                return Ok((WEB_MERCATOR_R * x.to_radians(), my));
            }
            Fast::WebMercatorToGeo => {
                let lat = (2.0 * (y / WEB_MERCATOR_R).exp().atan() - std::f64::consts::FRAC_PI_2)
                    .to_degrees();
                return Ok(((x / WEB_MERCATOR_R).to_degrees(), lat));
            }
            Fast::None => {}
        }
        let (mut x, mut y) = (x, y);
        if self.src.geographic {
            // Mercator is undefined at the poles; QGIS clips similarly when rendering.
            let lim = if self.dst.mercator {
                85.051_128_78
            } else {
                90.0
            };
            y = y.clamp(-lim, lim);
            x = x.to_radians();
            y = y.to_radians();
        }
        let mut p = (x, y);
        proj4rs::transform::transform(&self.src.proj, &self.dst.proj, &mut p).map_err(|e| {
            anyhow!(
                "transform {} -> {} failed: {e:?}",
                self.src.authid,
                self.dst.authid
            )
        })?;
        if self.dst.geographic {
            p = (p.0.to_degrees(), p.1.to_degrees());
        }
        if !p.0.is_finite() || !p.1.is_finite() {
            bail!("non-finite transform result");
        }
        Ok(p)
    }

    /// Densified bounding box transform (QgsCoordinateTransform::transformBoundingBox).
    pub fn rect(&self, r: &Rect) -> Result<Rect> {
        if self.is_identity() {
            return Ok(*r);
        }
        const N: usize = 20;
        let mut out = Rect::empty();
        for i in 0..=N {
            let t = i as f64 / N as f64;
            let x = r.xmin + (r.xmax - r.xmin) * t;
            let y = r.ymin + (r.ymax - r.ymin) * t;
            for (px, py) in [(x, r.ymin), (x, r.ymax), (r.xmin, y), (r.xmax, y)] {
                if let Ok((tx, ty)) = self.point(px, py) {
                    out.expand(tx, ty);
                }
            }
        }
        if out.is_empty() {
            bail!("could not transform rectangle");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identifiers() {
        assert_eq!(normalise("epsg:3857").unwrap(), "EPSG:3857");
        assert_eq!(
            normalise("urn:ogc:def:crs:EPSG::4326").unwrap(),
            "EPSG:4326"
        );
        assert_eq!(
            normalise("http://www.opengis.net/def/crs/EPSG/0/25833").unwrap(),
            "EPSG:25833"
        );
        assert_eq!(normalise("CRS:84").unwrap(), "CRS:84");
        assert!(Crs::from_ogc("EPSG:4326").unwrap().axis_inverted());
        assert!(!Crs::from_ogc("CRS:84").unwrap().axis_inverted());
        assert!(!Crs::from_ogc("EPSG:3857").unwrap().axis_inverted());
    }

    #[test]
    fn wgs84_to_web_mercator() {
        let t = Transform::new(&Crs::wgs84(), &Crs::from_ogc("EPSG:3857").unwrap());
        let (x, y) = t.point(10.0, 60.0).unwrap();
        assert!((x - 1_113_194.9).abs() < 1.0, "{x}");
        assert!((y - 8_399_737.9).abs() < 1.0, "{y}");
        let back = Transform::new(&Crs::from_ogc("EPSG:3857").unwrap(), &Crs::wgs84());
        let (lon, lat) = back.point(x, y).unwrap();
        assert!((lon - 10.0).abs() < 1e-7 && (lat - 60.0).abs() < 1e-7);
    }

    #[test]
    fn web_mercator_fast_path_matches_proj4rs() {
        let (geo, merc) = (Crs::wgs84(), Crs::from_ogc("EPSG:3857").unwrap());
        let slow = |x: f64, y: f64, s: &Crs, d: &Crs| {
            let mut t = Transform::new(s, d);
            t.fast = Fast::None;
            t.point(x, y).unwrap()
        };
        for (lon, lat) in [
            (10.0, 60.0),
            (-179.9, -85.0),
            (179.9, 84.9),
            (0.0, 0.0),
            (-73.98, 40.75),
        ] {
            let a = Transform::new(&geo, &merc).point(lon, lat).unwrap();
            let b = slow(lon, lat, &geo, &merc);
            assert!(
                (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6,
                "{a:?} vs {b:?}"
            );
            let c = Transform::new(&merc, &geo).point(a.0, a.1).unwrap();
            let d = slow(a.0, a.1, &merc, &geo);
            assert!(
                (c.0 - d.0).abs() < 1e-9 && (c.1 - d.1).abs() < 1e-9,
                "{c:?} vs {d:?}"
            );
        }
    }

    /// cargo test --release -- --ignored --nocapture transform_speed
    #[test]
    #[ignore]
    fn transform_speed() {
        for to in ["EPSG:3857", "EPSG:25833"] {
            let t = Transform::new(&Crs::wgs84(), &Crs::from_ogc(to).unwrap());
            let n = 1_000_000;
            let start = std::time::Instant::now();
            let mut acc = 0.0;
            for i in 0..n {
                let (x, _) = t.point(10.0 + (i % 1000) as f64 * 1e-3, 60.0).unwrap();
                acc += x;
            }
            let ns = start.elapsed().as_nanos() as f64 / n as f64;
            println!("EPSG:4326 -> {to}: {ns:.0} ns/point ({acc:.0})");
        }
    }

    #[test]
    fn wgs84_to_utm33() {
        let t = Transform::new(&Crs::wgs84(), &Crs::from_ogc("EPSG:25833").unwrap());
        let (x, y) = t.point(15.0, 60.0).unwrap();
        assert!((x - 500_000.0).abs() < 0.01, "{x}");
        assert!((y - 6_651_411.2).abs() < 1.0, "{y}");
    }
}
