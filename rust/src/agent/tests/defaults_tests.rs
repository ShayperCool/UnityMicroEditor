use super::*;
use crate::{
    authoring::{NativeDefaultsTemplate, native_component_types},
    compiler::NilMap,
    sysutil::testing::write_executable,
    util::agent_immutable_json,
};
use defaults::{
    DefaultsCatalog, DefaultsProfile, candidate_template_name, defaults_directory, immutable_file,
    read_defaults_catalog,
};
use serde_json::json;

pub(super) struct Fixture {
    pub service: Service,
    pub project: String,
    pub editor: String,
    pub profile: DefaultsProfile,
    pub hash: String,
    _guards: (TempDir, TempDir),
}
pub(super) fn fixture() -> Fixture {
    let (guard, project) = review_project();
    write(
        &join(&[&project, "Packages/manifest.json"]),
        r#"{"dependencies":{}}"#,
    );
    let (editor_guard, editor_root) = temp_dir();
    for relative in [
        "Data/Managed/UnityEngine",
        "Data/Resources/PackageManager/BuiltInPackages",
    ] {
        fs::create_dir_all(join(&[&editor_root, relative])).unwrap();
    }
    let editor = join(&[&editor_root, "Unity"]);
    write_executable(
        &editor,
        &format!(
            "#!/bin/sh\ntouch '{}'\nexit 99\n",
            join(&[&project, "editor-executed"])
        ),
    );
    write(
        &join(&[
            &editor_root,
            "Data/Managed/UnityEngine/UnityEngine.CoreModule.dll",
        ]),
        "synthetic native module fingerprint",
    );
    let service = Service::new(Options {
        project: project.clone(),
        editor: editor.clone(),
        no_editor: true,
        lifetime: Some(ctx()),
        ..Default::default()
    });
    let (profile, hash) = component_defaults_profile(&ctx(), &project, &editor).unwrap();
    Fixture {
        service,
        project,
        editor,
        profile,
        hash,
        _guards: (guard, editor_guard),
    }
}
fn install_catalog(fixture: &Fixture) {
    let directory = defaults_directory(&fixture.project, &fixture.hash);
    let mut catalog = DefaultsCatalog {
        schema_version: 1,
        profile: fixture.profile.clone(),
        profile_sha256: fixture.hash.clone(),
        templates: NilMap::make(),
        receipts: NilMap::make(),
        source_sha256: crate::sha256(b"source snapshot"),
    };
    for (class, name) in native_component_types() {
        let content = format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_ObjectHideFlags: 0\n  m_PrefabInstance: {{fileID: 0}}\n  m_Component:\n  - component: {{fileID: 2}}\n  - component: {{fileID: 3}}\n  m_Name: Template\n  m_IsActive: 0\n--- !u!4 &2\nTransform:\n  m_ObjectHideFlags: 0\n  m_PrefabInstance: {{fileID: 0}}\n  m_GameObject: {{fileID: 1}}\n  m_Father: {{fileID: 0}}\n  m_Children: []\n--- !u!{class} &3\n{name}:\n  m_ObjectHideFlags: 0\n  m_PrefabInstance: {{fileID: 0}}\n  m_GameObject: {{fileID: 1}}\n  m_Enabled: 1\n"
        );
        let full = format!("UnityEngine.{name}");
        catalog.templates.insert(
            full.clone(),
            NativeDefaultsTemplate {
                schema_version: 1,
                profile: fixture.hash.clone(),
                unity_version: fixture.profile.unity_version.clone(),
                unity_revision: fixture.profile.unity_revision.clone(),
                component_type: full.clone(),
                sha256: crate::sha256(content.as_bytes()),
                content: content.clone(),
            },
        );
        immutable_file(
            &join(&[&directory, &candidate_template_name(&full)]),
            content.as_bytes(),
        )
        .unwrap();
    }
    agent_immutable_json(&join(&[&directory, "catalog.json"]), &catalog).unwrap();
}

#[test]
fn cache_read_never_executes_editor() {
    let fixture = fixture();
    let service = &fixture.service;
    let missing = service.component_schema(
        &ctx(),
        &any_map(json!({"componentType": "BoxCollider", "requestId": "missing"})),
    );
    assert_eq!(code(&missing), "component_defaults_missing");
    install_catalog(&fixture);
    let value = service
        .component_schema(
            &ctx(),
            &any_map(json!({"componentType": "BoxCollider", "requestId": "cached"})),
        )
        .unwrap();
    assert!(is_map(&value));
    let result = decoded(&value);
    assert_eq!(result["backend"], "versioned_component_cache");
    assert_eq!(result["editorStarted"], false);
    assert_eq!(result["profileSha256"], fixture.hash.as_str());
    assert_eq!(result["components"].as_array().unwrap().len(), 1);
    assert!(
        service.client().is_none(),
        "cache read constructed an Editor client"
    );
    assert!(
        absent(&join(&[&fixture.project, "editor-executed"])),
        "ordinary schema query executed the Editor"
    );
    assert_eq!(code(&service.editor_client(&ctx())), "editor_disabled");
}

#[test]
fn cache_rejects_changed_native_file_and_sdk() {
    let fixture = fixture();
    install_catalog(&fixture);
    let directory = defaults_directory(&fixture.project, &fixture.hash);
    let read = || read_defaults_catalog(&fixture.project, &fixture.profile, &fixture.hash);
    read().unwrap();
    let path = join(&[&directory, "BoxCollider.unity"]);
    let contents = fs::read(&path).unwrap();
    write(&path, [contents.as_slice(), b"# changed\n"].concat());
    assert!(read().is_err(), "changed native default file was accepted");
    write(&path, &contents);
    let catalog_path = join(&[&directory, "catalog.json"]);
    let mut catalog: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    catalog["templates"]["UnityEngine.BoxCollider"]["unityRevision"] = json!("different");
    write(&catalog_path, bytes(&catalog));
    assert!(
        read().is_err(),
        "changed native default revision was accepted"
    );
    write(
        &join(&[
            &crate::sysutil::dir(&fixture.editor),
            "Data/Managed/UnityEngine/UnityEngine.CoreModule.dll",
        ]),
        "new native module",
    );
    let (_, changed) =
        component_defaults_profile(&ctx(), &fixture.project, &fixture.editor).unwrap();
    assert_ne!(
        changed, fixture.hash,
        "changed SDK module did not invalidate native defaults"
    );
}

#[test]
fn publication_guard_rejects_changed_settings() {
    let fixture = fixture();
    install_catalog(&fixture);
    let guard = fixture
        .service
        .component_defaults_guard(&ctx(), fixture.hash.clone());
    guard().unwrap();
    write(
        &join(&[&fixture.project, "ProjectSettings/PhysicsManager.asset"]),
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!55 &1\nPhysicsManager:\n  m_Gravity: {x: 0, y: -5, z: 0}\n",
    );
    assert_eq!(guard().unwrap_err().code, "component_defaults_changed");
}

#[test]
fn source_preparation_retry_keeps_resolved_seed_after_activation() {
    let (_guard, project) = temp_dir();
    let first_baseline = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/previous/current.json",
    ]);
    let service = Service::new(Options {
        project: project.clone(),
        editor: "/pinned/Unity".into(),
        scene: "Assets/First.unity".into(),
        no_editor: true,
        ..Default::default()
    });
    let previous = Preparation {
        schema_version: 1,
        project: project.clone(),
        scene: service.scene(),
        baseline: first_baseline.clone(),
        host_player: "/host/first".into(),
        native_catalog: "/catalog/first".into(),
        engine_profile: "/profile/first".into(),
        reference_graph: join(&[
            &project,
            "Library/UnityMicroEditor/native-bootstrap/graph.json",
        ]),
        backends: NilMap::from([("preparation", "authored_source".to_string())]),
        ..Default::default()
    };
    agent_immutable_json(
        &join(&[&crate::sysutil::dir(&first_baseline), "result.json"]),
        &previous,
    )
    .unwrap();
    service.select(&first_baseline, "Assets/First.unity");
    let directory = join(&[&project, "Library/UnityMicroEditor/preparations/retry"]);
    let first = service
        .source_preparation_seed(&AnyMap::new(), &directory)
        .unwrap();
    service.select("", "Assets/Second.unity");
    let second = service
        .source_preparation_seed(&AnyMap::new(), &directory)
        .unwrap();
    assert_eq!(second, first, "retry changed the accepted seed");
    assert_eq!(second.host_player, previous.host_player);
    assert_eq!(second.scene, "Assets/First.unity");
    assert!(!second.reference_graph.is_empty());
    assert!(
        service
            .source_preparation_seed(
                &any_map(json!({"scene": "Assets/Second.unity"})),
                &directory
            )
            .is_err(),
        "same request ID accepted different explicit options"
    );
}

#[test]
fn source_preparation_seed_does_not_use_own_graph_as_public_oracle() {
    let (_guard, project) = temp_dir();
    let baseline = join(&[
        &project,
        "Library/UnityMicroEditor/preparations/previous/current.json",
    ]);
    let service = Service::new(Options {
        project: project.clone(),
        editor: "/pinned/Unity".into(),
        scene: "Assets/Test.unity".into(),
        baseline: baseline.clone(),
        ..Default::default()
    });
    let previous = Preparation {
        schema_version: 1,
        project: project.clone(),
        scene: service.scene(),
        baseline: baseline.clone(),
        host_player: "/host/first".into(),
        backends: NilMap::from([("preparation", "authored_source".to_string())]),
        ..Default::default()
    };
    agent_immutable_json(
        &join(&[&crate::sysutil::dir(&baseline), "result.json"]),
        &previous,
    )
    .unwrap();
    let seed = service
        .source_preparation_seed(
            &AnyMap::new(),
            &join(&[&project, "Library/UnityMicroEditor/preparations/retry"]),
        )
        .unwrap();
    assert_eq!(seed.host_player, "/host/first");
    assert!(
        seed.reference_graph.is_empty(),
        "our graph was mislabeled as public engine evidence"
    );
}

#[test]
fn source_preparation_and_defaults_bootstrap_honor_no_editor() {
    let fixture = fixture();
    let service = &fixture.service;
    service.select("", "Assets/Test.unity");
    let outcome = service.prepare_source(&ctx(), &any_map(json!({"requestId": "missing-host"})));
    assert_eq!(
        code(&outcome),
        "native_host_missing",
        "source preparation did not fail before Editor bootstrap"
    );
    assert_eq!(
        code(&service.capture_component_defaults(&ctx())),
        "editor_disabled",
        "defaults bootstrap bypassed no-editor"
    );
    assert!(
        service.client().is_none(),
        "no-editor preparation constructed Editor client"
    );
    assert!(absent(&join(&[&fixture.project, "editor-executed"])));
}
