use super::{
    hierarchy::id,
    store::*,
    yaml_edit::{TextAsset, json_numbers, trim_trailing_comments},
    yamlcodec::{self, Kind, Node, Style},
};
use serde_json::Value;
/// Whether a source span carries a YAML comment. Comments attach to nodes and
/// would have to be re-emitted with them; that part of the emitter is not
/// implemented, so spans whose nodes would be re-encoded must be comment-free.
pub(crate) fn has_comment(text: &str) -> bool {
    let bytes = text.as_bytes();
    let (mut quote, mut previous) = (0u8, b'\n');
    let mut index = 0;
    while index < bytes.len() {
        let c = bytes[index];
        if quote != 0 {
            if quote == b'"' && c == b'\\' {
                index += 2;
                continue;
            }
            if c == quote {
                if quote == b'\'' && bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                quote = 0;
                previous = c;
            }
            index += 1;
            continue;
        }
        let blank = index == 0 || matches!(bytes[index - 1], b' ' | b'\t' | b'\n' | b'\r');
        if c == b'#' && blank {
            return true;
        }
        // A quote opens a scalar only where a token may start.
        if (c == b'\'' || c == b'"')
            && matches!(previous, b'\n' | b':' | b'-' | b',' | b'[' | b'{' | b'?')
        {
            quote = c;
        }
        if c == b'\n' {
            previous = b'\n';
        } else if !matches!(c, b' ' | b'\t' | b'\r') {
            previous = c;
        }
        index += 1;
    }
    false
}
fn line_start(body: &str, offset: usize) -> usize {
    body[..offset].rfind('\n').map_or(0, |index| index + 1)
}
fn column(body: &str, offset: usize) -> usize {
    body[line_start(body, offset)..offset].chars().count()
}
fn prefab_field_text(key: &str, indent: usize, node: &Node, newline: &str) -> Result<String> {
    let text = yamlcodec::encode_node(node)?;
    let prefix = " ".repeat(indent);
    if node.kind != Kind::Scalar && !node.flow {
        let child = " ".repeat(indent + 2);
        return Ok(format!(
            "{prefix}{key}:{newline}{child}{}{newline}",
            text.replace('\n', &format!("{newline}{child}"))
        ));
    }
    Ok(format!("{prefix}{key}: {text}{newline}"))
}
/// Unity's reader accepts a narrower YAML subset than the general emitter writes. In particular,
/// nonempty modification arrays must be block sequences, not flow sequences
/// placed alone on the next line beneath their field name.
fn prefab_node_styles(node: &mut Node) {
    match node.kind {
        Kind::Sequence => node.flow = node.content.is_empty(),
        Kind::Mapping => node.flow = node.field("fileID").is_some(),
        Kind::Scalar => {}
    }
    for child in &mut node.content {
        prefab_node_styles(child);
    }
}
fn canonical_prefab_refs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("fileID") {
                let canonical = id(&Value::Object(map.clone()));
                map.insert("fileID".into(), Value::String(canonical));
            }
            map.values_mut().for_each(canonical_prefab_refs);
        }
        Value::Array(items) => items.iter_mut().for_each(canonical_prefab_refs),
        _ => {}
    }
}
fn canonical_record(node: &Node) -> Option<Vec<u8>> {
    let mut value = yamlcodec::decode(node).ok()?;
    canonical_prefab_refs(&mut value);
    encode_json(&json_numbers(&value)).ok()
}
/// Reuse the original nodes for unchanged records, preserving lexical scalar
/// tags and unknown attributes rather than re-encoding their plain values.
fn preserve_prefab_record_nodes(old: &Node, replacement: &mut Node) -> bool {
    if old.kind != Kind::Sequence || replacement.kind != Kind::Sequence {
        return false;
    }
    let previous: Vec<Option<Vec<u8>>> = old.content.iter().map(canonical_record).collect();
    let mut reused = false;
    for candidate in &mut replacement.content {
        let Some(after) = canonical_record(candidate) else {
            continue;
        };
        if let Some(index) = previous
            .iter()
            .position(|before| before.as_ref() == Some(&after))
        {
            *candidate = old.content[index].clone();
            reused = true;
        }
    }
    reused
}
pub(crate) fn set_prefab_modification_field(
    asset: &mut TextAsset,
    instance_id: &str,
    key: &str,
    value: &Value,
) -> Result<()> {
    let position = asset
        .position(instance_id)
        .ok_or_else(|| failure("not_found", "prefab instance is absent", None))?;
    let doc = &asset.docs[position];
    let body = &doc.body;
    let (modification, root_index) = doc
        .node
        .field("m_Modification")
        .filter(|(node, _)| node.kind == Kind::Mapping)
        .ok_or_else(|| failure("invalid_prefab", "prefab modification map is absent", None))?;
    let mut replacement = yamlcodec::value_node(value)?;
    let found = modification.field(key);
    let reused = found.is_some_and(|(old, _)| preserve_prefab_record_nodes(old, &mut replacement));
    prefab_node_styles(&mut replacement);
    let root_end = doc
        .node
        .content
        .get(root_index + 2)
        .map_or(body.len(), |next| line_start(body, next.start));
    let (start, end, text) = match found {
        Some((_, index)) if !modification.flow => {
            let key_node = &modification.content[index];
            let start = line_start(body, key_node.start);
            let limit = modification
                .content
                .get(index + 2)
                .map_or(root_end, |next| line_start(body, next.start));
            if reused && has_comment(&body[start..limit]) {
                return Err(yamlcodec::unported_comment());
            }
            let text = prefab_field_text(
                key,
                column(body, key_node.start),
                &replacement,
                asset.newline,
            )?;
            (start, trim_trailing_comments(body, start, limit), text)
        }
        _ => {
            // Normalize only a previously inline modification map. Preserve unknown
            // sibling nodes, their tags and styles during this conversion.
            let mut copy = modification.clone();
            copy.flow = false;
            match found {
                Some((_, index)) => copy.content[index + 1] = replacement,
                None => {
                    copy.content
                        .push(Node::scalar(key, yamlcodec::STR, Style::Plain));
                    copy.content.push(replacement);
                }
            }
            let root_key = &doc.node.content[root_index];
            let start = line_start(body, root_key.start);
            if has_comment(&body[start..root_end]) {
                return Err(yamlcodec::unported_comment());
            }
            let text = prefab_field_text(
                "m_Modification",
                column(body, root_key.start),
                &copy,
                asset.newline,
            )?;
            (start, trim_trailing_comments(body, start, root_end), text)
        }
    };
    asset.docs[position].body.replace_range(start..end, &text);
    asset.refresh()
}
