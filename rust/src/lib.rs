//! Independent Rust implementations of Unity Micro Editor's source formats.
//!
//! The layers live in the crates of `rust/crates`, each rebuilt only when it or
//! a layer below it changes; their modules are re-exported here under the
//! paths they always had.
pub use ume_agentcontrol::agentcontrol;
pub use ume_authoring::authoring;
pub use ume_base::{
    ENGINE_PROFILE, ENGINE_VERSION, cache, cancel, editorbridge, flags, graph, hashstore, hex,
    jsondoc, lmdb, packages, parity, paths, project, protocol, registry, release, runtimeartifact,
    sha256, sysutil, util, yaml,
};
pub use ume_compiler::{compiler, hotreload, sourcegraph};
pub use ume_mcp::mcp;
pub use ume_native::{engine, native};
pub use ume_playerbake::playerbake;
pub use ume_remote::remote;
pub use ume_runner::runner;

pub mod agent;

pub mod cache_prune;
