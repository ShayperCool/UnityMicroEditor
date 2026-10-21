use super::Package;
use super::files::{engine_comparable_hash, inside, join};
use super::resolve::Resolver;
use anyhow::{Result, bail};

impl Resolver<'_> {
    /// Project PackageCache copies of `name@version`, sorted.
    pub(crate) fn materialized(&self, name: &str, version: &str) -> Vec<String> {
        let project_cache = join(&[&self.project, "Library", "PackageCache"]);
        let mut paths: Vec<String> = self
            .cached(name, version)
            .iter()
            .filter(|path| inside(&project_cache, path))
            .cloned()
            .collect();
        paths.sort();
        paths
    }
    pub(crate) fn builtin_package(
        &mut self,
        name: &str,
        profile_path: &str,
        requested: &str,
    ) -> Result<Package> {
        let profile =
            self.package_at(name, profile_path, "builtin", requested, "engine_builtin")?;
        let paths = self.materialized(name, &profile.version);
        if paths.is_empty() {
            return Ok(profile);
        }
        let profile_hash = self.comparable_hash(&profile.path)?;
        let mut result = Package::default();
        for path in paths {
            let candidate = self.package_at(
                name,
                &path,
                "builtin",
                requested,
                "verified_engine_cache_copy",
            )?;
            let hash = self.comparable_hash(&candidate.path)?;
            if candidate.version != profile.version || hash != profile_hash {
                bail!("builtin PackageCache content differs from selected engine profile: {name}");
            }
            if !result.path.is_empty() && result.content_hash != candidate.content_hash {
                bail!("ambiguous materialized builtin package contents");
            }
            result = candidate;
        }
        result.profile_path = profile.path.clone();
        result.profile_content_hash = profile.content_hash;
        self.inputs.insert(
            join(&[&profile.path, "package.json"]),
            profile.manifest_hash,
        );
        Ok(result)
    }
    pub(crate) fn comparable_hash(&mut self, path: &str) -> Result<String> {
        if let Some(hash) = self.comparable.get(path) {
            return Ok(hash.clone());
        }
        let hash = engine_comparable_hash(
            self.ctx,
            path,
            self.options.max_expanded_bytes,
            self.options.max_files,
        )?;
        self.comparable.insert(path.into(), hash.clone());
        Ok(hash)
    }
}
