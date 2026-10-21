//! JSON file targets of the runner, decoded by these
//! rules: case-insensitive struct fields, `null` leaving a field unchanged and
//! decoding that continues after a type error and then reports the first one.
use super::{os_error, text_error};
use crate::{
    jsondoc::{self, Json, Member, Target},
    project::{Map, Value},
};
use anyhow::Result;
use std::collections::BTreeMap;

pub(crate) struct Document {
    pub data: Vec<u8>,
    pub json: Json,
}

/// Reads a file and checks that it is well-formed JSON.
pub(crate) fn read_document(path: &str) -> Result<Document> {
    let data = std::fs::read(path).map_err(|e| os_error("open", path, &e))?;
    let json = jsondoc::parse(&data).map_err(text_error)?;
    Ok(Document { data, json })
}

/// The members of an object target; `null` leaves the target untouched.
pub(crate) fn members<'a>(
    json: &'a Json,
    at: Target<'_>,
    target_type: &str,
) -> Result<&'a [Member], String> {
    match json {
        Json::Null => Ok(&[]),
        Json::Object(members) => Ok(members),
        other => Err(at.mismatch(other.kind(), target_type)),
    }
}

/// Remembers the first error while decoding continues.
#[derive(Default)]
pub(crate) struct First(Option<String>);
impl First {
    pub fn note(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.0.get_or_insert(error);
        }
    }
    pub fn finish(self) -> Result<(), String> {
        self.0.map_or(Ok(()), Err)
    }
}

/// A list target: `null` keeps the list, an array replaces it.
pub(crate) fn set_list<T: Default>(
    target: &mut Option<Vec<T>>,
    value: &Json,
    at: Target<'_>,
    target_type: &str,
    mut element: impl FnMut(&mut T, &Json) -> Result<(), String>,
) -> Result<(), String> {
    let items = match value {
        Json::Null => return Ok(()),
        Json::Array(items) => items,
        other => return Err(at.mismatch(other.kind(), target_type)),
    };
    let mut first = First::default();
    let mut list = Vec::with_capacity(items.len());
    for item in items {
        let mut entry = T::default();
        first.note(element(&mut entry, item));
        list.push(entry);
    }
    *target = Some(list);
    first.finish()
}

pub(crate) fn set_strings(
    target: &mut Option<Vec<String>>,
    value: &Json,
    at: Target<'_>,
) -> Result<(), String> {
    set_list(target, value, at, "array", |entry, item| {
        jsondoc::set_string(entry, item, at)
    })
}

/// A string-to-string map target: entries are added to the existing map.
pub(crate) fn set_string_map(
    target: &mut Option<BTreeMap<String, String>>,
    value: &Json,
    at: Target<'_>,
) -> Result<(), String> {
    let members = match value {
        Json::Null => return Ok(()),
        Json::Object(members) => members,
        other => return Err(at.mismatch(other.kind(), "object")),
    };
    let map = target.get_or_insert_with(BTreeMap::new);
    let mut first = First::default();
    for member in members {
        let mut entry = String::new();
        first.note(jsondoc::set_string(&mut entry, &member.value, at));
        map.insert(member.key.clone(), entry);
    }
    first.finish()
}

/// Decoding into a dynamic value: numbers become f64, the last duplicate key wins.
pub(crate) fn any_value(json: &Json, at: Target<'_>) -> Result<Value, String> {
    Ok(match json {
        Json::Null => Value::Null,
        Json::Bool(flag) => Value::Bool(*flag),
        Json::Number(literal) => Value::Float(
            literal
                .parse()
                .ok()
                .filter(|number: &f64| number.is_finite())
                .ok_or_else(|| at.mismatch(&format!("number {literal}"), "float64"))?,
        ),
        Json::String(text) => Value::String(text.clone()),
        Json::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| any_value(item, at))
                .collect::<Result<_, _>>()?,
        ),
        Json::Object(members) => Value::Object(any_map(members, at)?),
    })
}
pub(crate) fn any_map(members: &[Member], at: Target<'_>) -> Result<Map, String> {
    let mut map = Map::new();
    for member in members {
        map.insert(member.key.clone(), any_value(&member.value, at)?);
    }
    Ok(map)
}

/// Reads a JSON object file: `None` is the absent map of a `null` document.
pub(crate) fn read_any_map(path: &str) -> Result<Option<Map>> {
    let document = read_document(path)?;
    match &document.json {
        Json::Null => Ok(None),
        Json::Object(members) => Ok(Some(any_map(members, Target::VALUE).map_err(text_error)?)),
        other => Err(text_error(Target::VALUE.mismatch(other.kind(), "object"))),
    }
}

/// The `runtimeInitializers` member of a baseline manifest as a list of objects;
/// `Value::Null` is the absent list of an absent or `null` field.
pub(crate) fn read_runtime_initializers(path: &str) -> Result<Value> {
    let document = read_document(path)?;
    let mut initializers = Value::Null;
    let root = members(&document.json, Target::VALUE, "object").map_err(text_error)?;
    let at = Target {
        strukt: "",
        field: "runtimeInitializers",
    };
    jsondoc::decode_struct(root, &["runtimeInitializers"], |_, member| {
        let items = match &member.value {
            Json::Null => return Ok(()),
            Json::Array(items) => items,
            other => return Err(at.mismatch(other.kind(), "array")),
        };
        let mut first = First::default();
        let mut list = Vec::with_capacity(items.len());
        for item in items {
            list.push(match item {
                Json::Object(members) => match any_map(members, at) {
                    Ok(map) => Value::Object(map),
                    Err(error) => {
                        first.note(Err(error));
                        Value::Null
                    }
                },
                Json::Null => Value::Null,
                other => {
                    first.note(Err(at.mismatch(other.kind(), "object")));
                    Value::Null
                }
            });
        }
        initializers = Value::Array(list);
        first.finish()
    })
    .map_err(text_error)?;
    Ok(initializers)
}
