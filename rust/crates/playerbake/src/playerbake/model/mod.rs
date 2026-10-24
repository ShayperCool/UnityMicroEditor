//! Models (`.fbx`) imported without the Editor: what the Editor's model
//! importer makes of a file, read through `ufbx`.
mod mesh;
mod settings;
mod space;
mod take;

pub use settings::Settings;

use crate::playerbake::typetree::Val;
use anyhow::{Result, anyhow, bail};
use std::{collections::HashMap, fmt::Write as _, path::Path};

const MESH: i64 = 43;
const CLIP: i64 = 74;

/// How the file is read: as the Editor's importer has it, a node's origin
/// moves to its pivot and its geometry the other way.
fn load_options() -> ufbx::LoadOpts<'static> {
    ufbx::LoadOpts {
        pivot_handling: ufbx::PivotHandling::AdjustToPivot,
        ..Default::default()
    }
}

/// A node of the model as Unity names it.
struct Node<'a> {
    fbx: &'a ufbx::Node,
    name: String,
    /// The path below the model's root, `a/b/c`; empty for the root.
    path: String,
    parent: Option<usize>,
}

/// The nodes of a model in the importer's order, the root first. A file
/// whose scene holds one node at its top has that node as the model's
/// root; the root is named after the file. A name met again is told apart
/// by a number, in order of the hierarchy.
fn nodes<'a>(scene: &'a ufbx::Scene, file_name: &str) -> Vec<Node<'a>> {
    let mut root: &ufbx::Node = &scene.root_node;
    if root.children.len() == 1 && root.mesh.is_none() {
        root = &root.children[0];
    }
    let mut nodes = vec![Node {
        fbx: root,
        name: file_name.to_string(),
        path: String::new(),
        parent: None,
    }];
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    fn walk<'a>(
        node: &'a ufbx::Node,
        parent: usize,
        nodes: &mut Vec<Node<'a>>,
        seen: &mut HashMap<String, usize>,
        taken: &mut std::collections::HashSet<String>,
    ) {
        for child in node.children.iter() {
            let base = child.element.name.to_string();
            let mut name = base.clone();
            if taken.contains(&name) {
                let count = seen.entry(base.clone()).or_insert(0);
                loop {
                    *count += 1;
                    name = format!("{base} {count}");
                    if !taken.contains(&name) {
                        break;
                    }
                }
            }
            taken.insert(name.clone());
            let path = match nodes[parent].path.as_str() {
                "" => name.clone(),
                above => format!("{above}/{name}"),
            };
            nodes.push(Node {
                fbx: child,
                name,
                path,
                parent: Some(parent),
            });
            let index = nodes.len() - 1;
            walk(child, index, nodes, seen, taken);
        }
    }
    walk(root, 0, &mut nodes, &mut seen, &mut taken);
    nodes
}

/// What the importer makes of a model: its objects in the Player's form,
/// by file ID and class.
pub struct Imported {
    pub objects: Vec<(i64, i64, Val)>,
    /// What the tool does not import yet, by object.
    pub refused: Vec<String>,
}

/// The file IDs of a model's objects.
struct Ids<'a> {
    settings: &'a Settings,
    used: std::collections::HashSet<i64>,
}

impl Ids<'_> {
    fn of(&mut self, class: i64, name: &str) -> Result<i64> {
        if let Some(&id) = self.settings.recorded.get(&(class, name.to_string())) {
            self.used.insert(id);
            return Ok(id);
        }
        if self.settings.legacy_ids {
            // The next free ID of the class, as the importer gave them.
            let mut id = class * 100_000;
            while self.used.contains(&id)
                || self.settings.recorded.values().any(|known| *known == id)
            {
                id += 2;
            }
            self.used.insert(id);
            return Ok(id);
        }
        bail!("the file ID of {name}, a model's object of class {class}")
    }
}

/// Imports a model's meshes.
pub fn import(path: &Path, settings: &Settings) -> Result<Imported> {
    let scene = ufbx::load_file(&path.to_string_lossy(), load_options())
        .map_err(|error| anyhow!("{}: {}", path.display(), error.description))?;
    let file_name = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unit = if settings.use_file_scale {
        scene.settings.unit_meters as f32
    } else {
        1.0
    };
    let space = space::Space::new(settings.global_scale * unit);
    let nodes = nodes(&scene, &file_name);
    let by_element: HashMap<u32, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.fbx.element.element_id, index))
        .collect();
    let mut ids = Ids {
        settings,
        used: Default::default(),
    };
    let mut imported = Imported {
        objects: Vec::new(),
        refused: Vec::new(),
    };
    for node in &nodes {
        let Some(mesh) = node.fbx.mesh.as_ref() else {
            continue;
        };
        let skin = match mesh.skin_deformers.first() {
            None => None,
            Some(deformer) => {
                let mut paths = Vec::new();
                let mut bind_poses = Vec::new();
                let mut bones = Vec::new();
                for cluster in deformer.clusters.iter() {
                    let Some(bone) = cluster
                        .bone_node
                        .as_ref()
                        .and_then(|b| by_element.get(&b.element.element_id))
                    else {
                        bail!("{}: a bone outside the model", node.name);
                    };
                    bones.push(*bone);
                    paths.push(nodes[*bone].path.clone());
                    bind_poses.push(space.matrix(&cluster.mesh_node_to_bone));
                }
                // The bone above all others, or the one their paths share.
                let ancestor = |above: usize, below: usize| {
                    let mut at = Some(below);
                    while let Some(index) = at {
                        if index == above {
                            return true;
                        }
                        at = nodes[index].parent;
                    }
                    false
                };
                let mut root = bones.first().copied().unwrap_or(0);
                while !bones.iter().all(|&bone| ancestor(root, bone)) {
                    match nodes[root].parent {
                        Some(parent) => root = parent,
                        None => break,
                    }
                }
                Some(mesh::Skin {
                    paths,
                    root: nodes[root].path.clone(),
                    bind_poses,
                })
            }
        };
        // A mesh is named after its node; the root's after the node the
        // file has there, not after the file.
        let name = match node.parent {
            None => node.fbx.element.name.to_string(),
            Some(_) => node.name.clone(),
        };
        match mesh::build(&name, node.fbx, mesh, settings, &space, skin.as_ref()) {
            Ok(value) => {
                let id = ids.of(MESH, &name)?;
                imported.objects.push((id, MESH, value));
            }
            Err(error) => imported
                .refused
                .push(format!("mesh {}: {error:#}", node.name)),
        }
    }
    if settings.import_animation && settings.animation_type != 0 {
        let animated: Vec<take::Animated<'_>> = nodes
            .iter()
            .map(|node| take::Animated {
                node: node.fbx,
                path: &node.path,
            })
            .collect();
        for clip in take::clip_settings(&scene, settings) {
            if clip.extras {
                imported
                    .refused
                    .push(format!("clip {}: events or curves of its own", clip.name));
                continue;
            }
            let Some(fields) = take::clip(&scene, &animated, &clip, &space) else {
                imported
                    .refused
                    .push(format!("clip {}: no take {}", clip.name, clip.take));
                continue;
            };
            match crate::playerbake::clip::bake_clip_parts(
                &fields,
                &crate::playerbake::clip::NoScripts,
            ) {
                Ok(baked) => {
                    let id = ids.of(CLIP, &clip.name)?;
                    imported.objects.push((id, CLIP, baked.value));
                }
                Err(error) => imported
                    .refused
                    .push(format!("clip {}: {error:#}", clip.name)),
            }
        }
    }
    Ok(imported)
}

/// The clips a model's import makes, by file ID, with their names and
/// whether they loop, as a controller that plays them needs to know.
pub fn clips(path: &Path, settings: &Settings) -> Result<Vec<(i64, String, bool)>> {
    if !settings.import_animation || settings.animation_type == 0 {
        return Ok(Vec::new());
    }
    let listed = match settings.clips.is_empty() {
        false => settings.clips.clone(),
        true => {
            let scene = ufbx::load_file(&path.to_string_lossy(), load_options())
                .map_err(|error| anyhow!("{}: {}", path.display(), error.description))?;
            take::clip_settings(&scene, settings)
        }
    };
    let mut ids = Ids {
        settings,
        used: Default::default(),
    };
    let mut out = Vec::new();
    for clip in listed {
        out.push((ids.of(CLIP, &clip.name)?, clip.name, clip.loop_time));
    }
    Ok(out)
}

/// A description of a model file: its settings, nodes, meshes and takes.
pub fn inspect(path: &std::path::Path) -> Result<String> {
    let scene = ufbx::load_file(&path.to_string_lossy(), load_options())
        .map_err(|error| anyhow!("{}: {}", path.display(), error.description))?;
    let mut out = String::new();
    let settings = &scene.settings;
    writeln!(
        out,
        "units {} (original {}), axes {:?} {:?} {:?}, fps {}, creator {:?}",
        settings.unit_meters,
        settings.original_unit_meters,
        settings.axes.right,
        settings.axes.up,
        settings.axes.front,
        settings.frames_per_second,
        scene.metadata.creator
    )?;
    fn walk(out: &mut String, node: &ufbx::Node, depth: usize) -> std::fmt::Result {
        let t = &node.local_transform;
        writeln!(
            out,
            "{}node {:?} root={} t=({:.4},{:.4},{:.4}) r=({:.4},{:.4},{:.4},{:.4}) s=({:.4},{:.4},{:.4}) mesh={:?} attrib={:?} materials={:?}",
            "  ".repeat(depth),
            node.element.name,
            node.is_root,
            t.translation.x,
            t.translation.y,
            t.translation.z,
            t.rotation.x,
            t.rotation.y,
            t.rotation.z,
            t.rotation.w,
            t.scale.x,
            t.scale.y,
            t.scale.z,
            node.mesh.as_ref().map(|m| m.element.name.to_string()),
            node.attrib_type,
            node.materials
                .iter()
                .map(|m| m.element.name.to_string())
                .collect::<Vec<_>>(),
        )?;
        if std::env::var_os("UME_FBX_DUMP").is_some() {
            let w = &node.node_to_world;
            let g = &node.geometry_to_node;
            writeln!(
                out,
                "{}  world t=({:.4},{:.4},{:.4}) geometry t=({:.4},{:.4},{:.4}) pivots: rot={:?} scale={:?} geometric={}",
                "  ".repeat(depth),
                w.m03,
                w.m13,
                w.m23,
                g.m03,
                g.m13,
                g.m23,
                ufbx::find_prop(&node.element.props, "RotationPivot").map(|p| p.value_vec4.y),
                ufbx::find_prop(&node.element.props, "ScalingPivot").map(|p| p.value_vec4.y),
                node.has_geometry_transform,
            )?;
        }
        for child in node.children.iter() {
            walk(out, child, depth + 1)?;
        }
        Ok(())
    }
    walk(&mut out, &scene.root_node, 0)?;
    for mesh in scene.meshes.iter() {
        if std::env::var_os("UME_FBX_DUMP").is_some() {
            for face in mesh.faces.iter().take(6) {
                let corners: Vec<String> = (face.index_begin..face.index_begin + face.num_indices)
                    .map(|i| {
                        let i = i as usize;
                        let p = mesh.vertex_position[i];
                        let c = if mesh.vertex_color.exists {
                            let c = mesh.vertex_color[i];
                            format!(" c=({:.3},{:.3},{:.3},{:.3})", c.x, c.y, c.z, c.w)
                        } else {
                            String::new()
                        };
                        let uv = if mesh.vertex_uv.exists {
                            let t = mesh.vertex_uv[i];
                            format!(" uv=({:.4},{:.4})", t.x, t.y)
                        } else {
                            String::new()
                        };
                        format!(
                            "v{}=({:.4},{:.4},{:.4}){c}{uv}",
                            mesh.vertex_indices[i], p.x, p.y, p.z
                        )
                    })
                    .collect();
                writeln!(out, "  face {}", corners.join(" "))?;
            }
        }
        writeln!(
            out,
            "mesh {:?}: vertices {}, indices {}, faces {}, triangles {}, normals {}, uv {}, colors {}, tangents {}, skins {}, blends {}, materials {}",
            mesh.element.name,
            mesh.num_vertices,
            mesh.num_indices,
            mesh.num_faces,
            mesh.num_triangles,
            mesh.vertex_normal.exists,
            mesh.uv_sets.len(),
            mesh.color_sets.len(),
            mesh.vertex_tangent.exists,
            mesh.skin_deformers.len(),
            mesh.blend_deformers.len(),
            mesh.materials.len(),
        )?;
    }
    for stack in scene.anim_stacks.iter() {
        writeln!(
            out,
            "take {:?}: {}..{} layers {}",
            stack.element.name,
            stack.time_begin,
            stack.time_end,
            stack.layers.len()
        )?;
    }
    Ok(out)
}
