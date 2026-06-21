use serde_json::{Map, Value};
use smallvec::SmallVec;
use std::borrow::Cow;

/// Metadata key. `&'static str` keys — every macro-generated key, every
/// log-backend literal, every tracing field name — stay borrowed and cost no
/// allocation; dynamically built keys become owned.
pub type MetaKey = Cow<'static, str>;

/// Ordered, insertion-keyed log metadata.
///
/// Backed by an inline `SmallVec`, so the common entry (a handful of short,
/// static keys) carries its metadata with no heap allocation. Entries beyond
/// the inline capacity spill to the heap, as a `HashMap` always would.
///
/// Keys are [`Cow<'static, str>`](MetaKey): static keys stay borrowed.
/// Iteration is insertion-ordered, which makes formatter output deterministic.
/// `insert` overwrites on duplicate key, matching the `HashMap` it replaces.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta(SmallVec<[(MetaKey, Value); 4]>);

impl Meta {
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self(SmallVec::with_capacity(capacity))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k.as_ref() == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0
            .iter_mut()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k.as_ref() == key)
    }

    /// Insert `value` under `key`, overwriting and returning any prior value.
    pub fn insert(&mut self, key: impl Into<MetaKey>, value: Value) -> Option<Value> {
        let key = key.into();
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(std::mem::replace(&mut slot.1, value))
        } else {
            self.0.push((key, value));
            None
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0
            .iter()
            .position(|(k, _)| k.as_ref() == key)
            .map(|pos| self.0.remove(pos).1)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, &Value) -> bool) {
        self.0.retain(|(k, v)| keep(k.as_ref(), v));
    }

    pub fn iter(&self) -> MetaIter<'_> {
        self.0.iter().map(ref_entry)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_ref())
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.0.iter().map(|(_, v)| v)
    }

    /// Clone the entries into a fresh JSON object. Used by `LogInfo`'s value
    /// conversions, which need a `serde_json::Map` regardless of the `serde`
    /// feature.
    pub fn to_json_object(&self) -> Map<String, Value> {
        self.0
            .iter()
            .map(|(k, v)| (k.as_ref().to_owned(), v.clone()))
            .collect()
    }
}

fn ref_entry((k, v): &(MetaKey, Value)) -> (&str, &Value) {
    (k.as_ref(), v)
}

pub type MetaIter<'a> =
    std::iter::Map<std::slice::Iter<'a, (MetaKey, Value)>, fn(&'a (MetaKey, Value)) -> (&'a str, &'a Value)>;

impl<'a> IntoIterator for &'a Meta {
    type Item = (&'a str, &'a Value);
    type IntoIter = MetaIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for Meta {
    type Item = (MetaKey, Value);
    type IntoIter = smallvec::IntoIter<[(MetaKey, Value); 4]>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<K: Into<MetaKey>> FromIterator<(K, Value)> for Meta {
    fn from_iter<I: IntoIterator<Item = (K, Value)>>(iter: I) -> Self {
        Self(iter.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl From<std::collections::HashMap<String, Value>> for Meta {
    fn from(map: std::collections::HashMap<String, Value>) -> Self {
        map.into_iter().collect()
    }
}

impl std::ops::Index<&str> for Meta {
    type Output = Value;

    fn index(&self, key: &str) -> &Value {
        self.get(key)
            .unwrap_or_else(|| panic!("no metadata entry for key `{key}`"))
    }
}

#[cfg(feature = "serde")]
mod serde_impl {
    use super::{Meta, MetaKey};
    use serde::de::{MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;
    use std::fmt;

    impl Serialize for Meta {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (k, v) in &self.0 {
                map.serialize_entry(k.as_ref(), v)?;
            }
            map.end()
        }
    }

    impl<'de> Deserialize<'de> for Meta {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct MetaVisitor;

            impl<'de> Visitor<'de> for MetaVisitor {
                type Value = Meta;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a map of log metadata")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Meta, A::Error> {
                    let mut meta = Meta::with_capacity(access.size_hint().unwrap_or(0));
                    while let Some((key, value)) = access.next_entry::<String, Value>()? {
                        meta.insert(MetaKey::Owned(key), value);
                    }
                    Ok(meta)
                }
            }

            deserializer.deserialize_map(MetaVisitor)
        }
    }
}
