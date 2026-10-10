//! Independent Rust implementations of Unity Micro Editor's source formats.
pub mod agent;
pub mod agentcontrol;
pub mod authoring;
pub mod cache;
pub mod cancel;
pub mod compiler;
pub mod editorbridge;
pub mod flags;
pub mod graph;
pub mod jsondoc;
pub mod lmdb;
pub mod mcp;
pub mod native;
pub mod packages;
pub mod parity;
pub mod paths;
pub mod playerbake;
pub mod project;
pub mod protocol;
pub mod registry;
pub mod runner;
pub mod runtimeartifact;
pub mod sourcegraph;
pub mod sysutil;
pub mod util;
pub mod yaml;

pub const ENGINE_VERSION: &str = "6000.6.3f1";
pub const ENGINE_PROFILE: &str = include_str!("../../engine-profile.json");

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}
