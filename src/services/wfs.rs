//! WFS 1.1.0 (QGIS services/wfs), read-only. Only layers listed in the project's
//! `WFSLayers` property are published, as in QGIS.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::crs::{Crs, Transform};
use crate::expression::{Expr, to_str};
use crate::geom::{self, Rect};
use crate::project::{MapLayer, Project};
use crate::provider::Feature;
use crate::server::{Context, OwsError, Response, Service, ServiceRegistry, esc};

pub struct Wfs;

pub fn register(r: &mut ServiceRegistry) {
    r.register(Arc::new(Wfs));
}

impl Service for Wfs {
    fn name(&self) -> &'static str {
        "WFS"
    }
    fn version(&self) -> &'static str {
        "1.1.0"
    }
    fn execute(&self, ctx: &Context) -> Result<Response, OwsError> {
        let req = ctx.params.get("REQUEST").unwrap_or("");
        let is = |n: &str| req.eq_ignore_ascii_case(n);
        if req.is_empty() {
            return Err(OwsError::new(
                "OperationNotSupported",
                "Please add or check the value of the REQUEST parameter",
                501,
            ));
        }
        if is("GetCapabilities") {
            Ok(Response::ok("text/xml; charset=utf-8", capabilities(ctx)))
        } else if is("DescribeFeatureType") {
            describe(ctx)
        } else if is("GetFeature") {
            get_feature(ctx)
        } else {
            Err(OwsError::new(
                "OperationNotSupported",
                format!("Request {req} is not supported"),
                501,
            ))
        }
    }
}

/// WFS type names are layer nicknames with spaces replaced (QgsWfs typeName handling).
pub fn type_name(project: &Project, l: &MapLayer) -> String {
    project.nickname(l).replace(' ', "_")
}

pub fn published(project: &Project, name: &str) -> Option<Arc<MapLayer>> {
    let name = name.split_once(':').map(|(_, n)| n).unwrap_or(name);
    project
        .wfs_layers()
        .into_iter()
        .find(|l| type_name(project, l) == name)
}

fn capabilities(ctx: &Context) -> String {
    let project = ctx.project;
    let url = esc(&ctx.service_url);
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<WFS_Capabilities version=\"1.1.0\" xmlns=\"http://www.opengis.net/wfs\" xmlns:gml=\"http://www.opengis.net/gml\" xmlns:ogc=\"http://www.opengis.net/ogc\" xmlns:ows=\"http://www.opengis.net/ows\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://www.opengis.net/wfs http://schemas.opengis.net/wfs/1.1.0/wfs.xsd\">\n",
    );
    let _ = writeln!(
        s,
        " <ows:ServiceIdentification>\n  <ows:Title>{}</ows:Title>\n  <ows:Abstract>{}</ows:Abstract>\n  <ows:ServiceType>WFS</ows:ServiceType>\n  <ows:ServiceTypeVersion>1.1.0</ows:ServiceTypeVersion>\n  <ows:Fees>{}</ows:Fees>\n  <ows:AccessConstraints>{}</ows:AccessConstraints>\n </ows:ServiceIdentification>",
        esc(&project.service_title()),
        esc(project.text_prop("WMSServiceAbstract").unwrap_or("")),
        esc(project.text_prop("WMSFees").unwrap_or("conditions unknown")),
        esc(project.text_prop("WMSAccessConstraints").unwrap_or("None"))
    );
    s += " <ows:OperationsMetadata>\n";
    for (op, formats) in [
        ("GetCapabilities", vec!["text/xml"]),
        (
            "DescribeFeatureType",
            vec!["XMLSCHEMA", "text/xml; subtype=gml/3.1.1"],
        ),
        (
            "GetFeature",
            vec![
                "text/xml; subtype=gml/3.1.1",
                "application/json",
                "application/vnd.geo+json",
                "application/geo+json",
            ],
        ),
    ] {
        let _ = writeln!(
            s,
            "  <ows:Operation name=\"{op}\">\n   <ows:DCP><ows:HTTP><ows:Get xlink:href=\"{url}\"/></ows:HTTP></ows:DCP>"
        );
        let _ = writeln!(
            s,
            "   <ows:Parameter name=\"outputFormat\">{}</ows:Parameter>\n  </ows:Operation>",
            formats
                .iter()
                .map(|f| format!("<ows:Value>{}</ows:Value>", esc(f)))
                .collect::<String>()
        );
    }
    s += " </ows:OperationsMetadata>\n <FeatureTypeList>\n  <Operations><Operation>Query</Operation></Operations>\n";
    for l in project.wfs_layers() {
        let crs = l.crs();
        s += "  <FeatureType>\n";
        let _ = writeln!(s, "   <Name>{}</Name>", esc(&type_name(project, &l)));
        let _ = writeln!(s, "   <Title>{}</Title>", esc(l.display_title()));
        let _ = writeln!(
            s,
            "   <Abstract>{}</Abstract>",
            esc(l.abstract_.as_deref().unwrap_or(""))
        );
        let _ = writeln!(s, "   <DefaultSRS>{}</DefaultSRS>", crs.urn());
        s += "   <OutputFormats><Format>text/xml; subtype=gml/3.1.1</Format><Format>application/json</Format></OutputFormats>\n";
        if let Some(w) = l.wgs84() {
            let r = |v: f64| geom::round(v, 6);
            let _ = writeln!(
                s,
                "   <ows:WGS84BoundingBox dimensions=\"2\"><ows:LowerCorner>{} {}</ows:LowerCorner><ows:UpperCorner>{} {}</ows:UpperCorner></ows:WGS84BoundingBox>",
                r(w.xmin),
                r(w.ymin),
                r(w.xmax),
                r(w.ymax)
            );
        }
        s += "  </FeatureType>\n";
    }
    s += " </FeatureTypeList>\n <ogc:Filter_Capabilities>\n  <ogc:Spatial_Capabilities><ogc:GeometryOperands><ogc:GeometryOperand>gml:Envelope</ogc:GeometryOperand></ogc:GeometryOperands><ogc:SpatialOperators><ogc:SpatialOperator name=\"BBOX\"/></ogc:SpatialOperators></ogc:Spatial_Capabilities>\n  <ogc:Id_Capabilities><ogc:FID/></ogc:Id_Capabilities>\n </ogc:Filter_Capabilities>\n</WFS_Capabilities>\n";
    s
}

/// Field names become element names; replace characters that are not valid in an
/// XML NCName (QGIS writes e.g. `<qgs:feature::id>`, which is not well-formed).
pub fn xml_name(n: &str) -> String {
    let mut s: String = n
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        s.insert(0, '_');
    }
    s
}

fn gml_geometry_type(l: &MapLayer) -> &'static str {
    match l.geometry.as_str() {
        "Point" => "gml:MultiPointPropertyType",
        "Line" => "gml:MultiCurvePropertyType",
        "Polygon" => "gml:MultiSurfacePropertyType",
        _ => "gml:GeometryPropertyType",
    }
}

fn type_layers(ctx: &Context, required: bool) -> Result<Vec<Arc<MapLayer>>, OwsError> {
    let mut names = ctx.params.list("TYPENAME");
    names.extend(ctx.params.list("TYPENAMES"));
    if names.is_empty() {
        if required {
            return Err(OwsError::bad(
                "RequestNotWellFormed",
                "TYPENAME is mandatory",
            ));
        }
        return Ok(ctx.project.wfs_layers());
    }
    names
        .iter()
        .map(|n| {
            published(ctx.project, n).ok_or_else(|| {
                OwsError::bad(
                    "RequestNotWellFormed",
                    format!("TypeName '{n}' could not be found"),
                )
            })
        })
        .collect()
}

fn describe(ctx: &Context) -> Result<Response, OwsError> {
    let layers = type_layers(ctx, false)?;
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<schema xmlns=\"http://www.w3.org/2001/XMLSchema\" xmlns:qgs=\"http://www.qgis.org/gml\" xmlns:gml=\"http://www.opengis.net/gml\" targetNamespace=\"http://www.qgis.org/gml\" elementFormDefault=\"qualified\" version=\"1.0\">\n <import namespace=\"http://www.opengis.net/gml\" schemaLocation=\"http://schemas.opengis.net/gml/3.1.1/base/gml.xsd\"/>\n",
    );
    for l in layers {
        let tn = esc(&type_name(ctx.project, &l));
        let _ = writeln!(
            s,
            " <element name=\"{tn}\" type=\"qgs:{tn}Type\" substitutionGroup=\"gml:_Feature\"/>"
        );
        let _ = writeln!(
            s,
            " <complexType name=\"{tn}Type\">\n  <complexContent>\n   <extension base=\"gml:AbstractFeatureType\">\n    <sequence>"
        );
        let _ = writeln!(
            s,
            "     <element name=\"geometry\" type=\"{}\" minOccurs=\"0\" maxOccurs=\"1\"/>",
            gml_geometry_type(&l)
        );
        for f in l
            .provider()?
            .fields
            .iter()
            .filter(|f| !l.hidden_wfs.contains(&f.name))
        {
            let _ = writeln!(
                s,
                "     <element name=\"{}\" type=\"{}\" nillable=\"true\"/>",
                xml_name(&f.name),
                f.kind.xsd()
            );
        }
        s += "    </sequence>\n   </extension>\n  </complexContent>\n </complexType>\n";
    }
    s += "</schema>\n";
    Ok(Response::ok(
        "text/xml; subtype=gml/3.1.1; charset=utf-8",
        s,
    ))
}

/// A feature query against one published layer (shared with the OGC API).
pub struct Query {
    pub bbox: Option<(Rect, Crs)>,
    pub fids: Option<Vec<i64>>,
    pub filter: Option<Expr>,
    pub properties: Option<Vec<String>>,
    pub out_crs: Crs,
}

pub fn run_query(l: &MapLayer, q: &Query) -> Result<Vec<Feature>, OwsError> {
    let layer_crs = l.crs();
    let search = match &q.bbox {
        Some((r, crs)) => Some(
            Transform::new(crs, &layer_crs)
                .rect(r)
                .map_err(|e| OwsError::bad("InvalidParameterValue", format!("BBOX: {e:#}")))?,
        ),
        None => None,
    };
    let mut fs = l.provider()?.features(search)?;
    if let Some(s) = search {
        fs.retain(|f| {
            f.geometry
                .as_ref()
                .is_some_and(|g| geom::intersects_rect(g, &s))
        });
    }
    if let Some(ids) = &q.fids {
        fs.retain(|f| ids.contains(&f.fid));
    }
    if let Some(e) = &q.filter {
        fs.retain(|f| e.matches(&f.attrs));
    }
    let t = Transform::new(&layer_crs, &q.out_crs);
    // As in QGIS, a PROPERTYNAME list without the geometry drops the geometry.
    let keep_geometry = q
        .properties
        .as_ref()
        .is_none_or(|p| p.iter().any(|n| n.eq_ignore_ascii_case("geometry")));
    for f in &mut fs {
        if !keep_geometry {
            f.geometry = None;
        }
        f.attrs.retain(|k, _| {
            !l.hidden_wfs.contains(k)
                && q.properties
                    .as_ref()
                    .is_none_or(|p| p.iter().any(|n| n == k))
        });
        if !t.is_identity() {
            f.geometry = f.geometry.as_ref().map(|g| geom::transform(g, &t));
        }
    }
    Ok(fs)
}

pub fn geojson_feature(id: String, f: &Feature, precision: usize) -> Value {
    let props: serde_json::Map<String, Value> = f
        .attrs
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut o = json!({"type": "Feature", "id": id, "geometry": Value::Null, "properties": props});
    if let Some(g) = &f.geometry {
        let b = geom::bbox(g);
        if !b.is_empty() {
            o["bbox"] = json!([
                geom::jnum(b.xmin, precision),
                geom::jnum(b.ymin, precision),
                geom::jnum(b.xmax, precision),
                geom::jnum(b.ymax, precision)
            ]);
        }
        o["geometry"] = geom::to_geojson(g, precision);
    }
    o
}

fn get_feature(ctx: &Context) -> Result<Response, OwsError> {
    let p = ctx.params;
    let mut layers = type_layers(ctx, false)?;
    let fid_param = p.list("FEATUREID");
    if p.list("TYPENAME").is_empty() && p.list("TYPENAMES").is_empty() {
        if fid_param.is_empty() {
            return Err(OwsError::bad(
                "RequestNotWellFormed",
                "TYPENAME or FEATUREID is mandatory",
            ));
        }
        layers.retain(|l| {
            fid_param
                .iter()
                .any(|f| f.starts_with(&format!("{}.", type_name(ctx.project, l))))
        });
    }
    let out_crs_param = p.get("SRSNAME");
    let bbox = match p.get("BBOX") {
        Some(b) => {
            let r = Rect::parse(b).ok_or_else(|| {
                OwsError::bad("InvalidParameterValue", format!("Invalid BBOX '{b}'"))
            })?;
            // QGIS reads KVP BBOX values as x,y regardless of the CRS suffix.
            let crs_id = b.split(',').nth(4).map(str::trim).or(out_crs_param);
            Some((r, crs_id))
        }
        None => None,
    };
    let filter = match p.get("EXP_FILTER") {
        Some(e) => Some(Expr::parse(e).map_err(|err| {
            OwsError::bad(
                "InvalidParameterValue",
                format!("The EXP_FILTER expression has errors: {err}"),
            )
        })?),
        None => None,
    };
    let max = p
        .f64("MAXFEATURES")
        .or_else(|| p.f64("COUNT"))
        .map(|v| v as usize)
        .unwrap_or(usize::MAX);
    let start = p.f64("STARTINDEX").map(|v| v as usize).unwrap_or(0);
    let properties = Some(p.list("PROPERTYNAME")).filter(|v| !v.is_empty());
    let format = p
        .get_or("OUTPUTFORMAT", "text/xml; subtype=gml/3.1.1")
        .to_ascii_lowercase();
    let geojson = format.contains("json");
    if !geojson && !(format.contains("gml/3") || format == "text/xml" || format == "gml3") {
        return Err(OwsError::bad(
            "InvalidParameterValue",
            format!("OUTPUTFORMAT '{format}' is not supported"),
        ));
    }

    let mut results = vec![];
    for l in &layers {
        let tn = type_name(ctx.project, l);
        let out_crs = match out_crs_param {
            Some(c) => Crs::from_ogc(c)
                .map_err(|e| OwsError::bad("InvalidParameterValue", format!("SRSNAME: {e:#}")))?,
            None => l.crs(),
        };
        let bbox = match &bbox {
            Some((r, crs_id)) => Some((
                *r,
                match crs_id {
                    Some(c) => Crs::from_ogc(c).map_err(|e| {
                        OwsError::bad("InvalidParameterValue", format!("BBOX CRS: {e:#}"))
                    })?,
                    None => l.crs(),
                },
            )),
            None => None,
        };
        let fids = (!fid_param.is_empty()).then(|| {
            fid_param
                .iter()
                .filter_map(|f| f.strip_prefix(&format!("{tn}.")))
                .filter_map(|f| f.parse().ok())
                .collect()
        });
        let q = Query {
            bbox,
            fids,
            filter: filter.clone(),
            properties: properties.clone(),
            out_crs,
        };
        let fs = run_query(l, &q)?;
        results.push((l.clone(), tn, q.out_crs, fs));
    }
    // MAXFEATURES / STARTINDEX apply to the whole response.
    let mut skip = start;
    let mut left = max;
    for (_, _, _, fs) in results.iter_mut() {
        let drop_n = skip.min(fs.len());
        fs.drain(..drop_n);
        skip -= drop_n;
        fs.truncate(left);
        left -= fs.len();
    }

    // Like QGIS, the collection extent is the requested BBOX, else the layers' extents.
    let ext = match &bbox {
        Some((r, _)) => *r,
        None => {
            let mut e = Rect::empty();
            for (l, _, crs, _) in &results {
                if let Some(r) = l
                    .full_extent()
                    .and_then(|x| Transform::new(&l.crs(), crs).rect(&x).ok())
                {
                    e.union(&r);
                }
            }
            e
        }
    };
    if geojson {
        let mut all = vec![];
        let mut precision = 6;
        for (l, tn, _, fs) in &results {
            precision = ctx.project.wfs_precision(l);
            for f in fs {
                all.push(geojson_feature(format!("{tn}.{}", f.fid), f, precision));
            }
        }
        let mut doc = json!({"type": "FeatureCollection", "features": all});
        if !ext.is_empty() {
            doc["bbox"] = json!([
                geom::jnum(ext.xmin, precision),
                geom::jnum(ext.ymin, precision),
                geom::jnum(ext.xmax, precision),
                geom::jnum(ext.ymax, precision)
            ]);
        }
        return Ok(Response {
            status: 200,
            content_type: "application/vnd.geo+json; charset=utf-8".into(),
            body: serde_json::to_vec(&doc).unwrap_or_default(),
        });
    }

    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<wfs:FeatureCollection xmlns:wfs=\"http://www.opengis.net/wfs\" xmlns:ogc=\"http://www.opengis.net/ogc\" xmlns:gml=\"http://www.opengis.net/gml\" xmlns:ows=\"http://www.opengis.net/ows\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:qgs=\"http://www.qgis.org/gml\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n",
    );
    let envelope = |r: &Rect, crs: &Crs, precision: usize, ind: &str| -> String {
        let a = if crs.axis_inverted() { r.swapped() } else { *r };
        format!(
            "{ind}<gml:boundedBy>\n{ind} <gml:Envelope srsName=\"{}\">\n{ind}  <gml:lowerCorner>{} {}</gml:lowerCorner>\n{ind}  <gml:upperCorner>{} {}</gml:upperCorner>\n{ind} </gml:Envelope>\n{ind}</gml:boundedBy>\n",
            crs.urn(),
            geom::round(a.xmin, precision),
            geom::round(a.ymin, precision),
            geom::round(a.xmax, precision),
            geom::round(a.ymax, precision)
        )
    };
    if let (false, Some((l, _, crs, _))) = (ext.is_empty(), results.first()) {
        s += &envelope(&ext, crs, ctx.project.wfs_precision(l), "");
    }
    for (l, tn, crs, fs) in &results {
        let precision = ctx.project.wfs_precision(l);
        let tn = esc(tn);
        for f in fs {
            let _ = writeln!(
                s,
                "<gml:featureMember>\n <qgs:{tn} gml:id=\"{tn}.{}\">",
                f.fid
            );
            if let Some(g) = &f.geometry {
                s += &envelope(&geom::bbox(g), crs, precision, "  ");
                let _ = writeln!(
                    s,
                    "  <qgs:geometry>{}</qgs:geometry>",
                    geom::to_gml3(g, &crs.urn(), precision, crs.axis_inverted())
                );
            }
            for (k, v) in &f.attrs {
                let k = xml_name(k);
                if v.is_null() {
                    let _ = writeln!(s, "  <qgs:{k} xsi:nil=\"true\"/>");
                } else {
                    let _ = writeln!(s, "  <qgs:{k}>{}</qgs:{k}>", esc(&to_str(v)));
                }
            }
            let _ = writeln!(s, " </qgs:{tn}>\n</gml:featureMember>");
        }
    }
    s += "</wfs:FeatureCollection>\n";
    Ok(Response::ok(
        "text/xml; subtype=gml/3.1.1; charset=utf-8",
        s,
    ))
}
