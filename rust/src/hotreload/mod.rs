//! C# hot reload of a watched session. After a successful compile the
//! runner asks for a code patch: the patch builder compares the assemblies
//! the running Player loaded with the newest ones and emits patch assemblies,
//! and the applier inside the Player redirects the changed methods to them.
//! An edit outside the supported set is never approximated: the attempt says
//! why, and only a Player restart on the newest compile applies it.
mod attempt;
mod tools;
mod wire;

#[cfg(test)]
mod tests;

pub use attempt::{
    ACK_TIMEOUT, Answer, Assembly, Attempt, CODE_APPLY_FAILED, CODE_BUILDER_FAILED,
    CODE_CALLBACK_FAILED, CODE_FIELD_NOT_RESTORED, Failure, Outcome, Patched, PlayerCode, Refusal,
    Unchanged, bounded, loaded_assemblies, newest_assemblies, wait_code_ack,
};
pub use tools::{
    Applier, BUILD_TIMEOUT, BuilderService, Sdk, Setup, Tools, prepare_patcher, stage_applier,
};
pub use wire::{
    Ack, AckFailure, BuilderAssembly, BuilderRequest, BuilderResult, Descriptor,
    DescriptorAssembly, Detour, RUNTIME_ASSEMBLY, Reason, ReloadRequest,
};

/// What an edit that cannot be patched does to the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Fallback {
    /// Restart the Player on the newest compile, as a session without hot reload does.
    #[default]
    Restart,
    /// Keep the Player on its code and report why; a `recompile` command restarts it.
    Report,
}
impl Fallback {
    /// The value of `--hot-reload-fallback`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "restart" => Some(Self::Restart),
            "report" => Some(Self::Report),
            _ => None,
        }
    }
}

/// The environment variable the Player's Mono reads its JIT options from.
pub const MONO_ARGUMENTS: &str = "MONO_ARGUMENTS";
/// A redirect does not reach the copies an inlining JIT made of a method, so
/// a Player that takes code patches runs without inlining.
pub const MONO_ARGUMENTS_VALUE: &str = "-O=-float32,-inline";
/// The Player argument that names the code reload request file.
pub const PLAYER_ARGUMENT: &str = "--unity-cli-code-reload";
