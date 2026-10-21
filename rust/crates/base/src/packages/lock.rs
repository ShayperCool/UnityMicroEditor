use super::Context;
use super::files::{is_not_exist, join, lstat, path_error};
use anyhow::{Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

/// Holds the resolver's advisory `flock` until dropped.
pub(crate) struct ResolveLock(File);
impl Drop for ResolveLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
/// Advisory process locks serialize budget checks and publication. The kernel releases
/// them if a resolver crashes; no stale PID or lease timeout is trusted.
pub(crate) fn acquire_resolve_lock(ctx: &Context, cache: &str) -> Result<ResolveLock> {
    let path = join(&[cache, ".resolve.lock"]);
    match lstat(&path) {
        Ok(info) if !info.file_type().is_file() => {
            bail!("package resolver lock must be a regular file")
        }
        Ok(_) => {}
        Err(error) if is_not_exist(&error) => {}
        Err(error) => return Err(error),
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(path_error("open", &path))?;
    loop {
        ctx.err()?;
        match file.try_lock() {
            Ok(()) => return Ok(ResolveLock(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let wait = Duration::from_millis(20);
        std::thread::sleep(ctx.remaining().map_or(wait, |left| left.min(wait)));
    }
}
