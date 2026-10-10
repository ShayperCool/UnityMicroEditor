use super::current::{Pointer, resolution_hash};
use super::download::{ContentReceipt, RegistryMetadata};
use super::files::{
    MAX_JSON, atomic_bytes, bounded_read, cache_size_within, canonical, ensure_cache, hash_bytes,
    inside, is_absolute, is_not_exist, join, json_bytes, manifest_at, parent, read_dir, read_json,
    stat, tree_hash, utf8,
};
use super::git::is_git;
use super::lock::acquire_resolve_lock;
use super::semver::{compare_version, parse_version, parse_version_lenient, valid_package_name};
use super::{Context, LockEntry, LockFile, Manifest, Options, Package, Snapshot};
use anyhow::{Context as _, Result, bail};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) struct Resolver<'a> {
    pub ctx: &'a Context,
    pub project: String,
    pub editor: String,
    pub cache: String,
    pub options: Options,
    pub manifest: Manifest,
    pub lock: LockFile,
    pub inputs: BTreeMap<String, String>,
    pub builtins: BTreeMap<String, String>,
    pub embedded: BTreeMap<String, String>,
    pub caches: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    pub chosen: BTreeMap<String, Package>,
    pub direct: BTreeMap<String, String>,
    pub floors: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, RegistryMetadata>,
    pub memo: BTreeMap<String, Package>,
    pub comparable: BTreeMap<String, String>,
    pub diagnostics: Vec<String>,
    pub http: std::cell::OnceCell<ureq::Agent>,
}

/// Resolves package identities and dependency closure. It never writes
/// Packages/manifest.json or packages-lock.json. A changed lock is a proposal.
pub fn resolve(ctx: &Context, project: &str, editor: &str, options: Options) -> Result<Snapshot> {
    let mut options = options;
    let project = canonical(project)?;
    stat(&join(&[&project, "Assets"])).context("explicit project Assets")?;
    if options.max_download_bytes == 0 {
        options.max_download_bytes = 256 << 20;
    }
    if options.max_expanded_bytes == 0 {
        options.max_expanded_bytes = 1 << 30;
    }
    if options.max_files == 0 {
        options.max_files = 100000;
    }
    if options.max_packages == 0 {
        options.max_packages = 1024;
    }
    if options.max_cache_bytes == 0 {
        options.max_cache_bytes = 4 << 30;
    }
    if options.max_download_bytes < 1
        || options.max_expanded_bytes < 1
        || options.max_cache_bytes < 1
        || options.max_files < 1
        || options.max_packages < 1
    {
        bail!("package resource bounds must be positive");
    }
    if options.max_download_bytes > 2 << 30
        || options.max_expanded_bytes > 2 << 30
        || options.max_cache_bytes > 1 << 40
        || options.max_files > 200000
        || options.max_packages > 1024
    {
        bail!("package resource bounds exceed supported validation limits");
    }
    let cache = ensure_cache(&project, &options.cache_root)?;
    let _lock = acquire_resolve_lock(ctx, &cache)?;
    let mut r = Resolver {
        ctx,
        project: project.clone(),
        editor: String::new(),
        cache: cache.clone(),
        options,
        manifest: Manifest::default(),
        lock: LockFile::default(),
        inputs: BTreeMap::new(),
        builtins: BTreeMap::new(),
        embedded: BTreeMap::new(),
        caches: BTreeMap::new(),
        chosen: BTreeMap::new(),
        direct: BTreeMap::new(),
        floors: BTreeMap::new(),
        metadata: BTreeMap::new(),
        memo: BTreeMap::new(),
        comparable: BTreeMap::new(),
        diagnostics: Vec::new(),
        http: std::cell::OnceCell::new(),
    };
    r.check_cache_budget(0)?;
    let manifest_path = join(&[&project, "Packages", "manifest.json"]);
    let manifest_hash;
    (r.manifest, manifest_hash) = read_json(&manifest_path)?;
    r.inputs.insert(manifest_path, manifest_hash.clone());
    if r.manifest.resolution_strategy.is_empty() {
        r.manifest.resolution_strategy = "lowest".into();
    }
    match r.manifest.resolution_strategy.as_str() {
        "lowest" | "highestPatch" | "highestMinor" | "highest" => {}
        other => bail!("unsupported resolutionStrategy {other:?}"),
    }
    let lock_path = join(&[&project, "Packages", "packages-lock.json"]);
    let lock_hash = match read_json(&lock_path) {
        Ok((lock, hash)) => {
            r.lock = lock;
            hash
        }
        Err(error) if is_not_exist(&error) => "absent".into(),
        Err(error) => return Err(error),
    };
    r.inputs.insert(lock_path, lock_hash.clone());
    let original_lock = r.lock.clone();
    if r.manifest.enable_lock_file == Some(false) {
        r.lock = LockFile::default();
        r.diagnostics
            .push("enableLockFile=false: authored lock preserved but ignored for selection".into());
    }
    let version_path = join(&[&project, "ProjectSettings", "ProjectVersion.txt"]);
    match bounded_read(&version_path, MAX_JSON) {
        Ok(data) => {
            r.inputs.insert(version_path, hash_bytes(&data));
        }
        Err(error) if is_not_exist(&error) => {}
        Err(error) => return Err(error),
    }
    if !editor.is_empty() {
        r.editor = canonical(editor)?;
        r.index_builtins()?;
    }
    r.index_embedded()?;
    r.index_caches()?;
    for (name, request) in &r.manifest.dependencies {
        if !valid_package_name(name) || request.is_empty() {
            bail!("invalid project dependency {name:?}");
        }
        r.direct.insert(name.clone(), request.clone());
    }
    for name in r.embedded.keys() {
        r.direct.insert(name.clone(), "embedded".into());
    }
    // Minimum constraints only increase. Previously selected versions remain valid
    // upgrade candidates, preventing oscillation when their dependency sets differ.
    let mut rounds = 0;
    loop {
        if rounds > r.options.max_packages * 4 {
            bail!("package dependency resolution did not converge");
        }
        rounds += 1;
        let mut changed = false;
        for (name, request) in r.direct.clone() {
            let p = r.select_package(&name, &request, true)?;
            let same = r.chosen.get(&name).is_some_and(|old| {
                old.version == p.version
                    && old.source == p.source
                    && old.content_hash == p.content_hash
            });
            if !same {
                r.chosen.insert(name, p);
                changed = true;
            }
        }
        let names: Vec<String> = r.chosen.keys().cloned().collect();
        for name in names {
            let dependencies = r.chosen[&name].dependencies.clone();
            for (dep, request) in &dependencies {
                parse_version(request).with_context(|| format!("{name} dependency {dep}"))?;
                let raise = r
                    .floors
                    .get(dep)
                    .is_none_or(|old| compare_version(request, old) == Ordering::Greater);
                if raise {
                    r.floors.insert(dep.clone(), request.clone());
                    changed = true;
                }
                if r.direct.contains_key(dep) {
                    // Project SemVer declarations are minimum versions.
                    continue;
                }
                let floor = r.floors[dep].clone();
                let next = r
                    .select_package(dep, &floor, false)
                    .with_context(|| format!("{name} dependency"))?;
                let same = r.chosen.get(dep).is_some_and(|old| {
                    old.version == next.version && old.content_hash == next.content_hash
                });
                if !same {
                    r.chosen.insert(dep.clone(), next);
                    changed = true;
                }
                if r.chosen.len() as i64 > r.options.max_packages {
                    bail!("package closure exceeds package bound");
                }
            }
        }
        if !changed {
            break;
        }
    }
    let depths = r.reachable_depths()?;
    let mut packages = Vec::new();
    let mut proposal = BTreeMap::new();
    for (name, depth) in &depths {
        let mut p = r.chosen[name].clone();
        p.depth = *depth;
        r.inputs
            .insert(join(&[&p.path, "package.json"]), p.manifest_hash.clone());
        let version = if p.source == "local" || p.source == "git" {
            p.requested.clone()
        } else {
            p.version.clone()
        };
        proposal.insert(
            name.clone(),
            LockEntry {
                version,
                depth: p.depth,
                source: p.source.clone(),
                dependencies: p.dependencies.clone(),
                url: p.registry.clone(),
                hash: p.revision.clone(),
            },
        );
        packages.push(p);
    }
    let proposal = LockFile {
        dependencies: proposal,
    };
    let mut snapshot = Snapshot {
        schema_version: 1,
        project: project.clone(),
        editor: r.editor.clone(),
        packages,
        inputs: r.inputs.clone(),
        manifest_hash,
        lock_hash,
        diagnostics: r.diagnostics.clone(),
        ..Snapshot::default()
    };
    // Comparing the decoded locks ignores formatting, retaining the original file.
    snapshot.lock_changed = original_lock != proposal;
    snapshot.resolution_hash = resolution_hash(&snapshot)?;
    let snapshot_dir = join(&[&cache, "snapshots", &snapshot.resolution_hash]);
    snapshot.snapshot_path = join(&[&snapshot_dir, "snapshot.json"]);
    if snapshot.lock_changed {
        snapshot.proposed_lock_path = join(&[&snapshot_dir, "packages-lock.proposed.json"]);
    }
    snapshot.validate(ctx)?;
    // Account for the published bytes and the current pointer before durable publication.
    let snapshot_data = json_bytes(&snapshot)?;
    let proposal_data = json_bytes(&proposal)?;
    if snapshot_data.len() as i64 > MAX_JSON || proposal_data.len() as i64 > MAX_JSON {
        bail!("resolved snapshot/lock exceeds JSON bound");
    }
    r.check_cache_budget((snapshot_data.len() + proposal_data.len() + 4096) as i64)?;
    if snapshot.lock_changed {
        atomic_bytes(&snapshot.proposed_lock_path, &proposal_data)?;
    }
    atomic_bytes(&snapshot.snapshot_path, &snapshot_data)?;
    Pointer {
        schema_version: 1,
        project,
        editor: r.editor.clone(),
        resolution_hash: snapshot.resolution_hash.clone(),
        snapshot_path: snapshot.snapshot_path.clone(),
    }
    .publish(&cache)?;
    Ok(snapshot)
}
impl Resolver<'_> {
    pub(crate) fn check_cache_budget(&self, extra: i64) -> Result<()> {
        if extra < 0 || extra > self.options.max_cache_bytes {
            bail!("package cache cannot reserve requested resource budget");
        }
        cache_size_within(self.ctx, &self.cache, self.options.max_cache_bytes - extra)
    }
    /// Indexes package folders by the identity in their manifest; folder names prove nothing.
    fn index_roots(&self, root: &str, kind: &str) -> Result<BTreeMap<String, String>> {
        let mut found = BTreeMap::new();
        for (name, entry) in read_dir(root)? {
            if !entry.is_dir() {
                continue;
            }
            let path = utf8(&Path::new(root).join(&name))?;
            let manifest = match manifest_at(&path) {
                Ok((manifest, _)) => manifest,
                Err(error) if is_not_exist(&error) => continue,
                Err(error) => return Err(error),
            };
            if found.insert(manifest.name.clone(), path).is_some() {
                bail!("duplicate {kind} package {}", manifest.name);
            }
        }
        Ok(found)
    }
    fn index_builtins(&mut self) -> Result<()> {
        let base = parent(&self.editor);
        let candidates = [
            join(&[
                &base,
                "Data",
                "Resources",
                "PackageManager",
                "BuiltInPackages",
            ]),
            join(&[&base, "Resources", "PackageManager", "BuiltInPackages"]),
        ];
        let Some(root) = candidates
            .iter()
            .find(|path| std::fs::metadata(path).is_ok_and(|info| info.is_dir()))
        else {
            bail!(
                "built-in package inventory absent for explicit Editor {}",
                self.editor
            );
        };
        self.builtins = self.index_roots(root, "built-in")?;
        Ok(())
    }
    fn index_embedded(&mut self) -> Result<()> {
        self.embedded = self.index_roots(&join(&[&self.project, "Packages"]), "embedded")?;
        Ok(())
    }
    fn index_caches(&mut self) -> Result<()> {
        let content = join(&[&self.cache, "content"]);
        for root in [
            join(&[&self.project, "Library", "PackageCache"]),
            content.clone(),
        ] {
            let entries = match read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if is_not_exist(&error) => continue,
                Err(error) => return Err(error),
            };
            for (name, entry) in entries {
                if !entry.is_dir() {
                    continue;
                }
                let path = utf8(&Path::new(&root).join(&name))?;
                let name = name.to_str().unwrap_or_default();
                if root == content {
                    let receipt_path = join(&[&self.cache, "receipts", &format!("{name}.json")]);
                    let registry =
                        read_json::<ContentReceipt>(&receipt_path).is_ok_and(|(receipt, _)| {
                            receipt.source == "registry" && receipt.content_hash == name
                        });
                    if !registry {
                        continue;
                    }
                }
                let Ok((manifest, _)) = manifest_at(&path) else {
                    continue;
                };
                self.caches
                    .entry(manifest.name)
                    .or_default()
                    .entry(manifest.version)
                    .or_default()
                    .push(path);
            }
        }
        Ok(())
    }
    pub(crate) fn cached(&self, name: &str, version: &str) -> &[String] {
        self.caches
            .get(name)
            .and_then(|versions| versions.get(version))
            .map_or(&[], Vec::as_slice)
    }
    pub(crate) fn package_at(
        &mut self,
        name: &str,
        root: &str,
        source: &str,
        requested: &str,
        proof: &str,
    ) -> Result<Package> {
        let root = canonical(root)?;
        if let Some(previous) = self.memo.get(&root) {
            if previous.name != name {
                bail!("package root identity collision");
            }
            let mut previous = previous.clone();
            previous.source = source.into();
            previous.requested = requested.into();
            previous.proof = proof.into();
            return Ok(previous);
        }
        let (m, manifest_hash) = manifest_at(&root)?;
        if m.name != name {
            bail!("package identity mismatch: expected {name}, got {}", m.name);
        }
        let content_hash = tree_hash(
            self.ctx,
            &root,
            self.options.max_expanded_bytes,
            self.options.max_files,
        )?;
        let testable =
            source == "embedded" || self.manifest.testables.iter().any(|item| item == name);
        let p = Package {
            name: name.into(),
            logical_root: format!("Packages/{name}"),
            path: root.clone(),
            version: m.version,
            source: source.into(),
            requested: requested.into(),
            content_hash,
            manifest_hash,
            dependencies: m.dependencies,
            kind: m.kind,
            unity: m.unity,
            unity_release: m.unity_release,
            testable,
            proof: proof.into(),
            ..Package::default()
        };
        self.memo.insert(root, p.clone());
        Ok(p)
    }
    fn select_package(&mut self, name: &str, request: &str, direct: bool) -> Result<Package> {
        let original_request = request;
        if let Some(path) = self.embedded.get(name).cloned() {
            return self.package_at(
                name,
                &path,
                "embedded",
                "embedded",
                "verified_manifest_content",
            );
        }
        if is_git(request) {
            return self.git_package(name, request);
        }
        if let Some(path) = request.strip_prefix("file:") {
            let path = if is_absolute(path) {
                path.to_string()
            } else {
                join(&[&self.project, "Packages", path])
            };
            if stat(&path)?.is_dir() {
                return self.package_at(name, &path, "local", request, "verified_manifest_content");
            }
            return self.local_tarball(name, &path, request);
        }
        parse_version(request)?;
        let mut request = request.to_string();
        if let Some(floor) = self.floors.get(name)
            && compare_version(floor, &request) == Ordering::Greater
        {
            request = floor.clone();
        }
        if let Some(path) = self.builtins.get(name).cloned() {
            return self.builtin_package(name, &path, &request);
        }
        if name.starts_with("com.unity.modules.") {
            bail!("required engine module unavailable in selected Editor: {name}");
        }
        let mut selected = request.clone();
        let registry = self.registry_for(name)?;
        let (floor_version, _) = parse_version_lenient(&request);
        if let Some(locked) = self.lock.dependencies.get(name)
            && locked.source == "registry"
            && locked.url == registry
            && compare_version(&locked.version, &request) != Ordering::Less
            && let Ok(locked_version) = parse_version(&locked.version)
            && (!floor_version.pre.is_empty() || locked_version.pre.is_empty())
        {
            selected = locked.version.clone();
        }
        if let Some(old) = self.chosen.get(name)
            && old.source == "registry"
            && compare_version(&old.version, &selected) == Ordering::Greater
        {
            let (old_version, _) = parse_version_lenient(&old.version);
            if !floor_version.pre.is_empty() || old_version.pre.is_empty() {
                selected = old.version.clone();
            }
        }
        let strategy = self.manifest.resolution_strategy.clone();
        if !direct && strategy != "lowest" && selected == request {
            selected = self.choose_registry_version(name, &request, &strategy)?;
        }
        if selected == request && self.cached(name, &selected).is_empty() {
            // Offline resolution can use a verified metadata snapshot to select an
            // available higher minimum. Folder names alone never define a registry.
            let strategy = if direct { "lowest" } else { strategy.as_str() };
            selected = self.choose_registry_version(name, &request, strategy)?;
        }
        let paths = self.cached(name, &selected).to_vec();
        if !paths.is_empty() {
            let mut result = Package::default();
            for path in paths {
                let mut p = self.package_at(
                    name,
                    &path,
                    "registry",
                    original_request,
                    "verified_manifest_content",
                )?;
                if p.version != selected {
                    bail!("cached package changed version during resolution");
                }
                if inside(&join(&[&self.cache, "content"]), &p.path) {
                    let receipt_path =
                        join(&[&self.cache, "receipts", &format!("{}.json", p.content_hash)]);
                    match read_json::<ContentReceipt>(&receipt_path) {
                        Ok((receipt, _))
                            if receipt.name == p.name
                                && receipt.version == p.version
                                && receipt.content_hash == p.content_hash
                                && receipt.registry == registry =>
                        {
                            p.proof = receipt.proof
                        }
                        _ => bail!("owned registry cache provenance/content mismatch"),
                    }
                }
                if !result.path.is_empty() && result.content_hash != p.content_hash {
                    bail!("conflicting cached contents for {name}@{selected}");
                }
                result = p;
            }
            result.registry = registry;
            return Ok(result);
        }
        if self.options.offline {
            bail!("offline package missing verified source: {name}@{selected}");
        }
        self.registry_package(name, &selected, original_request, &registry)
    }
    fn reachable_depths(&self) -> Result<BTreeMap<String, i64>> {
        fn visit(
            r: &Resolver,
            state: &mut BTreeMap<String, u8>,
            name: &str,
            stack: &mut Vec<String>,
        ) -> Result<()> {
            match state.get(name) {
                Some(1) => {
                    stack.push(name.into());
                    bail!("package dependency cycle: {}", stack.join(" -> "));
                }
                Some(2) => return Ok(()),
                _ => {}
            }
            let Some(p) = r.chosen.get(name) else {
                bail!("unresolved dependency {name}");
            };
            state.insert(name.into(), 1);
            stack.push(name.into());
            for dep in p.dependencies.keys() {
                visit(r, state, dep, stack)?;
            }
            stack.pop();
            state.insert(name.into(), 2);
            Ok(())
        }
        let mut state = BTreeMap::new();
        for name in self.direct.keys() {
            visit(self, &mut state, name, &mut Vec::new())?;
        }
        let mut depth: BTreeMap<String, i64> = BTreeMap::new();
        let mut queue: std::collections::VecDeque<String> = self.direct.keys().cloned().collect();
        for name in &queue {
            depth.insert(name.clone(), 0);
        }
        while let Some(name) = queue.pop_front() {
            let next = depth[&name] + 1;
            let Some(p) = self.chosen.get(&name) else {
                continue;
            };
            for dep in p.dependencies.keys() {
                if depth.get(dep).is_none_or(|old| next < *old) {
                    depth.insert(dep.clone(), next);
                    queue.push_back(dep.clone());
                }
            }
        }
        Ok(depth)
    }
}
