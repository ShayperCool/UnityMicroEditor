//! Tests of authored texture, mesh, material and ScriptableObject sources.
use super::{Fixture, data_json, fixture_session, json, write_fixture_file};
use crate::project::{
    manifest_json,
    mesh::validate_authored_mesh,
    parse_unity_yaml,
    session::asset_key,
    value::{self, Map, Value, array, number, object, str},
};
use std::{fs, path::Path};

pub(super) const AUTHORED_TEXTURE_META: &str = "fileFormatVersion: 2
guid: 22222222222222222222222222222222
TextureImporter:
  textureType: 0
  textureShape: 1
  alphaUsage: 1
  nPOTScale: 1
  swizzle: 50462976
  isReadable: 0
  mipmaps:
    sRGBTexture: 0
    enableMipMap: 1
  textureSettings:
    filterMode: 0
    wrapU: 1
    wrapV: 2
    aniso: 4
  platformSettings:
  - buildTarget: DefaultTexturePlatform
    maxTextureSize: 2048
    textureFormat: -1
    textureCompression: 0
  - buildTarget: Standalone
    overridden: 0
    maxTextureSize: 2048
    textureFormat: -1
    textureCompression: 1
";

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The header of an NRGBA PNG: only the IHDR chunk is read when decoding
/// dimensions, so the pixel chunk is a stored placeholder.
pub(super) fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8; 4], body: &[u8]| {
        out.extend((body.len() as u32).to_be_bytes());
        let mut checked = kind.to_vec();
        checked.extend(body);
        out.extend(&checked);
        out.extend(crc32(&checked).to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    header.extend([8, 6, 0, 0, 0]);
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
    chunk(b"IEND", &[]);
    out
}

fn asset(fields: serde_json::Value) -> Map {
    match json(fields) {
        Value::Object(map) => map,
        _ => unreachable!("asset descriptors are objects"),
    }
}

#[test]
fn raw_texture_uses_effective_uncompressed_importer() {
    let fixture = fixture_session();
    let mut session = fixture.session.lock();
    let encoded = png(4, 4);
    let physical = format!("{}/Assets/Texture.png", fixture.project);
    fs::write(&physical, &encoded).unwrap();
    write_fixture_file(format!("{physical}.meta"), AUTHORED_TEXTURE_META);
    let mut texture = asset(serde_json::json!({"path": "Assets/Texture.png", "kind": "texture"}));
    session.refresh_asset(&mut texture).unwrap();
    let settings = match value::parse_use_number(&value::text(&texture, "importerJson")).unwrap() {
        Value::Object(map) => map,
        other => panic!("importer settings are not an object: {other:?}"),
    };
    let setting = |key: &str| number(value::get(&settings, key));
    assert!(
        setting("sRGBTexture") == 0
            && setting("mipmapEnabled") == 1
            && setting("wrapU") == 1
            && setting("wrapV") == 2
            && setting("aniso") == 4,
        "lost authored linear/mip/sampler settings: {settings:?}"
    );
    assert_eq!(
        fs::read(&physical).unwrap(),
        encoded,
        "texture source changed"
    );
    write_fixture_file(
        format!("{physical}.meta"),
        AUTHORED_TEXTURE_META.replacen("overridden: 0", "overridden: 1", 1),
    );
    session.cache.files.clear();
    session.cache.observed.clear();
    assert!(
        session.refresh_asset(&mut texture).is_err(),
        "accepted effective compressed Standalone override"
    );
}

const SIMPLE_AUTHORED_MESH: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!43 &4300000
Mesh:
  m_Name: AuthoredMesh
  serializedVersion: 12
  m_MeshCompression: 0
  m_IsReadable: 1
  m_IndexFormat: 0
  m_IndexBuffer: 000000000000y
  m_VertexData:
    serializedVersion: 3
    m_VertexCount: 1
    m_DataSize: 12
    _typelessdata: 000000000000000000000000
    m_Channels:
    - {stream: 0, offset: 0, format: 0, dimension: 3}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
  m_SubMeshes:
  - {serializedVersion: 2, firstByte: 0, indexCount: 3, topology: 0, baseVertex: 0, firstVertex: 0, vertexCount: 1}
";

#[test]
fn authored_mesh_keeps_unity_identity_and_bytes_in_manifest() {
    let fixture = fixture_session();
    let mut s = fixture.session.lock();
    let path = "Assets/Meshes/Shape.asset";
    write_fixture_file(format!("{}/{path}", fixture.project), SIMPLE_AUTHORED_MESH);
    let mut mesh = asset(serde_json::json!({
        "kind": "scriptableObject", "guid": "22222222222222222222222222222222", "path": path,
    }));
    s.refresh_asset(&mut mesh).unwrap();
    assert!(
        value::text(&mesh, "kind") == "mesh"
            && number(value::get(&mesh, "classId")) == 43
            && value::text(&mesh, "typeName") == "UnityEngine.Mesh"
            && value::text(&mesh, "fileId") == "4300000",
        "Authored Mesh classified incorrectly: {mesh:?}"
    );
    let data = data_json(&Value::Object(mesh));
    assert!(
        value::text(&data, "m_IndexBuffer") == "000000000000y"
            && str(value::get(&data, "m_VertexData").field("_typelessdata"))
                == "000000000000000000000000",
        "Authored Mesh byte scalar changed"
    );
}

#[test]
#[allow(clippy::type_complexity)]
fn authored_mesh_rejects_unsupported_content() {
    let documents = parse_unity_yaml(SIMPLE_AUTHORED_MESH.as_bytes()).unwrap();
    let cases: [(&str, fn(&mut Map)); 8] = [
        ("compressed", |data| {
            data.insert("m_MeshCompression".into(), Value::Int(1));
        }),
        ("packed", |data| {
            data.insert(
                "m_CompressedMesh".into(),
                json(serde_json::json!({"m_Vertices": {"m_NumItems": 3}})),
            );
        }),
        ("skinned", |data| {
            data.insert("m_BindPose".into(), json(serde_json::json!([{}])));
        }),
        ("blend shapes", |data| {
            data.insert(
                "m_Shapes".into(),
                json(serde_json::json!({"vertices": [{}]})),
            );
        }),
        ("streamed", |data| {
            data.insert(
                "m_StreamData".into(),
                json(serde_json::json!({"path": "external.bin", "size": 12})),
            );
        }),
        ("wrong data size", |data| {
            let vertex = data
                .get_mut("m_VertexData")
                .unwrap()
                .as_object_mut()
                .unwrap();
            vertex.insert("m_DataSize".into(), Value::Int(11));
        }),
        ("invalid hex", |data| {
            data.insert("m_IndexBuffer".into(), "zzzz".into());
        }),
        ("external source", |data| {
            data.insert(
                "m_PrefabAsset".into(),
                json(
                    serde_json::json!({"fileID": "1", "guid": "11111111111111111111111111111111"}),
                ),
            );
        }),
    ];
    validate_authored_mesh(documents[0].fields()).unwrap();
    for (name, edit) in cases {
        let mut data = documents[0].fields().clone();
        edit(&mut data);
        assert!(
            validate_authored_mesh(&data).is_err(),
            "Unsupported source Mesh was accepted: {name}"
        );
    }
}

#[test]
fn all_real_project_authored_meshes() {
    let Ok(root) = std::env::var("UME_MESH_SOURCE_PROJECT") else {
        return;
    };
    if root.is_empty() {
        return;
    }
    fn walk(directory: &Path, count: &mut usize, failures: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(&path, count, failures);
                continue;
            }
            if !path.to_string_lossy().ends_with(".asset") {
                continue;
            }
            let documents = parse_unity_yaml(&fs::read(&path).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            if documents.len() != 1 || documents[0].class_id != 43 {
                continue;
            }
            *count += 1;
            if let Err(error) = validate_authored_mesh(documents[0].fields()) {
                failures.push(format!("{}: {error:#}", path.display()));
            }
        }
    }
    let (mut count, mut failures) = (0, Vec::new());
    walk(&Path::new(&root).join("Assets"), &mut count, &mut failures);
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(
        count >= 100,
        "Expected actual project mesh inventory, observed {count}"
    );
}

const AUTHORED_MATERIAL_FIXTURE: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  serializedVersion: 8
  m_Name: Authored
  m_Parent: {fileID: 0}
  m_Shader: {fileID: 4800000, guid: 33333333333333333333333333333333, type: 3}
  m_SavedProperties:
    m_TexEnvs:
    - _BaseMap:
        m_Texture: {fileID: 2800000, guid: 44444444444444444444444444444444, type: 3}
        m_Scale: {x: 2, y: 3}
        m_Offset: {x: 0.25, y: 0.5}
    m_Ints: []
    m_Floats:
    - _Metallic: 0.35
    m_Colors:
    - _EmissionColor: {r: 3, g: 2, b: 1, a: 1}
--- !u!114 &2561904340540659671
MonoBehaviour:
  m_EditorClassIdentifier: Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion
  version: 10
";

#[test]
fn authored_material_keeps_properties_and_native_references() {
    let fixture = fixture_session();
    let mut session = fixture.session.lock();
    let path = "Assets/Material.mat";
    let physical = format!("{}/{path}", fixture.project);
    write_fixture_file(&physical, AUTHORED_MATERIAL_FIXTURE);
    write_fixture_file(
        format!("{physical}.meta"),
        "guid: 22222222222222222222222222222222\n",
    );
    let mut material = asset(serde_json::json!({"path": path, "kind": "authoredMaterial"}));
    session.refresh_asset(&mut material).unwrap();
    let documents = parse_unity_yaml(AUTHORED_MATERIAL_FIXTURE.as_bytes()).unwrap();
    let expected = value::compact_map(documents[0].fields()).unwrap();
    assert!(
        value::get(&material, "dataJson").as_str() == Some(expected.as_str())
            && value::get(&material, "fileId").as_str() == Some("2100000"),
        "authored material data/identity was changed"
    );
    let references = array(value::get(&material, "references"));
    assert_eq!(
        references.len(),
        3,
        "lost source shader/texture/parent references: {references:?}"
    );
    assert_eq!(
        fs::read_to_string(&physical).unwrap(),
        AUTHORED_MATERIAL_FIXTURE,
        "source material was overwritten"
    );
}

#[test]
fn reject_unsupported_authored_material_variant_and_runtime_subasset() {
    for content in [
        AUTHORED_MATERIAL_FIXTURE.replacen(
            "m_Parent: {fileID: 0}",
            "m_Parent: {fileID: 2100000, guid: 55555555555555555555555555555555, type: 2}",
            1,
        ),
        AUTHORED_MATERIAL_FIXTURE.replacen(
            "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion",
            "Unknown.RuntimeAsset",
            1,
        ),
    ] {
        let fixture = fixture_session();
        let mut session = fixture.session.lock();
        let path = "Assets/Material.mat";
        let physical = format!("{}/{path}", fixture.project);
        write_fixture_file(&physical, content);
        write_fixture_file(
            format!("{physical}.meta"),
            "guid: 22222222222222222222222222222222\n",
        );
        let mut material = asset(serde_json::json!({"path": path, "kind": "authoredMaterial"}));
        assert!(
            session.refresh_asset(&mut material).is_err(),
            "accepted unresolved material inheritance/runtime subasset"
        );
    }
}

/// The stamps as Editors write them: 6000.6 with the class beside the script,
/// earlier ones with the script alone, and a material that carries all three.
#[test]
fn authored_material_leaves_out_the_version_stamps_of_the_pipelines() {
    let stamp = |id: &str, guid: &str, class: &str| {
        format!(
            "--- !u!114 &{id}\nMonoBehaviour:\n  m_ObjectHideFlags: 11\n  m_GameObject: {{fileID: 0}}\n  m_Enabled: 1\n  m_Script: {{fileID: 11500000, guid: {guid}, type: 3}}\n  m_Name: \n  m_EditorClassIdentifier: {class}\n  version: 10\n"
        )
    };
    let material = AUTHORED_MATERIAL_FIXTURE
        .split("--- !u!114")
        .next()
        .unwrap();
    let urp = "d0353a89b1f911e48b9e16bdc9f2e058";
    let graph = "639247ca83abc874e893eb93af2b5e44";
    let hdrp = "da692e001514ec24dbc4cca1949ff7e8";
    let urp_class =
        "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion";
    let graph_class = "Unity.ShaderGraph.Editor::UnityEditor.Rendering.BuiltIn.AssetVersion";
    let refresh = |content: String| {
        let fixture = fixture_session();
        let mut session = fixture.session.lock();
        let path = "Assets/Material.mat";
        let physical = format!("{}/{path}", fixture.project);
        write_fixture_file(&physical, content);
        write_fixture_file(
            format!("{physical}.meta"),
            "guid: 22222222222222222222222222222222\n",
        );
        let mut asset = asset(serde_json::json!({"path": path, "kind": "authoredMaterial"}));
        session
            .refresh_asset(&mut asset)
            .map(|()| value::get(&asset, "fileId").as_str().map(str::to_string))
            .map_err(|error| format!("{error:#}"))
    };
    let (head, body) = material.split_at(material.find("--- !u!21").unwrap());
    for content in [
        // Saved by 6000.6: both stamps, the first before the material.
        format!(
            "{head}{}{body}{}",
            stamp("-3627636042289115933", urp, urp_class),
            stamp("4795349616271329459", graph, graph_class)
        ),
        // Saved by an earlier Editor, in a project that had all three packages.
        format!(
            "{material}{}{}{}",
            stamp("1030908612866383966", hdrp, ""),
            stamp("4814849476012220932", graph, ""),
            stamp("8336996437747029529", urp, "")
        ),
    ] {
        assert_eq!(refresh(content), Ok(Some("2100000".to_string())));
    }
    for (content, why) in [
        (
            format!(
                "{material}{}",
                stamp("5", "99999999999999999999999999999999", "")
            ),
            "a script that is no stamp",
        ),
        (
            format!("{material}{}", stamp("5", urp, graph_class)),
            "a class that is not the script's",
        ),
        (
            format!(
                "{material}{}",
                stamp("5", urp, "Assembly-CSharp::Game.Data")
            ),
            "a class of the project",
        ),
        (
            format!("{material}{}", stamp("5", urp, urp_class))
                .replace("m_GameObject: {fileID: 0}", "m_GameObject: {fileID: 7}"),
            "a component of a GameObject",
        ),
    ] {
        let error = refresh(content).expect_err(why);
        assert!(
            error.contains("not a render pipeline version stamp") && error.contains("document 5"),
            "{why}: {error}"
        );
    }
}

const AUTHORED_MATERIAL_VARIANT_FIXTURE: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  serializedVersion: 8
  m_Name: Variant
  m_Parent: {fileID: 2100000, guid: 55555555555555555555555555555555, type: 2}
  m_Shader: {fileID: 4800000, guid: 33333333333333333333333333333333, type: 3}
  m_ModifiedSerializedProperties: 0
  m_ValidKeywords: []
  m_LightmapFlags: 4
  m_EnableInstancingVariants: 0
  m_DoubleSidedGI: 0
  m_CustomRenderQueue: -1
  stringTagMap: {}
  disabledShaderPasses: []
  m_SavedProperties:
    m_TexEnvs: []
    m_Ints: []
    m_Floats: []
    m_Colors:
    - _BaseColor: {r: 0.35520002, g: 0.3096, b: 0.21000001, a: 1}
";

fn material_variant_session(child: &str, parent: &str) -> (Fixture, Map) {
    let fixture = fixture_session();
    let (child_path, parent_path) = ("Assets/Child.mat", "Assets/Parent.mat");
    let physical = |path: &str| format!("{}/{path}", fixture.project);
    write_fixture_file(physical(child_path), child);
    write_fixture_file(
        physical(child_path) + ".meta",
        "guid: 22222222222222222222222222222222\n",
    );
    write_fixture_file(physical(parent_path), parent);
    write_fixture_file(
        physical(parent_path) + ".meta",
        "guid: 55555555555555555555555555555555\n",
    );
    {
        let mut session = fixture.session.lock();
        session
            .by_guid
            .insert("22222222222222222222222222222222".into(), child_path.into());
        session.by_guid.insert(
            "55555555555555555555555555555555".into(),
            parent_path.into(),
        );
    }
    (
        fixture,
        asset(serde_json::json!({"path": child_path, "kind": "authoredMaterial"})),
    )
}

#[test]
fn material_variant_keeps_sparse_overrides_and_tracks_parent() {
    let parent = AUTHORED_MATERIAL_FIXTURE.replacen(
        "m_Name: Authored",
        "m_Name: Parent\n  m_EnableInstancingVariants: 1\n  m_CustomRenderQueue: 2450\n  stringTagMap: {RenderType: TransparentCutout}",
        1,
    );
    let (fixture, mut material) =
        material_variant_session(AUTHORED_MATERIAL_VARIANT_FIXTURE, &parent);
    let mut session = fixture.session.lock();
    session.refresh_asset(&mut material).unwrap();
    let documents = parse_unity_yaml(AUTHORED_MATERIAL_VARIANT_FIXTURE.as_bytes()).unwrap();
    let expected = value::compact_map(documents[0].fields()).unwrap();
    assert_eq!(
        value::get(&material, "dataJson").as_str(),
        Some(expected.as_str()),
        "variant header defaults or sparse overrides were rewritten"
    );
    assert!(
        session.dependencies.contains_key("Assets/Parent.mat"),
        "parent changes will not invalidate variant preparation"
    );
    for (path, source) in [
        ("Assets/Child.mat", AUTHORED_MATERIAL_VARIANT_FIXTURE),
        ("Assets/Parent.mat", parent.as_str()),
    ] {
        let actual = fs::read_to_string(format!("{}/{path}", fixture.project)).unwrap();
        assert_eq!(actual, source, "source material changed {path}");
    }
}

#[test]
fn reject_material_variant_unknown_flags_different_shader_and_cycles() {
    let variant = AUTHORED_MATERIAL_VARIANT_FIXTURE;
    let cases = [
        (
            "header-mask",
            variant.replacen(
                "m_ModifiedSerializedProperties: 0",
                "m_ModifiedSerializedProperties: 8",
                1,
            ),
            AUTHORED_MATERIAL_FIXTURE.to_string(),
        ),
        (
            "bad-mask",
            variant.replacen(
                "m_ModifiedSerializedProperties: 0",
                "m_ModifiedSerializedProperties: unknown",
                1,
            ),
            AUTHORED_MATERIAL_FIXTURE.to_string(),
        ),
        (
            "shader-mismatch",
            variant.to_string(),
            AUTHORED_MATERIAL_FIXTURE.replacen(
                "33333333333333333333333333333333",
                "66666666666666666666666666666666",
                1,
            ),
        ),
        (
            "parent-cycle",
            variant.to_string(),
            variant.replacen(
                "55555555555555555555555555555555",
                "22222222222222222222222222222222",
                1,
            ),
        ),
        (
            "self-cycle",
            variant.replacen(
                "55555555555555555555555555555555",
                "22222222222222222222222222222222",
                1,
            ),
            AUTHORED_MATERIAL_FIXTURE.to_string(),
        ),
        (
            "missing-parent-local-id",
            variant.replacen(
                "m_Parent: {fileID: 2100000",
                "m_Parent: {fileID: 2100001",
                1,
            ),
            AUTHORED_MATERIAL_FIXTURE.to_string(),
        ),
    ];
    for (name, child, parent) in cases {
        let (fixture, mut material) = material_variant_session(&child, &parent);
        assert!(
            fixture.session.lock().refresh_asset(&mut material).is_err(),
            "accepted invalid material variant {name}"
        );
    }
}

#[test]
fn material_variant_preserves_native_parent_reference() {
    let (fixture, mut material) =
        material_variant_session(AUTHORED_MATERIAL_VARIANT_FIXTURE, AUTHORED_MATERIAL_FIXTURE);
    let mut session = fixture.session.lock();
    session.by_guid.insert(
        "55555555555555555555555555555555".into(),
        "Assets/ImportedModel.fbx".into(),
    );
    session.refresh_asset(&mut material).unwrap();
    let parent_found = array(value::get(&material, "references"))
        .iter()
        .any(|reference| {
            let data = object(reference);
            value::text(data, "property") == "m_Parent"
                && value::text(data, "guid") == "55555555555555555555555555555555"
                && value::text(data, "fileId") == "2100000"
        });
    assert!(parent_found, "native parent material reference disappeared");
}

const MULTI_SCRIPTABLE_GUID: &str = "22222222222222222222222222222222";
const MULTI_SCRIPTABLE_SCRIPT_GUID: &str = "33333333333333333333333333333333";

const MULTI_SCRIPTABLE_FIXTURE: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &11400000
MonoBehaviour:
  m_Name: Profile
  m_Script: {fileID: 11500000, guid: 33333333333333333333333333333333, type: 3}
  components:
  - {fileID: -8901537253420550725}
  - {fileID: 9223372036854775807}
  external: {fileID: 0}
--- !u!114 &-8901537253420550725
MonoBehaviour:
  m_Name: First
  m_Script: {fileID: 11500000, guid: 33333333333333333333333333333333, type: 3}
  sibling: {fileID: 9223372036854775807}
  profile: {fileID: 11400000}
--- !u!114 &9223372036854775807
MonoBehaviour:
  m_Name: Second
  m_Script: {fileID: 11500000, guid: 33333333333333333333333333333333, type: 3}
  sibling: {fileID: -8901537253420550725}
  profile: {fileID: 11400000}
";

#[test]
fn multi_scriptable_object_publishes_complete_cyclic_same_file_closure() {
    let fixture = fixture_session();
    let path = "Assets/Data/Profile.asset";
    let physical = format!("{}/{path}", fixture.project);
    write_fixture_file(&physical, MULTI_SCRIPTABLE_FIXTURE);
    write_fixture_file(
        format!("{physical}.meta"),
        format!("guid: {MULTI_SCRIPTABLE_GUID}\n"),
    );
    write_fixture_file(
        format!("{}/Assets/Profile.cs", fixture.project),
        "public class Profile {}",
    );
    write_fixture_file(
        format!("{}/Assets/Profile.cs.meta", fixture.project),
        format!("guid: {MULTI_SCRIPTABLE_SCRIPT_GUID}\n"),
    );
    {
        let mut s = fixture.session.lock();
        s.script_types.insert(
            MULTI_SCRIPTABLE_SCRIPT_GUID.into(),
            "Fixture.Profile".into(),
        );
        s.assets.insert(
            asset_key(MULTI_SCRIPTABLE_GUID, "11400000"),
            asset(serde_json::json!({
                "kind": "scriptableObject", "path": path, "guid": MULTI_SCRIPTABLE_GUID, "fileId": "11400000",
            })),
        );
    }
    let prepared = fixture.session.prepare(&fixture.output).unwrap();
    let manifest = manifest_json(&prepared.manifest).unwrap();
    let mut objects: std::collections::BTreeMap<String, &Map> = Default::default();
    for item in array(value::get(&manifest, "assets")) {
        let published = object(item);
        if value::text(published, "guid") != MULTI_SCRIPTABLE_GUID {
            continue;
        }
        let id = value::text(published, "fileId").into_owned();
        assert!(
            objects.insert(id.clone(), published).is_none(),
            "same-file object {id} published twice"
        );
        assert!(
            value::text(published, "kind") == "scriptableObject"
                && value::text(published, "typeName") == "Fixture.Profile",
            "invalid subasset type descriptor: {published:?}"
        );
        for reference in array(value::get(published, "references")) {
            let reference = object(reference);
            if value::text(reference, "property") == "m_Script"
                || value::text(reference, "fileId") == "0"
            {
                continue;
            }
            assert_eq!(
                value::text(reference, "guid"),
                MULTI_SCRIPTABLE_GUID,
                "same-file reference did not receive source GUID: {reference:?}"
            );
        }
    }
    assert!(
        objects.len() == 3
            && ["11400000", "-8901537253420550725", "9223372036854775807"]
                .iter()
                .all(|id| objects.contains_key(*id)),
        "complete signed64 source identities not preserved: {objects:?}"
    );
    assert_eq!(
        fs::read_to_string(&physical).unwrap(),
        MULTI_SCRIPTABLE_FIXTURE,
        "source asset was modified"
    );
    let warm = fixture.session.prepare(&fixture.output).unwrap();
    assert!(
        warm.cache.yaml_parses == 0 && warm.cache.file_reads == 0,
        "warm closure preparation reread unchanged SO sources: {:?}",
        warm.cache
    );
}

fn profile_session() -> Fixture {
    let fixture = fixture_session();
    fixture.session.lock().script_types.insert(
        MULTI_SCRIPTABLE_SCRIPT_GUID.into(),
        "Fixture.Profile".into(),
    );
    fixture
}

#[test]
fn scriptable_object_rejects_missing_local_references_and_source_identity() {
    for (name, source) in [
        (
            "dangling local reference",
            MULTI_SCRIPTABLE_FIXTURE.replacen(
                "components:\n  - {fileID: -8901537253420550725}",
                "components:\n  - {fileID: 123}",
                1,
            ),
        ),
        (
            "unsupported mixed class",
            MULTI_SCRIPTABLE_FIXTURE.replacen(
                "--- !u!114 &9223372036854775807",
                "--- !u!43 &9223372036854775807",
                1,
            ),
        ),
    ] {
        let documents = parse_unity_yaml(source.as_bytes()).unwrap();
        let fixture = profile_session();
        let mut profile = asset(serde_json::json!({
            "path": "Assets/Profile.asset", "guid": MULTI_SCRIPTABLE_GUID, "fileId": "11400000",
        }));
        assert!(
            fixture
                .session
                .lock()
                .refresh_scriptable_object(&mut profile, &documents)
                .is_err(),
            "invalid same-file source graph was accepted: {name}"
        );
    }
    let documents = parse_unity_yaml(MULTI_SCRIPTABLE_FIXTURE.as_bytes()).unwrap();
    let fixture = profile_session();
    for id in ["123", ""] {
        let mut profile = asset(serde_json::json!({
            "path": "Assets/Profile.asset", "guid": MULTI_SCRIPTABLE_GUID, "fileId": id,
        }));
        assert!(
            fixture
                .session
                .lock()
                .refresh_scriptable_object(&mut profile, &documents)
                .is_err(),
            "missing/ambiguous source identity {id:?} accepted"
        );
    }
}

#[test]
fn scriptable_object_preserves_external_reference_identity() {
    let source = MULTI_SCRIPTABLE_FIXTURE.replacen(
        "external: {fileID: 0}",
        "external: {fileID: 123, guid: 44444444444444444444444444444444, type: 2}",
        1,
    );
    let documents = parse_unity_yaml(source.as_bytes()).unwrap();
    let fixture = profile_session();
    let mut profile = asset(serde_json::json!({
        "path": "Assets/Profile.asset", "guid": MULTI_SCRIPTABLE_GUID, "fileId": "11400000",
    }));
    fixture
        .session
        .lock()
        .refresh_scriptable_object(&mut profile, &documents)
        .unwrap();
    // A rewritten copy is published, so the published data is the place where the
    // identity must survive.
    assert_eq!(
        str(documents[0].get("external").field("guid")),
        "44444444444444444444444444444444",
        "external asset GUID was rewritten as a same-file reference"
    );
    let data = data_json(&Value::Object(profile));
    assert_eq!(
        str(value::get(&data, "external").field("guid")),
        "44444444444444444444444444444444",
        "external asset GUID was rewritten as a same-file reference"
    );
    assert_eq!(
        str(array(value::get(&data, "components"))[0].field("guid")),
        MULTI_SCRIPTABLE_GUID
    );
}

#[test]
fn panel_settings_requires_pinned_built_in_script_identity() {
    const PANEL_FIXTURE: &str = "--- !u!114 &11400000
MonoBehaviour:
  m_Name: Panel
  m_Script: {fileID: 19101, guid: 0000000000000000e000000000000000, type: 0}
  m_EditorClassIdentifier: UnityEngine.dll::UnityEngine.UIElements.PanelSettings
";
    for (name, source) in [
        ("valid", PANEL_FIXTURE.to_string()),
        (
            "wrong script ID",
            PANEL_FIXTURE.replacen("19101", "19102", 1),
        ),
        (
            "wrong class",
            PANEL_FIXTURE.replacen(
                "UnityEngine.UIElements.PanelSettings",
                "UnityEngine.OtherType",
                1,
            ),
        ),
    ] {
        let documents = parse_unity_yaml(source.as_bytes()).unwrap();
        let fixture = fixture_session();
        let mut panel = asset(serde_json::json!({
            "path": "Assets/Panel.asset", "guid": MULTI_SCRIPTABLE_GUID, "fileId": "11400000",
        }));
        let result = fixture
            .session
            .lock()
            .refresh_scriptable_object(&mut panel, &documents);
        if name == "valid" {
            assert!(
                result.is_ok()
                    && value::text(&panel, "typeName") == "UnityEngine.UIElements.PanelSettings",
                "pinned PanelSettings mapping failed: {result:?}, {panel:?}"
            );
        } else {
            assert!(
                result.is_err(),
                "unverified engine MonoScript identity accepted: {name}"
            );
        }
    }
}
