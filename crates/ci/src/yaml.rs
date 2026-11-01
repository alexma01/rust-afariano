//! YAML → an ordered value tree.
//!
//! yaml-rust2 already resolves anchors and aliases; this module adds `<<`
//! merge keys and keeps mapping order (job order in the file matters).

use yaml_rust2::{Yaml, YamlLoader};

use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Kept as written (`1.10` must not become `1.1`).
    Float(String),
    Str(String),
    Seq(Vec<Value>),
    Map(Map),
}

/// A mapping that keeps insertion order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Map(Vec<(String, Value)>);

impl Map {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Replaces in place (keeping the position) or appends.
    pub fn insert(&mut self, key: String, value: Value) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let idx = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(idx).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut map = Map::default();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

impl Value {
    /// The scalar as text, the way GitLab reads a variable value or a script line.
    pub fn scalar_string(&self) -> Option<String> {
        match self {
            Value::Null => Some(String::new()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Int(i) => Some(i.to_string()),
            Value::Float(f) | Value::Str(f) => Some(f.clone()),
            Value::Seq(_) | Value::Map(_) => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Short type name for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::Int(_) | Value::Float(_) => "a number",
            Value::Str(_) => "a string",
            Value::Seq(_) => "a list",
            Value::Map(_) => "a mapping",
        }
    }
}

/// Parses the first YAML document. An empty file is an empty mapping.
pub fn parse(src: &str) -> Result<Value, Error> {
    let docs = YamlLoader::load_from_str(src).map_err(|e| Error::Yaml(e.to_string()))?;
    match docs.into_iter().next() {
        None => Ok(Value::Map(Map::default())),
        Some(doc) => convert(doc),
    }
}

fn key_string(key: &Yaml) -> Result<String, Error> {
    match key {
        Yaml::String(s) | Yaml::Real(s) => Ok(s.clone()),
        Yaml::Integer(i) => Ok(i.to_string()),
        Yaml::Boolean(b) => Ok(b.to_string()),
        Yaml::Null => Ok("null".to_string()),
        _ => Err(Error::Yaml("mapping keys must be scalars (lists and mappings are not allowed as keys)".into())),
    }
}

fn convert(node: Yaml) -> Result<Value, Error> {
    Ok(match node {
        Yaml::Null | Yaml::BadValue | Yaml::Alias(_) => Value::Null,
        Yaml::Boolean(b) => Value::Bool(b),
        Yaml::Integer(i) => Value::Int(i),
        Yaml::Real(r) => Value::Float(r),
        Yaml::String(s) => Value::Str(s),
        Yaml::Array(items) => Value::Seq(items.into_iter().map(convert).collect::<Result<_, _>>()?),
        Yaml::Hash(hash) => {
            let mut entries: Vec<(String, Value)> = Vec::with_capacity(hash.len());
            for (k, v) in hash {
                entries.push((key_string(&k)?, convert(v)?));
            }
            Value::Map(apply_merge_keys(entries)?)
        }
    })
}

/// `<<: *anchor` / `<<: [*a, *b]`: merged keys never override explicit keys, and
/// among several sources the first one wins (YAML merge-key spec). Merged keys
/// take the position of the `<<` entry.
fn apply_merge_keys(entries: Vec<(String, Value)>) -> Result<Map, Error> {
    if !entries.iter().any(|(k, _)| k == "<<") {
        return Ok(Map(entries));
    }
    let explicit: Vec<String> = entries.iter().filter(|(k, _)| k != "<<").map(|(k, _)| k.clone()).collect();
    let mut out = Map::default();
    for (key, value) in entries {
        if key != "<<" {
            out.insert(key, value);
            continue;
        }
        let sources = match value {
            Value::Map(m) => vec![m],
            Value::Seq(items) => items
                .into_iter()
                .map(|item| match item {
                    Value::Map(m) => Ok(m),
                    other => Err(Error::Yaml(format!("`<<` merge key expects mappings, got {}", other.kind()))),
                })
                .collect::<Result<_, _>>()?,
            Value::Null => vec![],
            other => {
                return Err(Error::Yaml(format!("`<<` merge key expects a mapping, got {}", other.kind())));
            }
        };
        for source in sources {
            for (k, v) in source.0 {
                if !explicit.contains(&k) && !out.contains_key(&k) {
                    out.insert(k, v);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: &Value) -> &Map {
        v.as_map().unwrap()
    }

    #[test]
    fn keeps_order() {
        let v = parse("b: 1\na: 2\nc: 3\n").unwrap();
        assert_eq!(map(&v).keys().collect::<Vec<_>>(), vec!["b", "a", "c"]);
    }

    #[test]
    fn aliases_and_merge_keys() {
        let src = r#"
.base: &base
  image: alpine
  tags: [a]
.more: &more
  image: debian
  retry: 1
job:
  <<: [*base, *more]
  tags: [b]
  script: *base_script_missing_is_error
"#;
        // An unknown alias is a YAML error.
        assert!(matches!(parse(src), Err(Error::Yaml(_))));

        let src = r#"
.base: &base
  image: alpine
  tags: [a]
.more: &more
  image: debian
  retry: 1
job:
  <<: [*base, *more]
  tags: [b]
"#;
        let v = parse(src).unwrap();
        let job = map(map(&v).get("job").unwrap());
        assert_eq!(job.get("image"), Some(&Value::Str("alpine".into())));
        assert_eq!(job.get("retry"), Some(&Value::Int(1)));
        assert_eq!(job.get("tags"), Some(&Value::Seq(vec![Value::Str("b".into())])));
        assert_eq!(job.keys().collect::<Vec<_>>(), vec!["image", "retry", "tags"]);
    }

    #[test]
    fn floats_kept_verbatim() {
        let v = parse("x: 1.10\n").unwrap();
        assert_eq!(map(&v).get("x"), Some(&Value::Float("1.10".into())));
    }

    #[test]
    fn empty_is_empty_map() {
        assert_eq!(parse("").unwrap(), Value::Map(Map::default()));
        assert_eq!(parse("# only a comment\n").unwrap(), Value::Map(Map::default()));
    }
}
