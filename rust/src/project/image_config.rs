//! Image dimensions read from the headers of PNG and JPEG data, with the decoder's error wording.
use anyhow::{Result, anyhow, bail};

fn eof() -> anyhow::Error {
    anyhow!("unexpected EOF")
}
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.data.len());
        let Some(end) = end else {
            self.at = self.data.len();
            return Err(eof());
        };
        let part = &self.data[self.at..end];
        self.at = end;
        Ok(part)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
}

fn png(data: &[u8]) -> Result<(i64, i64)> {
    let format = |text: &str| anyhow!("png: invalid format: {text}");
    let unsupported = |text: String| anyhow!("png: unsupported feature: {text}");
    let mut r = Cursor { data, at: 8 };
    // 0 start, 1 IHDR, 2 PLTE, 3 tRNS, 4 IDAT.
    let (mut stage, mut paletted, mut true_color, mut gray) = (0, false, false, false);
    let (mut depth, mut size) = (0u32, (0i64, 0i64));
    loop {
        let head = r.take(8)?;
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind = &head[4..8];
        let checked = |r: &mut Cursor, body: &[u8]| -> Result<()> {
            let sum = r.take(4)?;
            if u32::from_be_bytes([sum[0], sum[1], sum[2], sum[3]]) != crc32(&[kind, body]) {
                return Err(format("invalid checksum"));
            }
            Ok(())
        };
        match kind {
            b"IHDR" => {
                if stage != 0 {
                    return Err(format("chunk out of order"));
                }
                stage = 1;
                if length != 13 {
                    return Err(format("bad IHDR length"));
                }
                let body = r.take(13)?;
                if body[10] != 0 {
                    return Err(unsupported("compression method".into()));
                }
                if body[11] != 0 {
                    return Err(unsupported("filter method".into()));
                }
                if body[12] > 1 {
                    return Err(format("invalid interlace method"));
                }
                let w = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let h = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                if w <= 0 || h <= 0 {
                    return Err(format("non-positive dimension"));
                }
                if (w as i64 * h as i64).checked_mul(8).is_none() {
                    return Err(unsupported("dimension overflow".into()));
                }
                let valid = matches!(
                    (body[8], body[9]),
                    (1 | 2 | 4, 0 | 3) | (8, 0 | 2 | 3 | 4 | 6) | (16, 0 | 2 | 4 | 6)
                );
                if !valid {
                    return Err(unsupported(format!(
                        "bit depth {}, color type {}",
                        body[8], body[9]
                    )));
                }
                depth = body[8] as u32;
                paletted = body[9] == 3;
                true_color = body[9] == 2;
                gray = body[9] == 0;
                size = (w as i64, h as i64);
                checked(&mut r, body)?;
            }
            b"PLTE" => {
                if stage != 1 {
                    return Err(format("chunk out of order"));
                }
                stage = 2;
                let entries = length / 3;
                if length % 3 != 0 || entries == 0 || entries > 256 || entries > 1 << depth.min(31)
                {
                    return Err(format("bad PLTE length"));
                }
                let body = r.take(length as usize)?;
                if !paletted {
                    return Err(format("PLTE, color type mismatch"));
                }
                checked(&mut r, body)?;
            }
            b"tRNS" => {
                let ordered = if paletted {
                    stage == 2
                } else if true_color {
                    stage == 1 || stage == 2
                } else {
                    stage == 1
                };
                if !ordered {
                    return Err(format("chunk out of order"));
                }
                stage = 3;
                if !paletted {
                    return Err(format(if gray || true_color {
                        "bad tRNS length"
                    } else {
                        "tRNS, color type mismatch"
                    }));
                }
                if length > 256 {
                    return Err(format("bad tRNS length"));
                }
                let body = r.take(length as usize)?;
                checked(&mut r, body)?;
            }
            b"IDAT" => {
                if stage < 1 || stage == 1 && paletted {
                    return Err(format("chunk out of order"));
                }
                stage = 4;
            }
            b"IEND" => return Err(format("chunk out of order")),
            _ => {
                if length > 0x7fff_ffff {
                    return Err(anyhow!("png: invalid format: Bad chunk length: {length}"));
                }
                let body = r.take(length as usize)?;
                checked(&mut r, body)?;
            }
        }
        if paletted && stage >= 3 || !paletted && stage >= 1 {
            return Ok(size);
        }
    }
}

fn jpeg(data: &[u8]) -> Result<(i64, i64)> {
    let format = |text: &str| anyhow!("invalid JPEG format: {text}");
    let unsupported = |text: &str| anyhow!("unsupported JPEG feature: {text}");
    let mut r = Cursor { data, at: 2 };
    let (mut components, mut size, mut jfif) = (0usize, (0i64, 0i64), false);
    loop {
        let pair = r.take(2)?;
        let (mut first, mut marker) = (pair[0], pair[1]);
        while first != 0xff {
            first = marker;
            marker = r.byte()?;
        }
        if marker == 0 {
            continue;
        }
        while marker == 0xff {
            marker = r.byte()?;
        }
        if marker == 0xd9 {
            return Err(format("missing SOS marker"));
        }
        if (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = r.take(2)?;
        let n = (length[0] as i64) << 8 | length[1] as i64;
        if n < 2 {
            return Err(format("short segment length"));
        }
        let n = n as usize - 2;
        match marker {
            0xc0..=0xc2 => {
                if components != 0 {
                    return Err(format("multiple SOF markers"));
                }
                components = match n {
                    9 => 1,
                    15 => 3,
                    18 => 4,
                    _ => return Err(unsupported("number of components")),
                };
                let body = r.take(n)?;
                if body[0] != 8 {
                    return Err(unsupported("precision"));
                }
                size = (
                    (body[3] as i64) << 8 | body[4] as i64,
                    (body[1] as i64) << 8 | body[2] as i64,
                );
                if body[5] as usize != components {
                    return Err(format("SOF has wrong length"));
                }
                let mut sampling = [(0u8, 0u8); 4];
                for i in 0..components {
                    let id = body[6 + 3 * i];
                    if (0..i).any(|j| body[6 + 3 * j] == id) {
                        return Err(format("repeated component identifier"));
                    }
                    if body[8 + 3 * i] > 3 {
                        return Err(format("bad Tq value"));
                    }
                    let hv = body[7 + 3 * i];
                    let (mut h, mut v) = (hv >> 4, hv & 0x0f);
                    if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                        return Err(format("luma/chroma subsampling ratio"));
                    }
                    let ratio = || unsupported("luma/chroma subsampling ratio");
                    if h == 3 || v == 3 {
                        return Err(ratio());
                    }
                    let bad = match (components, i) {
                        (1, _) => {
                            (h, v) = (1, 1);
                            false
                        }
                        (3, 0) => v == 4,
                        (3, 1) => sampling[0].0 % h != 0 || sampling[0].1 % v != 0,
                        (3, _) => sampling[1] != (h, v),
                        (4, 0) => hv != 0x11 && hv != 0x22,
                        (4, 1 | 2) => hv != 0x11,
                        (4, _) => sampling[0] != (h, v),
                        _ => false,
                    };
                    if bad {
                        return Err(ratio());
                    }
                    sampling[i] = (h, v);
                }
                if jfif {
                    return Ok(size);
                }
            }
            0xda => {
                if components == 0 {
                    return Err(format("missing SOF marker"));
                }
                return Ok(size);
            }
            0xe0 => {
                let body = r.take(n)?;
                if n >= 5 {
                    jfif = &body[..5] == b"JFIF\0";
                }
            }
            0xc4 | 0xdb | 0xdd | 0xe1..=0xef | 0xfe => {
                r.take(n)?;
            }
            marker if marker < 0xc0 => return Err(format("unknown marker")),
            _ => return Err(unsupported("unknown marker")),
        }
    }
}

/// Width and height of a PNG or JPEG, or the decoder's error.
pub(crate) fn decode_config(data: &[u8]) -> Result<(i64, i64)> {
    if data.starts_with(&[0xff, 0xd8]) {
        jpeg(data)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(data)
    } else {
        bail!("image: unknown format")
    }
}
