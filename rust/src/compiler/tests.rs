//! Compiler tests. SDK tests are gated on environment variables and return early
//! without them.
use super::{
    nilable::unmarshal,
    sys::{self, compiler_command},
    *,
};
use crate::{
    cancel::Context,
    sha256, sysutil,
    util::{agent_immutable_json, atomic_json, file_hash, random_id, read_json_file, valid_sha256},
};
use serde_json::{json, value::RawValue};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    os::unix::fs::{PermissionsExt, symlink},
};
use tempfile::TempDir;

fn temp() -> (TempDir, String) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().to_str().unwrap().to_string();
    (directory, path)
}
fn write_graph_file(project: &str, relative: &str, contents: &str) -> String {
    let filename = join(&[project, relative]);
    sys::mkdir_all(&sysutil::dir(&filename)).unwrap();
    sys::write(&filename, contents.as_bytes()).unwrap();
    filename
}
fn strs(items: &[&str]) -> NilVec<String> {
    NilVec::from(items)
}
fn hash(path: &str) -> String {
    file_hash(path).unwrap()
}
fn manifest_json(text: &str) -> CompileManifest {
    unmarshal(text.as_bytes()).unwrap()
}
fn raw(bytes: Vec<u8>) -> Box<RawValue> {
    RawValue::from_string(String::from_utf8(bytes).unwrap()).unwrap()
}
fn sdk(variable: &str, reason: &str) -> Option<String> {
    let value = std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty());
    if value.is_none() {
        eprintln!("skipped: {reason}");
    }
    value
}
fn background() -> Context {
    Context::background()
}

#[test]
fn compiler_configuration_drift_and_new_sources_are_rejected() {
    let (_guard, project) = temp();
    let source = write_graph_file(&project, "Assets/Runtime/Script.cs", "class Script {}");
    let meta = write_graph_file(
        &project,
        "Assets/Runtime/Script.cs.meta",
        "guid: b1d15452ab70b6a669e9b8e3c0b91251\n",
    );
    let compiler = Compiler {
        project: project.clone(),
        sources: NilMap::from([(source.clone(), "hash".to_string())]),
        configuration: NilMap::from([(meta.clone(), hash(&meta))]),
        manifest: CompileManifest {
            compiler_inputs: NilMap::from([(source.clone(), "hash".to_string())]),
            ..Default::default()
        },
        ..Default::default()
    };
    compiler.check_configuration(&[source]).unwrap();
    let added = write_graph_file(
        &project,
        "Assets/Runtime/NewScript.cs",
        "class NewScript {}",
    );
    assert!(
        compiler.check_configuration(&[]).is_err(),
        "new runtime source was silently omitted from old DLL"
    );
    fs::remove_file(added).unwrap();
    sys::write(&meta, b"guid: c1d15452ab70b6a669e9b8e3c0b91251\n").unwrap();
    assert!(
        compiler.check_configuration(&[]).is_err(),
        "MonoScript GUID change accepted against stale binding graph"
    );
}

#[test]
fn compiler_rejects_changed_dll_and_importer_metadata_before_source_reuse() {
    let (_project, project) = temp();
    let (_sdk, sdk) = temp();
    let plugin = write_graph_file(&project, "Assets/Plugins/Plugin.dll", "original plugin");
    let meta = write_graph_file(
        &project,
        "Assets/Plugins/Plugin.dll.meta",
        "original importer flags",
    );
    let external = write_graph_file(&sdk, "ImmutableHost.dll", "original SDK/host reference");
    let mut inputs = NilMap::make();
    for path in [&plugin, &meta, &external] {
        inputs.insert(path, hash(path));
    }
    let mut compiler = Compiler {
        project: project.clone(),
        configuration: NilMap::make(),
        manifest: CompileManifest {
            compiler_inputs: inputs,
            ..Default::default()
        },
        ..Default::default()
    };
    compiler.collect_configuration_inputs();
    compiler.check_configuration(&[]).unwrap();
    for path in [&plugin, &meta, &external] {
        let original = fs::read(path).unwrap();
        sys::write(path, b"changed binary reference/import settings").unwrap();
        match compiler.compile(&background(), Some(std::slice::from_ref(path))) {
            Err(error) if error.to_string().contains("regenerate source graph") => {}
            other => {
                panic!("reused assemblies against modified baseline DLL input {path}: {other:?}")
            }
        }
        sys::write(path, &original).unwrap();
    }
    assert!(
        compiler
            .compile(
                &background(),
                Some(&["Packages/com.example/NewPlugin.dll".to_string()])
            )
            .is_err(),
        "untracked package DLL event silently reused the old managed graph"
    );
}

#[test]
fn compiler_rejects_missing_or_renamed_baseline_script_types() {
    let manifest = manifest_json(
        r#"{"scripts":[{"guid":"known-guid","path":"/project/Assets/AmbientSound.cs","assembly":"Game"},{"guid":"other-guid","path":"/project/Assets/Helpers.cs","assembly":"Other"}]}"#,
    );
    let compiler = Compiler {
        manifest,
        expected_types: NilMap::from([(
            "Game",
            NilMap::from([("Sample.CustomPrimary", "explicit-scene-guid".to_string())]),
        )]),
        ..Default::default()
    };
    let previous = strs(&[
        "Sample.AmbientSound",
        "Sample.CustomPrimary",
        "Sample.PrivateHelper",
    ]);
    compiler
        .validate_required_script_types(
            "Game",
            &previous,
            &strs(&["Sample.AmbientSound", "Sample.CustomPrimary"]),
        )
        .expect("accepted body edit or unrelated private helper deletion was rejected");
    for next in [
        strs(&["Sample.RenamedAmbientSound", "Sample.CustomPrimary"]),
        strs(&["OtherNamespace.AmbientSound", "Sample.CustomPrimary"]),
        strs(&["Sample.AmbientSound"]),
    ] {
        assert!(
            compiler
                .validate_required_script_types("Game", &previous, &next)
                .is_err(),
            "published stale GUID/type bindings after removing or renaming required script: {next:?}"
        );
    }
}

#[test]
fn compiler_collects_exact_scene_guid_type_bindings() {
    let (_guard, project) = temp();
    let manifest = join(&[&project, "scene.json"]);
    atomic_json(
        &manifest,
        &json!({"projectRoot": project, "assets": [
            {"kind": "script", "guid": "script-guid", "typeName": "Namespace.NonFilenameType"},
            {"kind": "script", "guid": "host-guid", "typeName": "Unity.HostType"},
        ]}),
    )
    .unwrap();
    let baseline = join(&[&project, "current.json"]);
    atomic_json(&baseline, &json!({"manifest": manifest})).unwrap();
    let plan = manifest_json(
        r#"{"scripts":[{"guid":"script-guid","assembly":"Game"},{"guid":"host-guid","assembly":"Unity.Host","hostProvided":true}]}"#,
    );
    let mut compiler = Compiler {
        project,
        manifest: plan,
        current_dlls: NilMap::from([("Game", "Game.dll".to_string())]),
        ..Default::default()
    };
    compiler.collect_expected_script_types(&baseline).unwrap();
    assert!(
        compiler
            .validate_required_script_types("Game", &[], &strs(&["Namespace.DifferentType"]))
            .is_err(),
        "scene GUID binding was lost when class name differed from source filename"
    );
    compiler
        .validate_required_script_types("Game", &[], &strs(&["Namespace.NonFilenameType"]))
        .unwrap();
}

/// The scene manifest is decoded into the struct that already holds the wrapper
/// document; fields the manifest omits keep the wrapper's values.
#[test]
fn compiler_binding_documents_merge() {
    let (_guard, project) = temp();
    let manifest = write_graph_file(
        &project,
        "scene.json",
        r#"{"assets":[{"guid":"script-guid"},null,{"kind":"script","guid":"third","typeName":"Third"}]}"#,
    );
    let baseline = join(&[&project, "current.json"]);
    atomic_json(
        &baseline,
        &json!({"manifest": manifest, "projectRoot": project, "assets": [
            {"kind": "script", "typeName": "Namespace.Kept"},
            {"kind": "script", "guid": "second", "typeName": "Second"},
        ]}),
    )
    .unwrap();
    let plan = manifest_json(
        r#"{"scripts":[{"guid":"script-guid","assembly":"Game"},{"guid":"second","assembly":"Game"},{"guid":"third","assembly":"Game"}]}"#,
    );
    let mut compiler = Compiler {
        project,
        manifest: plan,
        current_dlls: NilMap::from([("Game", "Game.dll".to_string())]),
        ..Default::default()
    };
    compiler.collect_expected_script_types(&baseline).unwrap();
    let expected: BTreeMap<String, String> = [
        ("Namespace.Kept", "script-guid"),
        ("Second", "second"),
        ("Third", "third"),
    ]
    .into_iter()
    .map(|(name, guid)| (name.to_string(), guid.to_string()))
    .collect();
    assert_eq!(*compiler.expected_types["Game"], expected);
}

fn valid_proof() -> LifecycleProof {
    LifecycleProof {
        schema_version: 1,
        verified: true,
        scriptable_object_barrier: true,
        phases: strs(&["Awake", "OnEnable", "OnDisable", "OnDestroy"]),
        ..Default::default()
    }
}
fn initializer(assembly: &str, type_name: &str, method: &str, phase: i64) -> RuntimeInitializer {
    RuntimeInitializer {
        assembly: assembly.into(),
        type_name: type_name.into(),
        method_name: method.into(),
        phase,
    }
}

#[test]
fn compiler_rejects_invalid_lifecycle_proof_and_cache_tampering() {
    let mut proof = valid_proof();
    proof.runtime_initializers = NilVec::from([initializer("Game", "Init", "Call", 2)]);
    assert!(valid_lifecycle_proof(&proof, "Game"));
    proof.runtime_initializers[0].assembly = "Other".into();
    assert!(
        !valid_lifecycle_proof(&proof, "Game"),
        "foreign assembly initializer accepted"
    );
    let (_guard, directory) = temp();
    let filename = write_graph_file(&directory, "Game.dll", "verified bytes");
    let digest = hash(&filename);
    assert!(verify_cache_files(
        &directory,
        &NilMap::from([("Game.dll", digest.clone())])
    ));
    assert!(
        !verify_cache_files(&directory, &NilMap::from([("../Game.dll", digest.clone())])),
        "cache key path traversal accepted"
    );
    sys::write(&filename, b"tampered bytes").unwrap();
    assert!(
        !verify_cache_files(&directory, &NilMap::from([("Game.dll", digest)])),
        "tampered DLL was reused"
    );
}

#[test]
fn compiler_references_keep_mono_sdk_and_emitted_dependencies() {
    let (_guard, managed) = temp();
    for name in [
        "mscorlib.dll",
        "UnityMicroEditor.dll",
        "Game.dll",
        "Dependency.dll",
    ] {
        write_graph_file(&managed, name, name);
    }
    let compiler = Compiler {
        base_references: strs(&["/sdk/mscorlib.dll"]),
        engine_references: strs(&["/sdk/UnityEngine.CoreModule.dll"]),
        current_dlls: NilMap::from([
            ("Game", join(&[&managed, "Game.dll"])),
            ("Dependency", join(&[&managed, "Dependency.dll"])),
        ]),
        configs: NilMap::from([(
            "Game",
            AssemblyConfig {
                name: "Game".into(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let refs = compiler
        .references(
            &CompileAssembly {
                name: "Game".into(),
                references: strs(&["Dependency"]),
                ..Default::default()
            },
            &BTreeMap::from([("Dependency".to_string(), "/new/Dependency.dll".to_string())]),
        )
        .unwrap();
    let text = refs.join("\n");
    assert!(
        text.contains("/sdk/mscorlib.dll")
            && !text.contains(&join(&[&managed, "mscorlib.dll"]))
            && text.contains("/new/Dependency.dll")
            && !text.contains(&join(&[&managed, "Dependency.dll"]))
            && !text.contains(&join(&[&managed, "Game.dll"])),
        "references mixed stale/current game DLLs or Mono SDK: {text}"
    );
}

fn assembly(name: &str, sources: &[&str], references: &[&str]) -> CompileAssembly {
    CompileAssembly {
        name: name.into(),
        sources: strs(sources),
        references: if references.is_empty() {
            NilVec::nil()
        } else {
            strs(references)
        },
        ..Default::default()
    }
}
fn string_map(items: &[(&str, &str)]) -> NilMap<String> {
    let mut map = NilMap::make();
    for (key, value) in items {
        map.insert(*key, value.to_string());
    }
    map
}

#[test]
fn compiler_changed_interface_rebuilds_dependent_graph() {
    let assemblies = [
        assembly("API", &["api.cs"], &[]),
        assembly("Unrelated", &["unrelated.cs"], &[]),
        assembly("Left", &["left.cs"], &["API"]),
        assembly("Right", &["right.cs"], &["API"]),
        assembly("Game", &["game.cs"], &["Left", "Right"]),
    ];
    let same = [
        ("unrelated.cs", "same"),
        ("left.cs", "same"),
        ("right.cs", "same"),
        ("game.cs", "same"),
    ];
    let mut previous = string_map(&same);
    previous.insert("api.cs", "old API".to_string());
    let mut current = string_map(&same);
    current.insert("api.cs", "new API".to_string());
    let dirty = dirty_assemblies(&assemblies, &previous, &current);
    let expected: BTreeSet<String> = ["API", "Left", "Right", "Game"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        dirty, expected,
        "changed interface did not invalidate the complete dependent graph"
    );
}

#[test]
fn compiler_body_only_change_keeps_dependent_assemblies() {
    let api = assembly("API", &["api.cs"], &[]);
    let left = assembly("Left", &["left.cs"], &["API"]);
    let game = assembly("Game", &["game.cs"], &["Left"]);
    let dirty = source_dirty_assemblies(
        &[api.clone(), left.clone(), game.clone()],
        &string_map(&[("api.cs", "old"), ("left.cs", "same"), ("game.cs", "same")]),
        &string_map(&[("api.cs", "new"), ("left.cs", "same"), ("game.cs", "same")]),
    );
    let changed = |items: &[(&str, bool)]| -> BTreeMap<String, bool> {
        items
            .iter()
            .map(|(name, flag)| (name.to_string(), *flag))
            .collect()
    };
    assert!(
        dirty.len() == 1
            && assembly_needs_compile(&api, &dirty, &BTreeMap::new())
            && !assembly_needs_compile(&left, &dirty, &changed(&[("API", false)])),
        "body-only source edit rebuilt an unchanged dependent declaration graph"
    );
    assert!(
        assembly_needs_compile(&left, &dirty, &changed(&[("API", true)])),
        "changed dependency declarations did not rebuild their consumer"
    );
    assert!(
        !assembly_needs_compile(&game, &dirty, &changed(&[("API", true), ("Left", false)])),
        "unchanged intermediate API caused an unnecessary transitive rebuild"
    );
    assert!(
        assembly_needs_compile(&game, &dirty, &changed(&[("Left", true)])),
        "changed intermediate API did not propagate to its consumer"
    );
}

#[test]
fn compiler_declaration_fingerprint_sdk() {
    let Some(editor) = sdk(
        "UME_DECLARATION_SDK",
        "set UME_DECLARATION_SDK to matching Unity Editor executable; uses its C# SDK only",
    ) else {
        return;
    };
    let (_project, project) = temp();
    let data = join(&[&sysutil::dir(&editor), "Data"]);
    let compilers = sys::glob_lossy(&join(&[&data, "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll"]));
    assert_eq!(
        compilers.len(),
        1,
        "expected one exact SDK compiler: {compilers:?}"
    );
    let (_root, root) = temp();
    let mut compiler = Compiler {
        project,
        cache: join(&[&root, "compiler-cache"]),
        dotnet: join(&[&data, "DotNetSdk/dotnet"]),
        csc: compilers[0].clone(),
        ..Default::default()
    };
    let ctx = background();
    let helper = compiler.prepare_lifecycle_helper(&ctx).unwrap();
    let (dotnet, csc) = (compiler.dotnet.clone(), compiler.csc.clone());
    let compile = |name: &str, source: &str| -> String {
        let folder = join(&[&root, name]);
        sys::mkdir(&folder).unwrap();
        let filename = join(&[&folder, "Shape.cs"]);
        sys::write(&filename, source.as_bytes()).unwrap();
        let dll = join(&[&folder, "Shape.dll"]);
        compiler_command(
            &ctx,
            &folder,
            &dotnet,
            &[
                &csc,
                "-nologo",
                "-target:library",
                "-nostdlib+",
                "-deterministic+",
                "-debug:portable",
                "-optimize-",
                &format!("-out:{dll}"),
                &format!(
                    "-reference:{}",
                    join(&[
                        &data,
                        "MonoBleedingEdge/lib/mono/unityaot-linux/mscorlib.dll"
                    ])
                ),
                &filename,
            ],
        )
        .unwrap();
        dll
    };
    const SOURCE: &str = r#"using System;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
[assembly: InternalsVisibleTo("FriendOne")]
[StructLayout(LayoutKind.Sequential, Pack=4)]
[Obsolete("original")]
public class Shape<T> where T : class {
 public const int Value = 1;
 internal int Field;
 private string Hidden() { return "before"; }
 public int Compute(int optional = 4) { return 41; }
}"#;
    let baseline = compile("baseline", SOURCE);
    let body = compile(
        "body",
        &SOURCE.replacen("return 41;", "return 42;", 1).replacen(
            "return \"before\";",
            "return \"after with a longer user-string literal\";",
            1,
        ),
    );
    let changed = compiler.declarations_changed(&ctx, &helper, &baseline, &body);
    assert!(
        matches!(changed, Ok(false)),
        "real IL/user-string/MVID changes were mistaken for declaration changes: {changed:?}"
    );
    assert_ne!(
        hash(&baseline),
        hash(&body),
        "SDK body fixture did not produce different real DLL bytes"
    );
    compiler.expected_types = NilMap::from([(
        "Shape",
        NilMap::from([("Shape`1", "script-guid".to_string())]),
    )]);
    compiler
        .validate_compiled_script_types(&ctx, &helper, "Shape", &baseline, &body)
        .expect("actual body-only compiled DLL rejected against its baseline script TypeDef");
    let renamed = compile(
        "renamed-type",
        &SOURCE.replacen("class Shape<T>", "class RenamedShape<T>", 1),
    );
    assert!(
        compiler
            .validate_compiled_script_types(&ctx, &helper, "Shape", &baseline, &renamed)
            .is_err(),
        "real SDK-compiled class rename did not reject stale MonoScript bindings"
    );
    for (name, from, to) in [
        ("constant", "Value = 1", "Value = 2"),
        (
            "attribute",
            "Obsolete(\"original\")",
            "Obsolete(\"changed\")",
        ),
        ("layout", "Pack=4", "Pack=8"),
        ("generic-constraint", "where T : class", "where T : struct"),
        ("friend-assembly", "FriendOne", "FriendTwo"),
        (
            "internal-field",
            "internal int Field",
            "internal long Field",
        ),
        ("default-parameter", "optional = 4", "optional = 5"),
        ("signature", "public int Compute", "public long Compute"),
    ] {
        let dll = compile(name, &SOURCE.replacen(from, to, 1));
        let changed = compiler.declarations_changed(&ctx, &helper, &baseline, &dll);
        assert!(
            matches!(changed, Ok(true)),
            "changed {name} did not invalidate the declaration fingerprint: {changed:?}"
        );
    }
    let bad = write_graph_file(&root, "malformed.dll", "not a managed PE");
    assert!(
        compiler
            .declarations_changed(&ctx, &helper, &baseline, &bad)
            .is_err(),
        "unknown metadata did not retain conservative dependency rebuild"
    );
}

#[test]
fn compiler_scene_and_native_changes_do_not_scan_managed_inputs() {
    // Deliberately invalid tracked configuration would fail a full scan.
    // Existing scene/native edits bypass it through watcher classification.
    let (_guard, project) = temp();
    let mut compiler = Compiler {
        project,
        configuration: NilMap::from([("/missing/config.cs.meta", "hash".to_string())]),
        ..Default::default()
    };
    let paths = |items: &[&str]| -> Vec<String> { items.iter().map(|s| s.to_string()).collect() };
    for changed in [
        paths(&["Assets/Scenes/SampleScene.unity"]),
        paths(&["Assets/floor.fbx", "Assets/floor.fbx.meta"]),
        paths(&["Assets/audio.wav"]),
        paths(&["Assets/Texture.png", "Assets/Texture.png.meta"]),
    ] {
        assert!(
            matches!(compiler.compile(&background(), Some(&changed)), Ok(None)),
            "non-code edit invoked compiler scans: {changed:?}"
        );
    }
    for changed in [
        "Assets/Script.cs",
        "Assets/Script.cs.meta",
        "Assets/Game.asmdef",
        "ProjectSettings/ProjectSettings.asset",
        "Packages/manifest.json",
        "Assets/NewDirectory",
    ] {
        assert!(
            compiler.compiler_changes(&paths(&[changed])),
            "source/config/topology edit skipped stale-code validation: {changed}"
        );
    }
}

#[test]
fn compiler_sdk_integration_without_editor() {
    let (Ok(baseline), Ok(player)) = (
        std::env::var("UME_COMPILER_BASELINE"),
        std::env::var("UME_COMPILER_PLAYER"),
    ) else {
        eprintln!(
            "skipped: optional SDK-only integration; set UME_COMPILER_BASELINE and UME_COMPILER_PLAYER"
        );
        return;
    };
    let ctx = background();
    let (manifest, _) = compiler_manifest(&baseline).unwrap();
    let cache = join(&[
        &manifest.project,
        "Temp/UnityPlay/compiler-proof",
        &random_id(),
    ]);
    let mut compiler = Compiler::new(
        &manifest.project,
        &baseline,
        &player,
        &manifest.unity,
        &cache,
    )
    .unwrap();
    let initial = compiler.compile(&ctx, None).unwrap();
    for assembly in &compiler.manifest.assemblies {
        assert!(
            !needs_netcode(assembly)
                || (initial.is_some() && compiler.netcode_processed.on(&assembly.name)),
            "plain Roslyn network baseline was reused without NGO postprocessing: {initial:?}"
        );
    }
    assert!(
        matches!(compiler.compile(&ctx, None), Ok(None)),
        "processed unchanged baseline should reuse verified source DLLs"
    );
    if let Some(initial) = &initial {
        let mut restarted = Compiler::new(
            &manifest.project,
            &baseline,
            &player,
            &manifest.unity,
            &cache,
        )
        .unwrap();
        let reused = restarted.compile(&ctx, None).unwrap().unwrap();
        assert!(
            reused.cache_reused
                && reused.player == initial.player
                && reused.compiled_assemblies.is_empty()
                && reused.runtime_initializers.len() == initial.runtime_initializers.len(),
            "fresh process repeated SDK compilation instead of reusing verified complete graph: {reused:?}"
        );
        for assembly in &manifest.assemblies {
            assert!(
                !needs_netcode(assembly) || restarted.netcode_processed.on(&assembly.name),
                "cached NGO postprocessing proof did not restore networking state"
            );
        }
    }
    // Simulate a stale source checksum in the compiler state, without
    // changing author-owned project sources or launching any Player.
    let leaf = compiler.manifest.assemblies.last().unwrap().clone();
    compiler
        .sources
        .insert(&leaf.sources[0], "stale source checksum".to_string());
    let update = compiler.compile(&ctx, None).unwrap().unwrap();
    assert!(
        update.player != player
            && update.compiled_assemblies.to_vec() == [leaf.name.clone()]
            && update.assemblies.len() == compiler.manifest.assemblies.len(),
        "stale source did not create a complete immutable Player revision: {update:?}"
    );
    fs::metadata(&update.player).unwrap();
    // Validate the real scene's GUID/full-name binding map against DLL TypeDefs,
    // without loading any assembly or starting another Unity process.
    let helper = compiler.prepare_lifecycle_helper(&ctx).unwrap();
    for (assembly, names) in &compiler.expected_types {
        if names.is_empty() {
            continue;
        }
        let previous = join(&[&player_managed(&player), &format!("{assembly}.dll")]);
        compiler
            .validate_compiled_script_types(
                &ctx,
                &helper,
                assembly,
                &previous,
                compiler.current_dlls.at(assembly),
            )
            .expect("actual scene baseline script bindings did not match emitted DLL");
    }
    assert!(
        matches!(compiler.compile(&ctx, None), Ok(None)),
        "unchanged code should reuse compiled revision"
    );
}

#[test]
fn compiler_warm_profile_sdk() {
    let (Ok(baseline), Ok(player), Ok(cache)) = (
        std::env::var("UME_COMPILER_BASELINE"),
        std::env::var("UME_COMPILER_PLAYER"),
        std::env::var("UME_COMPILER_PROFILE_CACHE"),
    ) else {
        eprintln!(
            "skipped: set compiler baseline/player/profile cache for SDK-only startup timings"
        );
        return;
    };
    let ctx = background();
    let (manifest, _) = compiler_manifest(&baseline).unwrap();
    let mut previous_player = String::new();
    for run in 0..2 {
        let mut compiler = Compiler::new(
            &manifest.project,
            &baseline,
            &player,
            &manifest.unity,
            &cache,
        )
        .unwrap();
        let update = compiler
            .compile(&ctx, None)
            .unwrap()
            .expect("expected processed game graph for old raw baseline");
        compiler.script_type_names(&ctx).unwrap();
        assert!(
            run == 0
                || (update.cache_reused
                    && update.player == previous_player
                    && update.compiled_assemblies.is_empty()),
            "unchanged SDK startup failed whole graph cache: {update:?}"
        );
        previous_player = update.player;
    }
}

struct Locator {
    _guard: TempDir,
    project: String,
    wrapper: String,
    scene: String,
    plan: CompileManifest,
    config: AssemblyConfig,
}
fn wrapper_json(plan: &CompileManifest, manifest: &str) -> serde_json::Value {
    json!({"compiler": plan, "manifest": manifest})
}
fn compiler_locator_fixture() -> Locator {
    let (guard, project) = temp();
    let source = write_graph_file(&project, "Assets/Known.cs", "public class Known {}");
    let dll = write_graph_file(
        &project,
        "Library/UnityMicroEditor/preparations/one/.compiler-preparation/Player/Player_Data/Managed/Assembly-CSharp.dll",
        "accepted unchanged DLL",
    );
    let digest = hash(&dll);
    let assembly = CompileAssembly {
        name: "Assembly-CSharp".into(),
        path: dll.clone(),
        sha256: digest.clone(),
        sources: NilVec::from([source]),
        assembly_input_hash: "a".repeat(64),
        ..Default::default()
    };
    let plan = CompileManifest {
        schema: 1,
        project: project.clone(),
        unity: UNITY_VERSION.into(),
        assemblies: NilVec::from([assembly.clone()]),
        compiler_inputs: NilMap::from([(dll, digest)]),
        ..Default::default()
    };
    let config = AssemblyConfig {
        name: assembly.name.clone(),
        sources: assembly.sources.clone(),
        defines: strs(&["UNITY_STANDALONE_LINUX"]),
        ..Default::default()
    };
    let canonical = canonical_configuration(&config).unwrap();
    let source_directory = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/one/.source-preparation",
    ]);
    let descriptor = join(&[
        &source_directory,
        "compiler-cache/baseline-assemblies",
        &assembly.assembly_input_hash,
        "compile.json",
    ]);
    #[derive(serde::Serialize)]
    struct Descriptor<'a> {
        #[serde(rename = "assemblyConfiguration")]
        assembly_configuration: Box<RawValue>,
        #[serde(rename = "compilerInputs")]
        compiler_inputs: BTreeMap<&'a str, String>,
        assemblies: [&'a CompileAssembly; 1],
    }
    agent_immutable_json(
        &descriptor,
        &Descriptor {
            compiler_inputs: BTreeMap::from([("assemblyConfiguration", sha256(&canonical))]),
            assembly_configuration: raw(canonical),
            assemblies: [&assembly],
        },
    )
    .unwrap();
    let scene = join(&[&source_directory, "source/revisions/current/scene.json"]);
    agent_immutable_json(
        &scene,
        &json!({"schemaVersion": 1, "supported": true, "projectRoot": project}),
    )
    .unwrap();
    let wrapper = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/one/current.json",
    ]);
    atomic_json(&wrapper, &wrapper_json(&plan, &scene)).unwrap();
    Locator {
        _guard: guard,
        project,
        wrapper,
        scene,
        plan,
        config,
    }
}

#[test]
fn compiler_locates_unchanged_dll_configuration_from_explicit_owned_source_manifest() {
    let fixture = compiler_locator_fixture();
    assert!(
        read_assembly_configuration(&fixture.wrapper, &fixture.plan.assemblies[0], None).is_err(),
        "fixture does not reproduce a descriptor outside baseline/DLL ancestors"
    );
    let (accepted, locator) = compiler_manifest(&fixture.wrapper).unwrap();
    assert_eq!(
        locator, fixture.scene,
        "exact owned scene locator was discarded"
    );
    let (actual, _) = read_assembly_configuration(&locator, &accepted.assemblies[0], None).unwrap();
    assert_eq!(
        actual, fixture.config,
        "unchanged accepted DLL lost its sibling source configuration"
    );
}

#[test]
fn compiler_rejects_foreign_or_symlink_scene_configuration_locators() {
    for name in [
        "foreign cache",
        "noncache",
        "symlink",
        "wrong project",
        "unsupported scene",
    ] {
        let fixture = compiler_locator_fixture();
        let (_other, other) = temp();
        let mut locator = fixture.scene.clone();
        match name {
            "foreign cache" => {
                locator = write_graph_file(&other, "Library/UnityMicroEditor/scene.json", "{}");
            }
            "noncache" => locator = write_graph_file(&fixture.project, "Assets/scene.json", "{}"),
            "symlink" => {
                locator = join(&[&sysutil::dir(&fixture.scene), "alias.json"]);
                symlink(&fixture.scene, &locator).unwrap();
            }
            "wrong project" => atomic_json(
                &fixture.scene,
                &json!({"schemaVersion": 1, "supported": true, "projectRoot": other}),
            )
            .unwrap(),
            _ => atomic_json(
                &fixture.scene,
                &json!({"schemaVersion": 1, "supported": false, "projectRoot": fixture.project}),
            )
            .unwrap(),
        }
        atomic_json(&fixture.wrapper, &wrapper_json(&fixture.plan, &locator)).unwrap();
        assert!(
            compiler_manifest(&fixture.wrapper).is_err(),
            "unowned or invalid locator redirected compiler configuration discovery: {name}"
        );
    }
}

#[test]
fn compiler_preparation_project_version_uses_exact_lines() {
    let version = format!("m_EditorVersion: {UNITY_VERSION}");
    for (name, text, valid) in [
        ("lf", format!("{version}\n"), true),
        ("crlf", format!("{version}\r\n"), true),
        ("no-final-newline", version.clone(), true),
        ("wrong", "m_EditorVersion: 6000.6.4f1\n".to_string(), false),
        ("suffix", format!("{version}-suffix\n"), false),
        ("duplicate", format!("{version}\n{version}\n"), false),
    ] {
        let (_guard, project) = temp();
        write_graph_file(&project, "ProjectSettings/ProjectVersion.txt", &text);
        let error = prepare_compiler_graph(
            &background(),
            &project,
            "unused-editor",
            "unused-player",
            &join(&[&project, "missing-graph.json"]),
            &join(&[&project, "Temp/compiler.json"]),
        )
        .unwrap_err();
        assert!(
            if valid {
                sys::is_not_exist(&error)
            } else {
                error.to_string().contains("verified Unity")
            },
            "project version preflight {name} validity={valid} err={error}"
        );
        assert!(
            sys::absent(&join(&[&project, "Temp"])),
            "compiler preflight failure mutated the output directory"
        );
    }
}

#[test]
fn compiler_native_bases_require_verified_direct_package_dependency() {
    let (_guard, project) = temp();
    let managed = join(&[&project, "Managed"]);
    let host = write_graph_file(
        &project,
        "Managed/Package.Runtime.dll",
        "verified native DLL fixture",
    );
    let named = |name: &str| AssemblyConfig {
        name: name.into(),
        ..Default::default()
    };
    let mut compiler = Compiler {
        manifest: CompileManifest {
            compiler_inputs: NilMap::from([(host.clone(), hash(&host))]),
            scripts: NilVec::from([CompileScript {
                assembly: "Package.Runtime".into(),
                host_provided: true,
                ..Default::default()
            }]),
            ..Default::default()
        },
        configs: NilMap::from([
            (
                "Game",
                AssemblyConfig {
                    dependencies: strs(&["Package.Runtime"]),
                    ..named("Game")
                },
            ),
            ("Other", named("Other")),
        ]),
        configuration_hashes: NilMap::make(),
        ..Default::default()
    };
    compiler.approve_prepared_native_bases(&managed).unwrap();
    assert!(
        compiler.configs["Game"].native_base.to_vec() == ["Package.Runtime"]
            && compiler.configs["Other"].native_base.is_empty(),
        "verified native bases were not limited to direct dependencies: {:?}",
        compiler.configs
    );
    compiler.configs.insert(
        "Other",
        AssemblyConfig {
            native_base: strs(&["Package.Runtime"]),
            ..named("Other")
        },
    );
    assert!(
        compiler.approve_prepared_native_bases(&managed).is_err(),
        "native base outside explicit assembly dependencies was approved"
    );
    compiler.configs.insert("Other", named("Other"));
    sys::write(&host, b"changed native DLL").unwrap();
    assert!(
        compiler.approve_prepared_native_bases(&managed).is_err(),
        "native base with changed deployment digest was approved"
    );
}

#[test]
fn compiler_native_only_manifest_requires_explicit_proof() {
    let inputs = || NilMap::from([("input", "digest".to_string())]);
    let script = |assembly: &str, host_provided: bool| {
        NilVec::from([CompileScript {
            assembly: assembly.into(),
            host_provided,
            ..Default::default()
        }])
    };
    let native = || CompileManifest {
        schema: 1,
        native_only: true,
        native_slots_sha: "a".repeat(64),
        compiler_inputs: inputs(),
        ..Default::default()
    };
    for (name, manifest, valid) in [
        (
            "empty-unverified",
            CompileManifest {
                schema: 1,
                compiler_inputs: inputs(),
                ..Default::default()
            },
            false,
        ),
        (
            "missing-slots",
            CompileManifest {
                native_slots_sha: String::new(),
                ..native()
            },
            false,
        ),
        (
            "gameplay-script",
            CompileManifest {
                scripts: script("Game", false),
                ..native()
            },
            false,
        ),
        (
            "contradictory-game-assembly",
            CompileManifest {
                assemblies: NilVec::from([assembly("Game", &[], &[])]),
                ..native()
            },
            false,
        ),
        (
            "native-only",
            CompileManifest {
                scripts: script("Package.Runtime", true),
                ..native()
            },
            true,
        ),
    ] {
        let (_guard, directory) = temp();
        let filename = join(&[&directory, "compile.json"]);
        atomic_json(&filename, &manifest).unwrap();
        let outcome = compiler_manifest(&filename);
        assert_eq!(outcome.is_ok(), valid, "{name}: {:?}", outcome.err());
    }
}

#[test]
fn compiler_native_managed_proof_checks_active_copy() {
    let (_guard, project) = temp();
    let original = write_graph_file(
        &project,
        "baseline/Host_Data/Managed/Package.Runtime.dll",
        "native package",
    );
    let active = write_graph_file(
        &project,
        "active/Host_Data/Managed/Package.Runtime.dll",
        "native package",
    );
    let compiler = Compiler {
        manifest: CompileManifest {
            compiler_inputs: NilMap::from([
                (original.clone(), hash(&original)),
                (
                    join(&[&project, "baseline/Host_Data/Managed/UnityMicroEditor.dll"]),
                    "bridge SDK refresh permitted".to_string(),
                ),
                (
                    join(&[&project, "baseline/Host_Data/Managed/Game.dll"]),
                    "game proof checked separately".to_string(),
                ),
            ]),
            ..Default::default()
        },
        ..Default::default()
    };
    let managed = sysutil::dir(&active);
    let game = BTreeSet::from(["Game".to_string()]);
    compiler
        .verify_staged_native_managed_inputs(&managed, &game)
        .unwrap();
    sys::write(&active, b"changed active copy").unwrap();
    assert!(
        compiler
            .verify_staged_native_managed_inputs(&managed, &game)
            .is_err(),
        "changed active package DLL was concealed by an intact original baseline cache"
    );
    fs::remove_file(&active).unwrap();
    symlink(&original, &active).unwrap();
    let error = compiler
        .verify_staged_native_managed_inputs(&managed, &game)
        .unwrap_err();
    assert!(
        error.to_string().contains("symlink"),
        "native package symlink accepted: {error}"
    );
}

fn source_extension_fixture() -> (TempDir, Compiler) {
    let (guard, project) = temp();
    let source = write_graph_file(&project, "Assets/Game/Known.cs", "class Known {}");
    let meta = write_graph_file(
        &project,
        "Assets/Game/Known.cs.meta",
        "guid: 10000000000000000000000000000001\nMonoImporter:\n  executionOrder: 0\n",
    );
    let definition = write_graph_file(&project, "Assets/Game/Game.asmdef", r#"{"name":"Game"}"#);
    let definition_meta = write_graph_file(
        &project,
        "Assets/Game/Game.asmdef.meta",
        "guid: 20000000000000000000000000000001\n",
    );
    let mut inputs = NilMap::make();
    for path in [&source, &meta, &definition, &definition_meta] {
        inputs.insert(path, hash(path));
    }
    let mut compiler = Compiler {
        project,
        sources: NilMap::from([(source.clone(), inputs.at(&source).to_string())]),
        configuration: NilMap::from([(meta.clone(), inputs.at(&meta).to_string())]),
        configs: NilMap::from([(
            "Game",
            AssemblyConfig {
                name: "Game".into(),
                sources: NilVec::from([source.clone()]),
                ..Default::default()
            },
        )]),
        current_dlls: NilMap::from([("Game", "Game.dll".to_string())]),
        manifest: CompileManifest {
            compiler_inputs: inputs,
            assemblies: NilVec::from([CompileAssembly {
                name: "Game".into(),
                sources: NilVec::from([source.clone()]),
                ..Default::default()
            }]),
            scripts: NilVec::from([CompileScript {
                guid: "10000000000000000000000000000001".into(),
                path: source,
                assembly: "Game".into(),
                ..Default::default()
            }]),
            ..Default::default()
        },
        ..Default::default()
    };
    compiler.collect_configuration_inputs();
    (guard, compiler)
}

#[test]
fn source_graph_extends_known_assembly_without_changing_accepted_state() {
    let (_guard, compiler) = source_extension_fixture();
    let added = write_graph_file(
        &compiler.project,
        "Assets/Game/New/Nested.cs",
        "namespace A { class Nested {} }",
    );
    let meta = write_graph_file(
        &compiler.project,
        "Assets/Game/New/Nested.cs.meta",
        "guid: 10000000000000000000000000000002\nMonoImporter:\n  executionOrder: 11\n",
    );
    write_graph_file(
        &compiler.project,
        "Assets/Game/New.meta",
        "guid: 30000000000000000000000000000001\nfolderAsset: yes\n",
    );
    let before = crate::util::marshal(&compiler.manifest).unwrap();
    let candidate = compiler
        .source_graph_candidate(&[added.clone(), meta.clone()])
        .unwrap();
    assert!(
        candidate
            .graph_source_assemblies()
            .get(&added)
            .map(String::as_str)
            == Some("Game")
            && candidate.manifest.scripts.len() == 2
            && !candidate.configuration.at(&meta).is_empty()
            && candidate.configs["Game"].sources.len() == 2,
        "new authored source/GUID was not added to the exact existing assembly: {:?}",
        candidate.manifest
    );
    let after = crate::util::marshal(&compiler.manifest).unwrap();
    assert!(
        before == after
            && compiler.configuration.at(&meta).is_empty()
            && compiler.configs["Game"].sources.len() == 1,
        "candidate discovery mutated accepted compiler graph"
    );
    candidate.check_configuration(&[]).unwrap();
    write_graph_file(
        &compiler.project,
        "Assets/Game/AppearedDuringCompile.cs",
        "class AppearedDuringCompile {}",
    );
    assert!(
        candidate.check_configuration(&[]).is_err(),
        "source appearing after snapshot could be omitted from a published Player"
    );
}

#[test]
fn source_graph_uses_guid_assembly_reference_and_default_scopes() {
    let (_guard, mut compiler) = source_extension_fixture();
    let project = compiler.project.clone();
    let reference = write_graph_file(
        &project,
        "Assets/Shared/Shared.asmref",
        r#"{"reference":"GUID:20000000000000000000000000000001"}"#,
    );
    let meta = write_graph_file(
        &project,
        "Assets/Shared/Shared.asmref.meta",
        "guid: 20000000000000000000000000000002\n",
    );
    for path in [&reference, &meta] {
        compiler.configuration.insert(path, hash(path));
        compiler.manifest.compiler_inputs.insert(path, hash(path));
    }
    let added = write_graph_file(&project, "Assets/Shared/Added.cs", "class Added {}");
    write_graph_file(
        &project,
        "Assets/Shared/Added.cs.meta",
        "guid: 10000000000000000000000000000002\n",
    );
    let candidate = compiler.source_graph_candidate(&[]).unwrap();
    assert_eq!(
        candidate
            .graph_source_assemblies()
            .get(&added)
            .map(String::as_str),
        Some("Game"),
        "GUID asmref did not resolve the nearest exact prepared assembly"
    );
    for (name, dll) in [
        ("Assembly-CSharp", "default.dll"),
        ("Assembly-CSharp-firstpass", "firstpass.dll"),
    ] {
        compiler.configs.insert(
            name,
            AssemblyConfig {
                name: name.into(),
                ..Default::default()
            },
        );
        compiler.current_dlls.insert(name, dll.to_string());
    }
    let roots = compiler.assembly_source_roots().unwrap();
    for (path, expected) in [
        ("Assets/Other/Default.cs", "Assembly-CSharp"),
        ("Assets/Plugins/Plugin.cs", "Assembly-CSharp-firstpass"),
        ("Assets/Game/Sub/Game.cs", "Game"),
    ] {
        let actual = compiler.source_assembly(&join(&[&project, path]), &roots);
        assert_eq!(actual.unwrap(), expected, "source scope {path}");
    }
}

#[test]
fn source_graph_rejects_unsupported_or_conflicting_additions() {
    for kind in [
        "missing-meta",
        "duplicate-script-guid",
        "duplicate-asset-guid",
        "new-asmdef",
        "changed-asmdef",
        "missing-slot",
        "symlink",
        "package-source",
    ] {
        let (_guard, mut compiler) = source_extension_fixture();
        let project = compiler.project.clone();
        let source = write_graph_file(&project, "Assets/Game/Added.cs", "class Added {}");
        write_graph_file(
            &project,
            "Assets/Game/Added.cs.meta",
            "guid: 10000000000000000000000000000002\n",
        );
        let mut changed = vec![source.clone()];
        match kind {
            "missing-meta" => fs::remove_file(format!("{source}.meta")).unwrap(),
            "duplicate-script-guid" => {
                write_graph_file(
                    &project,
                    "Assets/Game/Added.cs.meta",
                    "guid: 10000000000000000000000000000001\n",
                );
            }
            "duplicate-asset-guid" => {
                write_graph_file(
                    &project,
                    "Assets/Scene.unity.meta",
                    "guid: 10000000000000000000000000000002\n",
                );
            }
            "new-asmdef" => {
                write_graph_file(&project, "Assets/Game/Sub/New.asmdef", r#"{"name":"New"}"#);
            }
            "changed-asmdef" => {
                write_graph_file(&project, "Assets/Game/Game.asmdef", r#"{"name":"Changed"}"#);
            }
            "missing-slot" => {
                compiler.current_dlls.remove("Game");
            }
            "symlink" => {
                fs::remove_file(&source).unwrap();
                symlink(join(&[&project, "Assets/Game/Known.cs"]), &source).unwrap();
            }
            _ => changed.push("Packages/com.example/Added.cs".into()),
        }
        assert!(
            compiler.source_graph_candidate(&changed).is_err(),
            "unsupported {kind} produced a graph"
        );
    }
}

#[test]
fn compiler_assembly_slot_extension_preserves_host_flags() {
    let (_guard, directory) = temp();
    let filename = write_graph_file(
        &directory,
        "ScriptingAssemblies.json",
        r#"{"names":["UnityEngine.dll","Bridge.dll"],"types":[2,16],"hostFlag":{"preserve":true}}"#,
    );
    register_compiler_assembly_slots(
        &filename,
        &[assembly("Game", &[], &[]), assembly("Bridge", &[], &[])],
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&filename).unwrap(),
        "{\"hostFlag\":{\"preserve\":true},\"names\":[\"UnityEngine.dll\",\"Bridge.dll\",\"Game.dll\"],\"types\":[2,16,16]}\n",
        "host native assembly flags were overwritten"
    );
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct DeclarationProof {
    player: BTreeMap<String, Vec<String>>,
    editor: BTreeMap<String, Vec<String>>,
}

#[test]
fn compiler_player_source_declarations_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set the pinned Unity SDK for actual C# Player preprocessing proof",
    ) else {
        return;
    };
    let (_guard, cache) = temp();
    let mut compiler = Compiler {
        editor,
        cache: cache.clone(),
        ..Default::default()
    };
    compiler.initialize_sdk().unwrap();
    let ctx = background();
    let helper = compiler.prepare_lifecycle_helper(&ctx).unwrap();
    let source = write_graph_file(
        &cache,
        "Conditional.cs",
        "namespace Package.@event;\n#if UNITY_EDITOR\ninternal class DownloadableSample {}\n#elif DEVELOPMENT_BUILD && FEATURE\ninternal class RuntimeDebug {}\n#else\ninternal class ReleaseRuntime {}\n#endif\ninternal class Owner<T> { internal class Inner {} }\n",
    );
    let request = join(&[&cache, "declarations.json"]);
    atomic_json(
        &request,
        &json!([{"assemblyName": "Package.Runtime", "defines": ["DEVELOPMENT_BUILD", "FEATURE"],
            "editorDefines": ["UNITY_EDITOR"], "sources": [{"path": source, "originalPath": source}]}]),
    )
    .unwrap();
    let output = compiler_command(
        &ctx,
        &cache,
        &compiler.dotnet,
        &[&helper, "--source-type-declarations", &request],
    )
    .unwrap();
    let proof: BTreeMap<String, DeclarationProof> = serde_json::from_slice(&output).unwrap();
    let proof = &proof["Package.Runtime"];
    assert_eq!(
        proof.player[&source],
        [
            "Package.event.Owner`1",
            "Package.event.Owner`1+Inner",
            "Package.event.RuntimeDebug"
        ],
        "Player preprocessing/nested/file-scoped namespace proof incorrect"
    );
    assert_eq!(
        proof.editor[&source],
        [
            "Package.event.DownloadableSample",
            "Package.event.Owner`1",
            "Package.event.Owner`1+Inner"
        ],
        "Editor conditional declaration proof incorrect"
    );
}

#[test]
fn compiler_new_mono_behaviour_and_restart_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set UME_LIFECYCLE_SDK to the matching Unity executable; SDK only, no Editor or Player starts",
    ) else {
        return;
    };
    let (_guard, project) = temp();
    write_graph_file(
        &project,
        "ProjectSettings/ProjectVersion.txt",
        &format!("m_EditorVersion: {UNITY_VERSION}\n"),
    );
    write_graph_file(&project, "Packages/manifest.json", r#"{"dependencies":{}}"#);
    let mut sdk_paths = Compiler {
        editor: editor.clone(),
        ..Default::default()
    };
    sdk_paths.initialize_sdk().unwrap();
    let ctx = background();
    let compile = |dll: &str, source: &str, extra: &[&str]| {
        let filename = write_graph_file(
            &project,
            &format!("Temp/initial/{}.cs", sysutil::base(dll)),
            source,
        );
        sys::mkdir_all(&sysutil::dir(dll)).unwrap();
        let mut references: Vec<String> = sdk_paths.base_references.to_vec();
        references.extend(sdk_paths.engine_references.iter().cloned());
        references.extend(extra.iter().map(|path| path.to_string()));
        let mut arguments = vec![
            sdk_paths.csc.clone(),
            "-nologo".into(),
            "-target:library".into(),
            "-nostdlib+".into(),
            "-debug:portable".into(),
            format!("-out:{dll}"),
        ];
        for reference in &deduplicate_references(&references) {
            arguments.push(format!("-reference:{reference}"));
        }
        arguments.push(filename);
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        compiler_command(&ctx, &project, &sdk_paths.dotnet, &arguments).unwrap();
    };
    let player = write_graph_file(
        &project,
        "Temp/native-host/Host.x86_64",
        "SDK fixture executable, not launched",
    );
    fs::set_permissions(&player, fs::Permissions::from_mode(0o700)).unwrap();
    let managed = join(&[&sysutil::dir(&player), "Host_Data/Managed"]);
    let bridge = join(&[&managed, "UnityMicroEditor.dll"]);
    let bridge_text = r#"using System; using UnityEngine; namespace UnityMicroEditor.Raw {
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawLifecycleInstrumentedAttribute : Attribute { public RawLifecycleInstrumentedAttribute(int version) {} }
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawScriptableObjectInstrumentedAttribute : Attribute { public RawScriptableObjectInstrumentedAttribute(int version) {} }
public static class RawLifecycle {
 public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
 public static bool ShouldCollect(MonoBehaviour target, Phase phase) => false;
 public static bool ShouldCollect(ScriptableObject target, Phase phase) => false;
 public static void Enqueue(MonoBehaviour target, Phase phase, Action callback, int order) {}
 public static void Enqueue(ScriptableObject target, Phase phase, Action callback, int order) {}
}}"#;
    compile(&bridge, bridge_text, &[]);
    let package_text = r#"namespace Package.First { public class Known : UnityEngine.MonoBehaviour {} }
namespace Package.Second { [UnityEngine.DefaultExecutionOrder(-5000)] public class Known : UnityEngine.MonoBehaviour {
 protected virtual void Awake() { } protected virtual void OnDestroy() { }
} }"#;
    let package_directory = "Library/PackageCache/com.test.runtime@fixture";
    let package_root = join(&[&project, package_directory]);
    let known_package = format!("{package_directory}/Known.cs");
    let package_source = write_graph_file(&project, &known_package, package_text);
    let package_meta = write_graph_file(
        &project,
        &format!("{known_package}.meta"),
        "guid: 10000000000000000000000000000005\n",
    );
    let editor_only_text =
        "namespace Package { internal class DownloadableSample : UnityEngine.ScriptableObject {} }";
    let editor_only_source = write_graph_file(
        &project,
        &format!("{package_directory}/DownloadableSample.cs"),
        &format!("#if UNITY_EDITOR\n{editor_only_text}\n#endif\n"),
    );
    let editor_only_meta = write_graph_file(
        &project,
        &format!("{package_directory}/DownloadableSample.cs.meta"),
        "guid: 10000000000000000000000000000006\n",
    );
    let package_host = join(&[&managed, "Package.Runtime.dll"]);
    let package_editor = join(&[&project, "Library/ScriptAssemblies/Package.Runtime.dll"]);
    compile(&package_host, package_text, &[]);
    compile(
        &package_editor,
        &format!("{package_text}{editor_only_text}"),
        &[],
    );
    write_graph_file(
        &project,
        "Temp/native-host/Host_Data/ScriptingAssemblies.json",
        r#"{"names":["UnityMicroEditor.dll","Package.Runtime.dll"],"types":[16,16],"fixtureFlag":true}"#,
    );
    let known_text =
        "namespace Baseline { public class Known : Package.Second.Known { public int seed = 3; } }";
    let known = write_graph_file(&project, "Assets/Known.cs", known_text);
    write_graph_file(
        &project,
        "Assets/Known.cs.meta",
        "guid: 10000000000000000000000000000001\nMonoImporter:\n  executionOrder: 0\n",
    );
    let consumer_text = "public class Consumer { public Baseline.Known target; }";
    let consumer = write_graph_file(&project, "Assets/Consumer/Consumer.cs", consumer_text);
    write_graph_file(
        &project,
        "Assets/Consumer/Consumer.cs.meta",
        "guid: 10000000000000000000000000000002\n",
    );
    write_graph_file(
        &project,
        "Assets/Consumer/Consumer.asmdef",
        r#"{"name":"Consumer","references":["Assembly-CSharp"]}"#,
    );
    write_graph_file(
        &project,
        "Assets/Consumer/Consumer.asmdef.meta",
        "guid: 20000000000000000000000000000001\n",
    );
    let known_dll = join(&[&project, "Library/ScriptAssemblies/Assembly-CSharp.dll"]);
    let consumer_dll = join(&[&project, "Library/ScriptAssemblies/Consumer.dll"]);
    compile(&known_dll, known_text, &[&bridge, &package_host]);
    compile(
        &consumer_dll,
        consumer_text,
        &[&known_dll, &bridge, &package_host],
    );
    let graph_assembly =
        |name: &str, source: &str, path: &str, references: &[&str]| CompilerGraphAssembly {
            config: AssemblyConfig {
                name: name.into(),
                sources: NilVec::from([source.to_string()]),
                defines: strs(&["UNITY_6000_6"]),
                ..Default::default()
            },
            path: path.into(),
            references: strs(references),
            ..Default::default()
        };
    let script =
        |guid: &str, path: &str, assembly: &str, type_name: &str, host: bool| CompileScript {
            guid: guid.into(),
            path: path.into(),
            assembly: assembly.into(),
            type_name: type_name.into(),
            host_provided: host,
        };
    let mut graph = CompilerGraphExport {
        schema_version: 1,
        project: project.clone(),
        unity: editor.clone(),
        assemblies: NilVec::from([
            graph_assembly("Consumer", &consumer, &consumer_dll, &["Assembly-CSharp"]),
            graph_assembly(
                "Assembly-CSharp",
                &known,
                &known_dll,
                &["UnityMicroEditor", "Package.Runtime"],
            ),
        ]),
        scripts: NilVec::from([
            script(
                "10000000000000000000000000000001",
                &known,
                "Assembly-CSharp",
                "Baseline.Known",
                false,
            ),
            script(
                "10000000000000000000000000000002",
                &consumer,
                "Consumer",
                "Consumer",
                false,
            ),
            script(
                "10000000000000000000000000000006",
                &editor_only_source,
                "Package.Runtime",
                "Package.DownloadableSample",
                true,
            ),
            script(
                "10000000000000000000000000000005",
                &package_source,
                "Package.Runtime",
                "Package.Second.Known",
                true,
            ),
        ]),
        source_roots: NilVec::from([CompilerSourceRoot {
            path: "Packages/com.test.runtime".into(),
            source: package_root,
        }]),
        compiler_inputs: NilMap::make(),
        ..Default::default()
    };
    for path in [
        &package_source,
        &package_meta,
        &editor_only_source,
        &editor_only_meta,
    ] {
        graph.compiler_inputs.insert(path, hash(path));
    }
    graph.host_assemblies = NilVec::from([CompilerHostAssembly {
        name: "Package.Runtime".into(),
        path: package_editor.clone(),
        sha256: hash(&package_editor),
        sources: NilVec::from([package_source.clone(), editor_only_source.clone()]),
        defines: strs(&["UNITY_6000_6", "DEVELOPMENT_BUILD"]),
        editor_defines: strs(&["UNITY_EDITOR", "UNITY_EDITOR_LINUX"]),
        ..Default::default()
    }]);
    let graph_file = join(&[&project, "Temp/export.json"]);
    let mut baseline = join(&[&project, "Temp/prepared/compile.json"]);
    atomic_json(&graph_file, &graph).unwrap();
    // A public Editor export can reference a newer bridge than the prepared
    // native host. Never conceal this deployment drift with the Editor DLL.
    let editor_bridge = join(&[&project, "Library/ScriptAssemblies/UnityMicroEditor.dll"]);
    let api = "namespace UnityMicroEditor { public static class AgentRuntimeCommands { public static int Register() => 2; public sealed class Token {} } internal class EditorDetail { public class Hidden {} } }";
    compile(&editor_bridge, &format!("{bridge_text}{api}"), &[]);
    let old_bridge_hash = hash(&bridge);
    let prepare = |baseline: &str| {
        prepare_compiler_graph(&ctx, &project, &editor, &player, &graph_file, baseline)
    };
    let error = prepare(&baseline).unwrap_err().to_string();
    assert!(
        error.contains("native host runtime bridge is stale")
            && error.contains("AgentRuntimeCommands"),
        "stale Player API was not rejected before instrumentation: {error}"
    );
    assert!(
        sys::absent(&baseline),
        "stale runtime API published a successful compiler baseline"
    );
    assert_eq!(
        hash(&bridge),
        old_bridge_hash,
        "stale host was overwritten with the Editor bridge"
    );
    // Matching public types allow different implementation details and bodies.
    compile(
        &bridge,
        &format!(
            "{bridge_text}namespace UnityMicroEditor {{ public static class AgentRuntimeCommands {{ public static int Register() => 1; public sealed class Token {{}} }} }}"
        ),
        &[],
    );
    let missing_runtime =
        "namespace Package { public class EditorOnlyMissing : UnityEngine.MonoBehaviour {} }";
    let last = graph.scripts.len() - 1;
    let republish = |graph: &mut CompilerGraphExport, type_name: Option<&str>| {
        graph
            .compiler_inputs
            .insert(&package_source, hash(&package_source));
        graph.host_assemblies[0].sha256 = hash(&package_editor);
        if let Some(type_name) = type_name {
            graph.scripts[last].type_name = type_name.into();
        }
        atomic_json(&graph_file, graph).unwrap();
    };
    write_graph_file(
        &project,
        &known_package,
        &format!("{package_text}{missing_runtime}"),
    );
    compile(
        &package_editor,
        &format!("{package_text}{editor_only_text}{missing_runtime}"),
        &[],
    );
    republish(&mut graph, Some("Package.EditorOnlyMissing"));
    let error = prepare(&baseline).unwrap_err().to_string();
    assert!(
        error.contains("lacks exported MonoScript type Package.EditorOnlyMissing"),
        "missing native package type was accepted or concealed with Editor DLL: {error}"
    );
    // A verified native package does not make its private lifecycle messages
    // callable. Never bypass this boundary with reflection or a lost callback.
    let private_package = package_text.replace("protected virtual void", "private void");
    write_graph_file(&project, &known_package, &private_package);
    compile(&package_host, &private_package, &[]);
    compile(
        &package_editor,
        &format!("{private_package}{editor_only_text}"),
        &[],
    );
    republish(&mut graph, Some("Package.Second.Known"));
    let error = prepare(&baseline).unwrap_err().to_string();
    assert!(
        error.contains("inaccessible; a public/protected bridge is required"),
        "private native callback was accepted or silently dropped: {error}"
    );
    write_graph_file(&project, &known_package, package_text);
    compile(&package_host, package_text, &[]);
    compile(
        &package_editor,
        &format!("{package_text}{editor_only_text}"),
        &[],
    );
    republish(&mut graph, None);
    let prepared = prepare(&baseline).unwrap();
    assert_eq!(
        prepared.compiler.assemblies[0].name, "Assembly-CSharp",
        "exported dependency graph was not ordered"
    );
    assert!(
        !prepared.compiler.scripts.iter().any(|script| {
            script.path == editor_only_source || script.type_name == "Package.DownloadableSample"
        }),
        "inactive UNITY_EDITOR-only package ScriptableObject survived Player preprocessing"
    );
    assert!(
        !prepared
            .compiler
            .compiler_inputs
            .at(&editor_only_source)
            .is_empty()
            && !prepared
                .compiler
                .compiler_inputs
                .at(&editor_only_meta)
                .is_empty(),
        "inactive Editor script lost its configuration drift fingerprints"
    );
    baseline = join(&[&project, "Temp/scene-prepared/current.json"]);
    sys::mkdir_all(&sysutil::dir(&baseline)).unwrap();
    atomic_json(&baseline, &prepared).unwrap();
    let cache = join(&[&project, "Temp/revisions"]);
    let mut accepted =
        Compiler::new(&project, &baseline, &prepared.player, &editor, &cache).unwrap();
    // As a watched session compiles, unless UME_COMPILER_COLD=1.
    accepted.serve(&cache);
    assert_eq!(
        accepted.configs["Assembly-CSharp"].native_base.to_vec(),
        ["Package.Runtime"],
        "restart lost the verified native package base allowlist"
    );
    for phase in ["Awake", "OnDestroy"] {
        assert!(
            accepted.proofs["Assembly-CSharp"]
                .methods
                .iter()
                .any(|callback| {
                    callback.type_name == "Baseline.Known"
                        && callback.phase == phase
                        && callback.synthetic
                        && callback.order == -5000
                }),
            "protected inherited native {phase} lacks delayed source-owned wrapper proof"
        );
    }
    let types = accepted.script_type_names(&ctx).unwrap();
    assert_eq!(
        types.at("10000000000000000000000000000005"),
        "Package.Second.Known",
        "public package GUID/type descriptor lost with ambiguous filename: {types:?}"
    );
    assert!(
        matches!(accepted.compile(&ctx, None), Ok(None)),
        "fresh prepared graph recompiled"
    );
    let new_guid = "10000000000000000000000000000003";
    let added_text = r#"namespace AgentAdded { public class Known : UnityEngine.MonoBehaviour { public int revision = 7; public string label = "agent"; void Awake() { revision++; } } }"#;
    let added = write_graph_file(&project, "Assets/NewNamespace/Known.cs", added_text);
    write_graph_file(
        &project,
        "Assets/NewNamespace/Known.cs.meta",
        &format!("guid: {new_guid}\nMonoImporter:\n  executionOrder: 12\n"),
    );
    let update = accepted
        .compile(&ctx, Some(&[added.clone(), format!("{added}.meta")]))
        .unwrap()
        .expect("new API did not rebuild assembly and consumer");
    assert!(
        update.compiled_assemblies.to_vec() == ["Assembly-CSharp", "Consumer"]
            && update.source_hashes.len() == 3,
        "new API did not rebuild assembly and consumer: {update:?}"
    );
    let types = accepted.script_type_names(&ctx).unwrap();
    assert!(
        types.at(new_guid) == "AgentAdded.Known"
            && types.at("10000000000000000000000000000001") == "Baseline.Known",
        "exact per-source GUID/type binding lost with same class filename in two namespaces: {types:?}"
    );
    let proof = accepted.proofs["Assembly-CSharp"].clone();
    assert!(
        proof.methods.iter().any(|callback| {
            callback.source == added
                && callback.type_name == "AgentAdded.Known"
                && callback.phase == "Awake"
                && callback.order == 12
        }),
        "new authored MonoBehaviour lacks lifecycle/execution-order proof: {proof:?}"
    );
    // Publishing a new baseline must carry the accepted DLL SHA, expanded
    // source set, GUID/type binding and proof rather than the initial plan.
    let rebased = accepted.prepared_manifest(&ctx).unwrap();
    let rebased_file = join(&[&project, "Temp/rebased/current.json"]);
    sys::mkdir_all(&sysutil::dir(&rebased_file)).unwrap();
    atomic_json(
        &rebased_file,
        &CompilerPreparationResult {
            schema_version: 1,
            player: update.player.clone(),
            compiler: rebased,
        },
    )
    .unwrap();
    let mut rebased_compiler = Compiler::new(
        &project,
        &rebased_file,
        &update.player,
        &editor,
        &join(&[&project, "Temp/rebased-revisions"]),
    )
    .expect("published accepted compiler baseline cannot restart");
    assert!(
        matches!(rebased_compiler.compile(&ctx, None), Ok(None)),
        "published accepted sources unexpectedly recompiled"
    );
    assert_eq!(
        rebased_compiler
            .script_type_names(&ctx)
            .unwrap()
            .at(new_guid),
        "AgentAdded.Known",
        "published compiler baseline lost added GUID/type"
    );
    let accepted_dll = accepted.current_dlls.at("Assembly-CSharp").to_string();
    let untampered = fs::read(&accepted_dll).unwrap();
    let mut tampered = untampered.clone();
    tampered.push(0);
    sys::write(&accepted_dll, &tampered).unwrap();
    assert!(
        accepted
            .prepared_manifest(&ctx)
            .is_err_and(|error| error.to_string().contains("accepted DLL changed")),
        "changed accepted DLL was silently rebound into a prepared baseline"
    );
    sys::write(&accepted_dll, &untampered).unwrap();
    let mut restarted =
        Compiler::new(&project, &baseline, &prepared.player, &editor, &cache).unwrap();
    restarted.serve(&cache);
    let reused = restarted
        .compile(&ctx, None)
        .unwrap()
        .expect("restart lost verified extended source graph");
    assert!(
        reused.cache_reused
            && reused.player == update.player
            && reused.compiled_assemblies.is_empty(),
        "restart lost verified extended source graph: {reused:?}"
    );
    assert_eq!(
        restarted.script_type_names(&ctx).unwrap().at(new_guid),
        "AgentAdded.Known",
        "restart lost added script GUID"
    );
    let (old_player, old_scripts) = (restarted.player.clone(), restarted.manifest.scripts.len());
    let broken = write_graph_file(
        &project,
        "Assets/Broken.cs",
        "public class Broken { invalid C sharp }",
    );
    write_graph_file(
        &project,
        "Assets/Broken.cs.meta",
        "guid: 10000000000000000000000000000004\n",
    );
    assert!(
        restarted
            .compile(&ctx, Some(std::slice::from_ref(&broken)))
            .is_err(),
        "broken new C# published"
    );
    assert!(
        restarted.player == old_player
            && restarted.manifest.scripts.len() == old_scripts
            && !restarted.graph_source_assemblies().contains_key(&broken),
        "failed extension replaced accepted Player or source graph"
    );
    fs::remove_file(&broken).unwrap();
    fs::remove_file(format!("{broken}.meta")).unwrap();
    write_graph_file(
        &project,
        "Assets/NewNamespace/Known.cs",
        &added_text.replacen("AgentAdded", "RenamedNamespace", 1),
    );
    assert!(
        restarted
            .compile(&ctx, Some(std::slice::from_ref(&added)))
            .is_err_and(|error| error.to_string().contains("removed or renamed")),
        "accepted new script lost serialized type identity"
    );
    assert_eq!(
        restarted.player, old_player,
        "renamed serialized type replaced accepted Player"
    );

    // native-only-fresh-and-restart
    fs::rename(
        join(&[&project, "Assets"]),
        join(&[&project, "Temp/SavedGameAssets"]),
    )
    .unwrap();
    sys::mkdir(&join(&[&project, "Assets"])).unwrap();
    graph.assemblies = NilVec::nil();
    let mut native_scripts = NilVec::nil();
    native_scripts.append(
        graph
            .scripts
            .iter()
            .filter(|script| script.host_provided)
            .cloned(),
    );
    graph.scripts = native_scripts;
    atomic_json(&graph_file, &graph).unwrap();
    let native_baseline = join(&[&project, "Temp/native-prepared/compile.json"]);
    let native_prepared = prepare(&native_baseline).unwrap();
    assert!(
        native_prepared.compiler.native_only
            && native_prepared.compiler.assemblies.is_empty()
            && valid_sha256(&native_prepared.compiler.native_slots_sha),
        "empty game graph lost verified native-only provenance: {:?}",
        native_prepared.compiler
    );
    let native_cache = join(&[&project, "Temp/native-revisions"]);
    let mut native_compiler = Compiler::new(
        &project,
        &native_baseline,
        &native_prepared.player,
        &editor,
        &native_cache,
    )
    .unwrap();
    assert!(
        matches!(native_compiler.compile(&ctx, None), Ok(None)),
        "fresh native-only graph recompiled"
    );
    assert_eq!(
        native_compiler
            .script_type_names(&ctx)
            .unwrap()
            .at("10000000000000000000000000000005"),
        "Package.Second.Known",
        "native-only package GUID/type identity lost"
    );
    native_compiler.manifest.scripts = NilVec::nil();
    assert!(
        native_compiler
            .script_type_names(&ctx)
            .is_ok_and(|types| types.is_empty()),
        "empty native-only type inventory failed"
    );
    let first = write_graph_file(
        &project,
        "Assets/First.cs",
        "public class First : UnityEngine.MonoBehaviour {}",
    );
    write_graph_file(
        &project,
        "Assets/First.cs.meta",
        "guid: 10000000000000000000000000000007\n",
    );
    assert!(
        native_compiler
            .compile(&ctx, Some(std::slice::from_ref(&first)))
            .is_err(),
        "first gameplay script silently created an unprepared native assembly slot"
    );
    fs::remove_file(&first).unwrap();
    fs::remove_file(format!("{first}.meta")).unwrap();
    let slots = join(&[
        &sysutil::dir(&native_prepared.player),
        "Host_Data/ScriptingAssemblies.json",
    ]);
    sys::write(
        &slots,
        br#"{"names":["UnityMicroEditor.dll"],"types":[16]}"#,
    )
    .unwrap();
    assert!(
        Compiler::new(
            &project,
            &native_baseline,
            &native_prepared.player,
            &editor,
            &native_cache
        )
        .is_err_and(|error| error
            .to_string()
            .contains("native-only host registrations differ")),
        "native-only registration drift accepted at restart"
    );
}

#[test]
fn metadata_guid_index_streams_large_model_settings() {
    let guid = "A1000000000000000000000000000001";
    // The colliding text starts at a reader-fragment boundary inside one
    // importer line. Neither this nor an indented real PPtr is an asset GUID.
    let prefix = "  curve: ";
    let long_line = format!(
        "{prefix}{}guid: b1000000000000000000000000000001{}",
        "x".repeat(METADATA_GUID_BUFFER_SIZE - prefix.len()),
        "y".repeat(2 * 1024 * 1024)
    );
    let data = format!(
        "fileFormatVersion: 2\r\nguid: {guid}\r\nModelImporter:\r\n{long_line}\r\n  lastHumanDescriptionAvatarSource: {{fileID: 9000000, guid: c1000000000000000000000000000001, type: 3}}\r\n"
    );
    let (_guard, project) = temp();
    let filename = write_graph_file(&project, "Assets/Model.fbx.meta", &data);
    let before = fs::metadata(&filename).unwrap().modified().unwrap();
    let identity = MetadataGuidIndex::new().read(&filename).unwrap();
    assert_eq!(
        identity,
        MetadataGuidIdentity {
            guid: guid.to_lowercase(),
            sha256: sha256(data.as_bytes()),
            bytes: data.len() as i64,
        },
        "incorrect complete-file identity"
    );
    assert!(
        fs::read(&filename).unwrap() == data.as_bytes()
            && fs::metadata(&filename).unwrap().modified().unwrap() == before,
        "GUID indexing changed authored importer metadata"
    );
}

#[test]
fn metadata_guid_index_rejects_unsafe_identities_and_files() {
    for kind in [
        "duplicate-late",
        "malformed",
        "missing",
        "zero",
        "long-guid",
        "oversize",
        "symlink",
        "parent-symlink",
        "directory",
    ] {
        let (_guard, project) = temp();
        let model = "Assets/Model.fbx.meta";
        let mut filename = write_graph_file(
            &project,
            model,
            "guid: a1000000000000000000000000000001\nModelImporter:\n",
        );
        match kind {
            "duplicate-late" => {
                write_graph_file(
                    &project,
                    model,
                    &format!(
                        "guid: a1000000000000000000000000000001\n  settings: {}\nguid: a1000000000000000000000000000002\n",
                        "x".repeat(2 * 1024 * 1024)
                    ),
                );
            }
            "malformed" => {
                write_graph_file(&project, model, "guid: invalid\n");
            }
            "missing" => {
                write_graph_file(
                    &project,
                    model,
                    "ModelImporter:\n  reference: {guid: a1000000000000000000000000000001}\n",
                );
            }
            "zero" => {
                write_graph_file(&project, model, "guid: 00000000000000000000000000000000\n");
            }
            "long-guid" => {
                write_graph_file(
                    &project,
                    model,
                    &format!("guid: {}\n", "a".repeat(METADATA_GUID_BUFFER_SIZE)),
                );
            }
            "oversize" => fs::OpenOptions::new()
                .write(true)
                .open(&filename)
                .unwrap()
                .set_len(METADATA_GUID_MAXIMUM as u64 + 1)
                .unwrap(),
            "symlink" => {
                filename = join(&[&project, "Assets/Alias.fbx.meta"]);
                symlink(join(&[&project, model]), &filename).unwrap();
            }
            "parent-symlink" => {
                symlink(join(&[&project, "Assets"]), join(&[&project, "Alias"])).unwrap();
                filename = join(&[&project, "Alias/Model.fbx.meta"]);
            }
            _ => {
                filename = join(&[&project, "Assets/Folder.meta"]);
                sys::mkdir(&filename).unwrap();
            }
        }
        assert!(
            MetadataGuidIndex::new().read(&filename).is_err(),
            "unsafe {kind} accepted"
        );
    }
}

struct BrokenReader;
impl Read for BrokenReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

#[test]
fn metadata_guid_index_checks_complete_input_and_can_reuse_buffer() {
    let mut index = MetadataGuidIndex::new();
    assert!(
        index
            .scan(io::Cursor::new("guid: a1000000000000000000000000000001\n").chain(BrokenReader))
            .is_err(),
        "read failure after a valid GUID was accepted"
    );
    for guid in [
        "a1000000000000000000000000000001",
        "b1000000000000000000000000000002",
    ] {
        let identity = index
            .scan(io::Cursor::new(format!("guid: {guid}")))
            .unwrap();
        assert_eq!(identity.guid, guid, "reused scanner retained earlier GUID");
    }
    let oversize =
        io::Cursor::new("guid: a1000000000000000000000000000001\nModelImporter:\n  value: ")
            .chain(io::repeat(0).take(METADATA_GUID_MAXIMUM as u64));
    assert!(
        index.scan(oversize).is_err(),
        "streaming source exceeded its hard byte bound"
    );
}

#[test]
fn source_graph_addition_with_large_metadata_checks_project_wide_guid_collisions() {
    for collision in [false, true] {
        let (_guard, compiler) = source_extension_fixture();
        let source = write_graph_file(&compiler.project, "Assets/Game/Added.cs", "class Added {}");
        let guid = "10000000000000000000000000000002";
        write_graph_file(
            &compiler.project,
            "Assets/Game/Added.cs.meta",
            &format!("guid: {guid}\n"),
        );
        let asset_guid = if collision {
            guid
        } else {
            "30000000000000000000000000000001"
        };
        write_graph_file(
            &compiler.project,
            "Assets/Model.fbx.meta",
            &format!(
                "fileFormatVersion: 2\r\nguid: {asset_guid}\r\nModelImporter:\r\n  extraUserProperties: {}\r\n  nestedReference: {{guid: {guid}}}\r\n",
                "x".repeat(2 * 1024 * 1024)
            ),
        );
        let accepted: CompileManifest =
            unmarshal(&crate::util::marshal(&compiler.manifest).unwrap()).unwrap();
        let candidate = compiler.source_graph_candidate(&[source]);
        if collision {
            assert!(
                candidate
                    .is_err_and(|error| error.to_string().contains("collides with asset metadata")),
                "missed large model GUID collision"
            );
        } else {
            assert!(
                candidate.is_ok_and(|candidate| candidate.manifest.scripts.len() == 2),
                "valid large model prevented new MonoScript discovery"
            );
        }
        assert_eq!(
            accepted, compiler.manifest,
            "candidate scan changed the accepted compiler graph"
        );
    }
}

#[test]
fn metadata_guid_actual_model_read_only() {
    let Some(filename) = sdk(
        "UME_METADATA_PARITY_FILE",
        "set UME_METADATA_PARITY_FILE for actual large Unity model metadata",
    ) else {
        return;
    };
    let identity = MetadataGuidIndex::new().read(&filename).unwrap();
    assert!(
        identity.bytes > 1024 * 1024,
        "actual fixture does not exercise former 1 MiB metadata limit"
    );
    assert_eq!(
        hash(&filename),
        identity.sha256,
        "actual metadata complete-file SHA differs"
    );
}

#[test]
fn metadata_guid_actual_project_index_read_only() {
    let Some(project) = sdk(
        "UME_METADATA_PARITY_PROJECT",
        "set UME_METADATA_PARITY_PROJECT for actual project-wide GUID indexing",
    ) else {
        return;
    };
    let mut index = MetadataGuidIndex::new();
    let (mut count, mut large) = (0, 0);
    sys::walk_dir(&join(&[&project, "Assets"]), &mut |entry| {
        if entry.is_dir() || !entry.path.ends_with(".meta") {
            return Ok(sys::Walk::Continue);
        }
        let identity = index.read(entry.path)?;
        count += 1;
        if identity.bytes > 1024 * 1024 {
            large += 1;
        }
        Ok(sys::Walk::Continue)
    })
    .unwrap();
    assert!(
        count != 0 && large != 0,
        "actual project does not exercise large asset metadata"
    );
}

fn code_revision_fixture() -> (TempDir, Compiler, NilMap<String>) {
    let (guard, root) = temp();
    let write = |path: &str, value: &str, mode: u32| -> String {
        let filename = write_graph_file(&root, path, value);
        fs::set_permissions(&filename, fs::Permissions::from_mode(mode)).unwrap();
        filename
    };
    let player = write("staged/Host.x86_64", "native executable", 0o700);
    let dll = write("staged/Host_Data/Managed/Game.dll", "baseline game", 0o600);
    write("staged/Host_Data/Managed/Host.dll", "managed bridge", 0o600);
    write("staged/UnityPlayer.so", "native Unity", 0o600);
    let source = write("Assets/Game.cs", "class Game {}", 0o600);
    let helper = write(
        "compiler/lifecycle/test/LifecycleCompiler.dll",
        "verified lifecycle helper",
        0o600,
    );
    atomic_json(
        &join(&[&sysutil::dir(&helper), "files.json"]),
        &NilMap::from([("LifecycleCompiler.dll", hash(&helper))]),
    )
    .unwrap();
    let csc = write("sdk/csc.dll", "Roslyn", 0o600);
    write("sdk/Microsoft.CodeAnalysis.dll", "Roslyn dependency", 0o600);
    let dotnet = write("sdk/dotnet", "SDK executable", 0o600);
    let generator = write("generators/Unity.SourceGenerators.dll", "generator", 0o600);
    write("generators/Dependency.dll", "generator dependency", 0o600);
    let mut proof = valid_proof();
    proof.runtime_initializers = NilVec::from([initializer("Game", "Game", "Init", 2)]);
    let sources = NilMap::from([(source.clone(), hash(&source))]);
    let compiler = Compiler {
        project: root.clone(),
        cache: join(&[&root, "compiler"]),
        player: player.clone(),
        baseline_player: player,
        helper,
        csc,
        dotnet,
        generators: NilVec::from([generator]),
        current_dlls: NilMap::from([("Game", dll.clone())]),
        manifest: CompileManifest {
            schema: 1,
            project: root,
            assemblies: NilVec::from([CompileAssembly {
                name: "Game".into(),
                sha256: hash(&dll),
                sources: NilVec::from([source]),
                lifecycle_barrier: proof.clone(),
                ..Default::default()
            }]),
            compiler_inputs: sources.clone(),
            ..Default::default()
        },
        proofs: NilMap::from([("Game", proof)]),
        sources: sources.clone(),
        configuration: NilMap::make(),
        configuration_hashes: NilMap::from([("Game", "configuration".to_string())]),
        ..Default::default()
    };
    (guard, compiler, sources)
}
fn save_code_revision_fixture(
    compiler: &Compiler,
    sources: &NilMap<String>,
    key: &str,
) -> CompileUpdate {
    let folder = join(&[&compiler.cache, ".code-revision-test", "Player"]);
    sys::mkdir_all(&sysutil::dir(&folder)).unwrap();
    compiler_command(
        &background(),
        "",
        "cp",
        &["-a", &sysutil::dir(&compiler.player), &folder],
    )
    .unwrap();
    let player = join(&[&folder, &sysutil::base(&compiler.player)]);
    let dll = join(&[&folder, "Host_Data/Managed/Game.dll"]);
    sys::write(&dll, b"verified recompiled game").unwrap();
    let update = CompileUpdate {
        player,
        source_hashes: sources.clone(),
        assemblies: NilVec::from([dll]),
        ..Default::default()
    };
    let map = PlayerMap::read(&folder).unwrap();
    compiler
        .save_code_revision(key, &update, &compiler.proofs, &map)
        .unwrap();
    update
}

#[test]
fn code_revision_restores_verified_player_and_lifecycle_state() {
    let (_guard, mut compiler, sources) = code_revision_fixture();
    let key = compiler.code_revision_key(&background(), &sources).unwrap();
    let original = save_code_revision_fixture(&compiler, &sources, &key);
    let restored = compiler
        .restore_code_revision(&key, &sources)
        .expect("verified complete revision was not reused");
    assert!(
        restored.cache_reused
            && restored.player == original.player
            && restored.compiled_assemblies.is_empty()
            && restored.assemblies == original.assemblies
            && restored.runtime_initializers == compiler.proofs["Game"].runtime_initializers,
        "verified complete revision was not reused: {restored:?}"
    );
    let proofs = compiler.proofs.clone();
    compiler.use_code_revision(&restored, proofs);
    assert!(
        compiler.startup_complete
            && compiler.player == original.player
            && compiler.current_dlls.at("Game") == original.assemblies[0],
        "cached graph was not installed as current compilation state"
    );
}

#[test]
fn code_revision_identity_tracks_sources_bridge_native_sdk_and_generators() {
    let (_guard, mut compiler, sources) = code_revision_fixture();
    let ctx = background();
    let key = compiler.code_revision_key(&ctx, &sources).unwrap();
    let mut changed_sources = NilMap::make();
    for path in sources.keys() {
        changed_sources.insert(path, sha256(b"changed source"));
    }
    assert_ne!(
        compiler.code_revision_key(&ctx, &changed_sources).unwrap(),
        key,
        "changed source was not included in code revision identity"
    );
    let staged = sysutil::dir(&compiler.player);
    for path in [
        join(&[&staged, "Host_Data/Managed/Host.dll"]),
        join(&[&staged, "UnityPlayer.so"]),
        join(&[&sysutil::dir(&compiler.csc), "Microsoft.CodeAnalysis.dll"]),
        join(&[&sysutil::dir(&compiler.generators[0]), "Dependency.dll"]),
    ] {
        let original = fs::read(&path).unwrap();
        sys::write(&path, b"changed dependency").unwrap();
        assert_ne!(
            compiler.code_revision_key(&ctx, &sources).unwrap(),
            key,
            "changed dependency did not invalidate startup revision: {path}"
        );
        sys::write(&path, &original).unwrap();
    }
    assert_eq!(compiler.code_revision_key(&ctx, &sources).unwrap(), key);
    compiler
        .configuration_hashes
        .insert("Game", "changed assembly configuration".to_string());
    assert_ne!(
        compiler.code_revision_key(&ctx, &sources).unwrap(),
        key,
        "changed assembly configuration did not invalidate revision"
    );
}

#[test]
fn code_revision_rejects_corrupt_dll_added_files_and_invalid_proof() {
    for kind in ["dll", "native", "added-dll", "proof", "escape", "symlink"] {
        let (_guard, mut compiler, sources) = code_revision_fixture();
        let key = compiler.code_revision_key(&background(), &sources).unwrap();
        let update = save_code_revision_fixture(&compiler, &sources, &key);
        let path = compiler.code_revision_path(&key);
        let mut entry: CodeRevision = read_json_file(&path).unwrap();
        match kind {
            "dll" => sys::write(&update.assemblies[0], b"corrupted output").unwrap(),
            "native" => sys::write(
                &join(&[&sysutil::dir(&update.player), "UnityPlayer.so"]),
                b"corrupted output",
            )
            .unwrap(),
            "added-dll" => sys::write(
                &join(&[&sysutil::dir(&update.assemblies[0]), "Unexpected.dll"]),
                b"unverified assembly",
            )
            .unwrap(),
            "proof" => {
                let mut proof = entry.proofs["Game"].clone();
                proof.verified = false;
                entry.proofs.insert("Game", proof);
            }
            "escape" => entry.player = "../staged/Host.x86_64".into(),
            _ => {
                fs::remove_file(&update.assemblies[0]).unwrap();
                symlink(compiler.current_dlls.at("Game"), &update.assemblies[0]).unwrap();
            }
        }
        atomic_json(&path, &entry).unwrap();
        assert!(
            compiler.restore_code_revision(&key, &sources).is_none(),
            "corrupt cached {kind} selected a Player"
        );
    }
}

#[test]
fn code_revision_tracks_pinned_netcode_payload() {
    let (_guard, mut compiler, sources) = code_revision_fixture();
    let ctx = background();
    let helper = write_graph_file(
        &compiler.cache,
        "fake-netcode/Unity.Netcode.Editor.CodeGen.dll",
        "processed NGO helper",
    );
    compiler.netcode = Some(NetcodeProcessor {
        helper: helper.clone(),
        files: NilMap::from([(sysutil::base(&helper), hash(&helper))]),
        key: "verified-netcode-payload".into(),
        ..Default::default()
    });
    compiler.manifest.assemblies[0].references = strs(&["Unity.Netcode.Runtime"]);
    let key = compiler.code_revision_key(&ctx, &sources).unwrap();
    compiler.netcode.as_mut().unwrap().key = "new-verified-netcode-payload".into();
    assert_ne!(
        compiler.code_revision_key(&ctx, &sources).unwrap(),
        key,
        "changed NGO processed-output fingerprint did not invalidate graph"
    );
    sys::write(&helper, b"tampered NGO payload").unwrap();
    assert!(
        compiler
            .code_revision_key(&ctx, &sources)
            .is_err_and(|error| error.to_string().contains("dependency changed")),
        "corrupt postprocessor allowed revision reuse"
    );
}

#[test]
fn script_type_inventory_reuse_requires_exact_dll_and_bindings() {
    let (_guard, mut compiler, _) = code_revision_fixture();
    compiler.manifest.scripts =
        manifest_json(r#"{"scripts":[{"guid":"script-guid","path":"Game.cs","assembly":"Game"}]}"#)
            .scripts;
    let assemblies = BTreeMap::from([(
        "Game".to_string(),
        compiler.current_dlls.at("Game").to_string(),
    )]);
    let helper = compiler.helper.clone();
    let key = compiler
        .script_type_cache_key(&helper, &assemblies)
        .unwrap();
    let expected = NilMap::from([("script-guid", "Namespace.Game".to_string())]);
    compiler.save_script_type_cache(&key, &expected).unwrap();
    // The fixture's fake dotnet executable cannot run. Reuse must satisfy
    // script_type_names entirely from verified metadata produced previously.
    assert_eq!(
        compiler.script_type_names(&background()).unwrap(),
        expected,
        "unchanged defining DLL inventory was not reused"
    );
    compiler.manifest.scripts[0].guid = "changed-guid".into();
    assert_ne!(
        compiler
            .script_type_cache_key(&helper, &assemblies)
            .unwrap(),
        key,
        "changed GUID binding reused stale type inventory"
    );
    compiler.manifest.scripts[0].guid = "script-guid".into();
    compiler.expected_types = NilMap::from([(
        "Game",
        NilMap::from([("Namespace.Other", "script-guid".to_string())]),
    )]);
    assert_ne!(
        compiler
            .script_type_cache_key(&helper, &assemblies)
            .unwrap(),
        key,
        "changed required type binding reused inventory"
    );
    compiler.expected_types = NilMap::nil();
    assert_eq!(
        compiler
            .script_type_cache_key(&helper, &assemblies)
            .unwrap(),
        key
    );
    sys::write(compiler.current_dlls.at("Game"), b"changed real DLL output").unwrap();
    assert_ne!(
        compiler
            .script_type_cache_key(&helper, &assemblies)
            .unwrap(),
        key,
        "changed compiled DLL reused old type inventory"
    );
}

#[test]
fn script_type_inventory_rejects_corruption_foreign_guid_and_symlink() {
    for kind in ["corrupt", "foreign-guid", "symlink"] {
        let (_guard, mut compiler, _) = code_revision_fixture();
        compiler.manifest.scripts =
            manifest_json(r#"{"scripts":[{"guid":"known-guid","assembly":"Game"}]}"#).scripts;
        let key = "test-key";
        let mut types = NilMap::from([("known-guid", "Namespace.Game".to_string())]);
        if kind == "foreign-guid" {
            types.insert("foreign-guid", "Namespace.Other".to_string());
        }
        compiler.save_script_type_cache(key, &types).unwrap();
        if kind != "foreign-guid" {
            assert!(
                compiler.read_script_type_cache(key).is_some(),
                "valid inventory rejected"
            );
        }
        let path = join(&[&compiler.cache, "script-types", key, "types.json"]);
        match kind {
            "corrupt" => sys::write(&path, br#"{"known-guid":"Namespace.Corrupt"}"#).unwrap(),
            "symlink" => {
                let outside = join(&[&compiler.project, "unowned.json"]);
                fs::rename(&path, &outside).unwrap();
                symlink(&outside, &path).unwrap();
            }
            _ => {}
        }
        assert!(
            compiler.read_script_type_cache(key).is_none(),
            "corrupt cached {kind} was reused"
        );
    }
}

#[test]
fn netcode_postprocessor_sdk() {
    let Some(editor) = sdk(
        "UME_NETCODE_SDK",
        "set UME_NETCODE_SDK to Unity executable; only its SDK/package ILPP is used",
    ) else {
        return;
    };
    let Some(project) = sdk(
        "UNITY_MICRO_EDITOR_TEST_PROJECT",
        "set UNITY_MICRO_EDITOR_TEST_PROJECT to the prepared game project for this integration check",
    ) else {
        return;
    };
    let project = crate::project::lexpath::abs(&project).unwrap();
    let data = join(&[&sysutil::dir(&editor), "Data"]);
    let csc = sys::glob_lossy(&join(&[&data, "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll"]));
    assert_eq!(csc.len(), 1, "expected one pinned SDK");
    // The host Player of the project: `UME_HOT_RELOAD_HOST` names the
    // executable that auto-prepare built (below
    // Library/UnityMicroEditor/stage/project/Builds); without it the test
    // looks where earlier hosts were built.
    let player = std::env::var("UME_HOT_RELOAD_HOST")
        .ok()
        .filter(|host| !host.is_empty())
        .map(|host| crate::project::lexpath::abs(&host).unwrap())
        .unwrap_or_else(|| join(&[&project, "Builds/CLIPlayHybridHost/UnityPlayHost.x86_64"]));
    let managed = crate::compiler::player_managed(&player);
    let (_guard, root) = temp();
    let ctx = background();
    let mut compiler = Compiler {
        project: project.clone(),
        editor,
        cache: join(&[&root, "compiler"]),
        player: player.clone(),
        dotnet: join(&[&data, "DotNetSdk/dotnet"]),
        csc: csc[0].clone(),
        configs: NilMap::from([(
            "NetcodeFixture",
            AssemblyConfig {
                name: "NetcodeFixture".into(),
                defines: strs(&["UNITY_STANDALONE_LINUX"]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let processor = compiler.prepare_netcode_processor(&ctx).unwrap();
    let bcl = join(&[&data, "MonoBleedingEdge/lib/mono/unityaot-linux"]);
    let mut references = sys::glob_lossy(&join(&[&bcl, "*.dll"]));
    references.extend(sys::glob_lossy(&join(&[&bcl, "Facades/*.dll"])));
    references.extend(sys::glob_lossy(&join(&[&managed, "*.dll"])));
    let refs = deduplicate_references(&references);
    let source = write_graph_file(
        &root,
        "NetcodeFixture.cs",
        "using Unity.Netcode;\npublic sealed class NetcodeFixture : NetworkBehaviour {\n public readonly NetworkVariable<int> Counter = new NetworkVariable<int>();\n [Rpc(SendTo.Server)] public void UseRpc(int amount, RpcParams rpc = default) { Counter.Value += amount; }\n}",
    );
    let dll = join(&[&root, "NetcodeFixture.dll"]);
    let mut arguments = vec![
        compiler.csc.clone(),
        "-nologo".into(),
        "-target:library".into(),
        "-nostdlib+".into(),
        "-debug:portable".into(),
        format!("-out:{dll}"),
    ];
    arguments.extend(
        refs.iter()
            .map(|reference| format!("-reference:{reference}")),
    );
    arguments.push(source);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    compiler_command(&ctx, &root, &compiler.dotnet, &arguments).unwrap();
    let (processed, next_proof) = compiler
        .postprocess_netcode(
            &ctx,
            &root,
            &dll,
            &assembly("NetcodeFixture", &[], &["Unity.Netcode.Runtime"]),
            &refs,
            valid_proof(),
        )
        .unwrap();
    let result: serde_json::Value = read_json_file(&join(&[&root, "netcode-proof.json"])).unwrap();
    let rpc = &result["rpcs"][0];
    let calls = |name: &str| rpc[name].as_array().map_or(0, Vec::len);
    assert!(
        result["rpcCount"] == 1
            && result["rpcs"].as_array().map_or(0, Vec::len) == 1
            && rpc["method"] == "UseRpc"
            && rpc["handler"]
                .as_str()
                .is_some_and(|name| name.starts_with("__rpc_handler_"))
            && calls("sendCalls") >= 2
            && calls("writeCalls") != 0
            && calls("readCalls") != 0,
        "RPC send/receive payload code was not generated: {result}"
    );
    assert!(
        valid_lifecycle_proof(&next_proof, "NetcodeFixture")
            && next_proof.runtime_initializers.len() == 1
            && next_proof.runtime_initializers[0].method_name
                == "__UnityPlay_RuntimeInitialize_Netcode",
        "generated NetworkVariable serializers have no public initialization bridge: {next_proof:?}"
    );
    assert_ne!(
        hash(&dll),
        hash(&processed),
        "ILPP returned the original plain Roslyn assembly"
    );
    assert_eq!(
        compiler.prepare_netcode_processor(&ctx).unwrap().helper,
        processor.helper,
        "verified SDK helper cache was not reused"
    );
    let input = write_graph_file(&root, "PinnedProcessorInput", "original");
    compiler
        .netcode
        .as_mut()
        .unwrap()
        .inputs
        .insert(&input, hash(&input));
    sys::write(&input, b"modified").unwrap();
    assert!(
        compiler.prepare_netcode_processor(&ctx).is_err(),
        "postprocessor dependency drift silently reused compiled network code"
    );
}

#[test]
fn source_initializer_overlay_retains_native_engine_rows_once() {
    let (_guard, project) = temp();
    let path = write_graph_file(
        &project,
        "Temp/RuntimeInitializeOnLoads.json",
        r#"{"root":[{"assemblyName":"Game","methodName":"Initialize"},{"assemblyName":"Unity.InputSystem","methodName":"RuntimeInitialize"}],"extra":{"profile":1}}"#,
    );
    remove_source_native_initializers(&path, &[assembly("Game", &[], &[])]).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\"extra\":{\"profile\":1},\"root\":[{\"assemblyName\":\"Unity.InputSystem\",\"methodName\":\"RuntimeInitialize\"}]}\n",
        "native ownership table was clobbered"
    );
    remove_source_native_initializers(&join(&[&project, "Temp/absent.json"]), &[]).unwrap();
}

#[test]
fn values_keep_null_and_empty_apart() {
    #[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug)]
    #[serde(default)]
    struct Document {
        list: NilVec<String>,
        map: NilMap<String>,
    }
    let nil: Document = unmarshal(br#"{"list":null}"#).unwrap();
    assert_eq!(
        crate::util::marshal(&nil).unwrap(),
        br#"{"list":null,"map":null}"#
    );
    let empty: Document = unmarshal(br#"{"list":[],"map":{}}"#).unwrap();
    assert_eq!(
        crate::util::marshal(&empty).unwrap(),
        br#"{"list":[],"map":{}}"#
    );
    assert_ne!(nil, empty);
    let mut appended = NilVec::<String>::nil();
    appended.append(Vec::new());
    assert!(
        appended.is_nil(),
        "append of nothing must keep a nil slice nil"
    );
    assert_eq!(float_layout(0.25).to_string(), "0.25");
    assert_eq!(float_layout(3.0).to_string(), "3");
    assert_eq!(float_layout(0.0000001234).to_string(), "1.234e-7");
    assert_eq!(float_layout(1e21).to_string(), "1e+21");
}

#[test]
fn glob_and_match() {
    for (pattern, name, expected) in [
        ("*.dll", "a.dll", true),
        ("*.dll", "a.dlls", false),
        ("UnityEngine*.dll", "UnityEngine.CoreModule.dll", true),
        ("a?c", "abc", true),
        ("a[b-d]c", "acc", true),
        ("a[^b-d]c", "acc", false),
        ("a\\*c", "a*c", true),
        ("*", "a/b", false),
        ("com.unity.x@*", "com.unity.x@1.2.3", true),
    ] {
        assert_eq!(
            sys::path_match(pattern, name).unwrap(),
            expected,
            "{pattern} {name}"
        );
    }
    assert!(sys::path_match("a[", "a").is_err());
    let (_guard, root) = temp();
    for path in [
        "sdk/9.0/Roslyn/csc.dll",
        "sdk/8.0/Roslyn/csc.dll",
        "sdk/readme.txt",
    ] {
        write_graph_file(&root, path, "x");
    }
    assert_eq!(
        sys::glob(&join(&[&root, "sdk/*/Roslyn/csc.dll"])).unwrap(),
        [
            join(&[&root, "sdk/8.0/Roslyn/csc.dll"]),
            join(&[&root, "sdk/9.0/Roslyn/csc.dll"])
        ]
    );
    assert!(
        sys::glob(&join(&[&root, "absent/*.dll"]))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn compiler_command_reports_failures_and_cancellation() {
    let ctx = background();
    assert_eq!(
        compiler_command(&ctx, "", "sh", &["-c", "printf out; printf err >&2"]).unwrap(),
        b"outerr"
    );
    let error = compiler_command(&ctx, "", "sh", &["-c", "echo no; exit 3"]).unwrap_err();
    assert_eq!(error.message, "sh: exit status 3\nno\n");
    assert_eq!(error.output, b"no\n");
    let error = compiler_command(&ctx, "", "/nonexistent/tool", &[]).unwrap_err();
    assert_eq!(
        error.message,
        "tool: fork/exec /nonexistent/tool: no such file or directory\n"
    );
    let error = compiler_command(&ctx, "", "no-such-compiler-tool", &[]).unwrap_err();
    assert_eq!(
        error.message,
        "no-such-compiler-tool: exec: \"no-such-compiler-tool\": executable file not found in $PATH\n"
    );
    let (_guard, directory) = temp();
    assert_eq!(
        compiler_command(
            &ctx,
            &directory,
            "sh",
            &["-c", "printf %s \"$PWD:$DOTNET_NOLOGO\""]
        )
        .unwrap(),
        format!("{directory}:1").as_bytes()
    );
    let cancelled = ctx.with_cancel();
    cancelled.cancel();
    assert_eq!(
        compiler_command(&cancelled, "", "sh", &["-c", "true"])
            .unwrap_err()
            .message,
        "sh: canceled\n"
    );
    // The whole process group dies with the context, grandchildren included.
    let timed = ctx.with_timeout(std::time::Duration::from_millis(200));
    let started = std::time::Instant::now();
    let error = compiler_command(&timed, "", "sh", &["-c", "sleep 30 & sleep 30"]).unwrap_err();
    assert_eq!(error.message, "sh: signal: killed\n");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

/// Compiles the scripts of the behaviour fixtures and reads their layouts
/// back from the assemblies. The fixture layouts are what the Player objects
/// of the same classes were compared with, byte for byte.
#[test]
fn compiler_behaviour_layouts_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set UME_LIFECYCLE_SDK to the matching Unity executable; SDK only, no Editor or Player starts",
    ) else {
        return;
    };
    let (_guard, cache) = temp();
    let mut compiler = Compiler {
        editor,
        cache: cache.clone(),
        ..Default::default()
    };
    compiler.initialize_sdk().unwrap();
    let ctx = background();
    let helper = compiler.prepare_lifecycle_helper(&ctx).unwrap();
    let fixtures = join(&[
        env!("CARGO_MANIFEST_DIR"),
        "rust/tests/testdata/playerbake/behaviours",
    ]);
    let references: Vec<String> = compiler
        .base_references
        .iter()
        .chain(compiler.engine_references.iter())
        .cloned()
        .collect();
    let mut assemblies = Vec::new();
    for assembly in ["Assembly-CSharp", "Smb.Second"] {
        let dll = join(&[&cache, &format!("{assembly}.dll")]);
        let mut arguments = vec![
            "-nologo".to_string(),
            "-target:library".into(),
            "-nostdlib+".into(),
            "-langversion:9.0".into(),
            "-unsafe+".into(),
            "-nowarn:0169,0649,0414".into(),
            format!("-out:{dll}"),
        ];
        arguments.extend(references.iter().map(|path| format!("-reference:{path}")));
        let mut sources = sys::glob_lossy(&join(&[&fixtures, "Scripts", assembly, "*.cs"]));
        sources.sort();
        assert!(!sources.is_empty());
        arguments.extend(sources);
        let response = join(&[&cache, &format!("{assembly}.rsp")]);
        let text: String = arguments
            .iter()
            .map(|argument| format!("\"{argument}\"\n"))
            .collect();
        sys::write(&response, text.as_bytes()).unwrap();
        compiler_command(
            &ctx,
            &cache,
            &compiler.dotnet,
            &[&compiler.csc, &format!("@{response}")],
        )
        .unwrap();
        assemblies.push(dll);
    }
    assemblies.extend(references);

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Script {
        class_name: String,
    }
    let scripts: BTreeMap<String, Script> =
        read_json_file(&join(&[&fixtures, "scripts.json"])).unwrap();
    let mut expected = BTreeMap::new();
    let mut request = Vec::new();
    for (guid, script) in &scripts {
        let layout = join(&[&fixtures, "layouts", &format!("{}.json", script.class_name)]);
        // A script without a fixture layout is not a state machine behaviour.
        let (assembly, type_name) = match read_json_file::<crate::playerbake::ScriptLayout>(&layout)
        {
            Ok(layout) => {
                let name = match layout.namespace.as_str() {
                    "" => layout.class_name.clone(),
                    namespace => format!("{namespace}.{}", layout.class_name),
                };
                let assembly = layout.assembly.clone();
                expected.insert(guid.clone(), layout);
                (assembly, name)
            }
            Err(_) => ("Assembly-CSharp".to_string(), script.class_name.clone()),
        };
        request.push(json!({"guid": guid, "assembly": assembly, "type": type_name}));
    }
    assert!(expected.len() >= 30 && request.len() > expected.len());
    let request_path = join(&[&cache, "layouts.json"]);
    atomic_json(
        &request_path,
        &json!({"assemblies": assemblies, "scripts": request}),
    )
    .unwrap();
    let output = compiler_command(
        &ctx,
        &cache,
        &compiler.dotnet,
        &[&helper, "--behaviour-layouts", &request_path],
    )
    .unwrap();
    let actual: BTreeMap<String, crate::playerbake::ScriptLayout> =
        serde_json::from_slice(&output).unwrap();
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (guid, layout) in &actual {
        assert_eq!(layout, &expected[guid], "{}", layout.class_name);
    }
    // What grows past the reader's bound on its way down to the engine's
    // depth limit or holds a managed reference that any class may fill is
    // refused.
    let refused: BTreeSet<&str> = actual
        .values()
        .filter(|layout| !layout.unsupported.is_empty())
        .map(|layout| layout.class_name.as_str())
        .collect();
    assert_eq!(refused, BTreeSet::from(["SmbDeep", "SmbManaged2"]));
    // A class with serialization callbacks, in itself, in what it stores
    // inline or in what it may hold by reference, is marked: its objects
    // are what the callbacks leave.
    let receivers: BTreeSet<&str> = actual
        .values()
        .filter(|layout| layout.serialization_callbacks)
        .map(|layout| layout.class_name.as_str())
        .collect();
    assert_eq!(
        receivers,
        BTreeSet::from([
            "SmbCallback",
            "SmbCount",
            "SmbCountPlain",
            "SmbDictCb",
            "SmbEvent",
            "SmbNulls",
            "SmbWrap"
        ])
    );
    // The helper runs the constructors and callbacks of those classes. The
    // fixtures record its reply for every object the baker asks about, and
    // the baked objects made from those replies equal the engine's.
    let mut records = sys::glob_lossy(&join(&[&fixtures, "code", "*.json"]));
    records.sort();
    assert!(records.len() >= 30);
    let (mut requests, mut replies) = (Vec::new(), Vec::new());
    for record in &records {
        let record: serde_json::Value = read_json_file(record).unwrap();
        requests.push(record["request"].clone());
        replies.push(record["reply"].clone());
    }
    let objects_path = join(&[&cache, "objects.json"]);
    atomic_json(
        &objects_path,
        &json!({"assemblies": assemblies, "objects": requests}),
    )
    .unwrap();
    let output = compiler_command(
        &ctx,
        &cache,
        &compiler.dotnet,
        &[&helper, "--behaviour-objects", &objects_path],
    )
    .unwrap();
    let run: serde_json::Value = serde_json::from_slice(&output).unwrap();
    for (index, reply) in replies.iter().enumerate() {
        assert_eq!(&run["objects"][index], reply, "{}", records[index]);
    }
    // Code that asks the engine is refused by what it asked for.
    let refused = replies
        .iter()
        .filter_map(|reply| reply["refused"].as_str())
        .filter(|reason| reason.contains("a call into the engine in UnityEngine.AnimationCurve"))
        .count();
    assert!(refused >= 3);
    let callbacks: BTreeSet<&str> = actual
        .values()
        .filter(|layout| layout.overrides_state_machine_callbacks)
        .map(|layout| layout.class_name.as_str())
        .collect();
    assert_eq!(
        callbacks,
        BTreeSet::from([
            "SmbCbMachineEnter",
            "SmbCbMachineEnter3",
            "SmbCbMachineExit",
            "SmbCbMachineInherited"
        ])
    );
}

/// What a preparation was made from, read off its compiler plan: the
/// managed inputs it recorded must be as they were, and `Assets` must hold
/// no managed configuration that the plan does not know. This is the check
/// of a runner's start, made without one.
#[test]
fn a_compiler_plan_accepts_the_sources_it_was_made_with() {
    let (_guard, project) = temp();
    let asmdef = join(&[&project, "Assets/Lib/Lib.asmdef"]);
    write_graph_file(&project, "Assets/Main.unity", "scene");
    let plan = |name: &str, inputs: &[&str]| -> String {
        let mut recorded = NilMap::from([("public-input", "verified".to_string())]);
        for input in inputs {
            recorded.insert(*input, file_hash(input).unwrap());
        }
        let manifest = CompileManifest {
            schema: 1,
            project: project.clone(),
            compiler_inputs: recorded,
            ..Default::default()
        };
        let path = join(&[
            &project,
            "Library/UnityMicroEditor/preparations",
            name,
            "current.json",
        ]);
        fs::create_dir_all(sysutil::dir(&path)).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(
                &json!({"schemaVersion": 1, "project": project, "compiler": manifest}),
            )
            .unwrap(),
        )
        .unwrap();
        path
    };
    let plain = plan("plain", &[]);
    Compiler::graph_accepts_sources(&project, &plain).unwrap();
    // An assembly definition the plan does not know.
    write_graph_file(&project, "Assets/Lib/Lib.asmdef", r#"{"name":"Lib"}"#);
    let refused = Compiler::graph_accepts_sources(&project, &plain).unwrap_err();
    assert!(requires_new_graph(&format!("{refused:#}")), "{refused:#}");
    // A plan that was made with it accepts the sources, until the file
    // changes or goes.
    let with_library = plan("with-library", &[&asmdef]);
    Compiler::graph_accepts_sources(&project, &with_library).unwrap();
    fs::write(&asmdef, r#"{"name":"Lib","allowUnsafeCode":true}"#).unwrap();
    assert!(Compiler::graph_accepts_sources(&project, &with_library).is_err());
    fs::remove_file(&asmdef).unwrap();
    assert!(Compiler::graph_accepts_sources(&project, &with_library).is_err());
    Compiler::graph_accepts_sources(&project, &plain).unwrap();
    // The plan of another project is none of this one.
    let (_other_guard, other) = temp();
    assert!(Compiler::graph_accepts_sources(&other, &plain).is_err());
}

/// A Player directory with a few files, as `code_revision_fixture` has one.
fn player_map_fixture() -> (TempDir, String) {
    let (guard, root) = temp();
    for (path, text) in [
        ("Player/Host.x86_64", "native executable"),
        ("Player/Host_Data/Managed/Game.dll", "baseline game"),
        ("Player/Host_Data/Managed/Host.dll", "managed bridge"),
        ("Player/UnityPlayer.so", "native Unity"),
    ] {
        write_graph_file(&root, path, text);
    }
    (guard, join(&[&root, "Player"]))
}

/// Writes other bytes of the same length and gives the file its old
/// modification time back: only the change time and the content moved.
fn rewrite_keeping_size_and_mtime(path: &str, text: &str) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    assert_eq!(fs::metadata(path).unwrap().len(), text.len() as u64);
    // The change time has a coarse clock; let it move.
    std::thread::sleep(std::time::Duration::from_millis(20));
    sys::write(path, text.as_bytes()).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
}

fn copy_player(source: &str, destination: &str) {
    sys::mkdir_all(destination).unwrap();
    compiler_command(
        &background(),
        "",
        "cp",
        &["-a", "--reflink=auto", &format!("{source}/."), destination],
    )
    .unwrap();
}

#[test]
fn player_map_hashes_what_player_files_hashes() {
    let (_guard, player) = player_map_fixture();
    let map = PlayerMap::read(&player).unwrap();
    assert_eq!(map.files, player_files(&player).unwrap());
    assert_eq!(map.directory, player);
    assert_eq!(map.stamps.len(), map.files.len());
    assert!(map.still(), "an untouched directory is not still");
    // The rules of player_files hold: a symbolic link is not a Player file.
    symlink(
        join(&[&player, "UnityPlayer.so"]),
        join(&[&player, "link.so"]),
    )
    .unwrap();
    assert!(PlayerMap::read(&player).is_err() && player_files(&player).is_err());
}

#[test]
fn player_map_is_not_still_after_any_change() {
    let game = "Host_Data/Managed/Game.dll";
    for change in ["same size and mtime", "added", "removed", "replaced"] {
        let (_guard, player) = player_map_fixture();
        let map = PlayerMap::read(&player).unwrap();
        let path = join(&[&player, game]);
        match change {
            "same size and mtime" => rewrite_keeping_size_and_mtime(&path, "changed  game"),
            "added" => {
                write_graph_file(&player, "Host_Data/Managed/Extra.dll", "x");
            }
            "removed" => fs::remove_file(&path).unwrap(),
            _ => {
                let other = join(&[&player, "other"]);
                sys::write(&other, b"baseline game").unwrap();
                fs::rename(&other, &path).unwrap();
            }
        }
        assert!(
            !map.still(),
            "{change}: the map of a changed directory is still"
        );
    }
}

#[test]
fn player_map_stays_still_while_it_is_copied() {
    let (_guard, player) = player_map_fixture();
    let map = PlayerMap::read(&player).unwrap();
    let copy = join(&[&sysutil::dir(&player), "Copy"]);
    copy_player(&player, &copy);
    copy_player(&player, &join(&[&sysutil::dir(&player), "Again"]));
    assert!(
        map.still(),
        "copying a Player directory moved the stamps of its files"
    );
    let copied = map
        .copied_to(&copy)
        .expect("a cp -a copy is not recognized");
    assert_eq!(copied.files, player_files(&copy).unwrap());
    assert_eq!(copied.directory, copy);
    // The copy of a copy keeps it too.
    let next = join(&[&sysutil::dir(&player), "Next"]);
    copy_player(&copy, &next);
    assert!(copied.still());
    assert_eq!(
        copied.copied_to(&next).unwrap().files,
        player_files(&next).unwrap()
    );
}

#[test]
fn player_map_rejects_a_copy_that_differs() {
    let game = "Host_Data/Managed/Game.dll";
    for change in ["size", "mtime", "extra", "missing"] {
        let (_guard, player) = player_map_fixture();
        let map = PlayerMap::read(&player).unwrap();
        let copy = join(&[&sysutil::dir(&player), "Copy"]);
        copy_player(&player, &copy);
        let path = join(&[&copy, game]);
        match change {
            "size" => sys::write(&path, b"a longer recompiled game").unwrap(),
            "mtime" => fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(std::time::SystemTime::UNIX_EPOCH)
                .unwrap(),
            "extra" => {
                write_graph_file(&copy, "Host_Data/Managed/Extra.dll", "x");
            }
            _ => fs::remove_file(&path).unwrap(),
        }
        assert!(
            map.copied_to(&copy).is_none(),
            "{change}: a copy that differs took the map of its source"
        );
    }
}

/// The map a compile gives the Player directory it copied is the map of the
/// bytes in that directory, also when something else changed the source of
/// the copy after it was hashed, or the copy after it was made.
#[test]
fn copied_player_map_names_the_bytes_of_the_copy() {
    let game = "Host_Data/Managed/Game.dll";
    for change in ["source", "copy", "none"] {
        let (_guard, player) = player_map_fixture();
        let map = std::sync::Arc::new(PlayerMap::read(&player).unwrap());
        if change == "source" {
            rewrite_keeping_size_and_mtime(&join(&[&player, game]), "foreign  game");
        }
        let copy = join(&[&sysutil::dir(&player), "Copy"]);
        copy_player(&player, &copy);
        if change == "copy" {
            sys::write(&join(&[&copy, game]), b"foreign game, longer").unwrap();
        }
        let derived = PlayerMap::after_copy(Some(map.clone()), &copy).unwrap();
        assert_eq!(
            derived.files,
            player_files(&copy).unwrap(),
            "{change}: the map of a copy does not name its bytes"
        );
        let mut written = derived.clone();
        let dll = join(&[&copy, game]);
        sys::write(&dll, b"recompiled").unwrap();
        written.wrote(&dll, sha256(b"recompiled")).unwrap();
        assert_eq!(written.files, player_files(&copy).unwrap());
        assert!(
            written.still(),
            "{change}: the map of a written copy is not still"
        );
    }
}

fn process_alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// Processes whose command line names `text`.
fn processes_naming(text: &str) -> Vec<u32> {
    let mut found = Vec::new();
    for item in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = item.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Ok(line) = fs::read(format!("/proc/{pid}/cmdline"))
            && String::from_utf8_lossy(&line).contains(text)
            && process_alive(pid)
        {
            found.push(pid);
        }
    }
    found
}

/// A stand-in for `dotnet exec VBCSCompiler.dll -pipename:<name>`: it makes
/// the socket file and waits.
fn stand_in_server(root: &str) -> (String, String) {
    let dotnet = write_graph_file(
        root,
        "sdk/dotnet",
        "#!/bin/sh\npipe=${3#-pipename:}\n: > /tmp/$pipe\nexec sleep 300\n",
    );
    fs::set_permissions(&dotnet, fs::Permissions::from_mode(0o700)).unwrap();
    let csc = write_graph_file(root, "sdk/bincore/csc.dll", "csc");
    (dotnet, csc)
}

#[test]
fn compiler_server_restarts_when_a_reference_changes_or_it_ended() {
    let (_guard, root) = temp();
    let (dotnet, csc) = stand_in_server(&root);
    let server = CompilerServer::new(&dotnet, &csc, &join(&[&root, "output"]));
    let inputs = |hash: &str| BTreeMap::from([("/refs/A.dll".to_string(), hash.to_string())]);
    let first = server.client(&inputs("a1")).expect("a server starts");
    let pid = server.pid().unwrap();
    assert!(process_alive(pid) && sys::absent(&first.no_host));
    assert_eq!(
        first.arguments,
        [
            format!("-shared:{}", first.pipe),
            "-keepalive:86400".to_string()
        ]
    );
    assert!(first.pipe.starts_with("ume-") && first.pipe.ends_with("-1"));
    // The same content again, and another path: the same server.
    let mut more = inputs("a1");
    more.insert("/refs/B.dll".into(), "b1".into());
    assert_eq!(server.client(&more).unwrap().pipe, first.pipe);
    assert_eq!(server.pid(), Some(pid));
    // A path with other content: another server under another name.
    let second = server.client(&inputs("a2")).unwrap();
    assert_ne!(second.pipe, first.pipe);
    assert!(
        !process_alive(pid),
        "the server that saw the old content still runs"
    );
    assert!(sys::absent(&format!("/tmp/{}", first.pipe)));
    // A server that ended by itself is replaced under a new name.
    let pid = server.pid().unwrap();
    sysutil::kill(i64::from(pid), libc::SIGKILL).unwrap();
    let started = std::time::Instant::now();
    while process_alive(pid) && started.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let third = server.client(&inputs("a2")).unwrap();
    assert!(third.pipe != second.pipe && third.pipe != first.pipe);
    let pid = server.pid().unwrap();
    drop(server);
    assert!(!process_alive(pid), "a dropped server still runs");
    assert!(sys::absent(&format!("/tmp/{}", third.pipe)));
    assert!(processes_naming(&third.pipe).is_empty());
}

#[test]
fn compiler_server_is_left_after_three_failures() {
    let (_guard, root) = temp();
    let (dotnet, csc) = stand_in_server(&root);
    let server = CompilerServer::new(&dotnet, &csc, &join(&[&root, "output"]));
    let inputs = BTreeMap::new();
    for _ in 0..3 {
        assert!(server.client(&inputs).is_some());
        server.failed("test");
    }
    assert_eq!(
        server.client(&inputs),
        None,
        "a failing server is used again"
    );
    assert_eq!(server.pid(), None);
    // One that cannot start is a failure as well.
    let (_guard, root) = temp();
    let server = CompilerServer::new(
        &join(&[&root, "missing-dotnet"]),
        &join(&[&root, "csc.dll"]),
        &root,
    );
    for _ in 0..3 {
        assert_eq!(server.client(&inputs), None);
    }
    assert_eq!(server.client(&inputs), None);
}

/// Compiles with the SDK's csc, cold or through `server`, and returns the
/// exit, the listing and the bytes of the DLL and the PDB.
struct Csc {
    dotnet: String,
    csc: String,
    references: Vec<String>,
    generators: Vec<String>,
}
impl Csc {
    fn new(editor: &str) -> Self {
        let mut paths = Compiler {
            editor: editor.into(),
            ..Default::default()
        };
        paths.initialize_sdk().unwrap();
        let mut references: Vec<String> = paths.base_references.to_vec();
        references.extend(paths.engine_references.iter().cloned());
        Self {
            dotnet: paths.dotnet,
            csc: paths.csc,
            references: deduplicate_references(&references).to_vec(),
            generators: paths.generators.to_vec(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn compile(
        &self,
        server: Option<&CompilerServer>,
        directory: &str,
        name: &str,
        sources: &[String],
        extra: &[String],
        unsafe_code: bool,
    ) -> (bool, String, Option<Vec<u8>>, Option<Vec<u8>>) {
        let dll = join(&[directory, &format!("{name}.dll")]);
        let pdb = join(&[directory, &format!("{name}.pdb")]);
        let generated = join(&[directory, "Generated"]);
        sys::mkdir_all(&generated).unwrap();
        let mut text = String::new();
        let mut arguments: Vec<String> = [
            "-nologo",
            "-target:library",
            "-nostdlib+",
            "-langversion:9.0",
            "-deterministic+",
            "-debug:portable",
            "-optimize-",
            if unsafe_code { "-unsafe+" } else { "-unsafe-" },
        ]
        .map(String::from)
        .to_vec();
        arguments.push(format!("-out:{dll}"));
        arguments.push("-define:UNITY_6000_6".into());
        let mut inputs = BTreeMap::new();
        for reference in self.references.iter().chain(extra) {
            arguments.push(format!("-reference:{reference}"));
            inputs.insert(reference.clone(), hash(reference));
        }
        for generator in &self.generators {
            arguments.push(format!("-analyzer:{generator}"));
            inputs.insert(generator.clone(), hash(generator));
        }
        let additional = join(&[directory, &format!("{name}.UnityAdditionalFile.txt")]);
        sys::write(&additional, directory.as_bytes()).unwrap();
        arguments.push(format!("-generatedfilesout:{generated}"));
        arguments.push(format!("-additionalfile:{additional}"));
        arguments.extend(sources.iter().cloned());
        for argument in &arguments {
            text.push('"');
            text.push_str(&argument.replace('"', "\\\""));
            text.push_str("\"\n");
        }
        let response = join(&[directory, &format!("{name}.rsp")]);
        sys::write(&response, text.as_bytes()).unwrap();
        let result = csc_command(
            &background(),
            server,
            false,
            directory,
            &self.dotnet,
            &[&self.csc, &format!("@{response}")],
            &inputs,
            &[dll.clone(), pdb.clone()],
        );
        let (ok, listing) = match result {
            Ok(output) => (true, String::from_utf8_lossy(&output).into_owned()),
            Err(error) => (false, String::from_utf8_lossy(&error.output).into_owned()),
        };
        (ok, listing, fs::read(&dll).ok(), fs::read(&pdb).ok())
    }
}

/// A compile through the server writes the bytes a compile in a process of
/// its own writes into the same paths (the output path is an input of the
/// bytes), and a compile error has the same listing.
#[test]
fn compiler_server_matches_cold_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set UME_LIFECYCLE_SDK to the matching Unity executable; SDK only",
    ) else {
        return;
    };
    let (_guard, root) = temp();
    let csc = Csc::new(&editor);
    let server = CompilerServer::new(&csc.dotnet, &csc.csc, &join(&[&root, "output"]));
    let directory = join(&[&root, "work"]);
    sys::mkdir_all(&directory).unwrap();
    // Each compile starts without outputs: a failed one leaves what was there.
    let clear = |name: &str| {
        for output in ["dll", "pdb"] {
            let _ = fs::remove_file(join(&[&directory, &format!("{name}.{output}")]));
        }
    };
    let both = |name: &str, sources: &[String], extra: &[String], unsafe_code: bool| {
        clear(name);
        let cold = csc.compile(None, &directory, name, sources, extra, unsafe_code);
        clear(name);
        let served = csc.compile(Some(&server), &directory, name, sources, extra, unsafe_code);
        assert_eq!(
            cold.0, served.0,
            "{name}: exit differs:\n{}\n{}",
            cold.1, served.1
        );
        assert_eq!(cold.1, served.1, "{name}: listing differs");
        assert!(cold.2 == served.2, "{name}: DLL bytes differ");
        assert!(cold.3 == served.3, "{name}: PDB bytes differ");
        served
    };
    let source = join(&[&root, "work/Probe.cs"]);
    let body = |value: u32| {
        format!(
            "using UnityEngine;\npublic class Probe : MonoBehaviour {{\n    public int seed;\n    void Update() {{ seed += {value}; }}\n}}\n"
        )
    };
    let sources = [source.clone()];
    for value in [1, 1, 2, 3, 4, 5, 6] {
        sys::write(&source, body(value).as_bytes()).unwrap();
        assert!(both("Game", &sources, &[], false).0);
    }
    let pid = server.pid().expect("the edits ran through a server");
    // A compile error.
    sys::write(&source, b"public class Broken { int x = missing; }\n").unwrap();
    let broken = both("Game", &sources, &[], false);
    assert!(!broken.0 && broken.1.contains("CS0103"), "{}", broken.1);
    assert_eq!(server.pid(), Some(pid), "a compile error ended the server");
    // Unsafe code, and a second assembly that references the first.
    sys::write(
        &source,
        b"public static class Pointers { public static unsafe int Read(int* p) => *p; }\n",
    )
    .unwrap();
    assert!(both("Game", &sources, &[], true).0);
    let consumer = join(&[&root, "work/Consumer.cs"]);
    sys::write(
        &consumer,
        b"public static class Consumer { public static unsafe int Twice(int* p) => 2 * Pointers.Read(p); }\n",
    )
    .unwrap();
    let game = join(&[&directory, "Game.dll"]);
    assert!(both("Consumer", &[consumer], &[game], true).0);
    // A source that a Unity source generator emits code for.
    sys::write(
        &source,
        b"[assembly: Unity.Properties.GeneratePropertyBagsForAssembly]\n[Unity.Properties.GeneratePropertyBag] public partial class Bagged { public int value; }\n",
    )
    .unwrap();
    let generated = join(&[&directory, "Generated"]);
    let _ = fs::remove_dir_all(&generated);
    assert!(both("Game", &sources, &[], false).0);
    let mut emitted = Vec::new();
    sys::walk_dir(&generated, &mut |entry| {
        if !entry.is_dir() {
            emitted.push(sysutil::base(entry.path));
        }
        Ok(sys::Walk::Continue)
    })
    .unwrap();
    assert!(
        emitted.contains(&"Bagged_PropertyBag.cs".to_string()),
        "the Unity generator emitted nothing: {emitted:?}"
    );
    let pipe = server.pipe();
    assert_eq!(server.pid(), Some(pid), "the server was started again");
    drop(server);
    assert!(!process_alive(pid), "the server outlived its owner");
    assert!(
        processes_naming(&pipe).is_empty(),
        "a process named the pipe after the end"
    );
    assert!(
        sys::absent(&format!("/tmp/{pipe}")),
        "the socket was left behind"
    );
}

/// The server keeps reference metadata by path and modification time: a
/// reference rewritten in place with its old modification time is not read
/// again by it. The driver hands every reference with its content, and a
/// path with other content ends the server first.
#[test]
fn compiler_server_restarts_on_a_changed_reference_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set UME_LIFECYCLE_SDK to the matching Unity executable; SDK only",
    ) else {
        return;
    };
    let (_guard, root) = temp();
    let csc = Csc::new(&editor);
    let server = CompilerServer::new(&csc.dotnet, &csc.csc, &join(&[&root, "output"]));
    let directory = join(&[&root, "work"]);
    sys::mkdir_all(&directory).unwrap();
    let library = |value: u32, name: &str| {
        let source = join(&[&directory, &format!("{name}.cs")]);
        sys::write(
            &source,
            format!("public static class L {{ public const int X = {value}; }}\n").as_bytes(),
        )
        .unwrap();
        assert!(csc.compile(None, &directory, name, &[source], &[], false).0);
        join(&[&directory, &format!("{name}.dll")])
    };
    let lib = library(1, "Lib");
    let main = join(&[&directory, "Main.cs"]);
    sys::write(
        &main,
        b"public static class M { public const int Y = L.X; }\n",
    )
    .unwrap();
    let compile = |served: bool| {
        let result = csc.compile(
            served.then_some(&server),
            &directory,
            "Main",
            std::slice::from_ref(&main),
            std::slice::from_ref(&lib),
            false,
        );
        assert!(result.0, "{}", result.1);
        sha256(&result.2.unwrap())
    };
    let cold_first = compile(false);
    assert_eq!(compile(true), cold_first);
    let pid = server.pid().unwrap();
    // The reference gets other bytes of the same size and its old
    // modification time back.
    let modified = fs::metadata(&lib).unwrap().modified().unwrap();
    let other = fs::read(library(2, "Lib2")).unwrap();
    assert_eq!(other.len() as u64, fs::metadata(&lib).unwrap().len());
    std::thread::sleep(std::time::Duration::from_millis(20));
    sys::write(&lib, &other).unwrap();
    fs::File::options()
        .write(true)
        .open(&lib)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let served = compile(true);
    let cold = compile(false);
    assert_ne!(cold, cold_first, "the fixture did not change the reference");
    assert_eq!(
        served, cold,
        "the server compiled against the old reference"
    );
    assert!(
        !process_alive(pid),
        "the server that held the old reference still runs"
    );
}

/// A stand-in for `dotnet <program> --serve` that speaks the protocol of
/// `service.rs` in the way the file `mode` beside the program says, and for
/// `dotnet <program> <arguments>` run once.
fn stand_in_service(root: &str) -> (String, String, String) {
    let dotnet = write_graph_file(
        root,
        "sdk/dotnet",
        r#"#!/bin/sh
mode_file="$(dirname "$1")/mode"
if [ "$2" != "--serve" ]; then
  if [ "$2" = "fail" ]; then echo "Program.cs(1,1): error CS0103: boom" >&2; exit 1; fi
  echo "one-shot $2"; exit 0
fi
[ "$(cat "$mode_file")" = "noserve" ] && { echo "Unknown argument --serve"; exit 1; }
echo '{"ready":true,"protocol":1}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$(cat "$mode_file")" in
    ok) printf '{"id":%s,"exit":0,"output":"served %s\\n","error":"","kind":""}\n' "$id" "$id" ;;
    wrongid) printf '{"id":%s,"exit":0,"output":"","error":"","kind":""}\n' "$((id + 100))" ;;
    truncated) printf '{"id":%s,"exit":0' "$id"; exit 0 ;;
    exit) exit 3 ;;
    hang) sleep 300 ;;
    error) printf '{"id":%s,"exit":1,"output":"","error":"a defect of the helper\\n","kind":"error"}\n' "$id" ;;
    diagnostics) printf '{"id":%s,"exit":1,"output":"","error":"Program.cs(1,1): error CS0103: boom\\n","kind":"diagnostics"}\n' "$id" ;;
  esac
done
"#,
    );
    fs::set_permissions(&dotnet, fs::Permissions::from_mode(0o700)).unwrap();
    let program = write_graph_file(root, "helper/Helper.dll", "helper");
    let mode = join(&[root, "helper/mode"]);
    (dotnet, program, mode)
}

#[test]
fn service_answers_and_falls_back_on_every_failure_of_its_process() {
    let (_guard, root) = temp();
    let (dotnet, program, mode) = stand_in_service(&root);
    let set = |value: &str| sys::write(&mode, value.as_bytes()).unwrap();
    let ctx = background();
    let limit = std::time::Duration::from_millis(500);
    let service = Service::new("test helper", &dotnet, &program);
    set("ok");
    let reply = service.call(&ctx, &root, &["a"], limit).unwrap();
    assert_eq!((reply.exit, reply.output.as_str()), (0, "served 1\n"));
    let first = service.pid().unwrap();
    assert_eq!(
        service.call(&ctx, &root, &["b"], limit).unwrap().output,
        "served 2\n"
    );
    assert_eq!(
        service.pid(),
        Some(first),
        "a second request started another process"
    );
    for (failure, timed_out) in [
        ("wrongid", false),
        ("truncated", false),
        ("exit", false),
        ("hang", true),
    ] {
        set(failure);
        let error = service.call(&ctx, &root, &["c"], limit).unwrap_err();
        assert_eq!(
            matches!(error, ServiceError::TimedOut(_)),
            timed_out,
            "{failure}: {error}"
        );
        assert_eq!(service.pid(), None, "{failure}: the process was kept");
        assert!(!process_alive(first) || failure != "wrongid");
        set("ok");
        // The next request starts another process.
        assert!(
            service.call(&ctx, &root, &["d"], limit).is_ok(),
            "{failure}"
        );
        assert!(service.pid().is_some_and(|pid| pid != first));
    }
    let started = service.started();
    // Three failures in a row and the session runs the helper once per request.
    set("exit");
    for _ in 0..3 {
        assert!(service.call(&ctx, &root, &["e"], limit).is_err());
    }
    assert!(service.disabled());
    set("ok");
    assert!(service.call(&ctx, &root, &["f"], limit).is_err());
    assert_eq!(service.pid(), None);
    // The process that was running took the first, two new ones the others.
    assert_eq!(service.started(), started + 2);
    // A program without the mode is no service.
    let other = Service::new("test helper", &dotnet, &program);
    set("noserve");
    assert!(other.call(&ctx, &root, &["g"], limit).is_err());
    assert_eq!(other.pid(), None);
    // Dropping the handle ends the process.
    set("ok");
    let last = Service::new("test helper", &dotnet, &program);
    last.call(&ctx, &root, &["h"], limit).unwrap();
    let pid = last.pid().unwrap();
    drop(last);
    assert!(!process_alive(pid), "a dropped service still runs");
}

/// What the lifecycle helper's caller gets from the service is what the
/// helper run once gives: its output, or a compile error of the sources in
/// the same words. Any other failure of the service is not reported: the
/// request runs once more in a process of its own.
#[test]
fn helper_command_reports_only_what_a_one_shot_run_would() {
    let (_guard, root) = temp();
    let (dotnet, program, mode) = stand_in_service(&root);
    let set = |value: &str| sys::write(&mode, value.as_bytes()).unwrap();
    let ctx = background();
    let compiler = Compiler {
        dotnet: dotnet.clone(),
        serve_helper: true,
        ..Default::default()
    };
    set("ok");
    assert_eq!(
        compiler
            .helper_command(&ctx, &root, &[&program, "request.json"])
            .unwrap(),
        b"served 1\n"
    );
    for failure in ["error", "wrongid", "exit", "truncated"] {
        set(failure);
        assert_eq!(
            compiler
                .helper_command(&ctx, &root, &[&program, "request.json"])
                .unwrap(),
            b"one-shot request.json\n",
            "{failure}: a failure of the service was not run once more"
        );
    }
    set("diagnostics");
    let served = compiler
        .helper_command(&ctx, &root, &[&program, "fail"])
        .unwrap_err();
    let cold = compiler_command(&ctx, &root, &dotnet, &[&program, "fail"]).unwrap_err();
    assert_eq!(
        served.message, cold.message,
        "a compile error reads otherwise when served"
    );
    assert_eq!(served.output, cold.output);
    // Without the service: the helper runs once per request.
    let cold_compiler = Compiler {
        dotnet,
        ..Default::default()
    };
    set("ok");
    assert_eq!(
        cold_compiler
            .helper_command(&ctx, &root, &[&program, "x"])
            .unwrap(),
        b"one-shot x\n"
    );
}

/// The lifecycle helper served by one process gives, for the rewrite of an
/// assembly, `--type-definitions` and `--declaration-hashes`, the output and
/// the files its one-shot run gives, request after request; a compile error
/// in the same words; it refuses what it does not serve and a reference
/// whose bytes are not those the request names.
#[test]
fn compiler_lifecycle_service_matches_one_shot_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set UME_LIFECYCLE_SDK to the matching Unity executable; SDK only",
    ) else {
        return;
    };
    let (_guard, root) = temp();
    let ctx = background();
    let mut cold = Compiler {
        editor: editor.clone(),
        cache: join(&[&root, "compiler"]),
        ..Default::default()
    };
    cold.initialize_sdk().unwrap();
    let helper = cold.prepare_lifecycle_helper(&ctx).unwrap();
    let mut references: Vec<String> = cold.base_references.to_vec();
    references.extend(cold.engine_references.iter().cloned());
    let references = deduplicate_references(&references).to_vec();
    // The part of the bridge the rewrite compiles against.
    let bridge_source = write_graph_file(
        &root,
        "bridge/Bridge.cs",
        r#"using System; using UnityEngine; namespace UnityMicroEditor.Raw {
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawLifecycleInstrumentedAttribute : Attribute { public RawLifecycleInstrumentedAttribute(int version) {} }
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawScriptableObjectInstrumentedAttribute : Attribute { public RawScriptableObjectInstrumentedAttribute(int version) {} }
public static class RawLifecycle {
 public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
 public static bool ShouldCollect(MonoBehaviour target, Phase phase) => false;
 public static bool ShouldCollect(ScriptableObject target, Phase phase) => false;
 public static void Enqueue(MonoBehaviour target, Phase phase, Action callback, int order) {}
 public static void Enqueue(ScriptableObject target, Phase phase, Action callback, int order) {}
 public static bool IsActiveAndEnabled(Behaviour target) => true;
 public static void SetActive(GameObject target, bool value) {}
}}"#,
    );
    let bridge = join(&[&root, "bridge/UnityMicroEditor.dll"]);
    let mut arguments = vec![
        cold.csc.clone(),
        "-nologo".into(),
        "-target:library".into(),
        "-nostdlib+".into(),
        format!("-out:{bridge}"),
    ];
    arguments.extend(references.iter().map(|path| format!("-reference:{path}")));
    arguments.push(bridge_source);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    compiler_command(&ctx, &root, &cold.dotnet, &arguments).unwrap();
    let mut all_references = references.clone();
    all_references.push(bridge.clone());
    let reference_hashes: BTreeMap<String, String> = all_references
        .iter()
        .map(|path| (path.clone(), hash(path)))
        .collect();
    let state = |value: u32, broken: bool| {
        let text = format!(
            "using UnityEngine;\npublic class Probe : MonoBehaviour\n{{\n    public int seed;\n    void Awake() {{ seed += {value}; }}\n    void OnEnable() {{ seed++; }}\n    void Update() {{ seed += {value}; }}\n}}\n{}",
            if broken {
                "public class Broken { int x = missing; }\n"
            } else {
                ""
            }
        );
        let source = write_graph_file(&root, &format!("states/{value}-{broken}/Probe.cs"), &text);
        let folder = join(&[&root, &format!("states/{value}-{broken}/out")]);
        let request = join(&[&folder, "lifecycle-request.json"]);
        sys::mkdir_all(&folder).unwrap();
        atomic_json(
            &request,
            &json!({
                "AllowUnsafe": false,
                "AssemblyName": "Assembly-CSharp",
                "Defines": ["UNITY_6000_6"],
                "ExecutionOrders": {},
                "MarkerPath": join(&[&folder, "Marker.cs"]),
                "NativeBaseAssemblies": [],
                "ReferenceHashes": reference_hashes,
                "References": all_references,
                "Sources": [{"OriginalPath": join(&[&root, "Assets/Probe.cs"]), "OutputPath": join(&[&folder, "Probe.cs"]), "Path": source}],
            }),
        )
        .unwrap();
        (folder, request)
    };
    let files =
        |folder: &str| ["Probe.cs", "Marker.cs"].map(|name| fs::read(join(&[folder, name])).ok());
    let clear = |folder: &str| {
        for name in ["Probe.cs", "Marker.cs"] {
            let _ = fs::remove_file(join(&[folder, name]));
        }
    };
    let served = Compiler {
        dotnet: cold.dotnet.clone(),
        serve_helper: true,
        ..Default::default()
    };
    let states: Vec<(String, String)> = (1..=2).map(|value| state(value, false)).collect();
    let mut expected = Vec::new();
    for (folder, request) in &states {
        clear(folder);
        let output = compiler_command(&ctx, folder, &cold.dotnet, &[&helper, request]).unwrap();
        expected.push((output, files(folder)));
    }
    for round in 0..20 {
        let index = round % 2;
        let (folder, request) = &states[index];
        clear(folder);
        let output = served
            .helper_command(&ctx, folder, &[&helper, request])
            .unwrap();
        assert_eq!(
            output, expected[index].0,
            "round {round}: the served proof differs"
        );
        assert_eq!(
            files(folder),
            expected[index].1,
            "round {round}: the rewritten files differ"
        );
    }
    let service = served.helper_service.lock().unwrap().clone().unwrap();
    assert_eq!(service.started(), 1, "one process served every request");
    let pid = service.pid().unwrap();
    // A compile error: the same words, and the process stays.
    let (folder, request) = state(3, true);
    let cold_error =
        compiler_command(&ctx, &folder, &cold.dotnet, &[&helper, &request]).unwrap_err();
    let served_error = served
        .helper_command(&ctx, &folder, &[&helper, &request])
        .unwrap_err();
    assert!(
        served_error.message.contains("CS0103"),
        "{}",
        served_error.message
    );
    assert_eq!(served_error.message, cold_error.message);
    assert_eq!(service.pid(), Some(pid));
    // Metadata of two compiled assemblies, as the type and declaration checks ask.
    let dlls: Vec<String> = [
        "UnityEngine.CoreModule.dll",
        "UnityEngine.PhysicsModule.dll",
    ]
    .iter()
    .map(|name| {
        references
            .iter()
            .find(|path| path.ends_with(&format!("/{name}")))
            .unwrap()
            .clone()
    })
    .chain([bridge.clone()])
    .collect();
    for mode in ["--type-definitions", "--declaration-hashes"] {
        let mut arguments = vec![helper.as_str(), mode];
        arguments.extend(dlls.iter().map(String::as_str));
        let one_shot = compiler_command(&ctx, &root, &cold.dotnet, &arguments).unwrap();
        assert_eq!(
            served.helper_command(&ctx, &root, &arguments).unwrap(),
            one_shot,
            "{mode}"
        );
    }
    // A reference whose bytes are not those the request names fails the
    // served request, and the caller then runs it once.
    let (folder, request) = state(4, false);
    let mut document = read_json_file::<serde_json::Value>(&request).unwrap();
    document["ReferenceHashes"][&bridge] = json!(sha256(b"other bytes"));
    atomic_json(&request, &document).unwrap();
    let reply = service
        .call(
            &ctx,
            &folder,
            &[&request],
            std::time::Duration::from_secs(120),
        )
        .unwrap();
    assert!(
        reply.exit == 1
            && reply.kind == "error"
            && reply.error.contains("does not have the content"),
        "{reply:?}"
    );
    let output = served
        .helper_command(&ctx, &folder, &[&helper, &request])
        .unwrap();
    assert_eq!(
        output,
        compiler_command(&ctx, &folder, &cold.dotnet, &[&helper, &request]).unwrap()
    );
    // What runs game code, or is no request of a compile, is not served.
    for arguments in [
        vec!["--behaviour-objects", "/nowhere.json"],
        vec!["--behaviour-layouts", "/nowhere.json"],
        vec!["--source-type-declarations", "/nowhere.json"],
        vec!["a.json", "b.json"],
    ] {
        let reply = service
            .call(&ctx, &root, &arguments, std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            (reply.exit, reply.kind.as_str()),
            (64, "refused"),
            "{arguments:?}"
        );
    }
    assert_eq!(service.started(), 1);
    drop(served);
    drop(service);
    assert!(!process_alive(pid), "the served helper outlived its owner");
}

/// Makes a file look as written `seconds` ago: its modification time. The
/// change time cannot be set; a test that needs it old waits.
fn age(path: &str, seconds: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(seconds))
        .unwrap();
}

#[test]
fn hash_cache_answers_settled_files_and_reads_everything_else() {
    let (_guard, root) = temp();
    let settled = write_graph_file(&root, "settled.dll", "settled bytes");
    let same_size = write_graph_file(&root, "same-size.dll", "first  bytes");
    let replaced = write_graph_file(&root, "replaced.dll", "replaced bytes");
    for path in [&settled, &same_size, &replaced] {
        age(path, 60);
    }
    // Both times of a file must be two seconds old before its hash is kept.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let fresh = write_graph_file(&root, "fresh.dll", "fresh bytes");
    let cache = HashCache::on();
    for path in [&settled, &same_size, &replaced, &fresh] {
        assert_eq!(cache.file_hash(path).unwrap(), hash(path));
    }
    assert_eq!(cache.reads(), 4);
    // Settled and unchanged: answered without a read.
    for path in [&settled, &same_size, &replaced] {
        assert_eq!(cache.file_hash(path).unwrap(), hash(path));
    }
    assert_eq!(cache.reads(), 4, "a settled file was read again");
    // A file written within the last two seconds is read every time.
    assert_eq!(cache.file_hash(&fresh).unwrap(), hash(&fresh));
    assert_eq!(cache.reads(), 5, "a fresh file was answered from the cache");
    // Other bytes of the same size under the old modification time.
    let modified = fs::metadata(&same_size).unwrap().modified().unwrap();
    sys::write(&same_size, b"second bytes").unwrap();
    fs::File::options()
        .write(true)
        .open(&same_size)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        cache.file_hash(&same_size).unwrap(),
        sha256(b"second bytes")
    );
    // Replaced by a rename: another inode.
    let other = write_graph_file(&root, "other.dll", "other bytes");
    fs::rename(&other, &replaced).unwrap();
    assert_eq!(cache.file_hash(&replaced).unwrap(), sha256(b"other bytes"));
    // A file that is gone is an error, as it is without the cache.
    fs::remove_file(&settled).unwrap();
    assert!(cache.file_hash(&settled).is_err());
    // Off, every hash is a read.
    let off = HashCache::default();
    assert!(!off.is_on());
    assert_eq!(off.file_hash(&same_size).unwrap(), sha256(b"second bytes"));
    assert_eq!(off.reads(), 0);
}

/// A configuration input that changes between two compiles is refused
/// with the words it always had, also when its hash came from the cache.
#[test]
fn hash_cache_sees_a_configuration_input_that_changed() {
    let (_guard, root) = temp();
    let asmdef = write_graph_file(&root, "Assets/Game.asmdef", r#"{"name":"Game"}"#);
    age(&asmdef, 60);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let compiler = Compiler {
        project: root.clone(),
        configuration: NilMap::from([(asmdef.clone(), hash(&asmdef))]),
        hashes: HashCache::on(),
        ..Default::default()
    };
    compiler.check_fixed_configuration().unwrap();
    compiler.check_fixed_configuration().unwrap();
    assert_eq!(
        compiler.hashes.reads(),
        1,
        "the settled input was read twice"
    );
    let modified = fs::metadata(&asmdef).unwrap().modified().unwrap();
    sys::write(&asmdef, br#"{"name":"Gamf"}"#).unwrap();
    fs::File::options()
        .write(true)
        .open(&asmdef)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let error = compiler
        .check_fixed_configuration()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("managed assembly configuration/GUID changed; regenerate source graph"),
        "{error}"
    );
}

/// With the hashes of a session, the revision key still sees a tool, a
/// generator dependency and a file of the baseline Player that changed in
/// place under its old size and modification time.
#[test]
fn code_revision_identity_tracks_changes_with_the_hash_cache() {
    let (_guard, mut compiler, sources) = code_revision_fixture();
    compiler.hashes = HashCache::on();
    let ctx = background();
    // Old enough to be kept.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let key = compiler.code_revision_key(&ctx, &sources).unwrap();
    let reads = compiler.hashes.reads();
    assert_eq!(compiler.code_revision_key(&ctx, &sources).unwrap(), key);
    assert_eq!(
        compiler.hashes.reads(),
        reads,
        "settled tools were read again"
    );
    let staged = sysutil::dir(&compiler.player);
    for (path, text) in [
        (
            join(&[&sysutil::dir(&compiler.csc), "Microsoft.CodeAnalysis.dll"]),
            "Roslyn dependenca",
        ),
        (
            join(&[&sysutil::dir(&compiler.generators[0]), "Dependency.dll"]),
            "generator dependencz",
        ),
        (join(&[&staged, "UnityPlayer.so"]), "native Unitz"),
    ] {
        rewrite_keeping_size_and_mtime(&path, text);
        assert_ne!(
            compiler.code_revision_key(&ctx, &sources).unwrap(),
            key,
            "a changed input kept the key with the hash cache: {path}"
        );
    }
}

/// `UME_COMPILER_COLD=1` leaves the hash cache off, and with it the maps of
/// Player directories: each use reads the directory in full, as before a
/// session kept anything (caches6-4). With the cache on, the baseline is
/// read once.
#[test]
fn player_maps_are_kept_only_with_the_hash_cache() {
    let (_guard, mut compiler, _) = code_revision_fixture();
    let staged = sysutil::dir(&compiler.player);
    assert!(!compiler.hashes.is_on());
    let first = compiler.baseline_player_map(&staged).unwrap();
    let second = compiler.baseline_player_map(&staged).unwrap();
    assert!(!Arc::ptr_eq(&first, &second), "a cold compiler kept the map");
    assert!(compiler.known_player_map(&staged).is_none());
    compiler.hashes = HashCache::on();
    let first = compiler.baseline_player_map(&staged).unwrap();
    let second = compiler.baseline_player_map(&staged).unwrap();
    assert!(Arc::ptr_eq(&first, &second), "the warm compiler read the baseline again");
    assert!(compiler.known_player_map(&staged).is_some());
}

/// A runner that was killed leaves the socket of its server; the next
/// server of the same output removes it, and nothing of a runner that runs.
#[test]
fn compiler_server_removes_sockets_of_dead_runners() {
    let (_guard, root) = temp();
    let output = join(&[&root, "output"]);
    let prefix = format!("ume-{}", &sha256(output.as_bytes())[..16]);
    // A process id that runs nothing: the largest one not in /proc.
    let dead = (1..4_000_000u32)
        .rev()
        .find(|pid| fs::symlink_metadata(format!("/proc/{pid}")).is_err())
        .unwrap();
    let left = format!("/tmp/{prefix}-{dead}-3");
    let running = format!("/tmp/{prefix}-1-2");
    let other = format!("/tmp/ume-0000000000000000-{dead}-1");
    for path in [&left, &running, &other] {
        let _ = fs::remove_file(path);
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    }
    let (dotnet, csc) = stand_in_server(&root);
    let server = CompilerServer::new(&dotnet, &csc, &output);
    assert!(sys::absent(&left), "the socket of a dead runner stayed");
    assert!(
        !sys::absent(&running),
        "the socket of a running process was removed"
    );
    assert!(
        !sys::absent(&other),
        "the socket of another output was removed"
    );
    drop(server);
    for path in [&running, &other] {
        fs::remove_file(path).unwrap();
    }
}

/// A kept process above its memory limit is replaced before the next
/// request, and that is no failure: the session keeps using the service.
#[test]
fn kept_processes_are_replaced_above_their_memory_limit() {
    let (_guard, root) = temp();
    let (dotnet, program, mode) = stand_in_service(&root);
    sys::write(&mode, b"ok").unwrap();
    let ctx = background();
    let limit = std::time::Duration::from_secs(5);
    // Every process holds more than a kilobyte.
    let service = Service::new("test helper", &dotnet, &program).with_memory_limit(1024);
    let mut pids = Vec::new();
    for index in 0..5 {
        let reply = service.call(&ctx, &root, &["a"], limit).unwrap();
        assert_eq!(reply.exit, 0, "{index}");
        pids.push(service.pid().unwrap());
    }
    assert_eq!(
        service.started(),
        5,
        "a process above the limit was used again"
    );
    assert!(!service.disabled(), "a replacement counted as a failure");
    assert!(pids.iter().take(4).all(|pid| !process_alive(*pid)));
    // Within the limit the process stays.
    let kept = Service::new("test helper", &dotnet, &program);
    kept.call(&ctx, &root, &["a"], limit).unwrap();
    kept.call(&ctx, &root, &["a"], limit).unwrap();
    assert_eq!(kept.started(), 1);
    // The same for the compiler server.
    let (dotnet, csc) = stand_in_server(&root);
    let server =
        CompilerServer::new(&dotnet, &csc, &join(&[&root, "output"])).with_memory_limit(1024);
    let inputs = BTreeMap::new();
    let first = server.client(&inputs).unwrap();
    let second = server.client(&inputs).unwrap();
    assert_ne!(
        first.pipe, second.pipe,
        "a server above the limit was used again"
    );
    assert!(server.client(&inputs).is_some());
}
