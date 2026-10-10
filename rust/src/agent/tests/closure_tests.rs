use super::*;
use crate::{
    cancel::Canceled,
    compiler::NilMap,
    packages::{Package, Snapshot},
};
use defaults_closure::reference_closure;

const LOGICAL: &str = "ProjectSettings/GraphicsSettings.asset";

fn closure_project() -> (TempDir, String, Snapshot) {
    let (guard, project) = temp_dir();
    for name in ["Assets", "ProjectSettings"] {
        fs::create_dir_all(join(&[&project, name])).unwrap();
    }
    let snapshot = Snapshot {
        schema_version: 1,
        project: project.clone(),
        ..Default::default()
    };
    (guard, project, snapshot)
}
fn object(fields: &str) -> String {
    format!("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nSettings:\n{fields}")
}
fn seed(project: &str, fields: &str) -> NilMap<String> {
    let content = object(fields);
    write(&join(&[project, LOGICAL]), &content);
    NilMap::from([(LOGICAL, crate::sha256(content.as_bytes()))])
}
fn asset(project: &str, logical: &str, guid: &str, fields: &str) {
    write(&join(&[project, logical]), object(fields));
    write(
        &format!("{}.meta", join(&[project, logical])),
        format!(
            "fileFormatVersion: 2\nguid: {guid}\nNativeFormatImporter:\n  externalObjects: {{}}\n"
        ),
    );
}
fn guid(letter: &str) -> String {
    letter.repeat(32)
}

#[test]
fn tracks_reachable_assets_and_cycles() {
    let (_guard, project, snapshot) = closure_project();
    let (a, b, c) = (guid("a"), guid("b"), guid("c"));
    asset(
        &project,
        "Assets/Render.asset",
        &a,
        &format!("  material: {{fileID: 2100000, guid: {b}, type: 2}}\n"),
    );
    let material = format!("  back: {{fileID: 11400000, guid: {a}, type: 2}}\n");
    asset(&project, "Assets/Default.mat", &b, &material);
    write(
        &join(&[&project, "Assets/Default.mat.meta"]),
        format!(
            "fileFormatVersion: 2\nguid: {b}\nImporter:\n  texture: {{fileID: 2800000, guid: {c}, type: 3}}\n  userData: 'guid: not-a-reference'\n"
        ),
    );
    write(
        &join(&[&project, "Assets/Texture.png"]),
        "binary texture leaf",
    );
    write(
        &join(&[&project, "Assets/Texture.png.meta"]),
        format!("fileFormatVersion: 2\nguid: {c}\nTextureImporter:\n  externalObjects: {{}}\n"),
    );
    asset(
        &project,
        "Assets/Unrelated.asset",
        &guid("d"),
        "  value: 1\n",
    );
    let pipeline = format!("  pipeline: {{fileID: 11400000, guid: {a}, type: 2}}\n");
    let run = || {
        let mut inputs = seed(&project, &pipeline);
        reference_closure(&ctx(), &project, Some(&snapshot), &mut inputs).unwrap();
        inputs
    };
    let first = run();
    assert_eq!(first.len(), 7, "{first:?}");
    assert!(!first.at("Assets/Render.asset").is_empty());
    assert!(!first.at("Assets/Texture.png.meta").is_empty());
    assert!(first.at("Assets/Unrelated.asset").is_empty());
    asset(
        &project,
        "Assets/Unrelated.asset",
        &guid("d"),
        "  value: 2\n",
    );
    assert_eq!(
        first,
        run(),
        "unrelated source changed the defaults profile"
    );
    write(
        &join(&[&project, "Assets/Default.mat"]),
        object(&format!("{material}  changed: 1\n")),
    );
    let third = run();
    assert_ne!(
        first.at("Assets/Default.mat"),
        third.at("Assets/Default.mat"),
        "same-GUID material change did not invalidate profile"
    );
}

#[test]
fn no_external_guid_does_not_index() {
    let (_guard, project, _) = closure_project();
    let (_other_guard, other) = temp_dir();
    std::os::unix::fs::symlink(&other, join(&[&project, "Assets/Unsafe"])).unwrap();
    let mut inputs = seed(
        &project,
        "  shader: {fileID: 69, guid: 0000000000000000f000000000000000, type: 0}\n  zero: {fileID: 0, guid: 00000000000000000000000000000000, type: 0}\n",
    );
    let before = inputs.len();
    reference_closure(&ctx(), &project, None, &mut inputs).unwrap();
    assert_eq!(inputs.len(), before, "builtin-only profile changed");
    let absent_quality = || {
        NilMap::from([(
            "ProjectSettings/QualitySettings.asset",
            "absent".to_string(),
        )])
    };
    reference_closure(&ctx(), &project, None, &mut absent_quality()).unwrap();
    write(
        &join(&[&project, "ProjectSettings/QualitySettings.asset"]),
        object("  value: 1\n"),
    );
    assert!(
        reference_closure(&ctx(), &project, None, &mut absent_quality()).is_err(),
        "new settings file accepted against absent proof"
    );
}

#[test]
fn rejects_unsafe_identity_before_publication() {
    for mode in [
        "missing_asset",
        "duplicate_guid",
        "symlink",
        "binary_asset",
        "bad_nested_guid",
        "metadata_alias",
    ] {
        let (_guard, project, snapshot) = closure_project();
        let (_other_guard, other) = temp_dir();
        let guid = guid("a");
        asset(&project, "Assets/Pipeline.asset", &guid, "  value: 1\n");
        match mode {
            "missing_asset" => fs::remove_file(join(&[&project, "Assets/Pipeline.asset"])).unwrap(),
            "duplicate_guid" => asset(&project, "Assets/Duplicate.asset", &guid, "  value: 1\n"),
            "symlink" => {
                std::os::unix::fs::symlink(&other, join(&[&project, "Assets/Unsafe"])).unwrap()
            }
            "binary_asset" => write(
                &join(&[&project, "Assets/Pipeline.asset"]),
                "\x00binary serialized object",
            ),
            "bad_nested_guid" => write(
                &join(&[&project, "Assets/Pipeline.asset.meta"]),
                format!("guid: {guid}\nImporter:\n  ref: {{guid: broken}}\n"),
            ),
            _ => write(
                &join(&[&project, "Assets/Pipeline.asset.meta"]),
                format!("guid: {guid}\nImporter:\n  ref: &x {{value: 1}}\n  alias: *x\n"),
            ),
        }
        let mut inputs = seed(
            &project,
            &format!("  pipeline: {{fileID: 11400000, guid: {guid}, type: 2}}\n"),
        );
        let before = inputs.clone();
        assert!(
            reference_closure(&ctx(), &project, Some(&snapshot), &mut inputs).is_err(),
            "{mode}: unsafe closure was accepted"
        );
        assert_eq!(
            inputs, before,
            "{mode}: failed capture partially published inputs"
        );
    }
}

#[test]
fn captures_absence_and_invalidates_new_guid() {
    let (_guard, project, snapshot) = closure_project();
    let guid = guid("a");
    let run = || {
        let mut inputs = seed(
            &project,
            &format!("  optionalDebugShader: {{fileID: 4800000, guid: {guid}, type: 3}}\n"),
        );
        reference_closure(&ctx(), &project, Some(&snapshot), &mut inputs).unwrap();
        inputs
    };
    let key = format!("settingsReferenceGuid/{guid}");
    let missing = run();
    assert_eq!(missing.len(), 2, "{missing:?}");
    assert_eq!(missing.at(&key), "absent");
    asset(&project, "Assets/Resolved.asset", &guid, "  value: 1\n");
    let resolved = run();
    assert!(resolved.at(&key).is_empty());
    assert!(!resolved.at("Assets/Resolved.asset").is_empty());
    assert_ne!(
        missing, resolved,
        "newly imported GUID did not invalidate missing-state profile"
    );
}

#[test]
fn uses_resolved_package_and_streams_model_index() {
    let (_guard, project, mut snapshot) = closure_project();
    let (_package_guard, package_root) = temp_dir();
    let guid = guid("a");
    write(
        &join(&[&package_root, "Settings.asset"]),
        object("  enabled: 1\n"),
    );
    write(
        &join(&[&package_root, "Settings.asset.meta"]),
        format!("guid: {guid}\nImporter: {{}}\n"),
    );
    snapshot.packages = vec![Package {
        name: "com.test.render".into(),
        logical_root: "Packages/com.test.render".into(),
        path: package_root.clone(),
        content_hash: "f".repeat(64),
        ..Default::default()
    }];
    write(
        &join(&[&package_root, "Samples~/Duplicate.asset.meta"]),
        format!("guid: {guid}\n"),
    );
    write(
        &join(&[&project, "Assets/Unrelated.fbx.meta"]),
        format!(
            "guid: {}\nModelImporter:\n  big: {}\n",
            "b".repeat(32),
            "x".repeat(1804405)
        ),
    );
    let mut inputs = seed(
        &project,
        &format!("  pipeline: {{fileID: 11400000, guid: {guid}, type: 2}}\n"),
    );
    reference_closure(&ctx(), &project, Some(&snapshot), &mut inputs).unwrap();
    assert_eq!(inputs.len(), 3, "{inputs:?}");
    assert!(
        !inputs
            .at("Packages/com.test.render/Settings.asset")
            .is_empty(),
        "resolved package closure identity missing"
    );
    let cancelled = ctx().with_cancel();
    cancelled.cancel();
    let error = reference_closure(&cancelled, &project, Some(&snapshot), &mut inputs).unwrap_err();
    assert_eq!(
        error.downcast_ref::<Canceled>(),
        Some(&Canceled::Canceled),
        "cancellation was not returned"
    );
}

/// Needs an explicit read-only project and Editor path; no Editor is executed.
#[test]
fn actual_owned_project() {
    let (Ok(project), Ok(editor)) = (
        std::env::var("UNITY_MICRO_EDITOR_CLOSURE_PROJECT"),
        std::env::var("UNITY_MICRO_EDITOR_CLOSURE_EDITOR"),
    ) else {
        return;
    };
    let snapshot = crate::packages::resolve(
        &ctx(),
        &project,
        &editor,
        crate::packages::Options {
            offline: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut inputs: NilMap<String> = NilMap::make();
    for name in defaults::settings_names() {
        let logical = format!("ProjectSettings/{name}");
        let path = join(&[&project, &logical]);
        let hash = if absent(&path) {
            "absent".to_string()
        } else {
            util::file_hash(&path).unwrap()
        };
        inputs.insert(logical, hash);
    }
    reference_closure(&ctx(), &project, Some(&snapshot), &mut inputs).unwrap();
    let missing = inputs
        .iter()
        .filter(|(key, value)| key.starts_with("settingsReferenceGuid/") && *value == "absent")
        .count();
    println!(
        "read-only defaults closure inputs={} explicitlyMissingSettingsGUIDs={missing}",
        inputs.len()
    );
}
