//! `extends`: GitLab's deep merge. Mappings merge recursively, everything
//! else (lists included) is replaced; parents apply in order, the job's own
//! keys last.

use std::collections::HashMap;

use crate::Error;
use crate::yaml::{Map, Value};

/// GitLab allows 11 levels of inheritance.
const MAX_DEPTH: usize = 11;

/// Resolves `extends` for the entry `name` of the top-level mapping `root`.
pub struct Resolver<'a> {
    root: &'a Map,
    cache: HashMap<String, Map>,
}

impl<'a> Resolver<'a> {
    pub fn new(root: &'a Map) -> Self {
        Self { root, cache: HashMap::new() }
    }

    /// The entry with all its parents merged in, without the `extends` key.
    pub fn resolve(&mut self, name: &str) -> Result<Map, Error> {
        self.resolve_inner(name, &mut Vec::new())
    }

    fn resolve_inner(&mut self, name: &str, stack: &mut Vec<String>) -> Result<Map, Error> {
        if let Some(done) = self.cache.get(name) {
            return Ok(done.clone());
        }
        if stack.iter().any(|s| s == name) {
            stack.push(name.to_string());
            return Err(Error::Invalid(format!(
                "circular `extends`: {}",
                stack.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(" → ")
            )));
        }
        if stack.len() > MAX_DEPTH {
            return Err(Error::Invalid(format!("job `{}`: `extends` nests deeper than {MAX_DEPTH} levels", stack[0])));
        }
        let raw = match self.root.get(name) {
            Some(Value::Map(m)) => m.clone(),
            Some(other) => {
                let child = stack.last().map_or(name, String::as_str);
                return Err(Error::Invalid(format!(
                    "job `{child}`: `extends` target `{name}` is {}, not a mapping",
                    other.kind()
                )));
            }
            None => {
                let child = stack.last().map_or(name, String::as_str);
                return Err(Error::Invalid(format!("job `{child}`: `extends` target `{name}` is not defined")));
            }
        };
        let mut own = raw;
        let parents = match own.remove("extends") {
            None => Vec::new(),
            Some(Value::Str(s)) => vec![s],
            Some(Value::Seq(items)) => items
                .into_iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s),
                    other => Err(Error::Invalid(format!(
                        "job `{name}`: `extends` entries must be strings, got {}",
                        other.kind()
                    ))),
                })
                .collect::<Result<_, _>>()?,
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "job `{name}`: `extends` must be a string or a list, got {}",
                    other.kind()
                )));
            }
        };
        let resolved = if parents.is_empty() {
            own
        } else {
            stack.push(name.to_string());
            let mut base = Map::default();
            for parent in &parents {
                let p = self.resolve_inner(parent, stack)?;
                base = deep_merge(base, p);
            }
            stack.pop();
            deep_merge(base, own)
        };
        self.cache.insert(name.to_string(), resolved.clone());
        Ok(resolved)
    }
}

/// `over` wins; nested mappings merge.
pub fn deep_merge(mut base: Map, over: Map) -> Map {
    for (key, value) in over.iter() {
        let merged = match (base.get(key), value) {
            (Some(Value::Map(b)), Value::Map(o)) => Value::Map(deep_merge(b.clone(), o.clone())),
            _ => value.clone(),
        };
        base.insert(key.to_string(), merged);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml::parse;

    fn root(src: &str) -> Map {
        parse(src).unwrap().as_map().unwrap().clone()
    }

    #[test]
    fn deep_merge_semantics() {
        let r = root(
            r#"
.a:
  variables: {X: "1", Y: "1"}
  tags: [a]
  image: alpine
.b:
  variables: {Y: "2", Z: "2"}
  tags: [b]
job:
  extends: [.a, .b]
  variables: {Z: "3"}
  script: echo
"#,
        );
        let job = Resolver::new(&r).resolve("job").unwrap();
        let vars = job.get("variables").unwrap().as_map().unwrap();
        let got: Vec<_> = vars.iter().map(|(k, v)| (k.to_string(), v.scalar_string().unwrap())).collect();
        assert_eq!(got, vec![("X".into(), "1".into()), ("Y".into(), "2".into()), ("Z".into(), "3".into())]);
        assert_eq!(job.get("tags"), Some(&Value::Seq(vec![Value::Str("b".into())])));
        assert_eq!(job.get("image"), Some(&Value::Str("alpine".into())));
        assert!(!job.contains_key("extends"));
    }

    #[test]
    fn recursive_extends() {
        let r = root(
            r#"
.base: {image: alpine, retry: 1}
.mid: {extends: .base, retry: 2}
job: {extends: .mid, script: [x]}
"#,
        );
        let job = Resolver::new(&r).resolve("job").unwrap();
        assert_eq!(job.get("image"), Some(&Value::Str("alpine".into())));
        assert_eq!(job.get("retry"), Some(&Value::Int(2)));
    }

    #[test]
    fn cycles_and_missing() {
        let r = root("a: {extends: b}\nb: {extends: a}\n");
        let err = Resolver::new(&r).resolve("a").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("circular")), "{err:?}");

        let r = root("a: {extends: a}\n");
        assert!(Resolver::new(&r).resolve("a").is_err());

        let r = root("a: {extends: .nope}\n");
        let err = Resolver::new(&r).resolve("a").unwrap_err();
        assert_eq!(err, Error::Invalid("job `a`: `extends` target `.nope` is not defined".into()));
    }
}
