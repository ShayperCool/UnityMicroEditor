//! A model's hierarchy as the Editor's objects of its prefab: a game object
//! and transform per node, a mesh filter and renderer (or a skinned mesh
//! renderer) per node with a mesh, and an animator on the root of a rig.
//! A prefab instance of the model is unpacked from these documents, as
//! from a prefab's. References to the model's assets (meshes, the avatar)
//! name the model, so that an instance keeps them.
use crate::yaml::Document;
use serde_json::{Map, Value, json};

/// What a node of the model holds, with the file IDs of its objects.
pub struct Placed {
    pub game_object: i64,
    pub transform: i64,
    /// The node's name and its local transform in Unity's space.
    pub name: String,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub parent: Option<usize>,
    pub renderer: Option<Renderer>,
    /// The animator of the root: its file ID and the avatar's.
    pub animator: Option<(i64, Option<i64>)>,
}

/// A box as centre and extent.
pub type Bounds = ([f32; 3], [f32; 3]);

/// A skinned renderer's bones (as node positions), root bone and bounds.
pub type Skinning = (Vec<usize>, Option<usize>, Bounds);

pub struct Renderer {
    /// The mesh filter's file ID, for a renderer without a skin.
    pub filter: Option<i64>,
    pub renderer: i64,
    pub mesh: i64,
    pub materials: Vec<Value>,
    /// A skinned renderer's bones (as node positions), root bone and
    /// bounds.
    pub skin: Option<Skinning>,
}

fn local(id: i64) -> Value {
    json!({ "fileID": id.to_string() })
}

fn vector(v: [f32; 3]) -> Value {
    json!({ "x": v[0], "y": v[1], "z": v[2] })
}

fn document(class_id: i64, type_name: &str, file_id: i64, data: Value) -> Document {
    Document {
        class_id,
        file_id: file_id.to_string(),
        type_name: type_name.to_string(),
        data: match data {
            Value::Object(map) => map,
            _ => Map::new(),
        },
        stripped: false,
    }
}

/// The documents of the hierarchy, `guid` being the model's.
pub fn documents(nodes: &[Placed], guid: &str) -> Vec<Document> {
    let asset = |id: i64| json!({ "fileID": id.to_string(), "guid": guid, "type": "3" });
    let mut out = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let mut components = vec![json!({ "component": local(node.transform) })];
        if let Some(renderer) = &node.renderer {
            if let Some(filter) = renderer.filter {
                components.push(json!({ "component": local(filter) }));
            }
            components.push(json!({ "component": local(renderer.renderer) }));
        }
        if let Some((animator, _)) = node.animator {
            components.push(json!({ "component": local(animator) }));
        }
        out.push(document(
            1,
            "GameObject",
            node.game_object,
            json!({
                "serializedVersion": "6",
                "m_Component": components,
                "m_Layer": "0",
                "m_Name": node.name,
                "m_TagString": "Untagged",
                "m_NavMeshLayer": "0",
                "m_StaticEditorFlags": "0",
                "m_IsActive": "1",
            }),
        ));
        let children: Vec<Value> = nodes
            .iter()
            .filter(|other| other.parent == Some(index))
            .map(|child| local(child.transform))
            .collect();
        let [x, y, z, w] = node.rotation;
        out.push(document(
            4,
            "Transform",
            node.transform,
            json!({
                "m_GameObject": local(node.game_object),
                "serializedVersion": "2",
                "m_LocalRotation": { "x": x, "y": y, "z": z, "w": w },
                "m_LocalPosition": vector(node.position),
                "m_LocalScale": vector(node.scale),
                "m_ConstrainProportionsScale": "0",
                "m_Children": children,
                "m_Father": local(node.parent.map_or(0, |p| nodes[p].transform)),
            }),
        ));
        if let Some(renderer) = &node.renderer {
            if let Some(filter) = renderer.filter {
                out.push(document(
                    33,
                    "MeshFilter",
                    filter,
                    json!({
                        "m_GameObject": local(node.game_object),
                        "m_Mesh": asset(renderer.mesh),
                    }),
                ));
                out.push(document(
                    23,
                    "MeshRenderer",
                    renderer.renderer,
                    json!({
                        "m_GameObject": local(node.game_object),
                        "m_Enabled": "1",
                        "m_CastShadows": "1",
                        "m_ReceiveShadows": "1",
                        "m_DynamicOccludee": "1",
                        "m_MotionVectors": "1",
                        "m_LightProbeUsage": "1",
                        "m_ReflectionProbeUsage": "1",
                        "m_RenderingLayerMask": "1",
                        "m_Materials": renderer.materials,
                    }),
                ));
            }
            if let Some((bones, root, (center, extent))) = &renderer.skin {
                let bones: Vec<Value> = bones.iter().map(|&b| local(nodes[b].transform)).collect();
                out.push(document(
                    137,
                    "SkinnedMeshRenderer",
                    renderer.renderer,
                    json!({
                        "m_GameObject": local(node.game_object),
                        "m_Enabled": "1",
                        "m_CastShadows": "1",
                        "m_ReceiveShadows": "1",
                        "m_DynamicOccludee": "1",
                        "m_MotionVectors": "1",
                        "m_LightProbeUsage": "1",
                        "m_ReflectionProbeUsage": "1",
                        "m_RenderingLayerMask": "1",
                        "m_Materials": renderer.materials,
                        "m_Quality": "0",
                        "m_UpdateWhenOffscreen": "0",
                        "m_SkinnedMotionVectors": "1",
                        "m_Mesh": asset(renderer.mesh),
                        "m_Bones": bones,
                        "m_RootBone": local(root.map_or(0, |r| nodes[r].transform)),
                        "m_AABB": { "m_Center": vector(*center), "m_Extent": vector(*extent) },
                        "m_DirtyAABB": "0",
                    }),
                ));
            }
        }
        if let Some((animator, avatar)) = node.animator {
            out.push(document(
                95,
                "Animator",
                animator,
                json!({
                    "serializedVersion": "7",
                    "m_GameObject": local(node.game_object),
                    "m_Enabled": "1",
                    "m_Avatar": avatar.map_or(json!({ "fileID": "0" }), asset),
                    "m_Controller": { "fileID": "0" },
                    "m_CullingMode": "0",
                    "m_UpdateMode": "0",
                    "m_ApplyRootMotion": "0",
                    "m_LinearVelocityBlending": "0",
                    "m_StabilizeFeet": "0",
                    "m_WarningMessage": "",
                    "m_HasTransformHierarchy": "1",
                    "m_AllowConstantClipSamplingOptimization": "1",
                    "m_KeepAnimatorStateOnDisable": "0",
                    "m_WriteDefaultValuesOnDisable": "0",
                }),
            ));
        }
    }
    out
}
