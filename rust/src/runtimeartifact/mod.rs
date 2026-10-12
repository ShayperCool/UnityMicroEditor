//! Projects Unity 6000.6.3f1 native worker artifacts into Linux Mono
//! ContentFiles. Supported native data stays byte-for-byte unchanged.
mod adapt;
mod atlas;
mod builtin_lighting;
mod builtin_shader;
mod builtin_shader_cmd;
mod files;
mod mesh_cpu;
mod model_graph;
mod player_shader;
mod scripted;
mod shader_extract;
pub mod shader_sources;
mod texture;
mod ui;
mod unityfs_member;
mod video;

pub use adapt::{
    AudioMetadata, ExternalReference, Metadata, Object, RemovedObject, Resource, adapt,
    audio_profile, read_native_audio_resource, text_profile,
};
pub use atlas::{PackedAtlas, adapt_sprite_atlas, bind_packed_sprites};
pub use builtin_lighting::{BuiltinLightingIdentity, inspect_builtin_lighting};
pub use builtin_shader::{BuiltinShaderIdentity, ReadAt, inspect_builtin_shader};
pub use builtin_shader_cmd::{BuiltinShaderOptions, builtin_shader_command, extract_builtin};
pub use mesh_cpu::{MeshCpuFlags, MeshCpuProjection, read_mesh_cpu_flags, retain_model_mesh_cpu};
pub use model_graph::{
    ModelGraphObject, ModelGraphProperty, ModelGraphReference, hash_model_graph,
};
pub use player_shader::{
    PlayerShaderMetadata, extract_builtin_player_shader, extract_player_shader, read_player_shader,
};
pub use scripted::adapt_scripted;
pub use shader_extract::{shader_extract, shader_extract_command};
pub use texture::{TextureMetadata, read_shaped_texture_resources, read_texture_resources};
pub use ui::{adapt_shader, adapt_texture, adapt_ui, texture_profile};
pub use unityfs_member::{unpack_unityfs_block, verify_unityfs_member};
pub use video::adapt_video;

use anyhow::{Result, bail};

pub(crate) const MAXIMUM_BYTES: usize = 64 << 20;

/// Accepts a read only when it succeeded and passes the check; either failure
/// is reported as the same refusal.
pub(crate) fn req<T>(
    value: Result<T>,
    accept: impl FnOnce(&T) -> bool,
    message: &str,
) -> Result<T> {
    match value {
        Ok(value) if accept(&value) => Ok(value),
        _ => bail!("{message}"),
    }
}

/// Unity writes names as UTF-8. Undecodable bytes become U+FFFD, so a damaged
/// name stays reportable; identity checks that must be exact compare the raw
/// bytes instead.
pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub(crate) fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}
pub(crate) fn be64(data: &[u8]) -> u64 {
    u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ])
}
pub(crate) fn le16(data: &[u8]) -> u16 {
    u16::from_le_bytes([data[0], data[1]])
}
pub(crate) fn le32(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}
pub(crate) fn le64(data: &[u8]) -> u64 {
    u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ])
}

/// Unity stores GUID nibbles swapped within each byte.
pub(crate) fn ordinary_guid(native: &[u8]) -> String {
    crate::hex(
        &native
            .iter()
            .map(|value| value.rotate_left(4))
            .collect::<Vec<_>>(),
    )
}

pub(crate) struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }
    pub fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        if self.pos > self.data.len() || size > self.data.len() - self.pos {
            bail!("native serialized data exceeds its bounds");
        }
        let result = &self.data[self.pos..self.pos + size];
        self.pos += size;
        Ok(result)
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(le32(self.take(4)?))
    }
    pub fn cstring_bytes(&mut self, maximum: usize) -> Result<&'a [u8]> {
        if self.pos > self.data.len() {
            bail!("native string offset exceeds data bounds");
        }
        let rest = &self.data[self.pos..];
        match rest.iter().position(|&byte| byte == 0) {
            Some(end) if end <= maximum => {
                self.pos += end + 1;
                Ok(&rest[..end])
            }
            _ => bail!("native string is missing or exceeds bounds"),
        }
    }
    pub fn cstring(&mut self, maximum: usize) -> Result<String> {
        self.cstring_bytes(maximum).map(text)
    }
    pub fn string_bytes(&mut self) -> Result<&'a [u8]> {
        let size = req(
            self.u32(),
            |size| *size <= 4096,
            "native object string length exceeds bounds",
        )?;
        let data = self.take(size as usize)?;
        self.align();
        if self.pos > self.data.len() {
            bail!("native object string alignment exceeds bounds");
        }
        Ok(data)
    }
    pub fn string(&mut self) -> Result<String> {
        self.string_bytes().map(text)
    }
    pub fn align(&mut self) {
        self.pos = (self.pos + 3) & !3;
    }
}

/// Inline type tree node table shared by the tree-walking readers.
pub(crate) struct Tree<'a> {
    pub nodes: &'a [u8],
    pub chars: &'a [u8],
    pub count: usize,
}

impl<'a> Tree<'a> {
    pub fn level(&self, index: usize) -> u8 {
        self.nodes[index * 32 + 2]
    }
    pub fn end(&self, index: usize) -> usize {
        let mut next = index + 1;
        while next < self.count && self.level(next) > self.level(index) {
            next += 1;
        }
        next
    }
    pub fn is_array(&self, index: usize) -> bool {
        self.nodes[index * 32 + 3] & 1 != 0
    }
    pub fn byte_size(&self, index: usize) -> i32 {
        le32(&self.nodes[index * 32 + 12..]) as i32
    }
    pub fn aligned(&self, index: usize) -> bool {
        le32(&self.nodes[index * 32 + 20..]) & 0x4000 != 0
    }
    pub fn offset(&self, index: usize, field: usize) -> u32 {
        le32(&self.nodes[index * 32 + field..])
    }
    /// Local string table entry; None when out of range or unterminated.
    pub fn local(&self, offset: u32) -> Option<&'a [u8]> {
        let value = self.chars.get(offset as usize..)?;
        if offset as usize >= self.chars.len() {
            return None;
        }
        let last = value.iter().position(|&byte| byte == 0)?;
        Some(&value[..last])
    }
    /// Array nodes need a size child and an element child one level deeper.
    pub fn array_shape(&self, index: usize, next: usize) -> bool {
        let level = self.level(index).wrapping_add(1);
        index + 2 < next && self.level(index + 1) == level && self.level(index + 2) == level
    }
}
