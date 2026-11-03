//! GEOS operations PAL needs, through GDAL's OGR (GDAL links GEOS, so the output matches what
//! QGIS's own GEOS calls produce, vertex order included).

use super::shape::Pt;

/// GEOSBuffer(polygon, dist, quadsegs) via OGR_G_Buffer: the result's rings (exterior first),
/// or None when it is not a single polygon (or rGIS is built without GDAL).
#[cfg(feature = "raster")]
pub fn buffer_polygon(rings: &[Vec<Pt>], dist: f64, quadsegs: i32) -> Option<Vec<Vec<Pt>>> {
    use gdal_sys as g;
    // Little-endian WKB polygon.
    let mut wkb = vec![1u8];
    wkb.extend(3u32.to_le_bytes());
    wkb.extend((rings.len() as u32).to_le_bytes());
    for r in rings {
        wkb.extend((r.len() as u32).to_le_bytes());
        for p in r {
            wkb.extend(p.0.to_le_bytes());
            wkb.extend(p.1.to_le_bytes());
        }
    }
    // SAFETY: OGR owns the handles created here; each is destroyed exactly once and ring
    // handles from OGR_G_GetGeometryRef are borrowed from `b` only while it lives.
    unsafe {
        let mut h: g::OGRGeometryH = std::ptr::null_mut();
        let err = g::OGR_G_CreateFromWkb(
            wkb.as_ptr().cast(),
            std::ptr::null_mut(),
            &mut h,
            wkb.len() as std::ffi::c_int,
        );
        if err != g::OGRErr::OGRERR_NONE || h.is_null() {
            return None;
        }
        let b = g::OGR_G_Buffer(h, dist, quadsegs);
        g::OGR_G_DestroyGeometry(h);
        if b.is_null() {
            return None;
        }
        let out = (g::OGR_GT_Flatten(g::OGR_G_GetGeometryType(b))
            == g::OGRwkbGeometryType::wkbPolygon)
            .then(|| {
                (0..g::OGR_G_GetGeometryCount(b))
                    .map(|i| {
                        let r = g::OGR_G_GetGeometryRef(b, i);
                        (0..g::OGR_G_GetPointCount(r))
                            .map(|k| (g::OGR_G_GetX(r, k), g::OGR_G_GetY(r, k)))
                            .collect()
                    })
                    .collect()
            });
        g::OGR_G_DestroyGeometry(b);
        out
    }
}

#[cfg(not(feature = "raster"))]
pub fn buffer_polygon(_rings: &[Vec<Pt>], _dist: f64, _quadsegs: i32) -> Option<Vec<Vec<Pt>>> {
    None
}
