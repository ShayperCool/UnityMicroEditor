//! SpookyHash V2, 128 bits (Bob Jenkins, public domain), as Unity 6000.0
//! keys its source files, artifact files and asset database records with
//! it: seeds 0 and 0, the two 64-bit halves little-endian, first half first.
//! A message fed in pieces hashes as the whole message does.

const CONST: u64 = 0xdead_beef_dead_beef;
const VARIABLES: usize = 12;
const BLOCK: usize = VARIABLES * 8;
const BUFFER: usize = 2 * BLOCK;

/// The 128-bit hash of `data` with seeds 0 and 0.
pub fn hash128(data: &[u8]) -> [u8; 16] {
    let mut hasher = Spooky128::new();
    hasher.update(data);
    hasher.digest()
}

/// The incremental form: `update` any number of times, then `digest`.
#[derive(Clone)]
pub struct Spooky128 {
    data: [u8; BUFFER],
    state: [u64; VARIABLES],
    length: usize,
    remainder: usize,
}

impl Default for Spooky128 {
    fn default() -> Self {
        Self::new()
    }
}

impl Spooky128 {
    pub fn new() -> Self {
        Self::with_seeds(0, 0)
    }

    pub fn with_seeds(first: u64, second: u64) -> Self {
        let mut state = [0; VARIABLES];
        state[0] = first;
        state[1] = second;
        Self {
            data: [0; BUFFER],
            state,
            length: 0,
            remainder: 0,
        }
    }

    pub fn update(&mut self, mut message: &[u8]) {
        let total = message.len() + self.remainder;
        if total < BUFFER {
            self.data[self.remainder..total].copy_from_slice(message);
            self.length += message.len();
            self.remainder = total;
            return;
        }
        let mut h = if self.length < BUFFER {
            let [a, b, ..] = self.state;
            [a, b, CONST, a, b, CONST, a, b, CONST, a, b, CONST]
        } else {
            self.state
        };
        self.length += message.len();
        if self.remainder > 0 {
            let prefix = BUFFER - self.remainder;
            self.data[self.remainder..].copy_from_slice(&message[..prefix]);
            let buffer = self.data;
            mix(&words(&buffer[..BLOCK]), &mut h);
            mix(&words(&buffer[BLOCK..]), &mut h);
            message = &message[prefix..];
        }
        let whole = message.len() / BLOCK * BLOCK;
        for block in message[..whole].as_chunks::<BLOCK>().0 {
            mix(&words(block), &mut h);
        }
        let rest = &message[whole..];
        self.data[..rest.len()].copy_from_slice(rest);
        self.remainder = rest.len();
        self.state = h;
    }

    pub fn digest(&self) -> [u8; 16] {
        let (first, second) = if self.length < BUFFER {
            short(&self.data[..self.length], self.state[0], self.state[1])
        } else {
            let mut h = self.state;
            let mut data = self.data;
            let mut start = 0;
            let mut remainder = self.remainder;
            if remainder >= BLOCK {
                mix(&words(&data[..BLOCK]), &mut h);
                start = BLOCK;
                remainder -= BLOCK;
            }
            let block = &mut data[start..start + BLOCK];
            block[remainder..].fill(0);
            block[BLOCK - 1] = remainder as u8;
            end(&words(block), &mut h);
            (h[0], h[1])
        };
        let mut out = [0; 16];
        out[..8].copy_from_slice(&first.to_le_bytes());
        out[8..].copy_from_slice(&second.to_le_bytes());
        out
    }
}

fn words(block: &[u8]) -> [u64; VARIABLES] {
    let mut out = [0; VARIABLES];
    for (word, bytes) in out.iter_mut().zip(block.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    out
}

fn mix(data: &[u64; VARIABLES], s: &mut [u64; VARIABLES]) {
    const ROTATE: [u32; VARIABLES] = [11, 32, 43, 31, 17, 28, 39, 57, 55, 54, 22, 46];
    for i in 0..VARIABLES {
        s[i] = s[i].wrapping_add(data[i]);
        s[(i + 2) % VARIABLES] ^= s[(i + 10) % VARIABLES];
        s[(i + 11) % VARIABLES] ^= s[i];
        s[i] = s[i].rotate_left(ROTATE[i]);
        s[(i + 11) % VARIABLES] = s[(i + 11) % VARIABLES].wrapping_add(s[(i + 1) % VARIABLES]);
    }
}

fn end_partial(h: &mut [u64; VARIABLES]) {
    const ROTATE: [u32; VARIABLES] = [44, 15, 34, 21, 38, 33, 10, 13, 38, 53, 42, 54];
    for (i, rotation) in ROTATE.iter().enumerate() {
        let (a, b, c) = (
            (i + 11) % VARIABLES,
            (i + 1) % VARIABLES,
            (i + 2) % VARIABLES,
        );
        h[a] = h[a].wrapping_add(h[b]);
        h[c] ^= h[a];
        h[b] = h[b].rotate_left(*rotation);
    }
}

fn end(data: &[u64; VARIABLES], h: &mut [u64; VARIABLES]) {
    for (state, word) in h.iter_mut().zip(data) {
        *state = state.wrapping_add(*word);
    }
    for _ in 0..3 {
        end_partial(h);
    }
}

fn short_mix(h: &mut [u64; 4]) {
    const ROTATE: [u32; 12] = [50, 52, 30, 41, 54, 48, 38, 37, 62, 34, 5, 36];
    for (round, rotation) in ROTATE.iter().enumerate() {
        let i = (round + 2) % 4;
        h[i] = h[i].rotate_left(*rotation);
        h[i] = h[i].wrapping_add(h[(i + 1) % 4]);
        h[(i + 2) % 4] ^= h[i];
    }
}

fn short_end(h: &mut [u64; 4]) {
    const ROTATE: [u32; 11] = [15, 52, 26, 51, 28, 9, 47, 54, 32, 25, 63];
    for (round, rotation) in ROTATE.iter().enumerate() {
        let (a, b) = ((round + 3) % 4, (round + 2) % 4);
        h[a] ^= h[b];
        h[b] = h[b].rotate_left(*rotation);
        h[a] = h[a].wrapping_add(h[b]);
    }
}

/// The form of messages shorter than two blocks.
fn short(message: &[u8], first: u64, second: u64) -> (u64, u64) {
    let word = |at: usize| u64::from_le_bytes(message[at..at + 8].try_into().unwrap());
    let length = message.len();
    let mut remainder = length % 32;
    let mut h = [first, second, CONST, CONST];
    let mut at = 0;
    if length > 15 {
        while at + 32 <= length {
            h[2] = h[2].wrapping_add(word(at));
            h[3] = h[3].wrapping_add(word(at + 8));
            short_mix(&mut h);
            h[0] = h[0].wrapping_add(word(at + 16));
            h[1] = h[1].wrapping_add(word(at + 24));
            at += 32;
        }
        if remainder >= 16 {
            h[2] = h[2].wrapping_add(word(at));
            h[3] = h[3].wrapping_add(word(at + 8));
            short_mix(&mut h);
            at += 16;
            remainder -= 16;
        }
    }
    h[3] = h[3].wrapping_add((length as u64) << 56);
    let tail = &message[at..at + remainder];
    let byte = |index: usize, shift: u32| (tail[index] as u64) << shift;
    if remainder == 0 {
        h[2] = h[2].wrapping_add(CONST);
        h[3] = h[3].wrapping_add(CONST);
    } else {
        // Bytes 8 and on go to the fourth word, the first eight to the third.
        let mut high = 0u64;
        for index in 8..remainder {
            high = high.wrapping_add(byte(index, 8 * (index as u32 - 8)));
        }
        let mut low = 0u64;
        for index in 0..remainder.min(8) {
            low = low.wrapping_add(byte(index, 8 * index as u32));
        }
        h[3] = h[3].wrapping_add(high);
        h[2] = h[2].wrapping_add(low);
    }
    short_end(&mut h);
    (h[0], h[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|i| (i * 7 + 3) as u8).collect()
    }

    /// Values of the reference implementation (`SpookyV2.cpp`, through the
    /// `spookyhash` Python binding).
    #[test]
    fn matches_the_reference_at_every_length_class() {
        for (length, expected) in [
            (0, "1909f56bfc062723c751e8b465ee728b"),
            (1, "30ff9c894cfa874ba576111a658bb681"),
            (3, "6219cb8c14d27057452fd88ab0ca0b13"),
            (4, "8979203eb7a022fda1199874a7e55727"),
            (7, "422cb0be12949ea9eb8932b5bb0d8a68"),
            (8, "fdc5a396e888fa931978ef9a765d69d5"),
            (11, "5644ffdfa637a3bd77b028eafc2ec74f"),
            (12, "e3b807c96f30c055a496c0eeb40f5888"),
            (15, "e9fcf3a55725ff8055ed74303024919f"),
            (16, "edc4d07c4d236b99d7fa5d200f8f808a"),
            (31, "0c2eb989d843ccb8c543cc0c398c3b83"),
            (32, "f4400e6985990be1362d8493f14c5528"),
            (36, "15ecf8e5316b4fc575b84eb34d896352"),
            (95, "9d394355f0c3040affcf0285fe0eb5bb"),
            (96, "121ab5e1c48e921f9545a889701a485a"),
            (191, "0400e72167558b5a43cb7a1aa68bbebf"),
            (192, "cfb9b3c11f23d54b76f4d66207440df7"),
            (193, "c2b37456be9e8f89a21a94f0ba9f52e9"),
            (287, "ecb0bc9626b160ef7c9af940441c894b"),
            (288, "fbae789466efa270605170aa052a8f33"),
            (300, "a4b6dfcb2a2aed5f8c2a3e3dfa2d1411"),
            (1000, "051ba518322d078548bfb5a92c671224"),
            (20000, "bbfd3d4fa2b76a03cfb661e834e3301d"),
        ] {
            let data = pattern(length);
            assert_eq!(crate::hex(&hash128(&data)), expected, "length {length}");
            // In pieces of every size the result is the same.
            for piece in [1, 5, 95, 96, 97, 191, 192, 500] {
                let mut hasher = Spooky128::new();
                for chunk in data.chunks(piece) {
                    hasher.update(chunk);
                }
                assert_eq!(
                    crate::hex(&hasher.digest()),
                    expected,
                    "{length} by {piece}"
                );
            }
        }
        let mut seeded = Spooky128::with_seeds(1, 2);
        seeded.update(&pattern(1000));
        assert_eq!(
            crate::hex(&seeded.digest()),
            "956682451a9424a56f48f37a0dc5e6f3"
        );
    }

    /// A key of a 6000.0 ArtifactDB: the GUID, importer ID and importer hash
    /// of `CurrentRevisions` hashed to the record's key.
    #[test]
    fn keys_an_import_address_as_unity_6000_0_does() {
        let text = "30b0ac7041bba3108584a5109b610cfeffffffff00000000000000000000000000000000";
        let address: Vec<u8> = (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect();
        assert_eq!(
            crate::hex(&hash128(&address)),
            "15e853505d8814340254ac88d1c9b651"
        );
    }
}
