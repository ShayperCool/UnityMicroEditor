//! Tests of imported bindings, imported source closures and the asset reference worklist.
use super::{Fixture, fixture_session, json, message, write_fixture_file};
use crate::project::{
    NeedsImportError, Options, Session,
    expand::Expanded,
    imported_bindings::prepared_asset_identity,
    manifest_json,
    session::{Dependency, Inner, asset_key},
    value::{self, Map, Value, array, get, map_file_id, object, str},
    worklist::{ASSET_REFERENCE_CLOSURE_MAXIMUM, AssetReferenceWorklist},
};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
};

const BINDING_OWNER_A: &str = "22222222222222222222222222222222";
const BINDING_OWNER_B: &str = "33333333333333333333333333333333";
const BINDING_PREFAB_A: &str = "44444444444444444444444444444444";
const BINDING_PREFAB_B: &str = "55555555555555555555555555555555";
const BINDING_UNUSED_OWNER: &str = "66666666666666666666666666666666";
const BINDING_SCRIPT: &str = "77777777777777777777777777777777";
const BINDING_COMPONENT_ID: &str = "8107781485188562762";
const BINDING_ROOT_ID: &str = "8695755991542870949";

fn map(value: serde_json::Value) -> Map {
    object(&json(value)).clone()
}

fn binding_prefab(root: &str, transform: &str, component: &str, owner: &str) -> String {
    format!(
        "--- !u!1 &{root}
GameObject:
  m_Component:
  - component: {{fileID: {transform}}}
  - component: {{fileID: {component}}}
  m_Name: Target
  m_IsActive: 1
--- !u!4 &{transform}
Transform:
  m_GameObject: {{fileID: {root}}}
  m_LocalPosition: {{x: 0, y: 0, z: 0}}
  m_LocalRotation: {{x: 0, y: 0, z: 0, w: 1}}
  m_LocalScale: {{x: 1, y: 1, z: 1}}
  m_Father: {{fileID: 0}}
  m_Children: []
--- !u!114 &{component}
MonoBehaviour:
  m_GameObject: {{fileID: {root}}}
  m_Script: {{fileID: 11500000, guid: {BINDING_SCRIPT}, type: 3}}
  linkedOwner: {{fileID: 11400000, guid: {owner}, type: 2}}
"
    )
}

/// Records source proofs for the given imported assets and their metadata.
fn record_imported_source_proof(s: &mut Inner, paths: &[&str]) {
    for path in paths {
        for logical in [path.to_string(), format!("{path}.meta")] {
            let content = fs::read(format!("{}/{logical}", s.project)).unwrap();
            let proof = Dependency {
                path: logical.clone(),
                sha256: crate::sha256(&content),
                bytes: content.len() as i64,
                ..Dependency::default()
            };
            let mut updated = false;
            for existing in &mut s.catalog.source_dependencies {
                if existing.path == logical {
                    *existing = proof.clone();
                    updated = true;
                }
            }
            if !updated {
                s.catalog.source_dependencies.push(proof);
            }
        }
    }
}

/// Indexes the catalog assets by GUID and file ID.
fn imported_catalog_index(s: &Inner) -> HashMap<String, Map> {
    s.catalog
        .assets
        .iter()
        .map(|asset| {
            (
                asset_key(&value::text(asset, "guid"), &value::text(asset, "fileId")),
                asset.clone(),
            )
        })
        .collect()
}

/// A session with two imported owners and their bindings.
fn binding_fixture() -> Fixture {
    let f = fixture_session();
    let project = f.project.clone();
    write_fixture_file(
        format!("{project}/Assets/Binding.cs"),
        "public class Binding {}",
    );
    write_fixture_file(
        format!("{project}/Assets/Binding.cs.meta"),
        format!("guid: {BINDING_SCRIPT}\n"),
    );
    {
        let mut s = f.session.lock();
        s.script_types
            .insert(BINDING_SCRIPT.into(), "Fixture.Binding".into());
        for (name, guid) in [
            ("OwnerA.asset", BINDING_OWNER_A),
            ("OwnerB.asset", BINDING_OWNER_B),
        ] {
            write_fixture_file(
                format!("{project}/Assets/{name}"),
                "--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: Owner\n",
            );
            write_fixture_file(
                format!("{project}/Assets/{name}.meta"),
                format!("guid: {guid}\n"),
            );
        }
        let owner = |guid: &str, path: &str| {
            let mut owner = value::map([
                ("kind", "imported".into()),
                ("classId", Value::Int(114)),
                ("typeName", "Fixture.ImportedOwner".into()),
                ("fileId", "11400000".into()),
            ]);
            owner.insert("guid".into(), guid.into());
            owner.insert("path".into(), path.into());
            owner
        };
        let owner_a = owner(BINDING_OWNER_A, "Assets/OwnerA.asset");
        let owner_b = owner(BINDING_OWNER_B, "Assets/OwnerB.asset");
        s.assets
            .insert(asset_key(BINDING_OWNER_A, "11400000"), owner_a.clone());
        s.catalog.schema_version = 1;
        s.catalog.assets = vec![owner_a, owner_b];
        record_imported_source_proof(&mut s, &["Assets/OwnerA.asset", "Assets/OwnerB.asset"]);
        write_fixture_file(
            format!("{project}/Assets/TargetA.prefab"),
            binding_prefab(
                BINDING_ROOT_ID,
                "-8679921383154817045",
                "901",
                BINDING_OWNER_B,
            ),
        );
        write_fixture_file(
            format!("{project}/Assets/TargetA.prefab.meta"),
            format!("guid: {BINDING_PREFAB_A}\n"),
        );
        write_fixture_file(
            format!("{project}/Assets/TargetB.prefab"),
            binding_prefab("10", "11", BINDING_COMPONENT_ID, BINDING_OWNER_A),
        );
        write_fixture_file(
            format!("{project}/Assets/TargetB.prefab.meta"),
            format!("guid: {BINDING_PREFAB_B}\n"),
        );
        s.catalog.deferred_bindings = vec![
            map(
                serde_json::json!({"ownerGuid": BINDING_OWNER_A, "ownerFileId": "11400000", "propertyPath": "characterPrefab", "targetGuid": BINDING_PREFAB_A, "targetFileId": BINDING_ROOT_ID, "targetPath": "Assets/TargetA.prefab"}),
            ),
            map(
                serde_json::json!({"ownerGuid": BINDING_OWNER_B, "ownerFileId": "11400000", "propertyPath": "rootScope", "targetGuid": BINDING_PREFAB_B, "targetFileId": BINDING_COMPONENT_ID, "targetPath": "Assets/TargetB.prefab"}),
            ),
            map(
                serde_json::json!({"ownerGuid": BINDING_UNUSED_OWNER, "ownerFileId": "11400000", "propertyPath": "unused", "targetGuid": "99999999999999999999999999999999", "targetFileId": "123", "targetPath": "Assets/Absent.prefab"}),
            ),
        ];
        let stale: Vec<Value> = s
            .catalog
            .deferred_bindings
            .iter()
            .cloned()
            .map(Value::Object)
            .collect();
        s.baseline
            .insert("importedAssetBindings".into(), Value::Array(stale));
        s.index_dirty = true;
    }
    f
}

fn published_assets(manifest: &Map) -> BTreeMap<String, Map> {
    array(get(manifest, "assets"))
        .iter()
        .map(|item| {
            let a = object(item);
            (
                asset_key(&value::text(a, "guid"), &value::text(a, "fileId")),
                a.clone(),
            )
        })
        .collect()
}

#[test]
fn imported_bindings_publish_only_active_owners_and_recursive_cyclic_targets() {
    let f = binding_fixture();
    let sources = [
        "Assets/TargetA.prefab",
        "Assets/TargetA.prefab.meta",
        "Assets/TargetB.prefab",
        "Assets/TargetB.prefab.meta",
    ];
    let before: Vec<Vec<u8>> = sources
        .iter()
        .map(|path| fs::read(format!("{}/{path}", f.project)).unwrap())
        .collect();
    for _ in 0..2 {
        let prepared = f.session.prepare(&f.output).unwrap();
        let manifest = manifest_json(&prepared.manifest).unwrap();
        let bindings = array(get(&manifest, "importedAssetBindings"));
        assert_eq!(
            bindings.len(),
            2,
            "inactive owner binding or recursive cycle was published incorrectly: {bindings:?}"
        );
        let assets = published_assets(&manifest);
        for item in bindings {
            let b = object(item);
            let owner = assets.get(&asset_key(
                &value::text(b, "ownerGuid"),
                &value::text(b, "ownerFileId"),
            ));
            assert!(
                owner.is_some_and(|owner| value::text(owner, "kind") == "imported")
                    && prepared_asset_identity(
                        &assets,
                        &value::text(b, "targetGuid"),
                        &value::text(b, "targetFileId")
                    ),
                "binding owner/target lacks exact published identity: {b:?}"
            );
        }
        assert!(
            prepared_asset_identity(&assets, BINDING_PREFAB_A, BINDING_ROOT_ID)
                && prepared_asset_identity(&assets, BINDING_PREFAB_B, BINDING_COMPONENT_ID)
                && assets.contains_key(&asset_key(BINDING_OWNER_B, "11400000")),
            "recursive imported-owner/component target closure was lost"
        );
    }
    for (path, expected) in sources.iter().zip(&before) {
        assert_eq!(
            &fs::read(format!("{}/{path}", f.project)).unwrap(),
            expected,
            "source changed {path}"
        );
    }
}

#[test]
fn imported_binding_raw_owner_uses_current_authored_references() {
    let f = binding_fixture();
    let path = "Assets/RawOwner.asset";
    write_fixture_file(
        format!("{}/{path}", f.project),
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {{fileID: 11500000, guid: {BINDING_SCRIPT}, type: 3}}\n  ownTarget: {{fileID: {BINDING_ROOT_ID}, guid: {BINDING_PREFAB_A}, type: 3}}\n"
        ),
    );
    write_fixture_file(
        format!("{}/{path}.meta", f.project),
        format!("guid: {BINDING_UNUSED_OWNER}\n"),
    );
    {
        let mut s = f.session.lock();
        s.assets.insert(
            asset_key(BINDING_UNUSED_OWNER, "11400000"),
            map(
                serde_json::json!({"kind": "scriptableObject", "path": path, "guid": BINDING_UNUSED_OWNER, "fileId": "11400000"}),
            ),
        );
        s.index_dirty = true;
    }
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    for item in array(get(&manifest, "importedAssetBindings")) {
        assert_ne!(
            str(item.field("ownerGuid")),
            BINDING_UNUSED_OWNER,
            "raw owner received stale native deferred binding instead of its source reference"
        );
    }
}

#[test]
fn imported_binding_proves_inherited_component_alias() {
    let f = binding_fixture();
    let variant_guid = "88888888888888888888888888888888";
    write_fixture_file(
        format!("{}/Assets/Variant.prefab", f.project),
        format!(
            "--- !u!1001 &900\nPrefabInstance:\n  m_SourcePrefab: {{fileID: 100100000, guid: {BINDING_PREFAB_B}, type: 3}}\n  m_Modification:\n    m_TransformParent: {{fileID: 0}}\n    m_Modifications: []\n"
        ),
    );
    write_fixture_file(
        format!("{}/Assets/Variant.prefab.meta", f.project),
        format!("guid: {variant_guid}\n"),
    );
    let component: i64 = BINDING_COMPONENT_ID.parse().unwrap();
    let alias = ((component ^ 900) & 0x7fff_ffff_ffff_ffff).to_string();
    {
        let mut s = f.session.lock();
        let binding = &mut s.catalog.deferred_bindings[1];
        binding.insert("targetGuid".into(), variant_guid.into());
        binding.insert("targetFileId".into(), alias.as_str().into());
        binding.insert("targetPath".into(), "Assets/Variant.prefab".into());
        s.index_dirty = true;
    }
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let expected = format!("900:{BINDING_COMPONENT_ID}");
    let verified = array(get(&manifest, "assets")).iter().any(|item| {
        str(item.field("guid")) == variant_guid
            && array(item.field("objectAliases")).iter().any(|entry| {
                str(entry.field("fileId")) == alias && str(entry.field("objectId")) == expected
            })
    });
    assert!(
        verified,
        "inherited component target was not proven by the actual expanded prefab source alias"
    );
}

#[test]
#[allow(clippy::type_complexity)]
fn imported_binding_rejects_forged_targets_and_conflicting_properties() {
    let set = |key: &'static str, text: &'static str| {
        move |s: &mut Inner| {
            s.catalog.deferred_bindings[0].insert(key.into(), text.into());
        }
    };
    let cases: Vec<(&str, Box<dyn Fn(&mut Inner)>, &str)> = vec![
        (
            "unknown source object",
            Box::new(set("targetFileId", "123456")),
            "raw asset Assets/TargetA.prefab has no object 123456",
        ),
        (
            "foreign target path",
            Box::new(set("targetPath", "Assets/Other.prefab")),
            "imported binding target identity/path is invalid",
        ),
        (
            "rounded target ID",
            Box::new(set("targetFileId", "9223372036854775808")),
            "imported binding target identity/path is invalid",
        ),
        (
            "invalid property",
            Box::new(set("propertyPath", "eval()")),
            "imported binding owner/property is invalid",
        ),
        (
            "conflicting property",
            Box::new(|s: &mut Inner| {
                let mut b = s.catalog.deferred_bindings[0].clone();
                b.insert("targetGuid".into(), BINDING_PREFAB_B.into());
                b.insert("targetFileId".into(), BINDING_COMPONENT_ID.into());
                b.insert("targetPath".into(), "Assets/TargetB.prefab".into());
                s.catalog.deferred_bindings.push(b);
            }),
            "imported binding property has conflicting target identities",
        ),
    ];
    for (name, change, expected) in cases {
        let f = binding_fixture();
        let good = f.session.prepare(&f.output).unwrap();
        let before = fs::read(&good.manifest).unwrap();
        change(&mut f.session.lock());
        let error = f
            .session
            .prepare(&f.output)
            .expect_err("invalid imported binding target was silently dropped or published");
        assert!(
            message(&error).contains(expected),
            "{name}: {}",
            message(&error)
        );
        assert_eq!(
            before,
            fs::read(&good.manifest).unwrap(),
            "{name}: failed closure changed prior valid manifest"
        );
    }
}

#[test]
fn imported_bindings_clear_stale_baseline_list_with_empty_current_catalog() {
    let f = binding_fixture();
    f.session.lock().catalog.deferred_bindings.clear();
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    assert!(
        array(get(&manifest, "importedAssetBindings")).is_empty(),
        "cached baseline resurrected stale removed native bindings"
    );
    assert!(
        manifest.contains_key("importedAssetBindings"),
        "current binding list was left implicit"
    );
    assert!(
        !value::text(&manifest, "importedAssetBindings").contains(BINDING_UNUSED_OWNER),
        "inactive owner persisted"
    );
    assert!(
        fs::read_to_string(&prepared.manifest)
            .unwrap()
            .contains("\"importedAssetBindings\":[]")
    );
}

#[test]
fn imported_binding_rejects_replaced_root_in_cached_raw_prefab() {
    let f = binding_fixture();
    let good = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&good.manifest).unwrap();
    f.session.lock().assets.extend(published_assets(&manifest));
    let path = format!("{}/Assets/TargetA.prefab", f.project);
    let text = fs::read_to_string(&path).unwrap();
    write_fixture_file(&path, text.replace(BINDING_ROOT_ID, "8695755991542870950"));
    let error = f
        .session
        .prepare(&f.output)
        .expect_err("cached old root ID was accepted after actual prefab root replacement");
    assert!(
        message(&error).contains("has no object"),
        "{}",
        message(&error)
    );
    let current = manifest_json(&good.manifest).unwrap();
    assert_eq!(
        array(get(&current, "importedAssetBindings")).len(),
        2,
        "failed root replacement changed prior valid binding manifest"
    );
}

#[test]
fn imported_scriptable_source_traverses_internal_native_dependencies_and_prefab_bindings() {
    let f = binding_fixture();
    // The native pack links these objects internally, so the scene only names A.
    // B's exact prefab component binding must still be published before callbacks.
    write_fixture_file(
        format!("{}/Assets/OwnerA.asset", f.project),
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: Profile\n  modifier: {{fileID: 11400000, guid: {BINDING_OWNER_B}, type: 2}}\n"
        ),
    );
    {
        let mut s = f.session.lock();
        s.catalog.deferred_bindings = s.catalog.deferred_bindings[1..2].to_vec();
        record_imported_source_proof(&mut s, &["Assets/OwnerA.asset"]);
    }
    let prepared = f.session.prepare(&f.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let assets = published_assets(&manifest);
    let bindings = array(get(&manifest, "importedAssetBindings"));
    assert!(
        assets.contains_key(&asset_key(BINDING_OWNER_B, "11400000"))
            && bindings.len() == 1
            && str(bindings[0].field("ownerGuid")) == BINDING_OWNER_B
            && prepared_asset_identity(&assets, BINDING_PREFAB_B, BINDING_COMPONENT_ID),
        "native profile->modifier->prefab component closure was not published: {bindings:?}"
    );
}

#[test]
fn imported_scriptable_source_preserves_signed_local_subasset_cycles() {
    let f = binding_fixture();
    write_fixture_file(
        format!("{}/Assets/OwnerA.asset", f.project),
        "--- !u!114 &11400000\nMonoBehaviour:\n  nested: {fileID: -7405474662461247964}\n--- !u!114 &-7405474662461247964\nMonoBehaviour:\n  parent: {fileID: 11400000}\n",
    );
    let mut s = f.session.lock();
    record_imported_source_proof(&mut s, &["Assets/OwnerA.asset"]);
    s.index_guids().unwrap();
    let asset = s.assets[&asset_key(BINDING_OWNER_A, "11400000")].clone();
    let mut subasset = asset.clone();
    subasset.insert("fileId".into(), "-7405474662461247964".into());
    s.catalog.assets.push(subasset.clone());
    let index = imported_catalog_index(&s);
    let refs = s.imported_source_references(&asset, &index).unwrap();
    assert!(
        refs.len() == 1
            && map_file_id(&refs[0]) == "-7405474662461247964"
            && value::text(&refs[0], "guid") == BINDING_OWNER_A,
        "signed local reference was rounded or lost: {refs:?}"
    );
    let refs = s.imported_source_references(&subasset, &index).unwrap();
    assert!(
        refs.len() == 1 && map_file_id(&refs[0]) == "11400000",
        "local subasset cycle was not represented exactly: {refs:?}"
    );
}

#[test]
fn imported_scriptable_source_rejects_catalog_source_and_metadata_drift() {
    for name in [
        "source",
        "metadata",
        "missing source proof",
        "missing metadata proof",
        "baseline masks catalog hash",
        "missing object",
        "wrong class",
    ] {
        let f = binding_fixture();
        let mut s = f.session.lock();
        s.index_guids().unwrap();
        let key = asset_key(BINDING_OWNER_A, "11400000");
        let owner = format!("{}/Assets/OwnerA.asset", f.project);
        match name {
            "source" => write_fixture_file(
                &owner,
                "--- !u!114 &11400000\nMonoBehaviour:\n  changed: 1\n",
            ),
            "metadata" => write_fixture_file(
                format!("{owner}.meta"),
                format!("guid: {BINDING_OWNER_A}\nchanged: 1\n"),
            ),
            "missing source proof" => {
                s.catalog.source_dependencies.remove(0);
            }
            "missing metadata proof" => {
                s.catalog.source_dependencies.remove(1);
            }
            "baseline masks catalog hash" => {
                let digest = s.catalog.source_dependencies[0].sha256.clone();
                s.critical.insert("Assets/OwnerA.asset".into(), digest);
                s.catalog.source_dependencies[0].sha256 = "0".repeat(64);
            }
            "missing object" => {
                // The session asset and the catalog entry share one map, so both carry
                // the forged identity.
                s.assets
                    .get_mut(&key)
                    .unwrap()
                    .insert("fileId".into(), "123".into());
                s.catalog.assets[0].insert("fileId".into(), "123".into());
            }
            "wrong class" => {
                write_fixture_file(&owner, "--- !u!1 &11400000\nGameObject:\n  m_Name: Wrong\n");
                record_imported_source_proof(&mut s, &["Assets/OwnerA.asset"]);
            }
            _ => unreachable!(),
        }
        let asset = s.assets[&key].clone();
        let index = imported_catalog_index(&s);
        let error = s
            .imported_source_references(&asset, &index)
            .expect_err("unproven imported source closure was accepted");
        match name {
            "missing object" | "wrong class" => assert!(
                message(&error).contains("has no exact class114 object"),
                "{name}: {}",
                message(&error)
            ),
            // The GUID index already observed the metadata of this pass, so the
            // stat-validated cache refuses the rewrite before the proof is compared.
            "metadata" => assert!(
                message(&error).starts_with("source changed during preparation, retry: "),
                "{name}: {}",
                message(&error)
            ),
            _ => assert_eq!(
                error
                    .downcast_ref::<NeedsImportError>()
                    .map(|e| e.path.as_str()),
                Some("Assets/OwnerA.asset"),
                "{name}: {}",
                message(&error)
            ),
        }
    }
}

#[test]
fn imported_scriptable_source_keeps_opaque_native_debug_references_in_native_pack() {
    let f = binding_fixture();
    write_fixture_file(
        format!("{}/Assets/OwnerA.asset", f.project),
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  modifier: {{fileID: 11400000, guid: {BINDING_OWNER_B}, type: 2}}\n  obsoleteDebugShader: {{fileID: 4800000, guid: 8f96cd657dc40064aa21efcc7e50a2e7, type: 3}}\n"
        ),
    );
    let mut s = f.session.lock();
    record_imported_source_proof(&mut s, &["Assets/OwnerA.asset"]);
    s.index_guids().unwrap();
    let asset = s.assets[&asset_key(BINDING_OWNER_A, "11400000")].clone();
    let index = imported_catalog_index(&s);
    let refs = s.imported_source_references(&asset, &index).unwrap();
    assert!(
        refs.len() == 1 && value::text(&refs[0], "guid") == BINDING_OWNER_B,
        "native opaque pointers were reinterpreted or scripted dependency lost: {refs:?}"
    );
}

#[test]
fn imported_scriptable_source_reads_changed_indirect_native_owner_as_authored() {
    let f = binding_fixture();
    write_fixture_file(
        format!("{}/Assets/OwnerA.asset", f.project),
        format!(
            "--- !u!114 &11400000\nMonoBehaviour:\n  modifier: {{fileID: 11400000, guid: {BINDING_OWNER_B}, type: 2}}\n"
        ),
    );
    {
        let mut s = f.session.lock();
        record_imported_source_proof(&mut s, &["Assets/OwnerA.asset"]);
        s.catalog.deferred_bindings = s.catalog.deferred_bindings[1..2].to_vec();
    }
    let good = f.session.prepare(&f.output).unwrap();
    let before = fs::read(&good.manifest).unwrap();
    let changed = format!("{}/Assets/OwnerB.asset", f.project);
    write_fixture_file(
        &changed,
        "--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: ChangedModifier\n",
    );
    let digest = crate::sha256(&fs::read(&changed).unwrap());
    f.session
        .lock()
        .critical
        .insert("Assets/OwnerB.asset".into(), digest);
    let error = f.session.prepare(&f.output).expect_err(
        "indirect imported owner used a changed source hidden by baseline critical hashes",
    );
    // The changed source is read as authored text, where this one lacks a script.
    assert!(
        error.downcast_ref::<NeedsImportError>().is_none()
            && message(&error).contains("raw ScriptableObject Assets/OwnerB.asset"),
        "{}",
        message(&error)
    );
    assert_eq!(
        before,
        fs::read(&good.manifest).unwrap(),
        "invalid indirect dependency replaced the prior valid publication"
    );
}

#[test]
fn asset_reference_worklist_deduplicates_processed_exact_identities() {
    let mut work = AssetReferenceWorklist::new();
    for id in ["-7405474662461247964", "8107781485188562762"] {
        for _ in 0..40000 {
            work.add_identity(BINDING_PREFAB_A, id).unwrap();
        }
    }
    assert!(
        work.items.len() == 2 && work.seen.len() == 2,
        "duplicates consumed queue bound"
    );
    let last = work.pop().unwrap();
    assert_eq!(
        last.file_id, "8107781485188562762",
        "signed exact identity or LIFO order lost"
    );
    work.add_identity(BINDING_PREFAB_A, &last.file_id).unwrap();
    assert_eq!(
        work.items.len(),
        1,
        "processed cyclic identity was queued again"
    );
    assert!(work.pop().is_some(), "other subasset identity was merged");
    assert!(work.pop().is_none(), "worklist did not drain");
    // A drained Vec owns no popped slots.
    assert!(
        work.items.is_empty(),
        "completed queue entry retains source data"
    );
    for (guid, id) in [
        (BINDING_PREFAB_A, ""),
        (BINDING_PREFAB_A, "0"),
        ("", "1"),
        ("0000000000000000f000000000000000", "69"),
    ] {
        work.add_identity(guid, id).unwrap();
    }
    assert_eq!(
        work.seen.len(),
        2,
        "empty/null/builtin reference consumed identity bound"
    );
}

#[test]
fn asset_reference_worklist_rejects_too_many_unique_references_after_drain() {
    let mut work = AssetReferenceWorklist::new();
    for i in 0..ASSET_REFERENCE_CLOSURE_MAXIMUM {
        work.add_identity(BINDING_PREFAB_A, &(i + 1).to_string())
            .unwrap();
        assert!(work.pop().is_some(), "queue unexpectedly empty");
    }
    work.add_identity(BINDING_PREFAB_A, "1")
        .expect("existing identity rejected at bound");
    let error = work
        .add_identity(
            BINDING_PREFAB_A,
            &(ASSET_REFERENCE_CLOSURE_MAXIMUM + 1).to_string(),
        )
        .expect_err("processed identities bypassed total unique bound");
    assert_eq!(
        message(&error),
        "asset dependency closure exceeds supported unique-reference bound (32768)"
    );
    assert!(
        work.seen.len() == ASSET_REFERENCE_CLOSURE_MAXIMUM && work.items.is_empty(),
        "rejected reference changed accepted worklist"
    );
}

#[test]
fn imported_binding_wide_duplicate_scene_and_cached_asset_closure() {
    let f = binding_fixture();
    let mut s = f.session.lock();
    s.index_guids().unwrap();
    s.dependencies.clear();
    let seed = s.prepare_assets(&[]).unwrap();
    for item in &seed.assets {
        let asset = object(item);
        s.assets.insert(
            asset_key(&value::text(asset, "guid"), &value::text(asset, "fileId")),
            asset.clone(),
        );
    }
    let reference = Value::Object(value::map([
        ("guid", BINDING_PREFAB_B.into()),
        ("fileID", BINDING_COMPONENT_ID.into()),
    ]));
    let scene = [Expanded {
        class_id: 114,
        file_id: "11".into(),
        data: value::map([("manyReferences", Value::Array(vec![reference; 40000]))]),
        ..Expanded::default()
    }];
    for _ in 0..2 {
        let prepared = s.prepare_assets(&scene).unwrap();
        assert!(
            prepared.assets == seed.assets && prepared.imported_bindings == seed.imported_bindings,
            "duplicate PPtrs altered proven asset/binding identities"
        );
    }
}

#[test]
fn imported_binding_property_bound_still_rejects_distinct_bindings_sharing_target() {
    let f = binding_fixture();
    let mut s = f.session.lock();
    s.index_guids().unwrap();
    let template = s.catalog.deferred_bindings[0].clone();
    s.catalog.deferred_bindings = (0..32769)
        .map(|i| {
            let mut entry = template.clone();
            entry.insert("propertyPath".into(), format!("many[{i}]").into());
            entry
        })
        .collect();
    let mut closure = s.imported_binding_closure();
    let mut work = AssetReferenceWorklist::new();
    let owner = s.assets[&asset_key(BINDING_OWNER_A, "11400000")].clone();
    let error = closure
        .queue(&s, &owner, &mut work)
        .expect_err("deduplication removed distinct binding-property bound");
    assert_eq!(
        message(&error),
        "imported binding dependency closure exceeds supported bound"
    );
    assert_eq!(
        work.seen.len(),
        1,
        "same-target bindings were not deduplicated"
    );
}

#[test]
fn asset_reference_actual_prepared_manifest_read_only() {
    let variable = |name: &str| std::env::var(name).unwrap_or_default();
    let (project, baseline, catalog) = (
        variable("UNITY_MICRO_EDITOR_ASSET_PROJECT"),
        variable("UNITY_MICRO_EDITOR_ASSET_BASELINE"),
        variable("UNITY_MICRO_EDITOR_ASSET_CATALOG"),
    );
    if project.is_empty() || baseline.is_empty() {
        // Explicit owned actual project/baseline not supplied.
        return;
    }
    let session = Session::new(Options {
        project: project.clone(),
        manifest: baseline,
        native_catalog: catalog,
        ..Options::default()
    })
    .unwrap();
    let mut output = variable("UNITY_MICRO_EDITOR_ASSET_OUTPUT");
    if output.is_empty() {
        output = format!(
            "{project}/Library/UnityMicroEditor/asset-reference-check/{}",
            std::process::id()
        );
    }
    let first = session.prepare(&output).unwrap();
    let mut manifest = manifest_json(&first.manifest).unwrap();
    let second = session.prepare(&output).unwrap();
    let mut second_manifest = manifest_json(&second.manifest).unwrap();
    // Each successful publication intentionally owns a fresh random revision.
    manifest.remove("revision");
    second_manifest.remove("revision");
    assert!(
        first.objects == second.objects
            && first.assets == second.assets
            && manifest == second_manifest,
        "unchanged actual preparation changed semantic asset/binding closure"
    );
}
