//! OGC API – Features under `/wfs3` (QGIS services/wfs3), publishing the WFS layers.

use serde_json::{Value, json};

use crate::crs::Crs;
use crate::expression::Expr;
use crate::geom::Rect;
use crate::params::Params;
use crate::project::Project;
use crate::server::{OwsError, Request, Response};
use crate::services::wfs::{Query, geojson_feature, published, run_query, type_name};

fn error(status: u16, msg: impl Into<String>) -> Response {
    let mut r = Response::json(&json!([{"code": status.to_string(), "description": msg.into()}]));
    r.status = status;
    r
}

pub fn handle(
    req: &Request,
    params: &Params,
    project: Option<&Project>,
) -> Result<Response, OwsError> {
    let Some(project) = project else {
        return Ok(error(
            404,
            "Project not found, please check your MAP parameter or QGIS_PROJECT_FILE",
        ));
    };
    let root = req
        .base_url
        .strip_suffix(&req.path)
        .unwrap_or(&req.base_url)
        .to_string()
        + "/wfs3";
    let map = params
        .get("MAP")
        .map(|m| format!("?MAP={}", urlencoding::encode(m)))
        .unwrap_or_default();
    let link = |path: &str, rel: &str, ty: &str, title: &str| json!({"href": format!("{root}{path}{map}"), "rel": rel, "type": ty, "title": title});

    let path = req.path.strip_prefix("/wfs3").unwrap_or("");
    let path = path.trim_end_matches('/');
    let path = path
        .strip_suffix(".json")
        .or_else(|| path.strip_suffix(".geojson"))
        .unwrap_or(path);
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();

    let collection = |l: &crate::project::MapLayer| {
        let id = type_name(project, l);
        let mut c = json!({
            "id": id,
            "title": l.display_title(),
            "description": l.abstract_.clone().unwrap_or_default(),
            "crs": ["http://www.opengis.net/def/crs/OGC/1.3/CRS84"],
            "links": [link(&format!("/collections/{id}/items.geojson"), "items", "application/geo+json", "Features")],
        });
        if let Some(w) = l.wgs84() {
            c["extent"] = json!({"spatial": {"bbox": [[w.xmin, w.ymin, w.xmax, w.ymax]], "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"}});
        }
        c
    };

    Ok(match parts.as_slice() {
        [] => Response::json(&json!({
            "title": project.service_title(),
            "description": project.text_prop("WMSServiceAbstract").unwrap_or(""),
            "links": [
                link("", "self", "application/json", "This document"),
                link("/conformance", "conformance", "application/json", "Conformance classes"),
                link("/collections", "data", "application/json", "Feature collections"),
            ]
        })),
        ["conformance"] => Response::json(&json!({"conformsTo": [
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson"
        ]})),
        ["collections"] => Response::json(&json!({
            "collections": project.wfs_layers().iter().map(|l| collection(l)).collect::<Vec<_>>(),
            "links": [link("/collections", "self", "application/json", "Feature collections")]
        })),
        ["collections", id] => match published(project, id) {
            Some(l) => Response::json(&collection(&l)),
            None => error(404, format!("Collection '{id}' not found")),
        },
        ["collections", id, "items", rest @ ..] => {
            let Some(l) = published(project, id) else {
                return Ok(error(404, format!("Collection '{id}' not found")));
            };
            let crs84 = Crs::from_ogc("CRS:84").expect("CRS:84");
            if let [fid] = rest {
                let Ok(fid) = fid.parse::<i64>() else {
                    return Ok(error(400, "Invalid feature id"));
                };
                let q = Query {
                    bbox: None,
                    fids: Some(vec![fid]),
                    filter: None,
                    properties: None,
                    out_crs: crs84,
                };
                let fs = run_query(&l, &q)?;
                return Ok(match fs.first() {
                    Some(f) => geo(geojson_feature(
                        format!("{id}.{fid}"),
                        f,
                        project.wfs_precision(&l),
                    )),
                    None => error(404, format!("Feature {fid} not found")),
                });
            }
            let limit = params
                .f64("limit")
                .map(|v| v as usize)
                .unwrap_or(10)
                .clamp(1, 10_000);
            let offset = params.f64("offset").map(|v| v as usize).unwrap_or(0);
            let bbox = match params.get("bbox") {
                Some(b) => Some((
                    Rect::parse(b)
                        .ok_or_else(|| OwsError::bad("InvalidParameterValue", "invalid bbox"))?,
                    crs84.clone(),
                )),
                None => None,
            };
            // Attribute equality filters (queryables), e.g. ?NAME=Norway
            let fields = l.provider()?.fields.clone();
            let conds: Vec<String> = params
                .iter()
                .filter_map(|(k, v)| {
                    fields
                        .iter()
                        .find(|f| f.name.eq_ignore_ascii_case(k))
                        .map(|f| format!("\"{}\" = '{}'", f.name, v.replace('\'', "''")))
                })
                .collect();
            let filter = if conds.is_empty() {
                None
            } else {
                Some(Expr::parse(&conds.join(" AND ")).map_err(OwsError::internal)?)
            };
            let q = Query {
                bbox,
                fids: None,
                filter,
                properties: None,
                out_crs: crs84,
            };
            let all = run_query(&l, &q)?;
            let matched = all.len();
            let precision = project.wfs_precision(&l);
            let page: Vec<Value> = all
                .iter()
                .skip(offset)
                .take(limit)
                .map(|f| geojson_feature(format!("{id}.{}", f.fid), f, precision))
                .collect();
            let mut links = vec![link(
                &format!("/collections/{id}/items.geojson"),
                "self",
                "application/geo+json",
                "This page",
            )];
            if offset + page.len() < matched {
                let sep = if map.is_empty() { "?" } else { "&" };
                links.push(json!({"href": format!("{root}/collections/{id}/items.geojson{map}{sep}offset={}&limit={limit}", offset + limit), "rel": "next", "type": "application/geo+json", "title": "Next page"}));
            }
            geo(json!({
                "type": "FeatureCollection",
                "numberMatched": matched,
                "numberReturned": page.len(),
                "features": page,
                "links": links,
            }))
        }
        _ => error(404, format!("Path {} not found", req.path)),
    })
}

fn geo(v: Value) -> Response {
    let mut r = Response::json(&v);
    r.content_type = "application/geo+json".into();
    r
}
