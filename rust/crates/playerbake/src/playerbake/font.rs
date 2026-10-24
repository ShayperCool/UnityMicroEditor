//! A TrueType or OpenType font as the importer makes it a dynamic `Font`:
//! the file's bytes, its metrics at the import size, and the two objects it
//! holds, a `Font Material` of the built-in Font shader and an empty `Font
//! Texture` that the Player fills as it renders glyphs. They are written as
//! the documents a font asset would hold in YAML, which the authored
//! conversion then lays out.
use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value, json};
use std::path::Path;
use ume_base::yaml::Document;

pub const FONT_FILE_ID: i64 = 12_800_000;
const MATERIAL_FILE_ID: i64 = 2_100_000;
const TEXTURE_FILE_ID: i64 = 2_800_000;
/// The built-in Font shader, in the engine's default resources.
const FONT_SHADER: i64 = 10101;
const DEFAULT_RESOURCES: &str = "0000000000000000e000000000000000";

/// What the font's `head` and `hhea` tables give.
struct Metrics {
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
}

/// The font's family name (`name` table, name ID 1): the Windows Unicode
/// record if it has one, else the Macintosh Roman one.
fn family_name(data: &[u8]) -> Option<String> {
    let u16_at = |at: usize| -> Option<u16> {
        Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
    };
    let tables = u16_at(4)? as usize;
    let table = (0..tables).find_map(|i| {
        let at = 12 + 16 * i;
        if data.get(at..at + 4)? != b"name" {
            return None;
        }
        Some(u32::from_be_bytes(data.get(at + 8..at + 12)?.try_into().ok()?) as usize)
    })?;
    let count = u16_at(table + 2)? as usize;
    let strings = table + u16_at(table + 4)? as usize;
    let mut mac = None;
    for i in 0..count {
        let record = table + 6 + 12 * i;
        let (platform, encoding, name_id) =
            (u16_at(record)?, u16_at(record + 2)?, u16_at(record + 6)?);
        if name_id != 1 {
            continue;
        }
        let length = u16_at(record + 8)? as usize;
        let offset = u16_at(record + 10)? as usize;
        let bytes = data.get(strings + offset..strings + offset + length)?;
        match (platform, encoding) {
            (3, 1) | (0, _) => {
                let units: Vec<u16> = bytes
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                    .collect();
                return String::from_utf16(&units).ok();
            }
            (1, 0) => mac = Some(bytes.iter().map(|&b| b as char).collect()),
            _ => {}
        }
    }
    mac
}

fn metrics(data: &[u8]) -> Result<Metrics> {
    let u16_at = |at: usize| -> Result<u16> {
        Ok(u16::from_be_bytes(
            data.get(at..at + 2).context("font cut short")?.try_into()?,
        ))
    };
    let tables = u16_at(4)? as usize;
    let table = |tag: &[u8; 4]| -> Result<usize> {
        for i in 0..tables {
            let at = 12 + 16 * i;
            if data.get(at..at + 4) == Some(tag) {
                let offset = data.get(at + 8..at + 12).context("font cut short")?;
                return Ok(u32::from_be_bytes(offset.try_into()?) as usize);
            }
        }
        bail!("the font has no {} table", String::from_utf8_lossy(tag))
    };
    let head = table(b"head")?;
    let hhea = table(b"hhea")?;
    Ok(Metrics {
        units_per_em: u16_at(head + 18)?,
        ascender: u16_at(hhea + 4)? as i16,
        descender: u16_at(hhea + 6)? as i16,
        line_gap: u16_at(hhea + 8)? as i16,
    })
}

fn text(value: impl ToString) -> Value {
    Value::String(value.to_string())
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The documents of a font asset: the `Font` and the two objects it holds.
pub fn documents(path: &Path, meta: &Map<String, Value>) -> Result<Vec<Document>> {
    let data = std::fs::read(path)?;
    let setting = |name: &str| meta.get(name);
    let number = |name: &str, default: f64| -> f64 {
        match setting(name) {
            Some(Value::String(text)) => text.parse().unwrap_or(default),
            Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
            _ => default,
        }
    };
    if number("includeFontData", 1.0) == 0.0 {
        bail!("a font whose data is not included is left to the worker");
    }
    if number("ascentCalculationMode", 1.0) != 1.0 {
        bail!("a font's ascent other than its face's is left to the worker");
    }
    let size = number("fontSize", 16.0) as f32;
    let m = metrics(&data)?;
    // The ascent and descent by the size per unit; the line spacing by the
    // height in units times the size, over the units.
    let scale = size / m.units_per_em as f32;
    let ascent = m.ascender as f32 * scale;
    let descent = m.descender as f32 * scale;
    let height = (m.ascender as i32 - m.descender as i32 + m.line_gap as i32) as f32;
    let line_spacing = height * size / m.units_per_em as f32;
    let name = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // The names the importer recorded, or the font's own family name.
    let names = match setting("fontNames") {
        Some(Value::Array(names)) if !names.is_empty() => names.clone(),
        _ => family_name(&data)
            .map(|family| vec![Value::String(family)])
            .unwrap_or_default(),
    };
    let fallbacks = match setting("fallbackFontReferences") {
        Some(Value::Array(references)) => references.clone(),
        _ => Vec::new(),
    };
    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    let font = json!({
        "m_ObjectHideFlags": "0",
        "m_Name": name,
        "serializedVersion": "5",
        "m_LineSpacing": text(line_spacing),
        "m_DefaultMaterial": {"fileID": text(MATERIAL_FILE_ID)},
        "m_FontSize": text(size),
        "m_Texture": {"fileID": text(TEXTURE_FILE_ID)},
        "m_AsciiStartOffset": "0",
        "m_Tracking": "1",
        "m_CharacterSpacing": text(number("characterSpacing", 0.0) as i64),
        "m_CharacterPadding": text(number("characterPadding", 1.0) as i64),
        "m_ConvertCase": text(number("forceTextureCase", -2.0) as i64),
        "m_CharacterRects": [],
        "m_KerningValues": [],
        "m_PixelScale": "0.1",
        "m_FontData": hex,
        "m_Ascent": text(ascent),
        "m_Descent": text(descent),
        "m_DefaultStyle": "0",
        "m_FontNames": names,
        "m_FallbackFonts": fallbacks,
        "m_FontRenderingMode": text(number("fontRenderingMode", 0.0) as i64),
        "m_UseLegacyBoundsCalculation": text(number("useLegacyBoundsCalculation", 0.0) as i64),
        "m_ShouldRoundAdvanceValue": text(number("shouldRoundAdvanceValue", 1.0) as i64),
    });
    let material = json!({
        "m_ObjectHideFlags": "0",
        "m_Name": "Font Material",
        "m_Shader": {"fileID": text(FONT_SHADER), "guid": DEFAULT_RESOURCES, "type": "0"},
        "m_ValidKeywords": [],
        "m_InvalidKeywords": [],
        "m_LightmapFlags": "4",
        "m_EnableInstancingVariants": "0",
        "m_DoubleSidedGI": "0",
        "m_CustomRenderQueue": "-1",
        "stringTagMap": {},
        "disabledShaderPasses": [],
        "m_SavedProperties": {
            "serializedVersion": "3",
            "m_TexEnvs": [{"_MainTex": {
                "m_Texture": {"fileID": text(TEXTURE_FILE_ID)},
                "m_Scale": {"x": "1", "y": "1"},
                "m_Offset": {"x": "0", "y": "0"},
            }}],
            "m_Ints": [],
            "m_Floats": [],
            "m_Colors": [{"_Color": {"r": "1", "g": "1", "b": "1", "a": "1"}}],
        },
        "m_BuildTextureStacks": [],
    });
    let texture = json!({
        "m_ObjectHideFlags": "0",
        "m_Name": "Font Texture",
        "m_ImageContentsHash": {"serializedVersion": "2", "Hash": "00000000000000000000000000000000"},
        "m_ForcedFallbackFormat": "4",
        "m_DownscaleFallback": "0",
        "m_IsAlphaChannelOptional": "0",
        "serializedVersion": "2",
        "m_Width": "0",
        "m_Height": "0",
        "m_CompleteImageSize": "0",
        "m_MipsStripped": "0",
        "m_TextureFormat": "4",
        "m_MipCount": "1",
        "m_IsReadable": "0",
        "m_IsPreProcessed": "0",
        "m_IgnoreMipmapLimit": "0",
        "m_MipmapLimitGroupName": "",
        "m_StreamingMipmaps": "0",
        "m_StreamingMipmapsPriority": "0",
        "m_VTOnly": "0",
        "m_AlphaIsTransparency": "0",
        "m_ImageCount": "1",
        "m_TextureDimension": "2",
        "m_TextureSettings": {
            "serializedVersion": "2",
            "m_FilterMode": "1",
            "m_Aniso": "1",
            "m_MipBias": "0",
            "m_WrapU": "0",
            "m_WrapV": "0",
            "m_WrapW": "0",
        },
        "m_LightmapFormat": "0",
        "m_ColorSpace": "0",
        "m_PlatformBlob": "",
        "image data": "0",
        "_typelessdata": "",
        "m_StreamData": {"serializedVersion": "2", "offset": "0", "size": "0", "path": ""},
    });
    Ok(vec![
        Document {
            class_id: 128,
            file_id: FONT_FILE_ID.to_string(),
            type_name: "Font".into(),
            data: object(font),
            stripped: false,
        },
        Document {
            class_id: 21,
            file_id: MATERIAL_FILE_ID.to_string(),
            type_name: "Material".into(),
            data: object(material),
            stripped: false,
        },
        Document {
            class_id: 28,
            file_id: TEXTURE_FILE_ID.to_string(),
            type_name: "Texture2D".into(),
            data: object(texture),
            stripped: false,
        },
    ])
}

/// The block of a `.meta` an importer keeps its settings in, as a map.
pub fn importer_settings(meta: &[u8], importer: &str) -> Result<Map<String, Value>> {
    let text = std::str::from_utf8(meta).context("the .meta is not text")?;
    let mut block = String::from("--- !u!0 &1\n");
    let mut inside = false;
    let heading = format!("{importer}:");
    for line in text.lines() {
        if !line.starts_with([' ', '-']) {
            inside = line.starts_with(&heading);
        }
        if inside {
            block.push_str(line);
            block.push('\n');
        }
    }
    ume_base::yaml::parse_unity_yaml_lexical(block.as_bytes())?
        .into_iter()
        .find(|d| d.type_name == importer)
        .map(|d| d.data)
        .with_context(|| format!("the .meta has no {importer} block"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A font of only a table directory, `head` and `hhea`.
    fn font(units: u16, ascender: i16, descender: i16, gap: i16) -> Vec<u8> {
        let mut data = vec![0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0];
        let (head, hhea) = (12 + 32, 12 + 32 + 54);
        for (tag, offset, length) in [(b"head", head, 54u32), (b"hhea", hhea, 36)] {
            data.extend_from_slice(tag);
            data.extend_from_slice(&[0; 4]);
            data.extend_from_slice(&(offset as u32).to_be_bytes());
            data.extend_from_slice(&length.to_be_bytes());
        }
        let mut head_table = vec![0u8; 54];
        head_table[18..20].copy_from_slice(&units.to_be_bytes());
        let mut hhea_table = vec![0u8; 36];
        hhea_table[4..6].copy_from_slice(&ascender.to_be_bytes());
        hhea_table[6..8].copy_from_slice(&descender.to_be_bytes());
        hhea_table[8..10].copy_from_slice(&gap.to_be_bytes());
        data.extend(head_table);
        data.extend(hhea_table);
        data
    }

    #[test]
    fn metrics_are_the_importer_s_at_its_size() {
        // Noto Sans JP's metrics, and what the engine wrote for it at 16.
        let directory = std::env::temp_dir().join(format!("ume-font-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Sample.ttf");
        std::fs::write(&path, font(1000, 1160, -288, 0)).unwrap();
        let documents = documents(&path, &Map::new()).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        let field = |name: &str| documents[0].data[name].as_str().unwrap().to_string();
        assert_eq!(field("m_Ascent"), "18.560001");
        assert_eq!(field("m_Descent"), "-4.6080003");
        assert_eq!(field("m_LineSpacing"), "23.168");
        assert_eq!(documents[1].data["m_Name"], "Font Material");
        assert_eq!(documents[2].data["m_Name"], "Font Texture");
    }
}
