use super::{MAXIMUM_BYTES, Reader, Tree, adapt::Resource, le32, le64, req};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextureMetadata {
    pub width: i32,
    pub height: i32,
    pub format: i32,
    pub mip_count: i32,
    pub complete_image_size: u32,
}

struct Walk<'a> {
    tree: Tree<'a>,
    r: Reader<'a>,
    resources: Vec<Resource>,
    metadata: TextureMetadata,
}

impl<'a> Walk<'a> {
    // This reader resolves only local names; common-string offsets read as "".
    fn name(&self, index: usize) -> &'a [u8] {
        self.tree
            .local(self.tree.offset(index, 8))
            .unwrap_or_default()
    }
    fn walk(&mut self, index: usize) -> Result<()> {
        let next = self.tree.end(index);
        let size = self.tree.byte_size(index);
        if self.name(index) == b"m_StreamData" {
            let offset = self.r.take(8)?;
            let size = self.r.u32()?;
            let path = self.r.string()?;
            let start = le64(offset);
            if !path.is_empty() || size != 0 || start != 0 {
                if !path.starts_with("VirtualArtifacts/Primary/") || !path.ends_with(".resource") {
                    bail!("native texture resource has an unsupported virtual path");
                }
                self.resources.push(Resource {
                    virtual_path: path,
                    offset: start,
                    size: size as u64,
                });
            }
        } else if self.tree.is_array(index) {
            let length = req(
                self.r.u32(),
                |length| *length as usize <= MAXIMUM_BYTES,
                "native texture array size is invalid",
            )?;
            if !self.tree.array_shape(index, next) {
                bail!("native texture array element tree is invalid");
            }
            let element = index + 2;
            let byte_size = self.tree.byte_size(element);
            if byte_size >= 0 && !self.tree.aligned(element) {
                let bytes = length as u64 * byte_size as u64;
                if bytes > MAXIMUM_BYTES as u64 {
                    bail!("native texture array exceeds bounds");
                }
                self.r.take(bytes as usize)?;
            } else {
                for _ in 0..length {
                    self.walk(element)?;
                }
            }
        } else if size >= 0 {
            if size == 4 && self.r.pos + 4 <= self.r.data.len() {
                let value = le32(&self.r.data[self.r.pos..]);
                match self.name(index) {
                    b"m_Width" => self.metadata.width = value as i32,
                    b"m_Height" => self.metadata.height = value as i32,
                    b"m_TextureFormat" => self.metadata.format = value as i32,
                    b"m_MipCount" => self.metadata.mip_count = value as i32,
                    b"m_CompleteImageSize" => self.metadata.complete_image_size = value,
                    _ => {}
                }
            }
            self.r.take(size as usize)?;
        } else if index + 1 < next {
            let mut child = index + 1;
            while child < next {
                self.walk(child)?;
                child = self.tree.end(child);
            }
        } else {
            bail!("native texture type has an unsupported dynamic leaf");
        }
        if self.tree.aligned(index) {
            self.r.align();
            if self.r.pos > self.r.data.len() {
                bail!("native texture alignment exceeds object bounds");
            }
        }
        Ok(())
    }
}

/// Walks the imported inline tree rather than assuming offsets into Texture2D.
/// Pixel and mip bytes are skipped in constant time.
pub fn read_texture_resources(
    data: &[u8],
    tree: &[u8],
) -> Result<(Vec<Resource>, TextureMetadata)> {
    if tree.len() < 16 {
        bail!("native Texture2D type tree is missing");
    }
    let count = le32(&tree[8..12]) as usize;
    if count == 0 || count > 10000 || 16 + count * 32 > tree.len() {
        bail!("native Texture2D type tree nodes are invalid");
    }
    let mut walk = Walk {
        tree: Tree {
            nodes: &tree[16..16 + count * 32],
            chars: &tree[16 + count * 32..],
            count,
        },
        r: Reader::new(data),
        resources: Vec::new(),
        metadata: TextureMetadata::default(),
    };
    walk.walk(0)?;
    if walk.r.pos != data.len() {
        bail!("native Texture2D payload does not match its inline type tree");
    }
    let m = &walk.metadata;
    if m.width <= 0
        || m.height <= 0
        || m.width > 32768
        || m.height > 32768
        || m.format <= 0
        || m.mip_count <= 0
        || m.mip_count > 16
    {
        bail!("native Texture2D dimensions, format or mip count are invalid");
    }
    Ok((walk.resources, walk.metadata))
}
