//! Source graph tests. The initializer test lives with the function it tests, in
//! compiler/tests.rs. SDK and real-project tests are gated on environment variables
//! and return early without them.
use super::*;
use crate::{
    compiler::{
        Compiler, CompilerPreparationResult, compiler_command, deduplicate_references,
        publish_baseline,
    },
    packages::{self, Snapshot},
    util::{agent_id, agent_read_json, atomic_json, marshal, read_json_file},
};
use serde::Deserialize;
use serde_json::json;
use std::{cell::Cell, fs, os::unix::fs::DirBuilderExt};
use tempfile::TempDir;

fn temp() -> (TempDir, String) {
    let directory = TempDir::new().unwrap();
    let path = fs::canonicalize(directory.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    (directory, path)
}
fn write_graph_file(project: &str, relative: &str, contents: &str) -> String {
    let filename = sysutil::join(&[project, relative]);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(sysutil::dir(&filename))
        .unwrap();
    fs::write(&filename, contents).unwrap();
    filename
}
fn strs(items: &[&str]) -> NilVec<String> {
    NilVec::from(items)
}
fn background() -> Context {
    Context::background()
}
fn resolve(project: &str, editor: &str) -> Snapshot {
    packages::resolve(
        &background(),
        project,
        editor,
        packages::Options {
            offline: true,
            ..Default::default()
        },
    )
    .unwrap()
}
fn signed(mut profile: SourceEngineProfile) -> SourceEngineProfile {
    profile.profile_sha256 = profile_hash(&profile).unwrap();
    profile
}
fn fixture_profile(base_defines: &[&str]) -> SourceEngineProfile {
    SourceEngineProfile {
        schema_version: 1,
        unity_version: UNITY_VERSION.into(),
        unity_revision: "45d8eee7de74".into(),
        target: "LinuxStandalone64".into(),
        scripting_backend: "Mono".into(),
        base_defines: strs(base_defines),
        native_assemblies: NilMap::make(),
        ..Default::default()
    }
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

#[test]
fn source_version_intervals_and_constraints() {
    for (current, expression, expected) in [
        ("1.20.0", "[1.19,2)", true),
        ("1.20.0", "(1.20,2)", false),
        ("1.20.0", "[1.20.0]", true),
        ("6000.6.3f1", "2022.2.14f1", true),
        ("1.0.0-preview.2", "1.0.0", false),
        ("2.0.0", "(,2)", false),
    ] {
        assert_eq!(
            match_version(current, expression).unwrap(),
            expected,
            "{current} in {expression}"
        );
    }
    let defines = strs(&["A"]);
    for (constraints, expected) in [
        (strs(&["A || B", "!C"]), true),
        (strs(&["A", "C"]), false),
        (strs(&["!A || C"]), false),
    ] {
        assert_eq!(
            match_constraints(&constraints, &defines).unwrap(),
            expected,
            "constraints {constraints:?}"
        );
    }
    assert!(
        match_constraints(&strs(&["A && B"]), &[]).is_err(),
        "unsupported constraints silently accepted"
    );
}

#[test]
fn source_graph_derives_guid_asmref_platform_constraints_and_predefined_dependencies() {
    let (_directory, project) = temp();
    write_graph_file(&project, "Packages/manifest.json", r#"{"dependencies":{}}"#);
    write_graph_file(
        &project,
        "Packages/packages-lock.json",
        r#"{"dependencies":{}}"#,
    );
    write_graph_file(
        &project,
        "Assets/A/A.asmdef",
        r#"{"name":"A","references":["GUID:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],"versionDefines":[{"name":"Unity","expression":"[6000.6,6001)","define":"NEW_ENGINE"}],"defineConstraints":["NEW_ENGINE || OPTIONAL"]}"#,
    );
    write_graph_file(
        &project,
        "Assets/A/A.asmdef.meta",
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    write_graph_file(
        &project,
        "Assets/B/B.asmdef",
        r#"{"name":"B","autoReferenced":false,"noEngineReferences":true}"#,
    );
    write_graph_file(
        &project,
        "Assets/B/B.asmdef.meta",
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    );
    write_graph_file(
        &project,
        "Assets/Attached/Assembly.asmref",
        r#"{"reference":"GUID:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
    );
    write_graph_file(
        &project,
        "Assets/OnlyEditor/Editor.asmdef",
        r#"{"name":"OnlyEditor","includePlatforms":["Editor"],"references":["AbsentEditorAssembly"]}"#,
    );
    write_graph_file(
        &project,
        "Assets/OnlyEditor/Editor.asmdef.meta",
        "guid: cccccccccccccccccccccccccccccccc\n",
    );
    for (index, path) in [
        "Assets/A/A.cs",
        "Assets/A/Editor/StillA.cs",
        "Assets/B/B.cs",
        "Assets/Attached/Attached.cs",
        "Assets/OnlyEditor/Skipped.cs",
        "Assets/Loose.cs",
        "Assets/Plugins/Early.cs",
        "Assets/Editor/Excluded.cs",
    ]
    .iter()
    .enumerate()
    {
        let letter = (b'A' + index as u8) as char;
        let digit = (b'1' + index as u8) as char;
        write_graph_file(&project, path, &format!("public class Type{letter} {{}}"));
        write_graph_file(
            &project,
            &format!("{path}.meta"),
            &format!("guid: {}\n", digit.to_string().repeat(32)),
        );
    }
    let options = SourceGraphOptions {
        project: project.clone(),
        profile: Some(signed(SourceEngineProfile {
            development: true,
            ..fixture_profile(&["UNITY_STANDALONE_LINUX", "ENABLE_MONO"])
        })),
        packages: Some(resolve(&project, "")),
        ..Default::default()
    };
    let graph = build_source_graph(&background(), &options).unwrap();
    let by_name = |name: &str| {
        graph
            .assemblies
            .iter()
            .find(|assembly| assembly.config.name == name)
            .map(|assembly| assembly.config.clone())
            .unwrap_or_default()
    };
    assert_eq!(graph.assemblies.len(), 4, "{:?}", graph.assemblies);
    assert_eq!(by_name("A").sources.len(), 3);
    assert!(
        by_name("A")
            .defines
            .iter()
            .any(|define| define == "NEW_ENGINE")
    );
    assert_eq!(by_name("A").dependencies, strs(&["B"]));
    assert_eq!(
        by_name("Assembly-CSharp").dependencies,
        strs(&["A", "Assembly-CSharp-firstpass"])
    );
    assert_eq!(by_name("Assembly-CSharp").sources.len(), 1);
    assert!(by_name("B").config.no_engine_references);
    let again = build_source_graph(&background(), &options).unwrap();
    assert_eq!(
        marshal(&graph).unwrap(),
        marshal(&again).unwrap(),
        "source graph varies across unchanged source scans"
    );
}

/// Explicit opt-in uses existing public reference only as comparison/profile
/// evidence. All assembly decisions in the result come from source scanning.
#[test]
fn source_graph_actual_comparison() {
    let Some(project) = sdk(
        "UME_SOURCE_GRAPH_PROJECT",
        "set UME_SOURCE_GRAPH_PROJECT, _REFERENCE, _PLAYER, _PACKAGES for actual source graph comparison",
    ) else {
        return;
    };
    let variable = |name: &str| std::env::var(name).unwrap_or_default();
    let editor = variable("UME_LIFECYCLE_SDK");
    let reference = variable("UME_SOURCE_GRAPH_REFERENCE");
    let player = variable("UME_SOURCE_GRAPH_PLAYER");
    let ctx = background();
    let snapshot = if variable("UME_SOURCE_GRAPH_RESOLVE") == "1" {
        packages::resolve(
            &ctx,
            &project,
            &editor,
            packages::Options {
                offline: variable("UME_SOURCE_GRAPH_OFFLINE") == "1",
                ..Default::default()
            },
        )
        .unwrap()
    } else {
        Snapshot::from_json(
            &crate::util::read_agent_file(&variable("UME_SOURCE_GRAPH_PACKAGES"), 32 << 20)
                .unwrap(),
        )
        .unwrap()
    };
    let output = sysutil::join(&[
        &project,
        "Library/UnityMicroEditor/source-tests",
        &format!("TestGoSourceGraphActualComparison-{}", agent_id()),
    ]);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&output)
        .unwrap();
    let profile = capture_engine_profile(
        &ctx,
        &project,
        &editor,
        &player,
        &reference,
        &sysutil::join(&[&output, "engine-profile.json"]),
        Some(&snapshot),
    )
    .unwrap();
    let options = SourceGraphOptions {
        project: project.clone(),
        editor: editor.clone(),
        player,
        profile: Some(profile),
        packages: Some(snapshot),
        editor_domain: false,
    };
    let graph = build_source_graph(&ctx, &options).unwrap();
    atomic_json(&sysutil::join(&[&output, "graph.json"]), &graph).unwrap();
    let public_graph: CompilerGraphExport = agent_read_json::<Option<_>>(&reference, 32 << 20)
        .unwrap()
        .unwrap_or_default();
    let differences = compare_source_graphs(&graph, &public_graph);
    let comparison = sysutil::join(&[&output, "comparison.json"]);
    atomic_json(&comparison, &differences).unwrap();
    eprintln!(
        "own graph {} SDK +{} native assemblies; comparison {}; output {output}",
        graph.assemblies.len(),
        graph.host_assemblies.len(),
        String::from_utf8(marshal(&differences).unwrap()).unwrap()
    );
    if variable("UME_SOURCE_GRAPH_REQUIRE_MATCH") == "1" {
        assert!(
            differences.is_empty(),
            "own graph differs from explicit public reference; see {comparison}"
        );
    }
    if variable("UME_SOURCE_GRAPH_COMPILE") == "1" {
        let prepared = sysutil::join(&[&output, "compiler.json"]);
        let result = prepare_source_compiler_graph(&ctx, &options, &prepared).unwrap();
        assert_eq!(
            result.compiler.backend, "authored_source",
            "source compilation lost its backend"
        );
        let mut compiler = Compiler::new(
            &project,
            &prepared,
            &result.player,
            &editor,
            &sysutil::join(&[&output, "restart-cache"]),
        )
        .unwrap();
        compiler.compile(&ctx, None).unwrap();
        compiler.prepared_manifest(&ctx).unwrap();
        eprintln!(
            "own SDK compiler/restart accepted {} assemblies; Player {}",
            result.compiler.assemblies.len(),
            result.player
        );
    }
}

#[test]
fn source_plugin_v2_v3_linux_and_constraints() {
    for (name, metadata, defines, enabled, explicit) in [
        (
            "v2",
            "PluginImporter:\n  isExplicitlyReferenced: 1\n  platformData:\n  - first:\n      : Any\n    second:\n      enabled: 1\n      settings:\n        Exclude Linux64: 1\n  - first:\n      Standalone: Linux64\n    second:\n      enabled: 1\n      settings: {}\n",
            NilVec::nil(),
            true,
            true,
        ),
        (
            "v3",
            "PluginImporter:\n  defineConstraints: [DEVELOPMENT_BUILD]\n  platformData:\n    Any:\n      enabled: 1\n      settings: {}\n    Linux64:\n      enabled: 0\n      settings: {}\n",
            strs(&["DEVELOPMENT_BUILD"]),
            false,
            false,
        ),
        (
            "constraint",
            "PluginImporter:\n  defineConstraints: [UNITY_EDITOR]\n  platformData:\n    Any:\n      enabled: 1\n      settings: {}\n",
            NilVec::nil(),
            false,
            false,
        ),
    ] {
        let (_directory, root) = temp();
        let path = write_graph_file(&root, "Plugin.dll.meta", metadata);
        let plugin = read_source_plugin(path.strip_suffix(".meta").unwrap(), &defines).unwrap();
        assert_eq!(
            (plugin.enabled, plugin.explicit),
            (enabled, explicit),
            "{name}: {plugin:?}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            metadata,
            "legacy metadata was rewritten"
        );
    }
}

#[test]
fn source_response_explicit_plugins_and_option_order() {
    let (_directory, project) = temp();
    let source = write_graph_file(
        &project,
        "Assets/Assembly/Probe.cs",
        "public class Probe {}",
    );
    write_graph_file(
        &project,
        "Assets/csc.rsp",
        "-define:FROM_RSP -unsafe+ -optimize+ -optimize- -r:Chosen.dll\n",
    );
    let chosen = sysutil::join(&[&project, "Assets/Chosen.dll"]);
    let plugins = BTreeMap::from([
        (
            "Chosen.dll".to_string(),
            SourcePlugin {
                path: chosen.clone(),
                enabled: true,
                explicit: true,
                ..Default::default()
            },
        ),
        (
            "Automatic.dll".to_string(),
            SourcePlugin {
                path: sysutil::join(&[&project, "Assets/Automatic.dll"]),
                enabled: true,
                ..Default::default()
            },
        ),
    ]);
    let mut config = AssemblyConfig {
        name: "Assembly".into(),
        ..Default::default()
    };
    config.config.override_references = true;
    config.config.precompiled = strs(&["Chosen.dll"]);
    let mut inputs = NilMap::make();
    let sources = [source];
    apply_response_files(&project, &sources, &[], &mut config, &plugins, &mut inputs).unwrap();
    let references = plugin_references(&config, &plugins).unwrap();
    assert_eq!(references, NilVec::from([chosen]));
    assert!(config.config.allow_unsafe);
    assert_eq!(config.defines, strs(&["FROM_RSP"]));
    assert_eq!(config.compiler_options, strs(&["-optimize+", "-optimize-"]));
    config.config.precompiled = strs(&["Missing.dll"]);
    assert!(
        plugin_references(&config, &plugins).is_err(),
        "missing explicit plugin accepted"
    );
    write_graph_file(&project, "Assets/csc.rsp", "-analyzer:foreign.dll");
    assert!(
        apply_response_files(&project, &sources, &[], &mut config, &plugins, &mut inputs).is_err(),
        "unsupported analyzer silently accepted"
    );
}

#[test]
fn source_native_optional_reference_requires_unchanged_package_proof() {
    let (_directory, project) = temp();
    fs::create_dir(sysutil::join(&[&project, "Assets"])).unwrap();
    write_graph_file(
        &project,
        "Packages/manifest.json",
        r#"{"dependencies":{"com.test.native":"1.0.0"}}"#,
    );
    write_graph_file(
        &project,
        "Packages/com.test.native/package.json",
        r#"{"name":"com.test.native","version":"1.0.0"}"#,
    );
    let definition = write_graph_file(
        &project,
        "Packages/com.test.native/Native.asmdef",
        r#"{"name":"Native","references":["GUID:ffffffffffffffffffffffffffffffff"]}"#,
    );
    write_graph_file(
        &project,
        "Packages/com.test.native/Native.asmdef.meta",
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    let source = write_graph_file(
        &project,
        "Packages/com.test.native/Native.cs",
        "public class Native {}",
    );
    let meta = write_graph_file(
        &project,
        "Packages/com.test.native/Native.cs.meta",
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    );
    let snapshot = resolve(&project, "");
    let mut profile = fixture_profile(&["ENABLE_MONO", "UNITY_STANDALONE_LINUX"]);
    profile.native_assemblies.insert(
        "Native",
        SourceNativeAssembly {
            name: "Native".into(),
            package: "com.test.native".into(),
            package_content_hash: snapshot.packages[0].content_hash.clone(),
            unresolved_references: strs(&["GUID:ffffffffffffffffffffffffffffffff"]),
            configuration: AssemblyConfig {
                defines: profile.base_defines.clone(),
                ..Default::default()
            },
            source_hashes: NilMap::from([
                (
                    "Packages/com.test.native/Native.cs",
                    file_hash(&source).unwrap(),
                ),
                (
                    "Packages/com.test.native/Native.cs.meta",
                    file_hash(&meta).unwrap(),
                ),
            ]),
            ..Default::default()
        },
    );
    let mut options = SourceGraphOptions {
        project: project.clone(),
        profile: Some(signed(profile)),
        packages: Some(snapshot),
        ..Default::default()
    };
    build_source_graph(&background(), &options).unwrap();
    write_graph_file(
        &project,
        "Packages/com.test.native/Native.cs",
        "public class Native { public int changed; }",
    );
    options.packages = Some(resolve(&project, ""));
    assert!(
        build_source_graph(&background(), &options).is_err(),
        "modified package inherited absent reference exception"
    );
    decode_asmdef(&fs::read(definition).unwrap()).unwrap();
    assert!(
        !test_assembly(&SourceAsmdef {
            name: "Ordinary".into(),
            define_constraints: strs(&["DEBUG"]),
            ..Default::default()
        }),
        "ordinary development assembly treated as test"
    );
    assert!(
        test_assembly(&SourceAsmdef {
            name: "Performance".into(),
            precompiled_references: strs(&["nunit.framework.dll"]),
            ..Default::default()
        }),
        "explicit test dependency included in Player profile"
    );
}

#[test]
fn source_semver_prerelease_numeric_parts() {
    assert!(
        match_version("1.0.0-preview.10", "1.0.0-preview.2").unwrap(),
        "numeric prerelease precedence"
    );
}

#[test]
fn source_profile_unproves_native_body_drift_and_rejects_wrong_public_backend() {
    let (_directory, project) = temp();
    let ctx = background();
    write_graph_file(&project, "Assets/Keep.txt", "fixture");
    write_graph_file(
        &project,
        "ProjectSettings/ProjectVersion.txt",
        &format!("m_EditorVersion: {UNITY_VERSION}\n"),
    );
    write_graph_file(
        &project,
        "Packages/manifest.json",
        r#"{"dependencies":{"com.test.native":"1.0.0"}}"#,
    );
    write_graph_file(
        &project,
        "Packages/com.test.native/package.json",
        r#"{"name":"com.test.native","version":"1.0.0"}"#,
    );
    let source = write_graph_file(
        &project,
        "Packages/com.test.native/Native.cs",
        "public class Native { public static int Read() => 1; }",
    );
    let meta = write_graph_file(
        &project,
        "Packages/com.test.native/Native.cs.meta",
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    let editor = write_graph_file(&project, "Temp/sdk/Unity", "SDK fixture, never executed");
    let player = write_graph_file(
        &project,
        "Temp/player/Host.x86_64",
        "native fixture, never executed",
    );
    let managed = managed_directory(&player);
    let dll = write_graph_file(
        &project,
        "Temp/player/Host_Data/Managed/Native.dll",
        "native DLL fixture",
    );
    write_graph_file(
        &project,
        "Temp/player/Host_Data/ScriptingAssemblies.json",
        r#"{"names":["Native.dll"],"types":[16]}"#,
    );
    let bcl = write_graph_file(
        &project,
        "Temp/sdk/Data/NetStandard/ref/2.1.0/netstandard.dll",
        "BCL fixture",
    );
    let engine = write_graph_file(
        &project,
        "Temp/sdk/Data/PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed/UnityEngine.dll",
        "engine fixture",
    );
    let config = AssemblyConfig {
        name: "Native".into(),
        sources: NilVec::from([source.clone()]),
        defines: strs(&["ENABLE_MONO", "UNITY_STANDALONE_LINUX"]),
        ..Default::default()
    };
    let mut graph = CompilerGraphExport {
        schema_version: 1,
        backend: "unity_editor".into(),
        development: true,
        unity_version: UNITY_VERSION.into(),
        target: "StandaloneLinux64".into(),
        scripting_backend: "Mono2x".into(),
        project: project.clone(),
        unity: editor.clone(),
        host_assemblies: NilVec::from([CompilerHostAssembly {
            name: "Native".into(),
            path: dll.clone(),
            sources: config.sources.clone(),
            defines: config.defines.clone(),
            configuration: Some(Box::new(config.clone())),
            compiled_references: NilVec::from([bcl.clone(), engine.clone()]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let graph_path = sysutil::join(&[&project, "Temp/graph.json"]);
    atomic_json(&graph_path, &graph).unwrap();
    let logical = "Packages/com.test.native/Native.cs";
    let mut compiled = config.clone();
    compiled.sources = strs(&[logical]);
    let hash = |path: &str| file_hash(path).unwrap();
    let proof_path = sysutil::join(&[
        &sysutil::dir(&managed),
        "UnityMicroEditorSourceProfile.json",
    ]);
    let proof = json!({
        "schemaVersion": 1, "backend": "unity_editor_build_sources", "unityVersion": UNITY_VERSION,
        "target": "StandaloneLinux64", "scriptingBackend": "Mono2x", "development": true,
        "assemblies": {"Native": {
            "name": "Native", "dllSha256": hash(&dll), "configuration": compiled,
            "sources": {(logical): hash(&source), (format!("{logical}.meta")): hash(&meta)},
            "references": {(bcl.as_str()): hash(&bcl), (engine.as_str()): hash(&engine)},
        }},
    });
    atomic_json(&proof_path, &proof).unwrap();
    let snapshot = || {
        let mut snapshot = resolve(&project, "");
        snapshot.editor = editor.clone();
        snapshot
    };
    let output = sysutil::join(&[&project, "Temp/profiles/valid.json"]);
    let capture = |graph: &str, output: &str, snapshot: &Snapshot| {
        capture_engine_profile(
            &ctx,
            &project,
            &editor,
            &player,
            graph,
            output,
            Some(snapshot),
        )
    };
    capture(&graph_path, &output, &snapshot()).unwrap();
    load_engine_profile(&ctx, &output, &editor, &player).unwrap();
    fs::write(
        &source,
        "public class Native { public static int Read() => 2; }",
    )
    .unwrap();
    let snapshot = snapshot();
    // A changed body is no longer the host's: the profile keeps the DLL
    // without a source proof, so the graph compiles the assembly instead.
    let drift = sysutil::join(&[&project, "Temp/profiles/drift.json"]);
    capture(&graph_path, &drift, &snapshot).unwrap();
    let profile = load_engine_profile(&ctx, &drift, &editor, &player).unwrap();
    let fact = profile.native_assemblies.value("Native");
    assert!(
        fact.package.is_empty() && fact.source_hashes.is_empty(),
        "same type/name changed native method body still proven by the host: {fact:?}"
    );
    let failed = sysutil::join(&[&project, "Temp/profiles/relabeled.json"]);
    graph.backend = "authored_source".into();
    atomic_json(&graph_path, &graph).unwrap();
    let error = capture(&graph_path, &failed, &snapshot).unwrap_err();
    assert!(
        error.to_string().contains("explicit public Development"),
        "own graph relabeled as public profile: {error}"
    );
}

/// The native executable/bridge are deliberately synthetic SDK fixtures. No
/// public Editor evidence is asserted, and neither Editor nor Player executes.
/// Production profile capture is exercised separately against a real build.
#[test]
fn source_graph_new_asmdef_package_and_body_change_sdk() {
    let Some(editor) = sdk(
        "UME_LIFECYCLE_SDK",
        "set UME_LIFECYCLE_SDK; SDK only, no Editor or Player starts",
    ) else {
        return;
    };
    let (_directory, project) = temp();
    let ctx = background();
    write_graph_file(
        &project,
        "ProjectSettings/ProjectVersion.txt",
        &format!("m_EditorVersion: {UNITY_VERSION}\n"),
    );
    write_graph_file(
        &project,
        "Packages/manifest.json",
        r#"{"dependencies":{"com.test.pure":"1.0.0"}}"#,
    );
    write_graph_file(
        &project,
        "Packages/com.test.pure/package.json",
        r#"{"name":"com.test.pure","version":"1.0.0"}"#,
    );
    write_graph_file(
        &project,
        "Packages/com.test.pure/Pure.asmdef",
        r#"{"name":"Pure.Package","autoReferenced":false,"versionDefines":[{"name":"Unity","expression":"[6000.6,6001)","define":"PINNED_UNITY"}],"defineConstraints":["PINNED_UNITY"]}"#,
    );
    write_graph_file(
        &project,
        "Packages/com.test.pure/Pure.asmdef.meta",
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    let pure = write_graph_file(
        &project,
        "Packages/com.test.pure/Counter.cs",
        "namespace Pure { public static class Counter { public static int Read() => 7; } }",
    );
    write_graph_file(
        &project,
        "Packages/com.test.pure/Counter.cs.meta",
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    );
    write_graph_file(
        &project,
        "Assets/Game/Game.asmdef",
        r#"{"name":"New.Game","references":["GUID:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}"#,
    );
    write_graph_file(
        &project,
        "Assets/Game/Game.asmdef.meta",
        "guid: cccccccccccccccccccccccccccccccc\n",
    );
    let probe = write_graph_file(
        &project,
        "Assets/Game/Probe.cs",
        "public class Probe : UnityEngine.MonoBehaviour { public int revision = Pure.Counter.Read(); private void Awake() {} }",
    );
    write_graph_file(
        &project,
        "Assets/Game/Probe.cs.meta",
        "guid: dddddddddddddddddddddddddddddddd\nMonoImporter:\n  executionOrder: 0\n",
    );
    let mut tools = Compiler {
        editor: editor.clone(),
        ..Default::default()
    };
    tools.initialize_sdk().unwrap();
    let player = write_graph_file(
        &project,
        "Temp/native/Host.x86_64",
        "synthetic SDK executable, never launched",
    );
    let managed = managed_directory(&player);
    let bridge_source = write_graph_file(
        &project,
        "Temp/Bridge.cs",
        r#"namespace UnityMicroEditor { public sealed class InvokeOnHotReloadAttribute : System.Attribute {} }"#,
    );
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&managed)
        .unwrap();
    let mut references = tools.base_references.to_vec();
    references.extend(tools.engine_references.iter().cloned());
    let mut arguments = vec![
        tools.csc.clone(),
        "-nologo".into(),
        "-target:library".into(),
        "-nostdlib+".into(),
        format!(
            "-out:{}",
            sysutil::join(&[&managed, "UnityMicroEditor.dll"])
        ),
    ];
    arguments.extend(
        deduplicate_references(&references)
            .iter()
            .map(|path| format!("-reference:{path}")),
    );
    arguments.push(bridge_source);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    compiler_command(&ctx, &project, &tools.dotnet, &arguments).unwrap();
    // A Player holds the engine's modules, which the runtime bridge is
    // compiled against.
    for module in tools.engine_references.iter() {
        std::os::unix::fs::symlink(module, sysutil::join(&[&managed, &sysutil::base(module)]))
            .unwrap();
    }
    fs::write(
        sysutil::join(&[&project, "Temp/native/Host_Data/globalgamemanagers"]),
        crate::compiler::settings::empty_managers(),
    )
    .unwrap();
    write_graph_file(
        &project,
        "Temp/native/Host_Data/ScriptingAssemblies.json",
        r#"{"names":["UnityMicroEditor.dll"],"types":[16],"retainNative":true}"#,
    );
    write_graph_file(
        &project,
        "Temp/native/Host_Data/RuntimeInitializeOnLoads.json",
        r#"{"root":[{"assemblyName":"Pure.Package","methodName":"Old"},{"assemblyName":"Native.Engine","methodName":"Retain"}]}"#,
    );
    let mut profile = SourceEngineProfile {
        editor: editor.clone(),
        player: player.clone(),
        development: true,
        base_references: tools.base_references.clone(),
        engine_references: tools.engine_references.clone(),
        inputs: NilMap::make(),
        ..fixture_profile(&["UNITY_6000_6", "UNITY_STANDALONE_LINUX", "ENABLE_MONO"])
    };
    for path in &references {
        profile.inputs.insert(path, file_hash(path).unwrap());
    }
    let profile = signed(profile);
    let profile_sha256 = profile.profile_sha256.clone();
    let mut options = SourceGraphOptions {
        project: project.clone(),
        editor: editor.clone(),
        player,
        profile: Some(profile),
        packages: Some(resolve(&project, &editor)),
        editor_domain: false,
    };
    let output = sysutil::join(&[&project, "Temp/own/first.json"]);
    // The publication seam is exercised with a counting wrapper of the default.
    let published = Cell::new(0);
    let publish = |path: &str, value: &CompilerPreparationResult| {
        published.set(published.get() + 1);
        publish_baseline(path, value)
    };
    let result = prepare_source_compiler_graph_with(&ctx, &options, &output, &publish).unwrap();
    assert_eq!(published.get(), 2, "intermediate and final publication");
    assert_eq!(result.compiler.assemblies.len(), 2, "{:?}", result.compiler);
    assert_eq!(result.compiler.backend, "authored_source");
    assert!(
        !result.compiler.scripts.last().unwrap().guid.is_empty(),
        "GUID binding absent"
    );
    assert!(
        result.compiler.scripts.iter().any(|script| {
            script.path == probe
                && script.guid == "d".repeat(32)
                && script.type_name == "Probe"
                && script.assembly == "New.Game"
        }),
        "new GUID/full type/assembly assignment lost"
    );
    #[derive(Deserialize)]
    struct Slots {
        names: Vec<String>,
        #[serde(rename = "retainNative")]
        retain: bool,
    }
    let slots: Slots = read_json_file(&sysutil::join(&[
        &sysutil::dir(&managed_directory(&result.player)),
        "ScriptingAssemblies.json",
    ]))
    .unwrap();
    assert!(
        slots.retain
            && slots.names.iter().any(|name| name == "Pure.Package.dll")
            && slots.names.iter().any(|name| name == "New.Game.dll"),
        "new assembly slots clobbered native profile: {:?}",
        slots.names
    );
    let mut compiler = Compiler::new(
        &project,
        &output,
        &result.player,
        &editor,
        &sysutil::join(&[&project, "Temp/restart"]),
    )
    .unwrap();
    compiler.compile(&ctx, None).unwrap();
    let accepted = compiler.prepared_manifest(&ctx).unwrap();
    assert_eq!(
        accepted.engine_profile_sha256, profile_sha256,
        "restart lost source provenance"
    );
    let old_hash = result.compiler.assemblies[0].sha256.clone();
    fs::write(
        &pure,
        "namespace Pure { public static class Counter { public static int Read() => 13; } }",
    )
    .unwrap();
    options.packages = Some(resolve(&project, &editor));
    let next = prepare_source_compiler_graph(
        &ctx,
        &options,
        &sysutil::join(&[&project, "Temp/own/changed.json"]),
    )
    .unwrap();
    assert_ne!(
        next.compiler.assemblies[0].sha256, old_hash,
        "package body change reused old native/SDK DLL"
    );
    assert!(
        fs::metadata(sysutil::join(&[&project, "Library/ScriptAssemblies"])).is_err(),
        "own preparation relied on Editor-produced assembly directory"
    );
}

#[test]
fn module_packages_reference_their_engine_module_assemblies() {
    let (_directory, root) = temp();
    let support = format!("{root}/Editor/Data/PlaybackEngines/LinuxStandaloneSupport");
    let managed = format!("{support}/Variations/mono/Managed");
    fs::create_dir_all(&managed).unwrap();
    fs::write(
        format!("{support}/modules.asset"),
        "PlatformModuleSetup:\n  modules:\n  - name: Core\n    dependencies: []\n    controlledByBuiltinPackage: 0\n  - name: AI\n    dependencies:\n      - Core\n    controlledByBuiltinPackage: 1\n  - name: ParticleSystem\n    dependencies: []\n    controlledByBuiltinPackage: 1\n",
    )
    .unwrap();
    for module in ["Core", "AI"] {
        fs::write(format!("{managed}/UnityEngine.{module}Module.dll"), b"").unwrap();
    }
    let editor = format!("{root}/Editor/Unity");
    let found = package_module_references(
        &editor,
        [
            "com.unity.modules.ai",
            "com.unity.modules.core",
            "com.unity.modules.particlesystem",
            "com.unity.ai.navigation",
        ]
        .into_iter(),
    )
    .unwrap();
    // Core is not controlled by a package; the particle system module has no
    // assembly in this installation.
    assert_eq!(found, vec![format!("{managed}/UnityEngine.AIModule.dll")]);
    assert!(
        package_module_references(
            &format!("{root}/Missing/Unity"),
            ["com.unity.modules.ai"].into_iter()
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn the_runtime_package_leaves_out_a_reference_the_project_lacks() {
    for (name, accepted) in [
        (crate::packages::RUNTIME_PACKAGE, true),
        ("com.test.embedded", false),
    ] {
        let (_directory, project) = temp();
        fs::create_dir(sysutil::join(&[&project, "Assets"])).unwrap();
        write_graph_file(&project, "Packages/manifest.json", r#"{"dependencies":{}}"#);
        write_graph_file(
            &project,
            &format!("Packages/{name}/package.json"),
            &format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        );
        write_graph_file(
            &project,
            &format!("Packages/{name}/Runtime.asmdef"),
            r#"{"name":"Runtime","references":["Unity.Addressables"]}"#,
        );
        write_graph_file(
            &project,
            &format!("Packages/{name}/Runtime.asmdef.meta"),
            "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        );
        write_graph_file(
            &project,
            &format!("Packages/{name}/Runtime.cs"),
            "public class Runtime {}",
        );
        write_graph_file(
            &project,
            &format!("Packages/{name}/Runtime.cs.meta"),
            "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        );
        let options = SourceGraphOptions {
            project: project.clone(),
            profile: Some(signed(fixture_profile(&[
                "ENABLE_MONO",
                "UNITY_STANDALONE_LINUX",
            ]))),
            packages: Some(resolve(&project, "")),
            ..Default::default()
        };
        let result = build_source_graph(&background(), &options);
        assert_eq!(result.is_ok(), accepted, "{name}: {:?}", result.err());
    }
}

#[test]
fn any_platform_is_the_unnamed_any_entry_alone() {
    // Newtonsoft.Json's AOT build: "Any Platform" off and the Editor
    // excluded; the older `Any:` entry beside it changes nothing.
    let (_directory, project) = temp();
    let path = write_graph_file(&project, "Packages/p/AOT/Newtonsoft.Json.dll", "dll");
    write_graph_file(
        &project,
        "Packages/p/AOT/Newtonsoft.Json.dll.meta",
        "PluginImporter:\n  platformData:\n  - first:\n      '': Any\n    second:\n      enabled: 0\n      settings:\n        Exclude Editor: 1\n        Exclude Linux64: 0\n  - first:\n      Any: \n    second:\n      enabled: 1\n      settings: {}\n  - first:\n      Editor: Editor\n    second:\n      enabled: 0\n      settings: {}\n  - first:\n      Standalone: Linux64\n    second:\n      enabled: 1\n      settings: {}\n",
    );
    assert!(!options::read_editor_plugin(&path, &[]).unwrap().enabled);
    assert!(options::read_source_plugin(&path, &[]).unwrap().enabled);
    // Collections' ILSupport: only the older `Any:` entry, which then counts.
    let legacy = write_graph_file(&project, "Packages/p/ILSupport.dll", "dll");
    write_graph_file(
        &project,
        "Packages/p/ILSupport.dll.meta",
        "PluginImporter:\n  platformData:\n  - first:\n      Any: \n    second:\n      enabled: 1\n      settings: {}\n  - first:\n      Editor: Editor\n    second:\n      enabled: 0\n      settings: {}\n",
    );
    assert!(options::read_editor_plugin(&legacy, &[]).unwrap().enabled);
    assert!(options::read_source_plugin(&legacy, &[]).unwrap().enabled);
}

#[test]
fn editor_test_runner_users_get_nunit_with_overridden_references() {
    let plugin = |path: &str| SourcePlugin {
        path: path.into(),
        enabled: true,
        ..Default::default()
    };
    let plugins = BTreeMap::from([
        (
            "nunit.framework.dll".to_string(),
            plugin("/nunit.framework.dll"),
        ),
        (
            "Newtonsoft.Json.dll".to_string(),
            plugin("/Newtonsoft.Json.dll"),
        ),
    ]);
    let config = |dependencies: &[&str]| AssemblyConfig {
        name: "Overridden".into(),
        dependencies: strs(dependencies),
        config: crate::compiler::AssemblyOptions {
            override_references: true,
            precompiled: strs(&["Newtonsoft.Json.dll"]),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        editor_plugin_references(&config(&["UnityEditor.TestRunner"]), &plugins, "/editor")
            .to_vec(),
        vec![
            "/Newtonsoft.Json.dll".to_string(),
            "/nunit.framework.dll".to_string()
        ],
        "an assembly that refers to the test runner compiles against NUnit"
    );
    assert_eq!(
        editor_plugin_references(&config(&["UnityEngine.UI"]), &plugins, "/editor").to_vec(),
        vec!["/Newtonsoft.Json.dll".to_string()],
        "one that does not gets only what it names"
    );
}
