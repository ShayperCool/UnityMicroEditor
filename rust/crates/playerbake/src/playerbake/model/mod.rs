//! Models (`.fbx`) imported without the Editor: what the Editor's model
//! importer makes of a file, read through `ufbx`.
mod avatar;
mod hierarchy;
mod material;
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
const AVATAR: i64 = 90;

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
    /// The node's name in the model: the file's, sanitized and told apart
    /// from the names it shares; the root's is the file's name.
    name: String,
    /// The file's name of the node, sanitized; empty for a root the import
    /// makes.
    original: String,
    /// The path below the model's root, `a/b/c`; empty for the root.
    path: String,
    /// The path the importer hashes for the node's IDs, the root renamed
    /// `root` under `//RootNode`.
    key: String,
    parent: Option<usize>,
}

/// `ModelImporter::SanitizeName`: control characters become `_`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if (c as u32) < 0x20 { '_' } else { c })
        .collect()
}

/// Names that order as `SemiNumericCompare` orders them: without case, runs
/// of digits by their value.
fn natural(name: &str) -> Vec<(u8, u128, String)> {
    let mut parts = Vec::new();
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                digits.push(d);
                chars.next();
            }
            parts.push((0, digits.parse().unwrap_or(u128::MAX), String::new()));
        } else {
            chars.next();
            parts.push((1, 0, c.to_lowercase().collect()));
        }
    }
    parts
}

/// The file's nodes below its scene root, in pre-order, with their parents
/// (positions in the list) and names made unique as the import makes them:
/// among siblings (`nodeNameCollisionStrategy` 1), the first keeping its
/// name and the others, from the last, taking the first free `name 1`,
/// `name 2`, ...; or across the file (0), by passes over the nodes sorted by
/// name, depth and parent path until no name repeats.
fn file_nodes<'a>(
    scene: &'a ufbx::Scene,
    settings: &Settings,
) -> (Vec<&'a ufbx::Node>, Vec<Option<usize>>, Vec<String>) {
    let mut list: Vec<&ufbx::Node> = Vec::new();
    let mut parents: Vec<Option<usize>> = Vec::new();
    fn walk<'a>(
        node: &'a ufbx::Node,
        parent: Option<usize>,
        list: &mut Vec<&'a ufbx::Node>,
        parents: &mut Vec<Option<usize>>,
    ) {
        list.push(node);
        parents.push(parent);
        let index = list.len() - 1;
        for child in node.children.iter() {
            walk(child, Some(index), list, parents);
        }
    }
    for top in scene.root_node.children.iter() {
        walk(top, None, &mut list, &mut parents);
    }
    let mut names: Vec<String> = list
        .iter()
        .map(|node| sanitize(&node.element.name))
        .collect();
    if settings.name_collisions == 1 {
        let mut groups: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        for (index, parent) in parents.iter().enumerate() {
            groups.entry(*parent).or_default().push(index);
        }
        for siblings in groups.values() {
            let mut seen: std::collections::HashSet<String> = Default::default();
            let mut repeats = Vec::new();
            for &index in siblings {
                if !seen.insert(names[index].clone()) {
                    repeats.push(index);
                }
            }
            for &index in repeats.iter().rev() {
                let mut count = 1;
                let mut name = format!("{} {count}", names[index]);
                while seen.contains(&name) {
                    count += 1;
                    name = format!("{} {count}", names[index]);
                }
                seen.insert(name.clone());
                names[index] = name;
            }
        }
    } else {
        loop {
            let path = |index: usize, names: &[String]| {
                let mut parts = Vec::new();
                let mut at = parents[index];
                while let Some(parent) = at {
                    parts.push(names[parent].clone());
                    at = parents[parent];
                }
                parts.reverse();
                let depth = parts.len();
                (
                    depth,
                    parts.iter().map(|p| format!("/{p}")).collect::<String>(),
                )
            };
            let mut order: Vec<(Vec<u8>, usize, Vec<u8>, usize)> = (0..list.len())
                .map(|index| {
                    let (depth, above) = path(index, &names);
                    (
                        names[index].as_bytes().to_vec(),
                        depth,
                        above.into_bytes(),
                        index,
                    )
                })
                .collect();
            order.sort_by(|a, b| (&a.0, a.1, &a.2).cmp(&(&b.0, b.1, &b.2)));
            let mut changed = false;
            let mut count = 0;
            for at in 1..order.len() {
                if order[at].0 == order[at - 1].0 {
                    count += 1;
                    let index = order[at].3;
                    names[index] = format!("{} {count}", names[index]);
                    changed = true;
                } else {
                    count = 0;
                }
            }
            if !changed {
                break;
            }
        }
    }
    (list, parents, names)
}

/// The nodes of a model in the importer's order, the root first. The single
/// node at the top of a file is the model's root unless the import keeps
/// the hierarchy, it is a bone or a humanoid's extra root is wanted;
/// otherwise the import makes a root above the top nodes. The root is
/// named after the file; children are sorted by name when the import says
/// so.
fn nodes<'a>(scene: &'a ufbx::Scene, file_name: &str, settings: &Settings) -> Vec<Node<'a>> {
    let (list, parents, names) = file_nodes(scene, settings);
    let tops: Vec<usize> = (0..list.len()).filter(|&i| parents[i].is_none()).collect();
    let collapse = !settings.preserve_hierarchy
        && tops.len() == 1
        && list[tops[0]].attrib_type != ufbx::ElementType::Bone
        && !settings.extra_root;
    let (root_fbx, root_original, mut pending): (&ufbx::Node, String, Vec<usize>) = if collapse {
        let top = tops[0];
        let children = (0..list.len())
            .filter(|&i| parents[i] == Some(top))
            .collect();
        (list[top], sanitize(&list[top].element.name), children)
    } else {
        (&scene.root_node, String::new(), tops)
    };
    let sort = |children: &mut Vec<usize>| {
        if settings.sort_by_name {
            children.sort_by_key(|&i| natural(&names[i]));
        }
    };
    sort(&mut pending);
    let mut nodes = vec![Node {
        fbx: root_fbx,
        name: file_name.to_string(),
        original: root_original,
        path: String::new(),
        key: "//RootNode/root".to_string(),
        parent: None,
    }];
    fn walk<'a>(
        children: Vec<usize>,
        parent: usize,
        nodes: &mut Vec<Node<'a>>,
        list: &[&'a ufbx::Node],
        parents: &[Option<usize>],
        names: &[String],
        sort: &dyn Fn(&mut Vec<usize>),
    ) {
        for child in children {
            let name = names[child].clone();
            let path = match nodes[parent].path.as_str() {
                "" => name.clone(),
                above => format!("{above}/{name}"),
            };
            let key = format!("{}/{name}", nodes[parent].key);
            nodes.push(Node {
                fbx: list[child],
                name,
                original: sanitize(&list[child].element.name),
                path,
                key,
                parent: Some(parent),
            });
            let index = nodes.len() - 1;
            let mut below: Vec<usize> = (0..list.len())
                .filter(|&i| parents[i] == Some(child))
                .collect();
            sort(&mut below);
            walk(below, index, nodes, list, parents, names, sort);
        }
    }
    walk(pending, 0, &mut nodes, &list, &parents, &names, &sort);
    nodes
}

/// What the importer makes of a model: its objects in the Player's form,
/// by file ID and class.
pub struct Imported {
    pub objects: Vec<(i64, i64, Val)>,
    /// The model's hierarchy as the Editor's objects of its prefab.
    pub documents: Vec<crate::yaml::Document>,
    /// What the tool does not import yet, by object.
    pub refused: Vec<String>,
}

/// The file IDs of a model's objects: as the `.meta`'s table records them;
/// in a `.meta` of the format before 2019.3, the class's next free ID;
/// otherwise the importer's hash of the object's class and key
/// (`XXH64("Type:<class>-><key><n>")`, `n` counting IDs already taken).
struct Ids<'a> {
    settings: &'a Settings,
    used: std::collections::HashSet<i64>,
}

impl Ids<'_> {
    /// `name` is the object's name in a recorded table, `key` what the
    /// importer hashes.
    fn of(&mut self, class: i64, class_name: &str, name: &str, key: &str) -> i64 {
        if let Some(&id) = self.settings.recorded.get(&(class, name.to_string())) {
            self.used.insert(id);
            return id;
        }
        if self.settings.legacy_ids {
            let mut id = class * 100_000;
            while self.used.contains(&id)
                || self.settings.recorded.values().any(|known| *known == id)
            {
                id += 2;
            }
            self.used.insert(id);
            return id;
        }
        let mut n = 0;
        loop {
            let text = format!("Type:{class_name}->{key}{n}");
            let id = xxhash_rust::xxh64::xxh64(text.as_bytes(), 0) as i64;
            if self.used.insert(id) {
                return id;
            }
            n += 1;
        }
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
    let nodes = nodes(&scene, &file_name, settings);
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
        documents: Vec::new(),
        refused: Vec::new(),
    };
    // Each mesh node's mesh, its bones and root bone, and its bounds.
    type Skinned = Option<(Vec<usize>, usize)>;
    let mut meshes: HashMap<usize, (i64, Skinned, hierarchy::Bounds)> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
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
                Some((
                    mesh::Skin {
                        paths,
                        root: nodes[root].path.clone(),
                        bind_poses,
                    },
                    bones,
                    root,
                ))
            }
        };
        // A mesh is named after the file's name of its node.
        let name = node.original.clone();
        match mesh::build(
            &name,
            node.fbx,
            mesh,
            settings,
            &space,
            skin.as_ref().map(|(skin, ..)| skin),
        ) {
            Ok(value) => {
                let id = ids.of(MESH, "Mesh", &name, &name);
                let bounds = value
                    .field("m_LocalAABB")
                    .map(|b| {
                        let v = |name: &str| {
                            let field = |axis: &str| match b.field(name).and_then(|f| f.field(axis))
                            {
                                Some(Val::F(x)) => *x,
                                _ => 0.0,
                            };
                            [field("x"), field("y"), field("z")]
                        };
                        (v("m_Center"), v("m_Extent"))
                    })
                    .unwrap_or_default();
                meshes.insert(
                    index,
                    (id, skin.map(|(_, bones, root)| (bones, root)), bounds),
                );
                imported.objects.push((id, MESH, value));
            }
            Err(error) => imported
                .refused
                .push(format!("mesh {}: {error:#}", node.name)),
        }
    }
    // A generic rig's avatar, of every node.
    if settings.animation_type == 2 && settings.create_avatar {
        let bones: Vec<avatar::Bone> = nodes
            .iter()
            .map(|node| {
                let t = &node.fbx.local_transform;
                avatar::Bone {
                    path: node.path.clone(),
                    name: match node.parent {
                        None => String::new(),
                        Some(_) => node.name.clone(),
                    },
                    parent: node.parent.map_or(-1, |p| p as i64),
                    transform: match node.parent {
                        // The root stays where the model is placed.
                        None => ([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]),
                        Some(_) => (
                            space.position([
                                t.translation.x as f32,
                                t.translation.y as f32,
                                t.translation.z as f32,
                            ]),
                            space.rotation([
                                t.rotation.x as f32,
                                t.rotation.y as f32,
                                t.rotation.z as f32,
                                t.rotation.w as f32,
                            ]),
                            [t.scale.x as f32, t.scale.y as f32, t.scale.z as f32],
                        ),
                    },
                }
            })
            .collect();
        // An avatar's ID is the class's first, not a hash.
        let id = settings
            .recorded
            .get(&(AVATAR, "//RootNode".to_string()))
            .copied()
            .unwrap_or(AVATAR * 100_000);
        imported.objects.push((
            id,
            AVATAR,
            avatar::generic(&file_name, &bones, &settings.human),
        ));
    } else if settings.animation_type == 3 && settings.create_avatar {
        imported.refused.push("avatar: a humanoid rig".to_string());
    }
    // The materials the import makes of the file's own, by name, or the
    // assets a model of the format before finds.
    let mut embedded: HashMap<String, i64> = HashMap::new();
    let mut found: HashMap<String, serde_json::Value> = HashMap::new();
    if let Some((naming, search)) = settings.legacy_materials {
        for fbx_material in scene.materials.iter() {
            let name = sanitize(&fbx_material.element.name);
            if let Some(reference) = material::legacy_asset(fbx_material, path, naming, search) {
                found.insert(name, reference);
            }
        }
    } else if settings.import_materials {
        let project = path
            .ancestors()
            .find(|dir| dir.join("Assets").is_dir() && dir.join("ProjectSettings").is_dir())
            .unwrap_or(Path::new("."));
        let context = material::Context::of_project(project, path);
        for fbx_material in scene.materials.iter() {
            let name = sanitize(&fbx_material.element.name);
            if embedded.contains_key(&name) || settings.external_materials.contains_key(&name) {
                continue;
            }
            let id = ids.of(21, "Material", &name, &name);
            if let Some(document) = material::document(fbx_material, id, &context) {
                imported.documents.push(document);
                embedded.insert(name, id);
            }
        }
    }
    // The hierarchy, named in the `.meta`'s tables as the root and the
    // nodes' unique names.
    let rig = matches!(settings.animation_type, 2 | 3);
    let avatar_id = imported
        .objects
        .iter()
        .find(|(_, class, _)| *class == AVATAR)
        .map(|(id, ..)| *id);
    let mut placed = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let key = match node.parent {
            None => "//RootNode".to_string(),
            Some(_) => node.name.clone(),
        };
        let t = &node.fbx.local_transform;
        let (position, rotation, scale) = match node.parent {
            None => ([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]),
            Some(_) => (
                space.position([
                    t.translation.x as f32,
                    t.translation.y as f32,
                    t.translation.z as f32,
                ]),
                space.rotation([
                    t.rotation.x as f32,
                    t.rotation.y as f32,
                    t.rotation.z as f32,
                    t.rotation.w as f32,
                ]),
                [t.scale.x as f32, t.scale.y as f32, t.scale.z as f32],
            ),
        };
        let component = |class: &str| format!("{}/{class}", node.key);
        let game_object = ids.of(1, "GameObject", &key, &node.key);
        let transform = ids.of(4, "Transform", &key, &component("Transform"));
        let renderer = match meshes.remove(&index) {
            None => None,
            Some((mesh, skin, bounds)) => {
                let slots = node
                    .fbx
                    .mesh
                    .as_ref()
                    .map_or(1, |m| m.materials.len().max(1));
                let materials = (0..slots)
                    .map(|slot| settings.material(node.fbx, slot, &embedded, &found))
                    .collect();
                Some(match skin {
                    None => hierarchy::Renderer {
                        filter: Some(ids.of(33, "MeshFilter", &key, &component("MeshFilter"))),
                        renderer: ids.of(23, "MeshRenderer", &key, &component("MeshRenderer")),
                        mesh,
                        materials,
                        skin: None,
                    },
                    Some((bones, root)) => hierarchy::Renderer {
                        filter: None,
                        renderer: ids.of(
                            137,
                            "SkinnedMeshRenderer",
                            &key,
                            &component("SkinnedMeshRenderer"),
                        ),
                        mesh,
                        materials,
                        skin: Some((bones, Some(root), bounds)),
                    },
                })
            }
        };
        let animator = match (node.parent, rig) {
            (None, true) if settings.create_avatar => Some((
                ids.of(95, "Animator", &key, &component("Animator")),
                avatar_id,
            )),
            _ => None,
        };
        placed.push(hierarchy::Placed {
            game_object,
            transform,
            name: node.name.clone(),
            position,
            rotation,
            scale,
            parent: node.parent,
            renderer,
            animator,
        });
    }
    imported
        .documents
        .extend(hierarchy::documents(&placed, &settings.guid));
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
                    let id = match clip.internal_id {
                        0 => ids.of(CLIP, "AnimationClip", &clip.name, &sanitize(&clip.take)),
                        id => id,
                    };
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
        let id = match clip.internal_id {
            0 => ids.of(CLIP, "AnimationClip", &clip.name, &sanitize(&clip.take)),
            id => id,
        };
        out.push((id, clip.name, clip.loop_time));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(meta: &str) -> Settings {
        Settings::read(meta).unwrap()
    }

    #[test]
    fn hashed_ids_follow_the_importer() {
        let settings = settings("guid: 0\nModelImporter:\n  internalIDToNameTable: []\n");
        let mut ids = Ids {
            settings: &settings,
            used: Default::default(),
        };
        // The root of every model with hashed IDs, and gas_tanks' renderer
        // of `gas_tanks.Prop_Mid` (Dungeon).
        assert_eq!(
            ids.of(1, "GameObject", "//RootNode", "//RootNode/root"),
            919132149155446097
        );
        assert_eq!(
            ids.of(4, "Transform", "//RootNode", "//RootNode/root/Transform"),
            -8679921383154817045
        );
        assert_eq!(
            ids.of(
                23,
                "MeshRenderer",
                "gas_tanks.Prop_Mid",
                "//RootNode/root/gas_tanks.Prop_Mid/MeshRenderer"
            ),
            -3372668875357190546
        );
        // A key taken again counts up.
        let again = ids.of(1, "GameObject", "//RootNode", "//RootNode/root");
        assert_ne!(again, 919132149155446097);
    }

    #[test]
    fn recorded_and_legacy_ids_come_first() {
        let settings = settings(
            "guid: 0\nModelImporter:\n  fileIDToRecycleName:\n    100000: //RootNode\n    4300002: great_ice\n",
        );
        let mut ids = Ids {
            settings: &settings,
            used: Default::default(),
        };
        assert_eq!(ids.of(43, "Mesh", "great_ice", "great_ice"), 4300002);
        assert_eq!(ids.of(43, "Mesh", "aisberg", "aisberg"), 4300000);
        assert_eq!(ids.of(43, "Mesh", "other", "other"), 4300004);
    }

    #[test]
    fn names_sort_with_numbers_by_value() {
        let mut names = vec!["b10", "B2", "a", "b1"];
        names.sort_by_key(|n| natural(n));
        assert_eq!(names, ["a", "b1", "B2", "b10"]);
        assert_eq!(sanitize("a\tb"), "a_b");
    }
}
