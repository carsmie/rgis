//! Vector data providers (the subset of QGIS's `ogr` provider we support):
//! FlatGeobuf (local or `/vsicurl/` over HTTP range requests), GeoPackage,
//! ESRI Shapefile and GeoJSON.

use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, anyhow, bail};
use flatgeobuf::packed_r_tree::PackedRTree;
use flatgeobuf::{ColumnType, FallibleStreamingIterator, FeatureProperties, FgbReader};
use geo_types::{
    Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use geozero::{ColumnValue, PropertyProcessor, ToGeo};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::expression::Attrs;
use crate::geom::{Rect, bbox};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    Int,
    Real,
    Bool,
    Date,
    DateTime,
    String,
}

impl FieldKind {
    pub fn xsd(&self) -> &'static str {
        match self {
            FieldKind::Int => "long",
            FieldKind::Real => "double",
            FieldKind::Bool => "boolean",
            FieldKind::Date => "date",
            FieldKind::DateTime => "dateTime",
            FieldKind::String => "string",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub kind: FieldKind,
}

#[derive(Debug, Clone)]
pub struct Feature {
    pub fid: i64,
    pub geometry: Option<Geometry<f64>>,
    pub attrs: Attrs,
}

enum FgbSource {
    File(PathBuf),
    Http(Arc<HttpFile>),
}

struct GpkgSource {
    path: PathBuf,
    table: String,
    geom_col: String,
    fid_col: String,
    rtree: Option<String>,
    subset: Option<String>,
}

enum Kind {
    Fgb(FgbSource),
    Gpkg(GpkgSource),
    Memory(Vec<(Rect, Feature)>),
}

pub struct Provider {
    kind: Kind,
    pub fields: Vec<Field>,
    pub description: String,
    /// Data extent in the layer CRS (QgsVectorDataProvider::extent).
    pub extent: Option<Rect>,
}

/// Parsed OGR datasource string: `path|layername=x|subset=…` or `GPKG:path:layer`.
struct Uri {
    path: String,
    layer: Option<String>,
    subset: Option<String>,
}

fn parse_uri(ds: &str) -> Uri {
    if let Some(rest) = ds.strip_prefix("GPKG:")
        && let Some((p, l)) = rest.rsplit_once(':')
    {
        return Uri {
            path: p.into(),
            layer: Some(l.into()),
            subset: None,
        };
    }
    let mut parts = ds.split('|');
    let path = parts.next().unwrap_or("").to_string();
    let mut uri = Uri {
        path,
        layer: None,
        subset: None,
    };
    for p in parts {
        match p.split_once('=') {
            Some(("layername", v)) => uri.layer = Some(v.to_string()),
            Some(("subset", v)) => uri.subset = Some(v.to_string()).filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    uri
}

impl Provider {
    pub fn open(base_dir: &Path, provider_key: &str, datasource: &str) -> Result<Provider> {
        if provider_key != "ogr" {
            bail!("provider '{provider_key}' is not supported (only 'ogr')");
        }
        let uri = parse_uri(datasource);
        if let Some(url) = uri.path.strip_prefix("/vsicurl/") {
            let file = Arc::new(HttpFile::open(url)?);
            let (fields, extent) = fgb_header(&mut HttpReader::new(file.clone()))?;
            return Ok(Provider {
                kind: Kind::Fgb(FgbSource::Http(file)),
                fields,
                extent,
                description: format!("FlatGeobuf over HTTP {url}"),
            });
        }
        let mut path = PathBuf::from(&uri.path);
        if path.is_relative() {
            path = base_dir.join(path);
        }
        if !path.exists() {
            bail!("data source not found: {}", path.display());
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "fgb" => {
                let (fields, extent) =
                    fgb_header(&mut BufReader::new(std::fs::File::open(&path)?))?;
                Ok(Provider {
                    kind: Kind::Fgb(FgbSource::File(path.clone())),
                    fields,
                    extent,
                    description: format!("FlatGeobuf {}", path.display()),
                })
            }
            "gpkg" => open_gpkg(path, uri.layer, uri.subset),
            "shp" => open_shapefile(&path),
            "geojson" | "json" => open_geojson(&path),
            other => bail!("unsupported OGR format '.{other}' ({})", path.display()),
        }
    }

    /// Features intersecting `bbox` (in layer CRS), or all features.
    pub fn features(&self, bbox_filter: Option<Rect>) -> Result<Vec<Feature>> {
        let started = std::time::Instant::now();
        let out = match &self.kind {
            Kind::Fgb(FgbSource::File(p)) => fgb_features(
                BufReader::new(std::fs::File::open(p)?),
                bbox_filter,
                &self.fields,
            ),
            Kind::Fgb(FgbSource::Http(f)) => {
                fgb_features(HttpReader::new(f.clone()), bbox_filter, &self.fields)
            }
            Kind::Gpkg(g) => gpkg_features(g, &self.fields, bbox_filter),
            Kind::Memory(all) => Ok(all
                .iter()
                .filter(|(r, _)| bbox_filter.is_none_or(|b| b.intersects(r)))
                .map(|(_, f)| f.clone())
                .collect()),
        }?;
        debug!(
            "{}: {} features in {:?}",
            self.description,
            out.len(),
            started.elapsed()
        );
        Ok(out)
    }
}

// ---------------------------------------------------------------- FlatGeobuf

struct Props<'a>(&'a mut Attrs);

impl PropertyProcessor for Props<'_> {
    fn property(&mut self, _i: usize, name: &str, v: &ColumnValue) -> geozero::error::Result<bool> {
        let value = match v {
            ColumnValue::Byte(x) => json!(x),
            ColumnValue::UByte(x) => json!(x),
            ColumnValue::Bool(x) => json!(x),
            ColumnValue::Short(x) => json!(x),
            ColumnValue::UShort(x) => json!(x),
            ColumnValue::Int(x) => json!(x),
            ColumnValue::UInt(x) => json!(x),
            ColumnValue::Long(x) => json!(x),
            ColumnValue::ULong(x) => json!(x),
            ColumnValue::Float(x) => json!(x),
            ColumnValue::Double(x) => json!(x),
            ColumnValue::String(s) | ColumnValue::DateTime(s) => json!(s),
            ColumnValue::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
            ColumnValue::Binary(_) => Value::Null,
        };
        self.0.insert(name.to_string(), value);
        Ok(false)
    }
}

fn fgb_header<R: Read>(r: &mut R) -> Result<(Vec<Field>, Option<Rect>)> {
    let reader = FgbReader::open(r).context("invalid FlatGeobuf file")?;
    let header = reader.header();
    let extent = header
        .envelope()
        .filter(|e| e.len() >= 4)
        .map(|e| Rect::new(e.get(0), e.get(1), e.get(2), e.get(3)))
        .filter(|r| !r.is_empty());
    let fields = header
        .columns()
        .map(|cols| {
            cols.iter()
                .map(|c| Field {
                    name: c.name().to_string(),
                    kind: match c.type_() {
                        ColumnType::Bool => FieldKind::Bool,
                        ColumnType::Float | ColumnType::Double => FieldKind::Real,
                        ColumnType::String | ColumnType::Json | ColumnType::Binary => {
                            FieldKind::String
                        }
                        ColumnType::DateTime => FieldKind::DateTime,
                        _ => FieldKind::Int,
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((fields, extent))
}

fn memory_extent(all: &[(Rect, Feature)]) -> Option<Rect> {
    let mut e = Rect::empty();
    for (r, _) in all {
        e.union(r);
    }
    (!e.is_empty()).then_some(e)
}

fn fgb_features<R: Read + Seek>(
    mut r: R,
    filter: Option<Rect>,
    fields: &[Field],
) -> Result<Vec<Feature>> {
    let (count, indexed) = {
        let reader = FgbReader::open(&mut r)?;
        let h = reader.header();
        (h.features_count() as usize, h.index_node_size() > 0)
    };
    let mut out = vec![];
    let mut push = |fid: usize, f: &flatgeobuf::FgbFeature| -> Result<()> {
        let mut attrs = Attrs::new();
        for fl in fields {
            attrs.insert(fl.name.clone(), Value::Null);
        }
        f.process_properties(&mut Props(&mut attrs))?;
        out.push(Feature {
            fid: fid as i64,
            geometry: f.to_geo().ok(),
            attrs,
        });
        Ok(())
    };
    match filter {
        Some(b) if indexed && count > 0 => {
            // The reader is positioned right after the header: search the packed R-tree
            // ourselves to learn feature numbers (= OGR FIDs), then stream the same
            // selection through FgbReader::select_bbox (identical, offset-ordered list).
            let hits = PackedRTree::stream_search(
                &mut r,
                count,
                PackedRTree::DEFAULT_NODE_SIZE,
                b.xmin,
                b.ymin,
                b.xmax,
                b.ymax,
            )?;
            r.seek(SeekFrom::Start(0))?;
            let mut it = FgbReader::open(&mut r)?.select_bbox(b.xmin, b.ymin, b.xmax, b.ymax)?;
            let mut ids = hits.iter().map(|h| h.index);
            while let Some(f) = it.next()? {
                push(
                    ids.next()
                        .ok_or_else(|| anyhow!("FlatGeobuf index mismatch"))?,
                    f,
                )?;
            }
        }
        _ => {
            r.seek(SeekFrom::Start(0))?;
            let mut it = FgbReader::open(&mut r)?.select_all()?;
            let mut i = 0;
            while let Some(f) = it.next()? {
                let keep = match (filter, f.to_geo()) {
                    (Some(b), Ok(g)) => bbox(&g).intersects(&b),
                    _ => true,
                };
                if keep {
                    push(i, f)?;
                }
                i += 1;
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------- HTTP range reads (/vsicurl/)

const BLOCK: u64 = 256 * 1024;
const MAX_CACHED_BLOCKS: usize = 1024;

static IO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vsicurl")
        .enable_all()
        .build()
        .expect("io runtime")
});

/// A remote file read with HTTP Range requests and an in-memory block cache,
/// shared by all requests (the equivalent of GDAL's /vsicurl/).
pub struct HttpFile {
    url: String,
    len: u64,
    client: reqwest::Client,
    blocks: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
}

/// Run async HTTP work from synchronous provider code on a dedicated runtime.
fn run<T: Send + 'static>(
    fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    IO_RUNTIME.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv().map_err(|_| anyhow!("http task aborted"))?
}

impl HttpFile {
    fn open(url: &str) -> Result<HttpFile> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("rGIS/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let (c, u) = (client.clone(), url.to_string());
        let len = run(async move {
            let resp = c
                .get(&u)
                .header("Range", "bytes=0-0")
                .send()
                .await?
                .error_for_status()?;
            let total = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| anyhow!("{u} does not support HTTP range requests"))?;
            Ok(total)
        })
        .with_context(|| format!("cannot open /vsicurl/{url}"))?;
        info!("opened /vsicurl/{url} ({len} bytes)");
        Ok(HttpFile {
            url: url.to_string(),
            len,
            client,
            blocks: Mutex::new(HashMap::new()),
        })
    }

    fn block(&self, n: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.blocks.lock().get(&n) {
            return Ok(b.clone());
        }
        let start = n * BLOCK;
        let end = (start + BLOCK).min(self.len) - 1;
        let (c, u) = (self.client.clone(), self.url.clone());
        let data = run(async move {
            let resp = c
                .get(&u)
                .header("Range", format!("bytes={start}-{end}"))
                .send()
                .await?
                .error_for_status()?;
            Ok(resp.bytes().await?.to_vec())
        })?;
        debug!("vsicurl: fetched bytes {start}-{end} of {}", self.url);
        let data = Arc::new(data);
        let mut blocks = self.blocks.lock();
        if blocks.len() >= MAX_CACHED_BLOCKS {
            blocks.clear();
        }
        blocks.insert(n, data.clone());
        Ok(data)
    }
}

struct HttpReader {
    file: Arc<HttpFile>,
    pos: u64,
}

impl HttpReader {
    fn new(file: Arc<HttpFile>) -> HttpReader {
        HttpReader { file, pos: 0 }
    }
}

impl Read for HttpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.file.len || buf.is_empty() {
            return Ok(0);
        }
        let n = self.pos / BLOCK;
        let block = self.file.block(n).map_err(std::io::Error::other)?;
        let off = (self.pos - n * BLOCK) as usize;
        let len = buf.len().min(block.len() - off);
        buf[..len].copy_from_slice(&block[off..off + len]);
        self.pos += len as u64;
        Ok(len)
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
            SeekFrom::End(d) => self.file.len.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

// --------------------------------------------------------------- GeoPackage

fn quote(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

fn sqlite_kind(decl: &str) -> FieldKind {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        FieldKind::Int
    } else if d.contains("REAL")
        || d.contains("FLOAT")
        || d.contains("DOUBLE")
        || d.contains("NUMERIC")
    {
        FieldKind::Real
    } else if d.contains("BOOL") {
        FieldKind::Bool
    } else if d == "DATE" {
        FieldKind::Date
    } else if d.contains("DATETIME") {
        FieldKind::DateTime
    } else {
        FieldKind::String
    }
}

fn open_gpkg(path: PathBuf, layer: Option<String>, subset: Option<String>) -> Result<Provider> {
    let db =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let table: String = match layer {
        Some(l) => l,
        None => db.query_row(
            "SELECT table_name FROM gpkg_contents WHERE data_type = 'features' LIMIT 1",
            [],
            |r| r.get(0),
        )?,
    };
    let geom_col: String = db
        .query_row(
            "SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ?1",
            [&table],
            |r| r.get(0),
        )
        .with_context(|| format!("'{table}' is not a feature table in {}", path.display()))?;
    let mut fid_col = "rowid".to_string();
    let mut fields = vec![];
    let mut stmt = db.prepare(&format!("PRAGMA table_info({})", quote(&table)))?;
    let cols = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for c in cols {
        let (name, decl, pk) = c?;
        if pk == 1 {
            fid_col = name;
        } else if name != geom_col {
            fields.push(Field {
                name,
                kind: sqlite_kind(&decl),
            });
        }
    }
    drop(stmt);
    let rtree_name = format!("rtree_{table}_{geom_col}");
    let rtree = db
        .query_row(
            "SELECT name FROM sqlite_master WHERE name = ?1",
            [&rtree_name],
            |r| r.get::<_, String>(0),
        )
        .ok();
    let extent = db
        .query_row(
            "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name = ?1",
            [&table],
            |r| Ok(Rect::new(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
        .filter(|r| !r.is_empty());
    Ok(Provider {
        extent,
        description: format!("GeoPackage {}|layername={table}", path.display()),
        kind: Kind::Gpkg(GpkgSource {
            path,
            table,
            geom_col,
            fid_col,
            rtree,
            subset,
        }),
        fields,
    })
}

fn gpkg_features(g: &GpkgSource, fields: &[Field], filter: Option<Rect>) -> Result<Vec<Feature>> {
    let db =
        rusqlite::Connection::open_with_flags(&g.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let cols: String = fields
        .iter()
        .map(|f| format!(", {}", quote(&f.name)))
        .collect();
    let mut sql = format!(
        "SELECT {}, {}{cols} FROM {}",
        quote(&g.fid_col),
        quote(&g.geom_col),
        quote(&g.table)
    );
    let mut wheres = vec![];
    if let (Some(b), Some(rt)) = (filter, &g.rtree) {
        wheres.push(format!(
            "{} IN (SELECT id FROM {} WHERE minx <= {} AND maxx >= {} AND miny <= {} AND maxy >= {})",
            quote(&g.fid_col),
            quote(rt),
            b.xmax,
            b.xmin,
            b.ymax,
            b.ymin
        ));
    }
    if let Some(s) = &g.subset {
        // Subset strings come from the (trusted) project file, as in QGIS.
        wheres.push(format!("({s})"));
    }
    if !wheres.is_empty() {
        sql += &format!(" WHERE {}", wheres.join(" AND "));
    }
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = vec![];
    while let Some(row) = rows.next()? {
        let geometry = match row.get_ref(1)? {
            rusqlite::types::ValueRef::Blob(b) => geozero::wkb::GpkgWkb(b.to_vec()).to_geo().ok(),
            _ => None,
        };
        if let (Some(b), None, Some(geo)) = (filter, &g.rtree, &geometry)
            && !bbox(geo).intersects(&b)
        {
            continue;
        }
        let mut attrs = Attrs::new();
        for (i, f) in fields.iter().enumerate() {
            use rusqlite::types::ValueRef::*;
            let v = match row.get_ref(i + 2)? {
                Null => Value::Null,
                Integer(n) if f.kind == FieldKind::Bool => json!(n != 0),
                Integer(n) => json!(n),
                Real(x) => json!(x),
                Text(t) => json!(String::from_utf8_lossy(t)),
                Blob(_) => Value::Null,
            };
            attrs.insert(f.name.clone(), v);
        }
        out.push(Feature {
            fid: row.get(0)?,
            geometry,
            attrs,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- Shapefile

/// Field names and kinds from the dBase header (keeps column order).
fn dbf_fields(path: &Path) -> Result<Vec<Field>> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() < 32 {
        bail!("{} is not a dBase file", path.display());
    }
    let header_len = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let mut out = vec![];
    let mut off = 32;
    while off + 32 <= header_len.min(bytes.len()) && bytes[off] != 0x0D {
        let d = &bytes[off..off + 32];
        let name = String::from_utf8_lossy(&d[..11])
            .trim_end_matches('\0')
            .to_string();
        let kind = match d[11] {
            b'N' if d[17] == 0 => FieldKind::Int,
            b'N' | b'F' | b'B' | b'Y' | b'O' => FieldKind::Real,
            b'I' | b'+' => FieldKind::Int,
            b'L' => FieldKind::Bool,
            b'D' => FieldKind::Date,
            b'T' | b'@' => FieldKind::DateTime,
            _ => FieldKind::String,
        };
        out.push(Field { name, kind });
        off += 32;
    }
    Ok(out)
}

fn open_shapefile(path: &Path) -> Result<Provider> {
    let fields = dbf_fields(&path.with_extension("dbf"))?;
    let mut reader = shapefile::Reader::from_path(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let mut all = vec![];
    for (i, item) in reader.iter_shapes_and_records().enumerate() {
        let (shape, record) = item?;
        let geometry = Geometry::<f64>::try_from(shape).ok();
        let mut values: HashMap<String, shapefile::dbase::FieldValue> =
            record.into_iter().collect();
        let mut attrs = Attrs::new();
        for f in &fields {
            use shapefile::dbase::FieldValue as F;
            let v = match values.remove(&f.name) {
                Some(F::Character(s)) => s.map(|s| json!(s.trim_end())).unwrap_or(Value::Null),
                Some(F::Numeric(Some(n))) if f.kind == FieldKind::Int => json!(n as i64),
                Some(F::Numeric(n)) => n.map(|n| json!(n)).unwrap_or(Value::Null),
                Some(F::Float(n)) => n.map(|n| json!(n)).unwrap_or(Value::Null),
                Some(F::Logical(b)) => b.map(|b| json!(b)).unwrap_or(Value::Null),
                Some(F::Date(d)) => d
                    .map(|d| json!(format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())))
                    .unwrap_or(Value::Null),
                Some(F::Integer(n)) => json!(n),
                Some(F::Double(n)) | Some(F::Currency(n)) => json!(n),
                Some(F::Memo(s)) => json!(s),
                Some(other) => json!(format!("{other:?}")),
                None => Value::Null,
            };
            attrs.insert(f.name.clone(), v);
        }
        let r = geometry.as_ref().map(bbox).unwrap_or(Rect::empty());
        all.push((
            r,
            Feature {
                fid: i as i64,
                geometry,
                attrs,
            },
        ));
    }
    info!("loaded {} features from {}", all.len(), path.display());
    Ok(Provider {
        extent: memory_extent(&all),
        kind: Kind::Memory(all),
        fields,
        description: format!("Shapefile {}", path.display()),
    })
}

// ------------------------------------------------------------------ GeoJSON

fn coord(v: &Value) -> Option<Coord<f64>> {
    let a = v.as_array()?;
    Some(Coord {
        x: a.first()?.as_f64()?,
        y: a.get(1)?.as_f64()?,
    })
}

fn line(v: &Value) -> Option<LineString<f64>> {
    Some(v.as_array()?.iter().filter_map(coord).collect())
}

fn poly(v: &Value) -> Option<Polygon<f64>> {
    let mut rings = v.as_array()?.iter().filter_map(line);
    Some(Polygon::new(rings.next()?, rings.collect()))
}

pub fn geojson_geometry(g: &Value) -> Option<Geometry<f64>> {
    let items = || g.get("coordinates").and_then(|c| c.as_array());
    Some(match g.get("type")?.as_str()? {
        "Point" => Point(coord(g.get("coordinates")?)?).into(),
        "LineString" => line(g.get("coordinates")?)?.into(),
        "Polygon" => poly(g.get("coordinates")?)?.into(),
        "MultiPoint" => MultiPoint(
            items()?
                .iter()
                .filter_map(|v| coord(v).map(Point))
                .collect(),
        )
        .into(),
        "MultiLineString" => MultiLineString(items()?.iter().filter_map(line).collect()).into(),
        "MultiPolygon" => MultiPolygon(items()?.iter().filter_map(poly).collect()).into(),
        "GeometryCollection" => Geometry::GeometryCollection(
            g.get("geometries")?
                .as_array()?
                .iter()
                .filter_map(geojson_geometry)
                .collect(),
        ),
        _ => return None,
    })
}

fn open_geojson(path: &Path) -> Result<Provider> {
    let doc: Value = serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("invalid GeoJSON {}", path.display()))?;
    let features = doc
        .get("features")
        .and_then(|f| f.as_array())
        .ok_or_else(|| anyhow!("not a FeatureCollection"))?;
    let mut fields: Vec<Field> = vec![];
    let mut all = vec![];
    for (i, f) in features.iter().enumerate() {
        let mut attrs = Attrs::new();
        if let Some(props) = f.get("properties").and_then(|p| p.as_object()) {
            for (k, v) in props {
                if !fields.iter().any(|f| f.name == *k) {
                    let kind = match v {
                        Value::Number(n) if n.is_i64() => FieldKind::Int,
                        Value::Number(_) => FieldKind::Real,
                        Value::Bool(_) => FieldKind::Bool,
                        _ => FieldKind::String,
                    };
                    fields.push(Field {
                        name: k.clone(),
                        kind,
                    });
                }
                attrs.insert(k.clone(), v.clone());
            }
        }
        let geometry = f.get("geometry").and_then(geojson_geometry);
        let r = geometry.as_ref().map(bbox).unwrap_or(Rect::empty());
        let fid = f.get("id").and_then(|v| v.as_i64()).unwrap_or(i as i64);
        all.push((
            r,
            Feature {
                fid,
                geometry,
                attrs,
            },
        ));
    }
    for (_, f) in all.iter_mut() {
        for fl in &fields {
            f.attrs.entry(fl.name.clone()).or_insert(Value::Null);
        }
    }
    Ok(Provider {
        extent: memory_extent(&all),
        kind: Kind::Memory(all),
        fields,
        description: format!("GeoJSON {}", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ogr_uris() {
        let u = parse_uri("./data.gpkg|layername=roads|subset=\"type\" = 1");
        assert_eq!(u.path, "./data.gpkg");
        assert_eq!(u.layer.as_deref(), Some("roads"));
        assert_eq!(u.subset.as_deref(), Some("\"type\" = 1"));
        let u = parse_uri("GPKG:./x.gpkg:osm");
        assert_eq!(
            (u.path.as_str(), u.layer.as_deref()),
            ("./x.gpkg", Some("osm"))
        );
    }
}
