use super::files::{
    MAX_JSON, atomic_bytes, atomic_json, bounded_read, hash_bytes, join, mkdir_all, open,
    path_error, safe_relative, stat, utf8,
};
use super::resolve::Resolver;
use super::semver::{compare_version, parse_version, parse_version_lenient, valid_package_name};
use super::{Context, Package, nullable};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::time::{Duration, Instant};
use url::{Host, Url};

const USER_AGENT: &str = "UnityMicroEditor-PackageResolver/1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_REQUESTS: usize = 10;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Dist {
    #[serde(default, deserialize_with = "nullable")]
    pub tarball: String,
    #[serde(default, deserialize_with = "nullable")]
    pub integrity: String,
    #[serde(default, deserialize_with = "nullable")]
    pub shasum: String,
}
/// One version of a registry package document.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RegistryVersion {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub version: String,
    #[serde(default, deserialize_with = "nullable")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "nullable")]
    pub dist: Dist,
}
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RegistryMetadata {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub versions: BTreeMap<String, RegistryVersion>,
}
/// Records where a content-addressed package directory came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContentReceipt {
    pub name: String,
    pub version: String,
    pub source: String,
    pub content_hash: String,
    pub proof: String,
    pub registry: String,
}
fn safe_http(value: &str) -> Result<Url> {
    let mut url = Url::parse(value)?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("registry/download URL must be HTTP(S) without embedded credentials");
    }
    url.set_fragment(None);
    Ok(url)
}
/// Host and explicit port, the part of a URL that is safe to show in a failure.
fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.into(),
    }
}
/// Whether `url` lies in the registry's own scheme/host/path namespace, the only place a
/// registry bearer token may be sent.
fn in_namespace(base: Option<&Url>, url: &Url) -> bool {
    base.is_some_and(|base| {
        let prefix = format!("{}/", base.path().trim_end_matches('/'));
        base.origin() == url.origin() && url.path().starts_with(&prefix)
    })
}
/// Sensitive headers follow a redirect only to the same host or one of its subdomains.
fn same_domain_or_subdomain(initial: &Url, destination: &Url) -> bool {
    let parent = initial.host_str().unwrap_or_default();
    let sub = destination.host_str().unwrap_or_default();
    sub == parent
        || (matches!(destination.host(), Some(Host::Domain(_)))
            && sub
                .strip_suffix(parent)
                .is_some_and(|rest| rest.ends_with('.')))
}
struct Body<'a> {
    ctx: &'a Context,
    reader: Box<dyn Read>,
}
impl Read for Body<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.ctx.err().map_err(io::Error::other)?;
        self.reader.read(buffer)
    }
}
impl Resolver<'_> {
    fn publish_content_receipt(&self, p: &Package) -> Result<()> {
        atomic_json(
            &join(&[&self.cache, "receipts", &format!("{}.json", p.content_hash)]),
            &ContentReceipt {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source.clone(),
                content_hash: p.content_hash.clone(),
                proof: p.proof.clone(),
                registry: p.registry.clone(),
            },
        )
    }
    pub(crate) fn registry_for(&self, name: &str) -> Result<String> {
        let mut registry = self.manifest.registry.as_str();
        if registry.is_empty() {
            registry = "https://packages.unity.com";
        }
        let mut best: i64 = -1;
        for item in &self.manifest.scoped_registries {
            if item.name.is_empty() || item.scopes.is_empty() {
                bail!("scoped registry requires a name and scopes");
            }
            for scope in &item.scopes {
                if !valid_package_name(scope) {
                    bail!("unsupported registry scope {scope:?}");
                }
                if name == scope || name.starts_with(&format!("{scope}.")) {
                    if scope.len() as i64 == best
                        && registry.trim_end_matches('/') != item.url.trim_end_matches('/')
                    {
                        bail!("ambiguous scoped registry for {name}");
                    }
                    if scope.len() as i64 > best {
                        registry = &item.url;
                        best = scope.len() as i64;
                    }
                }
            }
        }
        if registry.contains(['?', '#']) {
            bail!("registry URL query/fragment unsupported");
        }
        Ok(safe_http(registry)?
            .as_str()
            .trim_end_matches('/')
            .to_string())
    }
    /// GET within a 90 s overall budget and at most ten requests. Redirects are followed
    /// by hand so that every hop is checked: targets must be plain HTTP(S), and the
    /// registry token never leaves the registry's own namespace.
    fn get(&self, address: &str, registry: &str) -> Result<Body<'_>> {
        let url = safe_http(address)?;
        let base = safe_http(registry).ok();
        let token = self
            .options
            .registry_tokens
            .get(registry)
            .filter(|token| !token.is_empty() && in_namespace(base.as_ref(), &url));
        let deadline = Instant::now() + HTTP_TIMEOUT;
        let mut current = url.clone();
        let mut authorized = token.is_some();
        let mut left_domain = false;
        let mut referer = String::new();
        for _ in 0..MAX_REQUESTS {
            self.ctx.err()?;
            let mut budget = deadline.saturating_duration_since(Instant::now());
            if let Some(left) = self.ctx.remaining() {
                budget = budget.min(left);
            }
            if budget.is_zero() {
                bail!("package fetch from {} timed out", authority(&url));
            }
            // One agent per resolve keeps connections alive across requests; every new
            // connection is another chance to stall.
            let agent = self.http.get_or_init(|| {
                ureq::Agent::config_builder()
                    .max_redirects(0)
                    .max_redirects_will_error(false)
                    .http_status_as_error(false)
                    .tls_config(
                        ureq::tls::TlsConfig::builder()
                            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                            .build(),
                    )
                    .build()
                    .into()
            });
            let mut request = agent
                .get(current.as_str())
                .config()
                .timeout_global(Some(budget))
                .build()
                .header("Accept", "application/json")
                .header("User-Agent", USER_AGENT);
            if authorized && let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            if !referer.is_empty() {
                request = request.header("Referer", &referer);
            }
            let response = request
                .call()
                .with_context(|| format!("package fetch from {}", authority(&current)))?;
            let status = response.status().as_u16();
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if matches!(status, 301 | 302 | 303 | 307 | 308) && !location.is_empty() {
                let next = current
                    .join(location)
                    .map_err(anyhow::Error::new)
                    .and_then(|next| safe_http(next.as_str()))
                    .with_context(|| format!("package redirect from {}", authority(&current)))?;
                left_domain |= !same_domain_or_subdomain(&url, &next);
                referer = if current.scheme() == "https" && next.scheme() == "http" {
                    String::new()
                } else {
                    current.to_string()
                };
                authorized = token.is_some() && !left_domain && in_namespace(base.as_ref(), &next);
                current = next;
                continue;
            }
            if status == 401 || status == 403 {
                bail!(
                    "registry authentication required ({status}) for {}",
                    authority(&url)
                );
            }
            if status != 200 {
                bail!("package fetch HTTP {status} from {}", authority(&url));
            }
            return Ok(Body {
                ctx: self.ctx,
                reader: Box::new(response.into_body().into_reader()),
            });
        }
        bail!("package redirect limit exceeded");
    }
    fn packument(&mut self, name: &str) -> Result<RegistryMetadata> {
        let registry = self.registry_for(name)?;
        let key = format!("{registry}/{name}");
        if let Some(metadata) = self.metadata.get(&key) {
            return Ok(metadata.clone());
        }
        let cache_path = join(&[
            &self.cache,
            "metadata",
            &format!("{}.json", hash_bytes(key.as_bytes())),
        ]);
        let data = if self.options.offline {
            bounded_read(&cache_path, MAX_JSON)
                .map_err(|_| anyhow!("offline registry metadata unavailable for {name}"))?
        } else {
            let mut data = Vec::new();
            self.get(&key, &registry)?
                .take(MAX_JSON as u64 + 1)
                .read_to_end(&mut data)?;
            if data.len() as i64 > MAX_JSON {
                bail!("registry metadata exceeds bounds");
            }
            data
        };
        let metadata: RegistryMetadata = serde_json::from_slice(&data)
            .with_context(|| format!("registry metadata for {name}"))?;
        if metadata.name != name || metadata.versions.is_empty() {
            bail!("registry identity/versions mismatch for {name}");
        }
        for (version, entry) in &metadata.versions {
            if parse_version(version).is_err() {
                continue;
            }
            if (!entry.name.is_empty() && entry.name != name)
                || (!entry.version.is_empty() && &entry.version != version)
            {
                bail!("registry version identity mismatch for {name}@{version}");
            }
        }
        if !self.options.offline {
            self.check_cache_budget(data.len() as i64)?;
            atomic_bytes(&cache_path, &data)?;
        }
        self.metadata.insert(key, metadata.clone());
        Ok(metadata)
    }
    pub(crate) fn choose_registry_version(
        &mut self,
        name: &str,
        minimum: &str,
        strategy: &str,
    ) -> Result<String> {
        let metadata = self.packument(name)?;
        let (floor, _) = parse_version_lenient(minimum);
        let mut candidates: Vec<&String> = Vec::new();
        for candidate in metadata.versions.keys() {
            let Ok(version) = parse_version(candidate) else {
                continue;
            };
            if compare_version(candidate, minimum).is_lt()
                || (floor.pre.is_empty() && !version.pre.is_empty())
                || (strategy == "highestPatch"
                    && (version.major != floor.major || version.minor != floor.minor))
                || (strategy == "highestMinor"
                    && (version.major != floor.major
                        || (floor.major == "0" && version.minor != floor.minor)))
            {
                continue;
            }
            candidates.push(candidate);
        }
        candidates.sort_by(|a, b| compare_version(a, b).then_with(|| a.cmp(b)));
        let chosen = if strategy == "lowest" {
            candidates.first()
        } else {
            candidates.last()
        };
        match chosen {
            Some(version) => Ok(version.to_string()),
            None => bail!("no registry version satisfies {name} >={minimum} ({strategy})"),
        }
    }
    pub(crate) fn registry_package(
        &mut self,
        name: &str,
        version: &str,
        requested: &str,
        registry: &str,
    ) -> Result<Package> {
        let metadata = self.packument(name)?;
        let Some(entry) = metadata.versions.get(version) else {
            bail!("registry version absent: {name}@{version}");
        };
        if entry.dist.tarball.is_empty() {
            bail!("registry tarball missing: {name}@{version}");
        }
        if entry.dist.integrity.is_empty() && entry.dist.shasum.is_empty() {
            bail!("registry integrity missing: {name}@{version}");
        }
        self.check_cache_budget(self.options.max_download_bytes + self.options.max_expanded_bytes)?;
        let mut body = self.get(&entry.dist.tarball, registry)?;
        let mut file = tempfile::Builder::new()
            .prefix(".download-")
            .rand_bytes(10)
            .permissions(fs::Permissions::from_mode(0o600))
            .tempfile_in(&self.cache)
            .map_err(path_error("open", &self.cache))?;
        let limit = self.options.max_download_bytes;
        let written = io::copy(&mut (&mut body).take(limit as u64 + 1), file.as_file_mut())?;
        drop(body);
        if written as i64 > limit {
            bail!("package download exceeds byte bound");
        }
        let filename = utf8(file.path())?;
        verify_integrity(&filename, &entry.dist.integrity, &entry.dist.shasum)?;
        let mut p = self.unpack_file(
            name,
            &filename,
            "registry",
            requested,
            "verified_registry_integrity",
        )?;
        if p.version != version {
            bail!("downloaded version mismatch: {name}@{}", p.version);
        }
        p.registry = registry.into();
        if p.dependencies != entry.dependencies {
            bail!("registry/archive dependency mismatch for {name}@{version}");
        }
        self.publish_content_receipt(&p)?;
        self.caches
            .entry(name.into())
            .or_default()
            .entry(p.version.clone())
            .or_default()
            .push(p.path.clone());
        Ok(p)
    }
    pub(crate) fn local_tarball(
        &mut self,
        name: &str,
        filename: &str,
        requested: &str,
    ) -> Result<Package> {
        self.check_cache_budget(self.options.max_expanded_bytes)?;
        let data = bounded_read(filename, self.options.max_download_bytes)?;
        self.inputs.insert(filename.into(), hash_bytes(&data));
        self.unpack_file(name, filename, "local", requested, "verified_local_archive")
    }
    fn unpack_file(
        &mut self,
        name: &str,
        filename: &str,
        source: &str,
        requested: &str,
        proof: &str,
    ) -> Result<Package> {
        let file = open(filename)?;
        let mut reader = flate2::read::MultiGzDecoder::new(file);
        self.extract(name, &mut reader, source, requested, proof, true)
    }
    pub(crate) fn extract(
        &mut self,
        name: &str,
        reader: &mut dyn Read,
        source: &str,
        requested: &str,
        proof: &str,
        strip_package: bool,
    ) -> Result<Package> {
        let stage_dir = tempfile::Builder::new()
            .prefix(".extract-")
            .rand_bytes(10)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&self.cache)
            .map_err(path_error("mkdir", &self.cache))?;
        let stage = utf8(stage_dir.path())?;
        let mut archive = tar::Archive::new(reader);
        let mut total: u64 = 0;
        let mut files: i64 = 0;
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        for entry in archive.entries()? {
            self.ctx.err()?;
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            // Archive-wide metadata, not a file: `git archive` of a commit records the
            // commit id there.
            if kind.is_pax_global_extensions() {
                continue;
            }
            let name = entry.path_bytes().into_owned();
            let shown = String::from_utf8_lossy(&name).into_owned();
            let mut sparse = kind.is_gnu_sparse();
            if let Some(extensions) = entry.pax_extensions()? {
                for extension in extensions {
                    sparse |= extension?.key_bytes().starts_with(b"GNU.sparse.");
                }
            }
            if sparse {
                bail!("sparse package archive entry unsupported: {shown}");
            }
            let is_dir = kind.is_dir() || (kind.is_file() && name.ends_with(b"/"));
            let mut relative = name.as_slice();
            if strip_package {
                relative = relative.strip_prefix(b"package/").unwrap_or(relative);
            }
            relative = relative.strip_suffix(b"/").unwrap_or(relative);
            if is_dir && matches!(relative, b"" | b"." | b"package") {
                continue;
            }
            let shown = String::from_utf8_lossy(relative).into_owned();
            if !safe_relative(relative) {
                bail!("unsafe package archive path {shown:?}");
            }
            let destination = stage_dir.path().join(OsStr::from_bytes(relative));
            if is_dir {
                mkdir_all(&destination)?;
                continue;
            }
            if !kind.is_file() {
                bail!("package archive link/special entry unsupported: {shown}");
            }
            if !seen.insert(relative.to_vec()) {
                bail!("duplicate archive file {shown}");
            }
            let size = entry.size();
            files += 1;
            total = total.saturating_add(size);
            if total > self.options.max_expanded_bytes as u64 || files > self.options.max_files {
                bail!("expanded package exceeds bounds");
            }
            if let Some(parent) = destination.parent() {
                mkdir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(entry.header().mode()? & 0o777)
                .open(&destination)
                .map_err(path_error("open", &destination))?;
            if io::copy(&mut entry, &mut file)? != size {
                bail!("package archive entry truncated: {shown}");
            }
        }
        let mut p = self.package_at(name, &stage, source, requested, proof)?;
        let destination = join(&[&self.cache, "content", &p.content_hash]);
        mkdir_all(join(&[&self.cache, "content"]))?;
        match fs::rename(&stage, &destination) {
            Ok(()) => {
                // The stage now lives at its content address; nothing is left to remove.
                let _ = stage_dir.keep();
            }
            Err(error) => {
                let exists = matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                );
                if !exists && !stat(&destination).is_ok_and(|info| info.is_dir()) {
                    return Err(path_error("rename", &stage)(error));
                }
                let existing = self.package_at(name, &destination, source, requested, proof);
                if !existing.is_ok_and(|existing| existing.content_hash == p.content_hash) {
                    bail!("content-addressed package cache conflict");
                }
            }
        }
        p.path = destination;
        Ok(p)
    }
}
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let digit = |byte: &u8| (*byte as char).to_digit(16);
    let digits: Option<Vec<u32>> = text.as_bytes().iter().map(digit).collect();
    match digits {
        Some(digits) if digits.len().is_multiple_of(2) => Ok(digits
            .chunks(2)
            .map(|pair| (pair[0] << 4 | pair[1]) as u8)
            .collect()),
        _ => bail!("invalid hexadecimal package digest"),
    }
}
/// Checks a downloaded archive against the strongest digest the registry lists in
/// `integrity`, or against the SHA-1 `shasum` when there is no `integrity`.
pub(crate) fn verify_integrity(filename: &str, integrity: &str, shasum: &str) -> Result<()> {
    let mut algorithm = "sha1";
    let expected = if !integrity.is_empty() {
        let mut best = 0;
        let mut decoded = Vec::new();
        for value in integrity.split_whitespace() {
            let Some((name, encoded)) = value.split_once('-') else {
                continue;
            };
            let rank = match name {
                "sha1" => 1,
                "sha256" => 2,
                "sha384" => 3,
                "sha512" => 4,
                _ => 0,
            };
            if rank <= best {
                continue;
            }
            decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            best = rank;
            algorithm = name;
        }
        if best == 0 {
            bail!("unsupported registry integrity algorithm");
        }
        decoded
    } else {
        decode_hex(shasum)?
    };
    let mut file = open(filename)?;
    fn digest<D: Digest + io::Write>(file: &mut fs::File) -> io::Result<Vec<u8>> {
        let mut hash = D::new();
        io::copy(file, &mut hash)?;
        Ok(hash.finalize().to_vec())
    }
    let actual = match algorithm {
        "sha1" => digest::<sha1::Sha1>(&mut file),
        "sha256" => digest::<sha2::Sha256>(&mut file),
        "sha384" => digest::<sha2::Sha384>(&mut file),
        _ => digest::<sha2::Sha512>(&mut file),
    }
    .map_err(path_error("read", filename))?;
    if actual != expected {
        bail!("package archive integrity mismatch");
    }
    Ok(())
}
