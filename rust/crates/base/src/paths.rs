use anyhow::{Context, Result, ensure};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

pub fn logical_path(path: &str) -> Result<&str> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', '\0', '\r', '\n']),
        "invalid logical source path {path:?}"
    );
    ensure!(
        !path.starts_with('/') && !path.ends_with('/') && !path.contains("//"),
        "source path must be normalized"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "source path must be normalized"
    );
    ensure!(
        !path.split('/').any(|p| p == "." || p == ".."),
        "source path must be normalized"
    );
    Ok(path)
}
pub fn authored_path(path: &str) -> Result<()> {
    logical_path(path)?;
    ensure!(
        path.starts_with("Assets/")
            && matches!(
                Path::new(path).extension().and_then(|s| s.to_str()),
                Some("unity" | "prefab")
            ),
        "authored graph requires a normalized Assets scene or prefab"
    );
    Ok(())
}
pub fn checked_source(project: &Path, logical: &str) -> Result<PathBuf> {
    logical_path(logical)?;
    let path = project
        .join(logical)
        .canonicalize()
        .with_context(|| format!("source not found: {logical}"))?;
    ensure!(
        path.starts_with(project),
        "source escapes declared root: {logical}"
    );
    ensure!(
        !path.starts_with(project.join("Library")),
        "project Library is not a declared package source"
    );
    Ok(path)
}
pub fn is_owned_cache_path(project: &Path, path: &Path) -> bool {
    project.is_absolute()
        && path.is_absolute()
        && !path
            .to_string_lossy()
            .split('/')
            .any(|p| p == "." || p == "..")
        && [
            project.join("Temp"),
            project.join("Library/UnityMicroEditor"),
        ]
        .iter()
        .any(|root| path.starts_with(root) && path != root)
}
pub fn checked_output(project: &Path, path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    ensure!(
        is_owned_cache_path(project, &absolute),
        "output must be a child of the explicit project's Temp or Library/UnityMicroEditor"
    );
    let mut current = project.to_path_buf();
    for component in absolute.strip_prefix(project)?.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(info) => ensure!(
                !info.file_type().is_symlink(),
                "symlinked output ancestor: {}",
                current.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(absolute)
}
pub fn tool_root() -> Result<PathBuf> {
    let root = if let Some(root) = env::var_os("UNITY_MICRO_EDITOR_ROOT") {
        PathBuf::from(root)
    } else {
        env::current_exe()?
            .parent()
            .context("no installation directory")?
            .to_path_buf()
    };
    let root = root
        .canonicalize()
        .context("set UNITY_MICRO_EDITOR_ROOT to the installation directory")?;
    for file in [
        "engine-profile.json",
        "tools/LifecycleCompiler/Program.cs",
        "Packages/com.unitymicroeditor.runtime/package.json",
    ] {
        ensure!(
            root.join(file).is_file(),
            "invalid Unity Micro Editor installation: {}",
            root.display()
        );
    }
    Ok(root)
}
/// Makes a path absolute: join with the working directory and clean lexically.
pub fn lexical_abs(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut clean = PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                clean.pop();
            }
            Component::Normal(part) => clean.push(part),
            _ => {}
        }
    }
    Ok(clean)
}
