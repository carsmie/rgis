//! Case-insensitive OWS request parameters (QgsServerParameters equivalent).

use indexmap::IndexMap;

#[derive(Debug, Clone, Default)]
pub struct Params {
    map: IndexMap<String, String>,
}

impl Params {
    pub fn from_query(query: &str) -> Self {
        let mut map = IndexMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = decode(k).to_ascii_uppercase();
            map.insert(k, decode(v));
        }
        Params { map }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map
            .get(&key.to_ascii_uppercase())
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.trim().parse().ok())
    }

    pub fn bool(&self, key: &str) -> bool {
        matches!(
            self.get(key).map(|s| s.to_ascii_lowercase()).as_deref(),
            Some("true" | "1" | "yes" | "on")
        )
    }

    pub fn service(&self) -> String {
        self.get("SERVICE").unwrap_or("").to_ascii_uppercase()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.map.iter()
    }
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    urlencoding::decode(&s).map(|c| c.into_owned()).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_case_insensitive() {
        let p = Params::from_query("service=WMS&Request=GetMap&layers=a,b&BBOX=1%2C2");
        assert_eq!(p.service(), "WMS");
        assert_eq!(p.get("REQUEST"), Some("GetMap"));
        assert_eq!(p.list("LAYERS"), vec!["a", "b"]);
        assert_eq!(p.get("bbox"), Some("1,2"));
        assert_eq!(p.get("missing"), None);
    }
}
