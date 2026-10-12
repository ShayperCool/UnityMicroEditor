//! The Player form of an importer-based sprite atlas (`.spriteatlasv2`) and of
//! the sprites it packs.
//!
//! The importer leaves the packed result in an Editor-only object: the
//! `SpriteAtlas` of the artifact has an empty render data map, and a
//! `CachedSpriteAtlasRuntimeData` beside it holds one frame per packed sprite.
//! A build moves the frames into the map and writes each packed sprite with
//! its key into that map, the atlas tag, a pointer to the atlas and no texture
//! of its own. Both were compared field by field with a build of the pinned
//! engine: the map equals the cached frames, and a packed sprite differs from
//! the imported one in those four fields alone.
use super::{
    MAXIMUM_BYTES,
    adapt::{ExternalReference, Metadata, Object, RemovedObject},
    ordinary_guid,
};
use crate::playerbake::{External, Object as Written, Read, Val, read_serialized, write};
use anyhow::{Result, bail};
use std::collections::BTreeSet;

const SPRITE_ATLAS: i32 = 687078895;
const CACHED_RUNTIME_DATA: i32 = 644342135;
const SPRITE_ATLAS_ASSET: i32 = 612988286;
const SPRITE_ATLAS_IMPORTER: i32 = 1210832254;
const TEXTURE_2D: i32 = 28;
const SPRITE: i32 = 213;

/// What the sprites of an atlas need to know of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedAtlas {
    /// The local ID of the `SpriteAtlas` in its file.
    pub local_id: i64,
    pub tag: String,
    /// The packed sprites: the GUID of the texture asset and the local ID of
    /// the sprite in it.
    pub sprites: BTreeSet<(String, i64)>,
}

fn pointer(value: Option<&Val>) -> Option<(i64, i64)> {
    let value = value?;
    match (value.field("m_FileID")?, value.field("m_PathID")?) {
        (Val::I(file), Val::I(path)) => Some((*file, *path)),
        _ => None,
    }
}

fn null_pointer() -> Val {
    Val::map([("m_FileID", Val::I(0)), ("m_PathID", Val::I(0))])
}

/// The fields of a serialized GUID: four words.
const GUID_WORDS: [&str; 4] = ["data[0]", "data[1]", "data[2]", "data[3]"];

fn guid_text(value: &Val) -> Option<String> {
    let mut native = Vec::with_capacity(16);
    for name in GUID_WORDS {
        let Val::U(word) = value.field(name)? else {
            return None;
        };
        native.extend_from_slice(&(*word as u32).to_le_bytes());
    }
    Some(ordinary_guid(&native))
}

fn guid_words(text: &str) -> Result<Val> {
    let digits = text.as_bytes();
    if digits.len() != 32 || !digits.iter().all(u8::is_ascii_hexdigit) {
        bail!("sprite atlas member GUID is invalid");
    }
    let native: Vec<u8> = digits
        .chunks(2)
        .map(|pair| {
            let byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
            byte.rotate_left(4)
        })
        .collect();
    let mut words = native
        .chunks(4)
        .map(|word| Val::U(u32::from_le_bytes(word.try_into().unwrap()) as u64));
    Ok(Val::map(
        GUID_WORDS.map(|name| (name, words.next().unwrap())),
    ))
}

fn set(value: &mut Val, name: &str, to: Val) -> Result<()> {
    match value.field_mut(name) {
        Some(field) => {
            *field = to;
            Ok(())
        }
        None => bail!("sprite atlas object lacks the field {name}"),
    }
}

fn read_bounded(input: &[u8]) -> Result<Vec<Read>> {
    if input.len() > MAXIMUM_BYTES {
        bail!("sprite atlas projection is limited to 64 MiB");
    }
    read_serialized(input)
}

/// Projects the artifact of a `.spriteatlasv2` to the atlas a Player loads:
/// the `SpriteAtlas` with the packed frames as its render data map, and the
/// packed textures. Native import content hashes must be verified by the
/// caller before using this projection.
pub fn adapt_sprite_atlas(input: &[u8]) -> Result<(Vec<u8>, Metadata, PackedAtlas)> {
    let objects = read_bounded(input)?;
    let mut result = Metadata::default();
    let (mut atlas, mut cached, mut textures) = (None, None, Vec::new());
    for object in &objects {
        match object.tree.class_id {
            SPRITE_ATLAS if atlas.is_none() => atlas = Some(object),
            CACHED_RUNTIME_DATA if cached.is_none() => cached = Some(object),
            TEXTURE_2D => textures.push(object),
            class @ (SPRITE_ATLAS_ASSET | SPRITE_ATLAS_IMPORTER | CACHED_RUNTIME_DATA) => {
                if class == CACHED_RUNTIME_DATA {
                    bail!("sprite atlas artifact holds more than one packed result");
                }
            }
            class => bail!("native class {class} is unsupported in a sprite atlas"),
        }
        if !matches!(object.tree.class_id, SPRITE_ATLAS | TEXTURE_2D) {
            result.removed.push(RemovedObject {
                file_id: object.local_id.to_string(),
                class_id: object.tree.class_id,
                reason: "editor-only".into(),
            });
        }
    }
    let (Some(atlas), Some(cached)) = (atlas, cached) else {
        bail!(
            "sprite atlas artifact holds no packed result; the project's sprite packer mode has to be Sprite Atlas V2"
        );
    };
    let mut value = atlas.tree.decode(&atlas.data)?;
    if value.field("m_IsVariant") != Some(&Val::Bool(false))
        || pointer(value.field("m_MasterAtlas")) != Some((0, 0))
    {
        bail!("a variant sprite atlas is not supported");
    }
    let Some(Val::Str(tag)) = value.field("m_Tag").cloned() else {
        bail!("sprite atlas has no tag");
    };
    let cached = cached.tree.decode(&cached.data)?;
    let Some(Val::Seq(frames)) = cached.field("frames") else {
        bail!("sprite atlas artifact has no packed frames");
    };
    if tag.is_empty() || frames.is_empty() {
        bail!("sprite atlas packs no sprite");
    }
    let texture_ids: BTreeSet<i64> = textures.iter().map(|texture| texture.local_id).collect();
    let mut sprites = BTreeSet::new();
    for frame in frames {
        let key = frame.field("first");
        let guid = key.and_then(|key| key.field("first")).and_then(guid_text);
        let (Some(guid), Some(Val::I(id))) = (guid, key.and_then(|key| key.field("second"))) else {
            bail!("sprite atlas frame has no sprite key");
        };
        let data = frame.field("second");
        let texture = pointer(data.and_then(|data| data.field("texture")));
        let alpha = pointer(data.and_then(|data| data.field("alphaTexture")));
        let secondary = data.and_then(|data| data.field("secondaryTextures"));
        if !texture.is_some_and(|(file, id)| file == 0 && texture_ids.contains(&id))
            || alpha != Some((0, 0))
            || !matches!(secondary, Some(Val::Seq(list)) if list.is_empty())
        {
            bail!(
                "sprite atlas frame of {guid} uses an alpha or secondary texture, or a texture outside the atlas"
            );
        }
        if !sprites.insert((guid, *id)) {
            bail!("sprite atlas packs a sprite twice");
        }
    }
    set(&mut value, "m_RenderDataMap", Val::Seq(frames.clone()))?;
    // What the Editor keeps of its sources points at objects this file does
    // not hold; a Player has no such field.
    if let Some(editor) = value.field_mut("m_EditorData") {
        set(editor, "packables", Val::Seq(Vec::new()))?;
        set(editor, "cachedData", null_pointer())?;
    }
    let data = atlas.tree.encode(&value)?;
    let mut written = vec![Written {
        local_id: atlas.local_id,
        tree: &atlas.tree,
        data,
        script: None,
    }];
    for texture in &textures {
        let decoded = texture.tree.decode(&texture.data)?;
        let stream = decoded.field("m_StreamData");
        if stream.and_then(|stream| stream.field("path")) != Some(&Val::Str(String::new())) {
            bail!("sprite atlas texture with streamed data is unsupported");
        }
        written.push(Written {
            local_id: texture.local_id,
            tree: &texture.tree,
            data: texture.data.clone(),
            script: None,
        });
    }
    written.sort_by_key(|object| object.local_id);
    for object in &written {
        result.objects.push(Object {
            file_id: object.local_id.to_string(),
            class_id: object.tree.class_id,
            type_name: if object.tree.class_id == SPRITE_ATLAS {
                "UnityEngine.U2D.SpriteAtlas"
            } else {
                "UnityEngine.Texture2D"
            }
            .into(),
            sha256: crate::sha256(&object.data),
        });
    }
    let output = write(&written, &[])?;
    result.profile = "spriteatlasv2".into();
    (
        result.serialized_version,
        result.source_target,
        result.runtime_target,
    ) = (23, -2, 24);
    result.input_sha256 = crate::sha256(input);
    result.output_sha256 = crate::sha256(&output);
    Ok((
        output,
        result,
        PackedAtlas {
            local_id: atlas.local_id,
            tag,
            sprites,
        },
    ))
}

/// Rewrites the sprites of an adapted texture that an atlas packs as a build
/// writes them, and makes the atlas the file's one external reference. The
/// metadata follows. A texture none of whose sprites the atlas packs is
/// refused: the caller paired it with the wrong atlas.
pub fn bind_packed_sprites(
    texture: &[u8],
    metadata: &mut Metadata,
    texture_guid: &str,
    atlas_guid: &str,
    atlas: &PackedAtlas,
) -> Result<Vec<u8>> {
    let objects = read_bounded(texture)?;
    if !metadata.external_references.is_empty() {
        bail!("a sprite texture with external references cannot be bound to an atlas");
    }
    let key = guid_words(texture_guid)?;
    let mut bound = 0;
    let mut written = Vec::new();
    for object in &objects {
        // The adapted file still lists the type of the importer it dropped.
        if !metadata
            .objects
            .iter()
            .any(|kept| kept.file_id == object.local_id.to_string())
        {
            continue;
        }
        let mut data = object.data.clone();
        if object.tree.class_id == SPRITE
            && atlas
                .sprites
                .contains(&(texture_guid.to_string(), object.local_id))
        {
            let mut value = object.tree.decode(&object.data)?;
            set(
                &mut value,
                "m_RenderDataKey",
                Val::map([("first", key.clone()), ("second", Val::I(object.local_id))]),
            )?;
            set(
                &mut value,
                "m_AtlasTags",
                Val::Seq(vec![Val::Str(atlas.tag.clone())]),
            )?;
            set(
                &mut value,
                "m_SpriteAtlas",
                Val::map([
                    ("m_FileID", Val::I(1)),
                    ("m_PathID", Val::I(atlas.local_id)),
                ]),
            )?;
            match value.field_mut("m_RD") {
                Some(render) => set(render, "texture", null_pointer())?,
                None => bail!("sprite has no render data"),
            }
            data = object.tree.encode(&value)?;
            bound += 1;
        }
        written.push((object, data));
    }
    if bound == 0 {
        bail!("the atlas packs no sprite of this texture");
    }
    let output = write(
        &written
            .iter()
            .map(|(object, data)| Written {
                local_id: object.local_id,
                tree: &object.tree,
                data: data.clone(),
                script: None,
            })
            .collect::<Vec<_>>(),
        &[External {
            guid: atlas_guid.to_string(),
        }],
    )?;
    for (object, data) in &written {
        if let Some(kept) = metadata
            .objects
            .iter_mut()
            .find(|kept| kept.file_id == object.local_id.to_string())
        {
            kept.sha256 = crate::sha256(data);
        }
    }
    metadata.external_references.push(ExternalReference {
        guid: atlas_guid.to_string(),
        kind: 3,
        path: String::new(),
    });
    metadata.output_sha256 = crate::sha256(&output);
    Ok(output)
}
