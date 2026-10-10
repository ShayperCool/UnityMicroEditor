//! Readable RGBAHalf cubemaps authored as Unity YAML.
use super::{
    manifest::reference_list,
    mesh::hex_len,
    session::Inner,
    value::{self, Map, Value, get, number, object},
};
use anyhow::{Result, bail};

impl Inner {
    pub(crate) fn refresh_authored_cubemap(&mut self, asset: &mut Map) -> Result<()> {
        let path = value::text(asset, "path").into_owned();
        let physical = self.source(&path)?;
        self.depend(&path)?;
        let documents = self.parse(&physical)?;
        if documents.len() != 1 || documents[0].class_id != 89 {
            bail!("raw cubemap {path} needs one class89 document");
        }
        let data = documents[0].fields();
        let n = |key: &str| number(get(data, key));
        let width = n("m_Width");
        if n("serializedVersion") != 4
            || width <= 0
            || width > 2048
            || n("m_Height") != width
            || n("m_TextureFormat") != 17
            || n("m_ColorSpace") != 0
            || n("m_MipCount") != 1
            || n("m_MipsStripped") != 0
            || n("m_ImageCount") != 6
            || n("m_TextureDimension") != 4
            || n("m_IsReadable") != 1
        {
            bail!("raw cubemap {path} requires readable RGBAHalf six-face single-mip source");
        }
        let stream = object(get(data, "m_StreamData"));
        if number(get(stream, "size")) != 0
            || number(get(stream, "offset")) != 0
            || !value::text(stream, "path").is_empty()
            || !value::text(data, "m_PlatformBlob").is_empty()
            || n("m_StreamingMipmaps") != 0
            || n("m_VTOnly") != 0
        {
            bail!("raw cubemap {path} contains unsupported streamed/platform data");
        }
        let pixels = hex_len(&value::text(data, "_typelessdata"))
            .ok()
            .map(|len| len as i64);
        if pixels != Some(6 * width * width * 8)
            || Some(n("image data")) != pixels
            || n("m_CompleteImageSize") != width * width * 8
        {
            bail!("raw cubemap {path} has inconsistent authored pixel bytes");
        }
        asset.insert("classId".into(), Value::Int(89));
        asset.insert("typeName".into(), "UnityEngine.Cubemap".into());
        asset.insert("fileId".into(), documents[0].file_id.as_str().into());
        asset.insert("name".into(), value::text(data, "m_Name").as_ref().into());
        let encoded = value::compact_map(data);
        asset.insert(
            "dataJson".into(),
            encoded.as_deref().unwrap_or_default().into(),
        );
        asset.insert("references".into(), reference_list(data));
        encoded.map(|_| ())
    }
}
