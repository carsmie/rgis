//! PostGIS layers (QgsPostgresProvider): QGIS `postgres` datasources, read with the
//! query QGIS renders with — `geom && ST_MakeEnvelope(bbox)` plus the datasource's `sql=`
//! clause, rows in database order.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use geozero::ToGeo;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::expression::Attrs;
use crate::geom::Rect;
use crate::provider::{Feature, Field, FieldKind, run, spawn_io};

/// A parsed QgsDataSourceUri: `key='value'` pairs, `table="schema"."table" (geom)`, `sql=…`.
#[derive(Debug, Default)]
pub struct PgUri {
    params: HashMap<String, String>,
    schema: String,
    table: String,
    geom: String,
    sql: Option<String>,
}

impl PgUri {
    pub fn parse(ds: &str) -> Result<PgUri> {
        let mut uri = PgUri::default();
        let mut rest = ds.trim();
        while !rest.is_empty() {
            // `sql=` runs to the end of the string (QgsDataSourceUri::setEncodedUri).
            if let Some(sql) = rest.strip_prefix("sql=") {
                let sql = sql.trim();
                uri.sql = (!sql.is_empty()).then(|| sql.to_string());
                break;
            }
            let (key, after) = rest
                .split_once('=')
                .ok_or_else(|| anyhow!("bad postgres datasource near '{rest}'"))?;
            let key = key.trim().to_string();
            let after = after.trim_start();
            let (value, tail) = if key == "table" {
                // "schema"."table" (geomcol)
                let (schema, t) = quoted(after, '"')?;
                let t = t
                    .strip_prefix('.')
                    .ok_or_else(|| anyhow!("table without schema"))?;
                let (table, t) = quoted(t, '"')?;
                uri.schema = schema;
                uri.table = table;
                let t = t.trim_start();
                let t = match t.strip_prefix('(') {
                    Some(g) => {
                        let (col, t) = g
                            .split_once(')')
                            .ok_or_else(|| anyhow!("unclosed geometry column"))?;
                        uri.geom = col.trim().trim_matches('"').to_string();
                        t
                    }
                    None => t,
                };
                (String::new(), t)
            } else if after.starts_with('\'') {
                quoted(after, '\'')?
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (after[..end].to_string(), &after[end..])
            };
            if key != "table" {
                uri.params.insert(key, value);
            }
            rest = tail.trim_start();
        }
        if uri.table.is_empty() || uri.geom.is_empty() {
            bail!("postgres datasource without table/geometry column");
        }
        Ok(uri)
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.params
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// tokio-postgres connection string; PGUSER / PGPASSWORD override the datasource's
    /// credentials (a pod can take them from a secret instead of the project file).
    fn conninfo(&self) -> Result<String> {
        if let Some(s) = self.get("service") {
            bail!("pg_service '{s}' is not supported; give host/dbname/user in the datasource");
        }
        if self.get("authcfg").is_some() {
            bail!("QGIS authcfg is not supported; set PGUSER/PGPASSWORD");
        }
        let esc = |v: &str| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"));
        let mut parts = vec![];
        for k in ["host", "port", "dbname"] {
            if let Some(v) = self.get(k) {
                parts.push(format!("{k}={}", esc(v)));
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .or_else(|| self.get("user").map(String::from));
        let password = std::env::var("PGPASSWORD")
            .ok()
            .or_else(|| self.get("password").map(String::from));
        if let Some(u) = user {
            parts.push(format!("user={}", esc(&u)));
        }
        if let Some(p) = password {
            parts.push(format!("password={}", esc(&p)));
        }
        parts.push("connect_timeout=5".into());
        parts.push("application_name=rGIS".into());
        Ok(parts.join(" "))
    }

    /// host:port/dbname, for logs (never the credentials).
    fn display(&self) -> String {
        format!(
            "{}:{}/{} {}.{}",
            self.get("host").unwrap_or("localhost"),
            self.get("port").unwrap_or("5432"),
            self.get("dbname").unwrap_or(""),
            self.schema,
            self.table
        )
    }
}

/// `'…'` / `"…"` with backslash escapes; returns (unquoted, rest).
fn quoted(s: &str, q: char) -> Result<(String, &str)> {
    let s = s
        .strip_prefix(q)
        .ok_or_else(|| anyhow!("expected {q} in '{s}'"))?;
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            if let Some((_, n)) = chars.next() {
                out.push(n);
            }
        } else if c == q {
            return Ok((out, &s[i + 1..]));
        } else {
            out.push(c);
        }
    }
    bail!("unterminated {q}")
}

fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// One client per connection string; tokio-postgres pipelines concurrent queries on it.
static CLIENTS: std::sync::LazyLock<Mutex<HashMap<String, Arc<tokio_postgres::Client>>>> =
    std::sync::LazyLock::new(Default::default);

fn client(conninfo: &str, name: &str) -> Result<Arc<tokio_postgres::Client>> {
    if let Some(c) = CLIENTS.lock().get(conninfo)
        && !c.is_closed()
    {
        return Ok(c.clone());
    }
    let ci = conninfo.to_string();
    let c = run(async move {
        let (client, connection) = tokio_postgres::connect(&ci, tokio_postgres::NoTls).await?;
        spawn_io(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection closed: {e}");
            }
        });
        // A runaway query must not hold a render slot forever.
        client
            .batch_execute("SET statement_timeout = 30000")
            .await?;
        Ok(Arc::new(client))
    })
    .with_context(|| format!("cannot connect to postgres {name}"))?;
    info!("connected to postgres {name}");
    CLIENTS.lock().insert(conninfo.to_string(), c.clone());
    Ok(c)
}

pub struct PgSource {
    conninfo: String,
    display: String,
    /// SELECT … FROM … (without WHERE), and the extra conditions.
    select: String,
    geom: String,
    srid: i32,
    sql: Option<String>,
    /// Attribute columns (as text in the query) and their kinds; fid from `key`.
    columns: Vec<Field>,
    key: Option<String>,
}

fn kind_of(pg_type: &str) -> FieldKind {
    match pg_type {
        "smallint" | "integer" | "bigint" => FieldKind::Int,
        "real" | "double precision" | "numeric" => FieldKind::Real,
        "boolean" => FieldKind::Bool,
        "date" => FieldKind::Date,
        t if t.starts_with("timestamp") => FieldKind::DateTime,
        _ => FieldKind::String,
    }
}

impl PgSource {
    pub fn open(datasource: &str) -> Result<(PgSource, Vec<Field>, String)> {
        let uri = PgUri::parse(datasource)?;
        if let Some(m) = uri.get("sslmode")
            && !matches!(m, "disable" | "allow" | "prefer")
        {
            bail!("postgres sslmode={m} is not supported (no TLS in rGIS)");
        }
        let conninfo = uri.conninfo()?;
        let display = uri.display();
        let c = client(&conninfo, &display)?;
        let (schema, table, geom) = (uri.schema.clone(), uri.table.clone(), uri.geom.clone());
        let rows = run(async move {
            Ok(c.query(
                "SELECT column_name::text, data_type::text FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 AND column_name <> $3 \
                 AND udt_name NOT IN ('geometry', 'geography', 'raster') ORDER BY ordinal_position",
                &[&schema, &table, &geom],
            )
            .await?)
        })
        .with_context(|| format!("cannot read columns of {display}"))?;
        let columns: Vec<Field> = rows
            .iter()
            .map(|r| Field {
                name: r.get::<_, String>(0),
                kind: kind_of(&r.get::<_, String>(1)),
            })
            .collect();
        if columns.is_empty() {
            bail!("postgres table {display} not found or has no columns");
        }
        let srid = uri.get("srid").and_then(|s| s.parse().ok()).unwrap_or(0);
        let key = uri.get("key").map(|k| k.trim_matches('"').to_string());
        let cols: Vec<String> = columns
            .iter()
            .map(|f| format!("{}::text", ident(&f.name)))
            .collect();
        let select = format!(
            "SELECT ST_AsBinary({g}), {cols} FROM {s}.{t}",
            g = ident(&uri.geom),
            cols = cols.join(", "),
            s = ident(&uri.schema),
            t = ident(&uri.table)
        );
        let fields = columns.clone();
        let description = format!("PostGIS {display}");
        Ok((
            PgSource {
                conninfo,
                display,
                select,
                geom: uri.geom.clone(),
                srid,
                sql: uri.sql.clone(),
                columns,
                key,
            },
            fields,
            description,
        ))
    }

    pub fn features(
        &self,
        bbox: Option<Rect>,
        keep: Option<&dyn Fn(&Attrs) -> bool>,
    ) -> Result<Vec<Feature>> {
        let mut conds = vec![];
        if let Some(b) = bbox {
            // QgsPostgresFeatureIterator::whereClauseRect: bbox only, as QGIS renders.
            conds.push(format!(
                "{} && ST_MakeEnvelope({}, {}, {}, {}, {})",
                ident(&self.geom),
                b.xmin,
                b.ymin,
                b.xmax,
                b.ymax,
                self.srid
            ));
        }
        if let Some(sql) = &self.sql {
            conds.push(format!("({sql})"));
        }
        let query = if conds.is_empty() {
            self.select.clone()
        } else {
            format!("{} WHERE {}", self.select, conds.join(" AND "))
        };
        let c = client(&self.conninfo, &self.display)?;
        let started = std::time::Instant::now();
        let rows = run(async move { Ok(c.query(query.as_str(), &[]).await?) })
            .with_context(|| format!("postgres query on {} failed", self.display))?;
        debug!(
            "postgres {}: {} rows in {:?}",
            self.display,
            rows.len(),
            started.elapsed()
        );
        let mut out = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let mut attrs = Attrs::default();
            for (j, f) in self.columns.iter().enumerate() {
                let raw: Option<String> = row.get(j + 1);
                let v = match raw {
                    None => Value::Null,
                    Some(s) => match f.kind {
                        FieldKind::Int => s
                            .parse::<i64>()
                            .map(|n| json!(n))
                            .unwrap_or(Value::String(s)),
                        FieldKind::Real => s
                            .parse::<f64>()
                            .map(|n| json!(n))
                            .unwrap_or(Value::String(s)),
                        FieldKind::Bool => json!(s == "true" || s == "t"),
                        _ => Value::String(s),
                    },
                };
                attrs.insert(f.name.clone(), v);
            }
            if keep.is_some_and(|k| !k(&attrs)) {
                continue;
            }
            let fid = self
                .key
                .as_ref()
                .and_then(|k| attrs.get(k))
                .and_then(|v| v.as_i64())
                .unwrap_or(i as i64);
            let wkb: Option<Vec<u8>> = row.get(0);
            let geometry = wkb.and_then(|b| geozero::wkb::Wkb(b).to_geo().ok());
            out.push(Feature {
                fid,
                geometry,
                attrs,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qgis_postgres_uri() {
        let u = PgUri::parse(
            "dbname='fkb5' host=10.0.0.1 port=5432 user='u' password='p\\'w' sslmode=disable key='ogc_fid' \
             srid=25833 type=Point checkPrimaryKeyUnicity='1' table=\"import2\".\"presentasjonsdata\" (tekstplassering) \
             sql=ST_GeometryType(\"tekstplassering\") IN ('ST_Point','ST_MultiPoint')",
        )
        .unwrap();
        assert_eq!(
            (u.schema.as_str(), u.table.as_str(), u.geom.as_str()),
            ("import2", "presentasjonsdata", "tekstplassering")
        );
        assert_eq!(u.get("dbname"), Some("fkb5"));
        assert_eq!(u.get("password"), Some("p'w"));
        assert_eq!(u.get("srid"), Some("25833"));
        assert_eq!(
            u.sql.as_deref(),
            Some("ST_GeometryType(\"tekstplassering\") IN ('ST_Point','ST_MultiPoint')")
        );
        assert!(!u.display().contains("p'w"));
    }
}
