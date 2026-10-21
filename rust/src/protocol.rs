//! The AssetImportWorker transport and what its messages are made of: frames,
//! relative blobs and the wire form of a GUID, which every supported release
//! shares, and the decoded results. What each message says is a release's own
//! (`engine`).
use crate::hex;
use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};

pub const MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;
pub type ImportAddress = [u8; 44];
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}
/// Fills the buffer completely: "EOF" before the first byte, "unexpected EOF" after it.
fn read_full(input: &mut impl Read, buffer: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        match input.read(&mut buffer[filled..]) {
            Ok(0) if filled == 0 => bail!("EOF"),
            Ok(0) => bail!("unexpected EOF"),
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => bail!("{error}"),
        }
    }
    Ok(())
}
pub fn read_frame(input: &mut impl Read) -> Result<Frame> {
    let mut header = [0u8; 9];
    read_full(input, &mut header)?;
    let n = u64::from_le_bytes(header[1..].try_into()?);
    let size = match header[0] {
        0 => {
            ensure!(n == 5, "unexpected handshake length");
            7
        }
        1 => {
            ensure!(
                n > 0 && n <= MAX_FRAME_PAYLOAD as u64,
                "application frame length is invalid"
            );
            n as usize
        }
        kind => bail!("unimplemented transport frame kind {kind}"),
    };
    let mut payload = vec![0; size];
    read_full(input, &mut payload)?;
    Ok(Frame {
        kind: header[0],
        payload,
    })
}
pub fn write_frame(output: &mut impl Write, frame: &Frame) -> Result<()> {
    let n = match frame.kind {
        0 => {
            ensure!(frame.payload.len() == 7, "handshake length is invalid");
            5
        }
        1 => {
            ensure!(
                !frame.payload.is_empty() && frame.payload.len() <= MAX_FRAME_PAYLOAD,
                "application frame length is invalid"
            );
            frame.payload.len()
        }
        kind => bail!("unimplemented transport frame kind {kind}"),
    };
    let mut header = [0u8; 9];
    header[0] = frame.kind;
    header[1..].copy_from_slice(&(n as u64).to_le_bytes());
    for mut data in [&header[..], &frame.payload[..]] {
        while !data.is_empty() {
            match output.write(data) {
                Ok(0) => bail!("short write"),
                Ok(count) => data = &data[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => bail!("{error}"),
            }
        }
    }
    Ok(())
}
pub fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset.checked_add(4).context("offset overflow")?)
            .context("field lies outside payload")?
            .try_into()?,
    ))
}
pub fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset.checked_add(8).context("offset overflow")?)
            .context("field lies outside payload")?
            .try_into()?,
    ))
}
pub fn put_u16(data: &mut [u8], field: usize, n: u16) {
    data[field..field + 2].copy_from_slice(&n.to_le_bytes());
}
pub fn put_u32(data: &mut [u8], field: usize, n: u32) {
    data[field..field + 4].copy_from_slice(&n.to_le_bytes());
}
pub fn put_u64(data: &mut [u8], field: usize, n: u64) {
    data[field..field + 8].copy_from_slice(&n.to_le_bytes());
}
pub fn relative_start(data: &[u8], field: usize) -> Result<usize> {
    let offset = u32_at(data, field)? as i32 as i64;
    let start = field as i64 + offset;
    ensure!(
        start >= 0 && start <= data.len() as i64,
        "blob points outside payload"
    );
    Ok(start as usize)
}
pub fn blob_string(data: &[u8], field: usize) -> Result<String> {
    ensure!(
        field.checked_add(4).is_some_and(|end| end <= data.len()),
        "blob string field lies outside payload"
    );
    let relative = u32_at(data, field)? as i32 as i64;
    if relative == 0 {
        return Ok(String::new());
    }
    let start = field as i64 + relative;
    ensure!(
        start >= 0 && start < data.len() as i64,
        "blob string points outside payload"
    );
    let tail = &data[start as usize..];
    let end = tail
        .iter()
        .position(|b| *b == 0)
        .context("blob string is not terminated")?;
    match std::str::from_utf8(&tail[..end]) {
        Ok(text) => Ok(text.into()),
        Err(_) => bail!("blob string contains invalid UTF-8"),
    }
}
pub fn blob_array(data: &[u8], field: usize, stride: usize) -> Result<(&[u8], usize)> {
    ensure!(
        stride > 0 && field.checked_add(8).is_some_and(|end| end <= data.len()),
        "blob array field is invalid"
    );
    let relative = u32_at(data, field)? as i32 as i64;
    let count = u32_at(data, field + 4)? as usize;
    if count == 0 {
        return Ok((&[], 0));
    }
    let start = field as i64 + relative;
    let bytes = (count as u64).checked_mul(stride as u64);
    let end = bytes.and_then(|bytes| bytes.checked_add(start.max(0) as u64));
    match end {
        Some(end) if relative != 0 && start >= 0 && end <= data.len() as u64 => {
            Ok((&data[start as usize..end as usize], count))
        }
        _ => bail!("blob array points outside payload"),
    }
}
pub fn guid_wire(text: &str) -> Result<[u8; 16]> {
    ensure!(
        text.len() == 32,
        "asset GUID must have 32 hexadecimal characters"
    );
    if let Some(byte) = text.bytes().find(|b| !b.is_ascii_hexdigit()) {
        // `%#U` of the offending byte, as encoding/hex reports it.
        let shown = char::from(byte);
        if shown.is_control() || (0x80..0xa1).contains(&byte) || byte == 0xad {
            bail!("asset GUID: hex: invalid byte: U+{byte:04X}");
        }
        bail!("asset GUID: hex: invalid byte: U+{byte:04X} '{shown}'");
    }
    let mut bytes = [0; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let value = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).context("asset GUID")?;
        *byte = value.rotate_left(4);
    }
    Ok(bytes)
}
pub fn guid_text(bytes: &[u8]) -> String {
    hex(&bytes.iter().map(|b| b.rotate_left(4)).collect::<Vec<_>>())
}
/// What a worker returned for one import, as the Editor keeps it in its
/// ArtifactDB: the address, the result ID and the dependency, metadata and
/// statistics blobs.
pub struct ImportRecord {
    pub address: ImportAddress,
    pub result: [u8; 16],
    pub blobs: Vec<Vec<u8>>,
}
#[derive(Debug, PartialEq)]
pub struct ImportResponse {
    pub address: ImportAddress,
    pub result_id: String,
    pub error_code: u8,
    pub flag: bool,
    pub error_message: String,
    pub paths: Vec<String>,
    pub artifact_blobs: Vec<Vec<u8>>,
    pub dependencies: Vec<ImportAddress>,
    pub consistency_failure_count: i32,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactFile {
    pub kind: u32,
    pub name: String,
    pub hash: [u8; 16],
    pub inline_data: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactMetadata {
    pub output_id: String,
    pub address: ImportAddress,
    pub files: Vec<ArtifactFile>,
}
impl Default for ArtifactMetadata {
    fn default() -> Self {
        Self {
            output_id: String::new(),
            address: [0; 44],
            files: Vec::new(),
        }
    }
}
// The JSON shape of the artifact structs: fixed byte arrays are number
// arrays, byte vectors are base64, and lists that were never filled are null.
mod wire_shape {
    use super::{ArtifactFile, ArtifactMetadata};
    use base64::Engine;
    use serde::{
        Deserialize, Deserializer, Serialize, Serializer,
        de::Error,
        ser::{SerializeStruct, SerializeTuple},
    };
    struct Bytes<'a>(&'a [u8]);
    impl Serialize for Bytes<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut state = serializer.serialize_tuple(self.0.len())?;
            for byte in self.0 {
                state.serialize_element(byte)?;
            }
            state.end()
        }
    }
    /// Fills as many elements as the JSON array offers and zeroes the rest.
    fn array<'de, D: Deserializer<'de>, const N: usize>(
        value: Option<Vec<u8>>,
    ) -> Result<[u8; N], D::Error> {
        let mut out = [0; N];
        for (target, byte) in out.iter_mut().zip(value.unwrap_or_default()) {
            *target = byte;
        }
        Ok(out)
    }
    impl Serialize for ArtifactFile {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut state = serializer.serialize_struct("ArtifactFile", 4)?;
            state.serialize_field("Type", &self.kind)?;
            state.serialize_field("Name", &self.name)?;
            state.serialize_field("Hash", &Bytes(&self.hash))?;
            let inline = (!self.inline_data.is_empty())
                .then(|| base64::engine::general_purpose::STANDARD.encode(&self.inline_data));
            state.serialize_field("InlineData", &inline)?;
            state.end()
        }
    }
    #[derive(Deserialize)]
    #[serde(default, rename_all = "PascalCase")]
    #[derive(Default)]
    struct File {
        r#type: Option<u32>,
        name: Option<String>,
        hash: Option<Vec<u8>>,
        inline_data: Option<String>,
    }
    impl<'de> Deserialize<'de> for ArtifactFile {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let file = File::deserialize(deserializer)?;
            Ok(Self {
                kind: file.r#type.unwrap_or_default(),
                name: file.name.unwrap_or_default(),
                hash: array::<D, 16>(file.hash)?,
                inline_data: base64::engine::general_purpose::STANDARD
                    .decode(file.inline_data.unwrap_or_default())
                    .map_err(D::Error::custom)?,
            })
        }
    }
    impl Serialize for ArtifactMetadata {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut state = serializer.serialize_struct("ArtifactMetaInfo", 3)?;
            state.serialize_field("OutputID", &self.output_id)?;
            state.serialize_field("Address", &Bytes(&self.address))?;
            state.serialize_field("Files", &(!self.files.is_empty()).then_some(&self.files))?;
            state.end()
        }
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Metadata {
        #[serde(rename = "OutputID")]
        output_id: Option<String>,
        #[serde(rename = "Address")]
        address: Option<Vec<u8>>,
        #[serde(rename = "Files")]
        files: Option<Vec<ArtifactFile>>,
    }
    impl<'de> Deserialize<'de> for ArtifactMetadata {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let metadata = Metadata::deserialize(deserializer)?;
            Ok(Self {
                output_id: metadata.output_id.unwrap_or_default(),
                address: array::<D, 44>(metadata.address)?,
                files: metadata.files.unwrap_or_default(),
            })
        }
    }
}
