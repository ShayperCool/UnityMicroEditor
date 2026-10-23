//! Texture2D and Sprite objects written from an image and its
//! `TextureImporter` settings, as a build for the Linux standalone target
//! writes them.
//!
//! Covered: PNG sources kept uncompressed (RGBA32, RGB24, Alpha8) at their
//! own size without mipmaps, single and multiple sprites with a full-rect
//! mesh. Anything else is refused, so a caller can leave it to the worker.
use super::{Kind, typetree::Val, unsupported};
use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value};
use std::{borrow::Cow, path::Path};

pub const TEXTURE_2D: i32 = 28;
pub const SPRITE: i32 = 213;
pub const TEXTURE_FILE_ID: i64 = 2800000;
pub const SPRITE_FILE_ID: i64 = 21300000;

/// The settings of a `.meta`'s `TextureImporter` block that decide what a
/// build writes.
pub struct Settings {
    importer: Map<String, Value>,
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

impl Settings {
    pub fn read(meta: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(meta).context("the .meta is not text")?;
        // The importer's block of the .meta, as a document of its own.
        let mut block = String::from("--- !u!0 &1\n");
        let mut inside = false;
        for line in text.lines() {
            if !line.starts_with([' ', '-']) {
                inside = line.starts_with("TextureImporter:");
            }
            if inside {
                block.push_str(line);
                block.push('\n');
            }
        }
        let document = block;
        let documents = ume_base::yaml::parse_unity_yaml_lexical(document.as_bytes())?;
        let importer = documents
            .into_iter()
            .find(|d| d.type_name == "TextureImporter")
            .map(|d| d.data)
            .context("the .meta has no TextureImporter block")?;
        Ok(Settings { importer })
    }

    fn get(&self, path: &str) -> Option<&Value> {
        let mut value = self.importer.get(path.split('.').next()?)?;
        for part in path.split('.').skip(1) {
            value = value.get(part)?;
        }
        Some(value)
    }

    fn int(&self, path: &str, default: i64) -> i64 {
        number(self.get(path)).map_or(default, |n| n as i64)
    }

    fn float(&self, path: &str, default: f64) -> f64 {
        number(self.get(path)).unwrap_or(default)
    }

    /// The platform entry a Standalone build reads: its own when overridden,
    /// otherwise the default one.
    fn platform(&self) -> Option<&Map<String, Value>> {
        let entries = self.get("platformSettings")?.as_array()?;
        let named = |name: &str| {
            entries
                .iter()
                .filter_map(Value::as_object)
                .find(|entry| entry.get("buildTarget").and_then(Value::as_str) == Some(name))
        };
        named("Standalone")
            .filter(|entry| number(entry.get("overridden")) == Some(1.0))
            .or_else(|| named("DefaultTexturePlatform"))
    }

    fn platform_int(&self, name: &str, default: i64) -> i64 {
        self.platform()
            .and_then(|entry| number(entry.get(name)))
            .map_or(default, |n| n as i64)
    }
}

/// An image decoded to 8-bit RGBA, rows from the top.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Whether the source has an alpha channel (or a transparent palette
    /// entry).
    pub alpha: bool,
}

pub fn decode_png(data: &[u8]) -> Result<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![0; reader.output_buffer_size().context("image too large")?];
    let frame = reader.next_frame(&mut buffer)?;
    let pixels = &buffer[..frame.buffer_size()];
    let (width, height) = (frame.width, frame.height);
    let count = (width * height) as usize;
    let (rgba, alpha) = match frame.color_type {
        png::ColorType::Rgba => (pixels.to_vec(), true),
        png::ColorType::Rgb => (
            pixels
                .chunks(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            false,
        ),
        png::ColorType::GrayscaleAlpha => (
            pixels
                .chunks(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            true,
        ),
        png::ColorType::Grayscale => (pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(), false),
        png::ColorType::Indexed => bail!("palette left unexpanded"),
    };
    if rgba.len() != count * 4 {
        bail!("decoded image has an unexpected size");
    }
    Ok(Image {
        width,
        height,
        rgba,
        alpha,
    })
}

/// The objects one image yields, with the data of the texture to be
/// streamed beside the file.
pub struct Textured {
    /// `(fileID in the source, class, value)`, the texture first.
    pub objects: Vec<(i64, i32, Val)>,
    /// The texture's pixel data, rows from the bottom.
    pub data: Vec<u8>,
}

const RGBA32: i64 = 4;
const RGB24: i64 = 3;
const ALPHA8: i64 = 1;

/// `texture` and `sprite` are the engine's sample objects of the two
/// classes; `guid` is the source's; `stream` names the file the data is
/// streamed from and its offset there.
pub fn convert(
    path: &Path,
    meta: &[u8],
    guid: &str,
    texture: &Kind,
    sprite: Option<&Kind>,
    stream: (&str, u64),
) -> Result<Textured> {
    let settings = Settings::read(meta)?;
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if extension != "png" {
        return Err(unsupported(format!(
            "{extension} images are left to the worker"
        )));
    }
    let image = decode_png(&std::fs::read(path)?)?;
    let max = settings.platform_int("maxTextureSize", 2048);
    if image.width as i64 > max || image.height as i64 > max {
        return Err(unsupported(
            "a texture larger than its maximum size is scaled",
        ));
    }
    if settings.int("mipmaps.enableMipMap", 1) != 0 {
        return Err(unsupported("mipmaps"));
    }
    let texture_type = settings.int("textureType", 0);
    let npot = !(image.width.is_power_of_two() && image.height.is_power_of_two());
    if npot && settings.int("nPOTScale", 1) != 0 && texture_type != 8 {
        return Err(unsupported(
            "a texture of a size not a power of two is scaled",
        ));
    }
    let format = match settings.platform_int("textureFormat", -1) {
        -1 if settings.platform_int("textureCompression", 1) == 0 => {
            if texture_type == 10 {
                ALPHA8
            } else if image.alpha && settings.int("alphaUsage", 1) != 0 {
                RGBA32
            } else {
                RGB24
            }
        }
        format @ (RGBA32 | RGB24 | ALPHA8) => format,
        other => return Err(unsupported(format!("texture format {other}"))),
    };
    let name = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (width, height) = (image.width as usize, image.height as usize);
    let data = pixels(&image, format);
    let filter = match settings.int("textureSettings.filterMode", -1) {
        -1 => 1,
        mode => mode,
    };
    let wrap = |axis: &str| match settings.int(&format!("textureSettings.wrap{axis}"), -1) {
        -1 => settings.int(
            "textureSettings.wrapMode",
            if texture_type == 8 { 1 } else { 0 },
        ),
        mode => mode,
    };
    let aniso = match settings.int("textureSettings.aniso", -1) {
        -1 => 1,
        aniso => aniso,
    };
    let mip_bias = match settings.float("textureSettings.mipBias", -100.0) {
        -100.0 => 0.0,
        bias => bias,
    };
    let mut value = texture.sample.clone();
    set(&mut value, "m_Name", Val::Str(name.clone()));
    set(&mut value, "m_Width", Val::I(width as i64));
    set(&mut value, "m_Height", Val::I(height as i64));
    set(&mut value, "m_CompleteImageSize", Val::U(data.len() as u64));
    set(&mut value, "m_MipsStripped", Val::I(0));
    set(&mut value, "m_TextureFormat", Val::I(format));
    set(&mut value, "m_MipCount", Val::I(1));
    set(
        &mut value,
        "m_IsReadable",
        Val::Bool(settings.int("isReadable", 0) != 0),
    );
    set(&mut value, "m_IsPreProcessed", Val::Bool(false));
    set(
        &mut value,
        "m_IgnoreMipmapLimit",
        Val::Bool(settings.int("ignoreMipmapLimit", 0) != 0),
    );
    set(
        &mut value,
        "m_MipmapLimitGroupName",
        Val::Str(String::new()),
    );
    set(&mut value, "m_StreamingMipmaps", Val::Bool(false));
    set(&mut value, "m_StreamingMipmapsPriority", Val::I(0));
    set(&mut value, "m_ImageCount", Val::I(1));
    set(&mut value, "m_TextureDimension", Val::I(2));
    set(
        &mut value,
        "m_TextureSettings",
        Val::map([
            ("m_FilterMode", Val::I(filter)),
            ("m_Aniso", Val::I(aniso)),
            ("m_MipBias", Val::F(mip_bias as f32)),
            ("m_WrapU", Val::I(wrap("U"))),
            ("m_WrapV", Val::I(wrap("V"))),
            ("m_WrapW", Val::I(wrap("W"))),
        ]),
    );
    set(&mut value, "m_LightmapFormat", Val::I(6));
    set(
        &mut value,
        "m_ColorSpace",
        Val::I(settings.int("mipmaps.sRGBTexture", 1)),
    );
    set(&mut value, "m_PlatformBlob", Val::Seq(Vec::new()));
    set(&mut value, "image data", Val::Bytes(Vec::new()));
    set(
        &mut value,
        "m_StreamData",
        Val::map([
            ("offset", Val::U(stream.1)),
            ("size", Val::U(data.len() as u64)),
            ("path", Val::Str(stream.0.to_string())),
        ]),
    );
    let mut objects = vec![(TEXTURE_FILE_ID, TEXTURE_2D, value)];
    if texture_type == 8 {
        let sprite = sprite.context("no type tree of Sprite")?;
        for (file_id, sprite_name, rect, pivot, border) in sprites(&settings, &name, width, height)?
        {
            objects.push((
                file_id,
                SPRITE,
                sprite_value(
                    &settings,
                    sprite,
                    guid,
                    &sprite_name,
                    rect,
                    pivot,
                    border,
                    file_id,
                )?,
            ));
        }
    }
    Ok(Textured { objects, data })
}

/// The image's pixels in a format, rows from the bottom as the engine
/// stores them.
fn pixels(image: &Image, format: i64) -> Vec<u8> {
    let width = image.width as usize;
    let mut data = Vec::with_capacity(image.rgba.len());
    for row in image.rgba.chunks(width * 4).rev() {
        for pixel in row.chunks(4) {
            match format {
                RGBA32 => data.extend_from_slice(pixel),
                RGB24 => data.extend_from_slice(&pixel[..3]),
                _ => data.push(pixel[3]),
            }
        }
    }
    data
}

fn set(value: &mut Val, name: &str, field: Val) {
    if let Some(slot) = value.field_mut(name) {
        *slot = field;
    }
}

type Rect = [f64; 4];

/// A sprite of an image: `(fileID, name, rect, pivot, border)`.
type SpriteRect = (i64, String, Rect, [f64; 2], [f64; 4]);

/// The sprites of the image: `(fileID, name, rect, pivot, border)`.
fn sprites(
    settings: &Settings,
    name: &str,
    width: usize,
    height: usize,
) -> Result<Vec<SpriteRect>> {
    let pivot_of = |alignment: i64, custom: Option<&Value>| -> [f64; 2] {
        match alignment {
            1 => [0.0, 1.0],
            2 => [0.5, 1.0],
            3 => [1.0, 1.0],
            4 => [0.0, 0.5],
            5 => [1.0, 0.5],
            6 => [0.0, 0.0],
            7 => [0.5, 0.0],
            8 => [1.0, 0.0],
            9 => [
                number(custom.and_then(|c| c.get("x"))).unwrap_or(0.5),
                number(custom.and_then(|c| c.get("y"))).unwrap_or(0.5),
            ],
            _ => [0.5, 0.5],
        }
    };
    let border_of = |border: Option<&Value>| -> [f64; 4] {
        ["x", "y", "z", "w"].map(|axis| number(border.and_then(|b| b.get(axis))).unwrap_or(0.0))
    };
    match settings.int("spriteMode", 1) {
        1 => Ok(vec![(
            SPRITE_FILE_ID,
            name.to_string(),
            [0.0, 0.0, width as f64, height as f64],
            pivot_of(settings.int("alignment", 0), settings.get("spritePivot")),
            border_of(settings.get("spriteBorder")),
        )]),
        2 => {
            let mut result = Vec::new();
            let entries = settings
                .get("spriteSheet.sprites")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for entry in entries {
                let id = number(entry.get("internalID"))
                    .map(|n| n as i64)
                    .filter(|id| *id != 0)
                    .context("a sprite of the sheet has no internalID")?;
                let rect = entry.get("rect");
                let rect = ["x", "y", "width", "height"]
                    .map(|axis| number(rect.and_then(|r| r.get(axis))).unwrap_or(0.0));
                let alignment = number(entry.get("alignment")).unwrap_or(0.0) as i64;
                result.push((
                    id,
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    rect,
                    pivot_of(alignment, entry.get("pivot")),
                    border_of(entry.get("border")),
                ));
            }
            Ok(result)
        }
        mode => Err(unsupported(format!("sprite mode {mode}"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn sprite_value(
    settings: &Settings,
    kind: &Kind,
    guid: &str,
    name: &str,
    rect: Rect,
    pivot: [f64; 2],
    border: [f64; 4],
    file_id: i64,
) -> Result<Val> {
    if settings.int("spriteMeshType", 1) != 0 {
        // A tight mesh follows the outline of the opaque pixels; a full rect
        // shows the same pixels.
    }
    let ppu = settings.float("spritePixelsToUnits", 100.0);
    let [x, y, w, h] = rect;
    let left = -pivot[0] * w / ppu;
    let right = (1.0 - pivot[0]) * w / ppu;
    let bottom = -pivot[1] * h / ppu;
    let top = (1.0 - pivot[1]) * h / ppu;
    let mut vertices = Vec::with_capacity(80);
    for (vx, vy) in [(left, top), (right, top), (left, bottom), (right, bottom)] {
        for component in [vx as f32, vy as f32, 0.0] {
            vertices.extend_from_slice(&component.to_le_bytes());
        }
    }
    vertices.extend_from_slice(&[0u8; 32]);
    let indices: Vec<u8> = [0u16, 1, 2, 2, 1, 3]
        .iter()
        .flat_map(|i| i.to_le_bytes())
        .collect();
    let mut value = kind.sample.clone();
    let float = |v: f64| Val::F(v as f32);
    let rect_val = |r: Rect| {
        Val::map([
            ("x", float(r[0])),
            ("y", float(r[1])),
            ("width", float(r[2])),
            ("height", float(r[3])),
        ])
    };
    let vector2 = |a: f64, b: f64| Val::map([("x", float(a)), ("y", float(b))]);
    set(&mut value, "m_Name", Val::Str(name.to_string()));
    set(&mut value, "m_Rect", rect_val(rect));
    set(&mut value, "m_Offset", vector2(0.0, 0.0));
    set(
        &mut value,
        "m_Border",
        Val::map([
            ("x", float(border[0])),
            ("y", float(border[1])),
            ("z", float(border[2])),
            ("w", float(border[3])),
        ]),
    );
    set(&mut value, "m_PixelsToUnits", float(ppu));
    set(&mut value, "m_Pivot", vector2(pivot[0], pivot[1]));
    set(
        &mut value,
        "m_Extrude",
        Val::U(settings.int("spriteExtrude", 1) as u64),
    );
    set(&mut value, "m_IsPolygon", Val::Bool(false));
    let words = super::managers::guid_words(guid).context("the source GUID is malformed")?;
    set(
        &mut value,
        "m_RenderDataKey",
        Val::Map(vec![
            (
                Cow::Borrowed("first"),
                Val::Map(
                    words
                        .iter()
                        .enumerate()
                        .map(|(i, w)| (Cow::Owned(format!("data[{i}]")), Val::U(*w as u64)))
                        .collect(),
                ),
            ),
            (Cow::Borrowed("second"), Val::I(file_id)),
        ]),
    );
    set(&mut value, "m_AtlasTags", Val::Seq(Vec::new()));
    set(&mut value, "m_PhysicsShape", Val::Seq(Vec::new()));
    set(&mut value, "m_Bones", Val::Seq(Vec::new()));
    set(&mut value, "m_ScriptableObjects", Val::Seq(Vec::new()));
    let Some(render) = value.field_mut("m_RD") else {
        bail!("the Sprite tree has no m_RD");
    };
    set(render, "textureRect", rect_val(rect));
    set(render, "textureRectOffset", vector2(0.0, 0.0));
    set(render, "atlasRectOffset", vector2(-1.0, -1.0));
    set(render, "settingsRaw", Val::U(0));
    set(
        render,
        "uvTransform",
        Val::map([
            ("x", float(ppu)),
            ("y", float(x + pivot[0] * w)),
            ("z", float(ppu)),
            ("w", float(y + pivot[1] * h)),
        ]),
    );
    set(render, "downscaleMultiplier", float(1.0));
    set(render, "m_IndexBuffer", Val::Bytes(indices));
    set(render, "m_Bindpose", Val::Seq(Vec::new()));
    set(render, "secondaryTextures", Val::Seq(Vec::new()));
    if let Some(Val::Seq(meshes)) = render.field_mut("m_SubMeshes") {
        let mut mesh = meshes.first().cloned().unwrap_or(Val::Zero);
        set(&mut mesh, "firstByte", Val::U(0));
        set(&mut mesh, "indexCount", Val::U(6));
        set(&mut mesh, "topology", Val::I(0));
        set(&mut mesh, "baseVertex", Val::U(0));
        set(&mut mesh, "firstVertex", Val::U(0));
        set(&mut mesh, "vertexCount", Val::U(4));
        *meshes = vec![mesh];
    }
    if let Some(data) = render.field_mut("m_VertexData") {
        set(data, "m_VertexCount", Val::U(4));
        set(data, "m_DataSize", Val::Bytes(vertices));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn pixels_are_stored_from_the_bottom_row_in_the_format() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let image = decode_png(&png_of(2, 2, &rgba)).unwrap();
        assert!(image.alpha);
        assert_eq!(
            pixels(&image, RGBA32),
            [9, 10, 11, 12, 13, 14, 15, 16, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            pixels(&image, RGB24),
            [9, 10, 11, 13, 14, 15, 1, 2, 3, 5, 6, 7]
        );
        assert_eq!(pixels(&image, ALPHA8), [12, 16, 4, 8]);
    }

    #[test]
    fn the_standalone_entry_counts_only_when_overridden() {
        let meta = b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\nTextureImporter:\n  spriteMode: 2\n  spriteSheet:\n    sprites:\n    - name: a\n      rect: {x: 0, y: 2, width: 4, height: 6}\n      alignment: 7\n      internalID: 42\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    maxTextureSize: 512\n  - buildTarget: Standalone\n    maxTextureSize: 64\n    overridden: 0\n";
        let settings = Settings::read(meta).unwrap();
        assert_eq!(settings.platform_int("maxTextureSize", 0), 512);
        let sprites = sprites(&settings, "image", 8, 8).unwrap();
        assert_eq!(sprites.len(), 1);
        let (id, name, rect, pivot, _) = &sprites[0];
        assert_eq!((*id, name.as_str()), (42, "a"));
        assert_eq!(*rect, [0.0, 2.0, 4.0, 6.0]);
        assert_eq!(*pivot, [0.5, 0.0]);
    }
}
