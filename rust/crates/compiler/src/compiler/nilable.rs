//! List and map values whose absence is observable: an absent list or map is
//! written as `null` and an empty one as `[]`/`{}`, and the compiler hashes
//! such documents into persisted cache keys.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
};

/// A list that is absent until something is appended or an empty one is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NilVec<T> {
    items: Vec<T>,
    present: bool,
}
impl<T> Default for NilVec<T> {
    fn default() -> Self {
        Self::nil()
    }
}
impl<T> NilVec<T> {
    pub const fn nil() -> Self {
        Self {
            items: Vec::new(),
            present: false,
        }
    }
    /// An empty list that is present (encoded as `[]`).
    pub const fn make() -> Self {
        Self {
            items: Vec::new(),
            present: true,
        }
    }
    pub fn is_nil(&self) -> bool {
        !self.present
    }
    /// Appends one value; the list becomes present.
    pub fn push(&mut self, value: T) {
        self.items.push(value);
        self.present = true;
    }
    /// Appends values: appending nothing to an absent list leaves it absent.
    pub fn append(&mut self, values: impl IntoIterator<Item = T>) {
        self.items.extend(values);
        self.present |= !self.items.is_empty();
    }
    /// Clears the items, keeping the list's presence.
    pub fn truncate(&mut self) {
        self.items.clear();
    }
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }
    pub fn skip(&self) -> bool {
        self.items.is_empty()
    }
}
impl<T> From<Vec<T>> for NilVec<T> {
    fn from(items: Vec<T>) -> Self {
        Self {
            items,
            present: true,
        }
    }
}
impl<T, const N: usize> From<[T; N]> for NilVec<T> {
    fn from(items: [T; N]) -> Self {
        Vec::from(items).into()
    }
}
impl From<&[&str]> for NilVec<String> {
    fn from(items: &[&str]) -> Self {
        items
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>()
            .into()
    }
}
impl<T> Deref for NilVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items
    }
}
impl<T> DerefMut for NilVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items
    }
}
impl<'a, T> IntoIterator for &'a NilVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}
impl<T: Serialize> Serialize for NilVec<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.present {
            self.items.serialize(serializer)
        } else {
            serializer.serialize_none()
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for NilVec<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<Vec<T>>::deserialize(deserializer)? {
            Some(items) => items.into(),
            None => Self::nil(),
        })
    }
}

/// A string-keyed map that keeps absent apart from empty; iteration is in key
/// order, which is also the order it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NilMap<V> {
    map: BTreeMap<String, V>,
    present: bool,
}
impl<V> Default for NilMap<V> {
    fn default() -> Self {
        Self::nil()
    }
}
impl<V> NilMap<V> {
    pub const fn nil() -> Self {
        Self {
            map: BTreeMap::new(),
            present: false,
        }
    }
    /// An empty map that is present (encoded as `{}`).
    pub const fn make() -> Self {
        Self {
            map: BTreeMap::new(),
            present: true,
        }
    }
    pub fn is_nil(&self) -> bool {
        !self.present
    }
    /// Assignment; every caller makes the map first.
    pub fn insert(&mut self, key: impl Into<String>, value: V) {
        self.map.insert(key.into(), value);
        self.present = true;
    }
    pub fn remove(&mut self, key: &str) -> Option<V> {
        self.map.remove(key)
    }
    pub fn entry_mut(&mut self, key: &str) -> Option<&mut V> {
        self.map.get_mut(key)
    }
    pub fn skip(&self) -> bool {
        self.map.is_empty()
    }
}
impl<V: Default + Clone> NilMap<V> {
    /// `m[key]`: the zero value when absent.
    pub fn value(&self, key: &str) -> V {
        self.map.get(key).cloned().unwrap_or_default()
    }
}
impl NilMap<String> {
    /// `m[key]` for string maps without cloning.
    pub fn at(&self, key: &str) -> &str {
        self.map.get(key).map_or("", String::as_str)
    }
}
impl NilMap<bool> {
    pub fn on(&self, key: &str) -> bool {
        self.map.get(key).copied().unwrap_or(false)
    }
}
impl<V> From<BTreeMap<String, V>> for NilMap<V> {
    fn from(map: BTreeMap<String, V>) -> Self {
        Self { map, present: true }
    }
}
impl<K: Into<String>, V, const N: usize> From<[(K, V); N]> for NilMap<V> {
    fn from(items: [(K, V); N]) -> Self {
        Self {
            map: items
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
            present: true,
        }
    }
}
impl<V> Deref for NilMap<V> {
    type Target = BTreeMap<String, V>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}
impl<'a, V> IntoIterator for &'a NilMap<V> {
    type Item = (&'a String, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, String, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}
impl<V: Serialize> Serialize for NilMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.present {
            self.map.serialize(serializer)
        } else {
            serializer.serialize_none()
        }
    }
}
impl<'de, V: Deserialize<'de>> Deserialize<'de> for NilMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(
            match Option::<BTreeMap<String, V>>::deserialize(deserializer)? {
                Some(map) => map.into(),
                None => Self::nil(),
            },
        )
    }
}

/// A JSON `null` decodes into any field as a no-op, leaving the default value.
pub fn nz<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
/// Decodes into a fresh value: a top-level `null` leaves the default.
pub fn unmarshal<'de, T: Default + Deserialize<'de>>(data: &'de [u8]) -> anyhow::Result<T> {
    Ok(serde_json::from_slice::<Option<T>>(data)?.unwrap_or_default())
}
pub fn is_false(value: &bool) -> bool {
    !*value
}
pub fn is_empty(value: &str) -> bool {
    value.is_empty()
}
