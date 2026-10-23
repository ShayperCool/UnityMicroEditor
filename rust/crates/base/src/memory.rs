//! What the tool keeps to be fast, chosen once for a process and the
//! processes it starts: `UME_MEMORY` (or `--memory` before a command) is
//! `low`, `balanced` (the default) or `fast`.
//!
//! - `low` keeps nothing it can take again: a session starts no compiler
//!   process that outlives a compile, stops its scene worker as soon as it
//!   is idle, imports with fewer workers, keeps no source text or parsed
//!   document between two reads, and gives freed memory back to the system
//!   after each step.
//! - `balanced` keeps the warm processes and caches a watched session
//!   needs to answer an edit in seconds, and stops what has been idle for
//!   two minutes.
//! - `fast` keeps idle processes ten minutes and imports with every core.
//!
//! A knob of its own (`UME_COMPILER_COLD`, `UME_SCENE_WORKER_IDLE_SECONDS`,
//! `UME_HASH_CACHE`) still decides over the mode.
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Memory {
    Low,
    Balanced,
    Fast,
}

pub const VARIABLE: &str = "UME_MEMORY";

impl Memory {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "low" => Some(Self::Low),
            "balanced" | "" => Some(Self::Balanced),
            "fast" => Some(Self::Fast),
            _ => None,
        }
    }
}

/// The mode of this process; an unknown value is the default.
pub fn memory() -> Memory {
    static MODE: OnceLock<Memory> = OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var(VARIABLE)
            .ok()
            .and_then(|value| Memory::parse(value.trim()))
            .unwrap_or(Memory::Balanced)
    })
}

/// Whether caches that only spare reading a file again are kept.
pub fn keeps_sources() -> bool {
    memory() != Memory::Low
}

/// How many import workers a step starts at most.
pub fn workers(available: usize) -> usize {
    match memory() {
        Memory::Low => available.clamp(1, 4),
        Memory::Balanced => available.clamp(2, 32),
        Memory::Fast => available.max(2),
    }
}

/// Gives the memory freed so far back to the system, where the allocator
/// keeps it for later allocations otherwise. In `low` mode after each step,
/// in the others after a session is ready.
pub fn release() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        malloc_trim(0);
    }
}

/// [`release`] when the mode is `low`.
pub fn release_if_low() {
    if memory() == Memory::Low {
        release();
    }
}

/// Takes a `--memory <mode>` or `--memory=<mode>` given before a command out
/// of the arguments into `UME_MEMORY`, which the processes started later see.
pub fn take_argument(args: &mut Vec<String>) -> Result<(), String> {
    let Some(first) = args
        .first()
        .filter(|arg| *arg == "--memory" || arg.starts_with("--memory="))
        .cloned()
    else {
        return Ok(());
    };
    args.remove(0);
    let value = match first.strip_prefix("--memory=") {
        Some(value) => value.to_string(),
        None if !args.is_empty() => args.remove(0),
        None => return Err("--memory requires low, balanced or fast".into()),
    };
    if Memory::parse(&value).is_none() {
        return Err(format!("--memory {value}: expected low, balanced or fast"));
    }
    // SAFETY: called first in `main`, before any thread starts.
    unsafe { std::env::set_var(VARIABLE, value) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_parsed() {
        assert_eq!(Memory::parse("low"), Some(Memory::Low));
        assert_eq!(Memory::parse("fast"), Some(Memory::Fast));
        assert_eq!(Memory::parse(""), Some(Memory::Balanced));
        assert_eq!(Memory::parse("huge"), None);
    }

    #[test]
    fn only_a_leading_option_is_taken() {
        let mut args: Vec<String> = ["run", "--memory", "low"].map(String::from).to_vec();
        take_argument(&mut args).unwrap();
        assert_eq!(args, ["run", "--memory", "low"]);
        let mut args: Vec<String> = ["--memory=huge", "run"].map(String::from).to_vec();
        assert!(take_argument(&mut args).is_err());
        let mut args: Vec<String> = ["--memory"].map(String::from).to_vec();
        assert!(take_argument(&mut args).is_err());
    }
}
