use super::files::{
    Walk, atomic_json, hash_bytes, inside, join, open, path_error, read_json, safe_relative, utf8,
    walk_dir,
};
use super::resolve::Resolver;
use super::{Context, Package};
use anyhow::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_LIMIT: usize = 65536;

pub(crate) fn commit_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("^[0-9a-fA-F]{40}$").unwrap())
}
pub(crate) fn is_git(value: &str) -> bool {
    value.starts_with("git+")
        || value.starts_with("git://")
        || value.starts_with("file://")
        || value.starts_with("ssh://")
        || value.starts_with("git@")
        || (value.contains(".git")
            && (value.starts_with("https://") || value.starts_with("http://")))
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitReference {
    pub remote: String,
    pub subdir: String,
    pub revision: String,
}
/// Splits a Git dependency into the remote handed to Git, the `?path=` package
/// subdirectory and the `#revision`.
pub(crate) fn parse_git(value: &str) -> Result<GitReference> {
    let value = value.strip_prefix("git+").unwrap_or(value);
    let (base, revision) = value.split_once('#').unwrap_or((value, ""));
    let mut remote = base;
    let mut subdir = String::new();
    if base.starts_with("git@") {
        if base.contains('?') {
            bail!("SCP Git URLs with package path require ssh:// syntax");
        }
        if !base.contains(':') {
            bail!("invalid SCP Git URL");
        }
    } else {
        let url = url::Url::parse(base)?;
        match url.scheme() {
            "https" | "http" | "ssh" | "git" | "file" => {}
            other => bail!("unsupported Git protocol {other:?}"),
        }
        // Checked on the text as well: URL parsing forgives a missing HTTP(S) authority.
        let authority = base.split_once("://").map_or("", |(_, rest)| rest);
        if url.scheme() != "file"
            && (authority.is_empty()
                || authority.starts_with('/')
                || url.host_str().is_none_or(str::is_empty))
        {
            bail!("Git URL host absent");
        }
        if (!url.username().is_empty() || url.password().is_some()) && url.scheme() != "ssh" {
            bail!("Git credentials in URL unsupported; use credential configuration");
        }
        for (key, value) in url.query_pairs() {
            if key != "path" {
                bail!("unsupported Git query {key:?}");
            }
            if subdir.is_empty() {
                subdir = value.into_owned();
            }
        }
        if subdir.contains(char::REPLACEMENT_CHARACTER) {
            bail!("non-UTF-8 Git package subdirectory unsupported");
        }
        remote = base.split_once('?').map_or(base, |(remote, _)| remote);
    }
    let subdir = subdir.strip_prefix('/').unwrap_or(&subdir).to_string();
    if !subdir.is_empty() && !safe_relative(subdir.as_bytes()) {
        bail!("unsafe Git package subdirectory");
    }
    if revision.contains(['\0', '\r', '\n']) || revision.starts_with('-') {
        bail!("unsafe Git revision");
    }
    Ok(GitReference {
        remote: remote.into(),
        subdir,
        revision: revision.into(),
    })
}
/// Binds a requested Git dependency to the exact commit and content that were verified.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitReceipt {
    requested: String,
    revision: String,
    subdirectory: String,
    path: String,
    content_hash: String,
    name: String,
    version: String,
}
fn git(repo: &str) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .stdin(Stdio::null());
    command
}
/// Keeps the first 64 KiB of a stream and discards the rest.
fn collect_bounded(mut source: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return kept,
            Ok(count) => {
                let keep = count.min(OUTPUT_LIMIT - kept.len());
                kept.extend_from_slice(&buffer[..keep]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return kept,
        }
    }
}
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
impl Resolver<'_> {
    pub(crate) fn git_package(&mut self, name: &str, request: &str) -> Result<Package> {
        let reference = parse_git(request)?;
        let mut commit = String::new();
        if !self.options.refresh_git
            && let Some(locked) = self.lock.dependencies.get(name)
            && locked.source == "git"
            && locked.version == request
        {
            commit = locked.hash.clone();
            if !commit_pattern().is_match(&commit) {
                bail!("locked Git revision must be a full commit hash");
            }
        }
        if commit.is_empty() && commit_pattern().is_match(&reference.revision) {
            commit = reference.revision.to_lowercase();
        }
        let key = hash_bytes(format!("{name}\0{request}\0{commit}").as_bytes());
        let receipt_path = join(&[&self.cache, "git", &format!("{key}.json")]);
        let unlocked_moving_ref =
            commit.is_empty() && self.manifest.enable_lock_file == Some(false);
        if !self.options.refresh_git
            && !unlocked_moving_ref
            && let Ok((receipt, _)) = read_json::<GitReceipt>(&receipt_path)
        {
            if receipt.name != name
                || receipt.requested != request
                || !commit_pattern().is_match(&receipt.revision)
                || (!commit.is_empty() && receipt.revision != commit)
                || !inside(&join(&[&self.cache, "content"]), &receipt.path)
            {
                bail!("Git cache receipt identity mismatch");
            }
            let mut p =
                self.package_at(name, &receipt.path, "git", request, "verified_git_commit")?;
            if p.content_hash != receipt.content_hash || p.version != receipt.version {
                bail!("verified Git cache content changed");
            }
            p.revision = receipt.revision;
            p.subdirectory = receipt.subdirectory;
            return self.git_materialized_copy(p);
        }
        if self.options.offline {
            // The copy UPM fingerprinted is local evidence too.
            if let Some(package) =
                self.git_fingerprinted_copy(name, request, &commit, &reference.subdir)?
            {
                return Ok(package);
            }
            bail!("offline Git package lacks verified commit/content receipt: {name}");
        }
        self.check_cache_budget(
            2 * self.options.max_download_bytes + self.options.max_expanded_bytes,
        )?;
        let repo_dir = tempfile::Builder::new()
            .prefix(".git-fetch-")
            .rand_bytes(10)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&self.cache)
            .map_err(path_error("mkdir", &self.cache))?;
        let repo = utf8(repo_dir.path())?;
        self.git_command(&repo, &["init", "--bare"])?;
        self.git_command(&repo, &["remote", "add", "origin", &reference.remote])?;
        let revision = if !commit.is_empty() {
            commit.as_str()
        } else if !reference.revision.is_empty() {
            reference.revision.as_str()
        } else {
            "HEAD"
        };
        if let Err(error) = self.git_command(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--depth=1",
                "--filter=blob:none",
                "--no-tags",
                "--",
                "origin",
                revision,
            ],
        ) {
            return match self.git_fingerprinted_copy(name, request, &commit, &reference.subdir)? {
                Some(package) => Ok(package),
                None => Err(error),
            };
        }
        let resolved =
            self.git_command(&repo, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])?;
        let resolved = resolved.trim().to_string();
        if !commit_pattern().is_match(&resolved) || (!commit.is_empty() && resolved != commit) {
            bail!("Git resolved commit differs from lock");
        }
        let object = if reference.subdir.is_empty() {
            resolved.clone()
        } else {
            format!("{resolved}:{}", reference.subdir)
        };
        let ctx = self.ctx.with_timeout(GIT_TIMEOUT);
        let mut child = git(&repo)
            .args(["archive", "--format=tar", &object])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stderr = thread::spawn(move || collect_bounded(stderr));
        let limit = self.options.max_expanded_bytes + self.options.max_files * 2048 + 1;
        let child = Mutex::new(child);
        let finished = AtomicBool::new(false);
        let (extracted, status) = thread::scope(|scope| {
            // Kill the archive process when the context ends.
            scope.spawn(|| {
                while !finished.load(Ordering::SeqCst) {
                    if ctx.err().is_err() {
                        let _ = child.lock().unwrap().kill();
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });
            let mut limited = (&mut stdout).take(limit as u64);
            let extracted = self.extract(
                name,
                &mut limited,
                "git",
                request,
                "verified_git_commit",
                false,
            );
            // Let the process finish writing its end-of-archive padding.
            if extracted.is_err() || io::copy(&mut limited, &mut io::sink()).is_err() {
                let _ = child.lock().unwrap().kill();
            }
            let status = loop {
                match child.lock().unwrap().try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => {}
                    Err(error) => break Err(error),
                }
                thread::sleep(Duration::from_millis(2));
            };
            finished.store(true, Ordering::SeqCst);
            (extracted, status)
        });
        drop(stdout);
        let stderr = stderr.join().unwrap_or_default();
        let mut p = extracted?;
        if !status.is_ok_and(|status| status.success()) {
            bail!(
                "Git package archive failed: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        self.check_cache_budget(0)?;
        check_git_materialized(&p.path)?;
        p.revision = resolved.clone();
        p.subdirectory = reference.subdir.clone();
        let receipt = GitReceipt {
            requested: request.into(),
            revision: resolved,
            subdirectory: reference.subdir,
            path: p.path.clone(),
            content_hash: p.content_hash.clone(),
            name: p.name.clone(),
            version: p.version.clone(),
        };
        atomic_json(&receipt_path, &receipt)?;
        self.git_materialized_copy(p)
    }
    /// A project PackageCache copy of a locked commit, for a remote the tool
    /// cannot read (a private repository the Editor fetched with its own
    /// credentials). UPM writes the commit it materialized as `_fingerprint`
    /// into the copy's `package.json`; only a copy whose fingerprint is the
    /// commit the lock names is taken, and the snapshot records that this is
    /// UPM's word, not a verified archive.
    fn git_fingerprinted_copy(
        &mut self,
        name: &str,
        request: &str,
        commit: &str,
        subdirectory: &str,
    ) -> Result<Option<Package>> {
        if commit.is_empty() {
            return Ok(None);
        }
        let project_cache = join(&[&self.project, "Library", "PackageCache"]);
        let paths: Vec<String> = self
            .caches
            .get(name)
            .into_iter()
            .flat_map(|versions| versions.values().flatten())
            .filter(|path| inside(&project_cache, path))
            .cloned()
            .collect();
        let mut result: Option<Package> = None;
        for path in paths {
            let mut text = Vec::new();
            open(join(&[&path, "package.json"]))?
                .take(1 << 20)
                .read_to_end(&mut text)?;
            let manifest: serde_json::Value = serde_json::from_slice(&text)?;
            if manifest["_fingerprint"].as_str() != Some(commit) {
                continue;
            }
            let mut package =
                self.package_at(name, &path, "git", request, "upm_fingerprinted_cache_copy")?;
            if result
                .as_ref()
                .is_some_and(|other| other.content_hash != package.content_hash)
            {
                bail!("ambiguous materialized Git package contents");
            }
            package.revision = commit.to_string();
            package.subdirectory = subdirectory.to_string();
            result = Some(package);
        }
        Ok(result)
    }
    /// A project PackageCache copy is reusable only after comparison with a freshly
    /// verified exact-commit archive or its unchanged receipt. Its folder suffix is
    /// deliberately not treated as a Git revision or as evidence of content.
    fn git_materialized_copy(&mut self, commit: Package) -> Result<Package> {
        let paths = self.materialized(&commit.name, &commit.version);
        if paths.is_empty() {
            return Ok(commit);
        }
        let verified = self.comparable_hash(&commit.path)?;
        let mut result = Package::default();
        for path in paths {
            let candidate = self.package_at(
                &commit.name,
                &path,
                "git",
                &commit.requested,
                "verified_git_cache_copy",
            )?;
            if self.comparable_hash(&candidate.path)? != verified {
                // The same package version may legitimately exist at another Git
                // commit. Use our verified commit rather than a nonmatching copy.
                continue;
            }
            if !result.path.is_empty() && result.content_hash != candidate.content_hash {
                bail!("ambiguous materialized Git package contents");
            }
            result = candidate;
        }
        if result.path.is_empty() {
            return Ok(commit);
        }
        result.revision = commit.revision;
        result.subdirectory = commit.subdirectory;
        result.commit_path = commit.path.clone();
        result.commit_content_hash = commit.content_hash;
        self.inputs
            .insert(join(&[&commit.path, "package.json"]), commit.manifest_hash);
        Ok(result)
    }
    fn git_command(&self, repo: &str, args: &[&str]) -> Result<String> {
        let ctx: Context = self.ctx.with_timeout(GIT_TIMEOUT);
        // One pipe for both streams keeps their interleaving.
        let (reader, writer) = io::pipe()?;
        let mut command = git(repo);
        command
            .args(args)
            .stdout(writer.try_clone()?)
            .stderr(writer);
        let mut child = command.spawn()?;
        drop(command);
        let output = thread::spawn(move || collect_bounded(reader));
        let text = |output: thread::JoinHandle<Vec<u8>>| {
            String::from_utf8_lossy(&output.join().unwrap_or_default()).into_owned()
        };
        let mut scanned = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let text = text(output);
                if !status.success() {
                    bail!("Git operation failed: {text}");
                }
                return Ok(text);
            }
            if let Err(error) = ctx.err() {
                stop(&mut child);
                let _ = output.join();
                return Err(error.into());
            }
            if scanned.elapsed() >= Duration::from_millis(100) {
                scanned = Instant::now();
                let mut size: i64 = 0;
                let limit = self.options.max_download_bytes;
                let _ = walk_dir(repo, &mut |entry| {
                    if !entry.is_dir() {
                        size += entry.info()?.len() as i64;
                        if size > limit {
                            bail!("Git fetch exceeds download budget");
                        }
                    }
                    Ok(Walk::Continue)
                });
                if size > limit {
                    stop(&mut child);
                    let _ = output.join();
                    bail!("Git repository fetch exceeds download bound");
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}
pub(crate) fn check_git_materialized(root: &str) -> Result<()> {
    walk_dir(root, &mut |entry| {
        if entry.is_dir() {
            return Ok(Walk::Continue);
        }
        if entry.name == OsStr::new(".gitmodules") {
            bail!("Git submodule packages require explicit materialization; unsupported");
        }
        let mut prefix = [0u8; 128];
        let count = open(entry.path)?
            .read(&mut prefix)
            .map_err(path_error("read", entry.path))?;
        if prefix[..count].starts_with(b"version https://git-lfs.github.com/spec/v1") {
            bail!("Git LFS pointer is not materialized; unsupported");
        }
        Ok(Walk::Continue)
    })
}
