//! A node's mesh as the model importer makes it: the polygons in the
//! node's space, mirrored on X into Unity's left-handed space and scaled,
//! triangulated, split into one submesh per material, identical corners
//! welded, and laid out in the Player's vertex streams.
use super::{
    settings::{Settings, Source},
    space::Space,
};
use crate::playerbake::typetree::Val;
use anyhow::{Result, bail};
use std::collections::HashMap;

/// The bones a skinned mesh is bound to, as the model's hierarchy names
/// them.
pub struct Skin {
    /// Each bone's path below the model's root, in the mesh's bone order.
    pub paths: Vec<String>,
    /// The path of the bone the renderer takes as its root.
    pub root: String,
    /// Each bone's bind pose: from the mesh's space into the bone's.
    pub bind_poses: Vec<[[f32; 4]; 4]>,
}

#[derive(Clone, Default)]
struct Corner {
    position: [f32; 3],
    normal: Option<[f32; 3]>,
    tangent: Option<[f32; 4]>,
    color: Option<[u8; 4]>,
    uvs: Vec<[f32; 2]>,
    /// Bone indices and weights, heaviest first.
    weights: Vec<(u32, f32)>,
}

impl Corner {
    fn key(&self) -> Vec<u32> {
        let mut key: Vec<u32> = self.position.iter().map(|v| v.to_bits()).collect();
        if let Some(normal) = &self.normal {
            key.extend(normal.iter().map(|v| v.to_bits()));
        }
        if let Some(tangent) = &self.tangent {
            key.extend(tangent.iter().map(|v| v.to_bits()));
        }
        if let Some(color) = &self.color {
            key.push(u32::from_le_bytes(*color));
        }
        for uv in &self.uvs {
            key.extend(uv.iter().map(|v| v.to_bits()));
        }
        for (bone, weight) in &self.weights {
            key.push(*bone);
            key.push(weight.to_bits());
        }
        key
    }
}

fn color(value: ufbx::Vec4) -> [u8; 4] {
    let byte = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(value.x), byte(value.y), byte(value.z), byte(value.w)]
}

fn aabb(points: impl Iterator<Item = [f32; 3]>) -> ([f32; 3], [f32; 3]) {
    let (mut min, mut max) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    let mut any = false;
    for point in points {
        any = true;
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    if !any {
        return ([0.0; 3], [0.0; 3]);
    }
    let center = [0, 1, 2].map(|a| (min[a] + max[a]) * 0.5);
    let extent = [0, 1, 2].map(|a| (max[a] - min[a]) * 0.5);
    (center, extent)
}

fn vector(v: [f32; 3]) -> Val {
    Val::map([
        ("x", Val::F(v[0])),
        ("y", Val::F(v[1])),
        ("z", Val::F(v[2])),
    ])
}

fn bounds((center, extent): ([f32; 3], [f32; 3])) -> Val {
    Val::map([("m_Center", vector(center)), ("m_Extent", vector(extent))])
}

fn matrix(m: &[[f32; 4]; 4]) -> Val {
    // Column by column, as `Matrix4x4f` serializes.
    let fields = (0..16)
        .map(|at| {
            let (column, row) = (at / 4, at % 4);
            (
                std::borrow::Cow::Owned(format!("e{row}{column}")),
                Val::F(m[row][column]),
            )
        })
        .collect();
    Val::Map(fields)
}

fn transform(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|r| m[r][0] * p[0] + m[r][1] * p[1] + m[r][2] * p[2] + m[r][3])
}

/// One vertex channel of the Player's layout: stream, offset, format and
/// dimension.
#[derive(Clone, Copy, Default)]
struct Channel {
    stream: u64,
    offset: u64,
    format: u64,
    dimension: u64,
}

const FLOAT: u64 = 0;
const UNORM8: u64 = 2;
const UINT32: u64 = 10;

/// Builds the Player's `Mesh` of a node.
pub fn build(
    name: &str,
    node: &ufbx::Node,
    mesh: &ufbx::Mesh,
    settings: &Settings,
    space: &Space,
    skin: Option<&Skin>,
) -> Result<Val> {
    if settings.keep_quads {
        bail!("{name}: quads kept as quads");
    }
    let geometry = space.matrix(&node.geometry_to_node);
    let rotate = |v: ufbx::Vec3| {
        let p = space.direction([v.x as f32, v.y as f32, v.z as f32]);
        // The geometry's own rotation, without its translation.
        [0, 1, 2].map(|r| geometry[r][0] * p[0] + geometry[r][1] * p[1] + geometry[r][2] * p[2])
    };
    let normalize = |v: [f32; 3]| {
        let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if length > 0.0 {
            v.map(|c| c / length)
        } else {
            v
        }
    };
    let mut uv_sets: Vec<&ufbx::VertexVec2> =
        mesh.uv_sets.iter().map(|set| &set.vertex_uv).collect();
    uv_sets.truncate(8);
    if settings.swap_uvs && uv_sets.len() >= 2 {
        uv_sets.swap(0, 1);
    }
    let colors = mesh.color_sets.first().map(|set| &set.vertex_color);
    let imported_normals = match settings.normals {
        Source::Import if mesh.vertex_normal.exists => Some(&mesh.vertex_normal),
        _ => None,
    };
    // The weights of each control point.
    let mut weights: Vec<Vec<(u32, f32)>> = Vec::new();
    if skin.is_some()
        && let Some(deformer) = mesh.skin_deformers.first()
    {
        for vertex in deformer.vertices.iter() {
            let mut list: Vec<(u32, f32)> = (vertex.weight_begin
                ..vertex.weight_begin + vertex.num_weights)
                .map(|i| {
                    let weight = &deformer.weights[i as usize];
                    (weight.cluster_index, weight.weight as f32)
                })
                .filter(|(_, weight)| *weight > 0.0)
                .collect();
            list.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            list.truncate(4);
            let total: f32 = list.iter().map(|(_, w)| w).sum();
            if total > 0.0 {
                for entry in &mut list {
                    entry.1 /= total;
                }
            }
            weights.push(list);
        }
    }
    let corner = |index: usize| -> Corner {
        let position = mesh.vertex_position[index];
        let position = transform(
            &geometry,
            space.position([position.x as f32, position.y as f32, position.z as f32]),
        );
        let vertex = mesh.vertex_indices[index] as usize;
        Corner {
            position,
            normal: imported_normals.map(|normals| normalize(rotate(normals[index]))),
            tangent: None,
            color: colors.filter(|c| c.exists).map(|c| color(c[index])),
            uvs: uv_sets
                .iter()
                .filter(|set| set.exists)
                .map(|set| {
                    let uv = set[index];
                    [uv.x as f32, uv.y as f32]
                })
                .collect(),
            weights: weights.get(vertex).cloned().unwrap_or_default(),
        }
    };
    // Triangles by material slot, each in face order; a mirrored triangle
    // turns the other way.
    let slots = mesh.materials.len().max(1);
    let mut triangles: Vec<Vec<[Corner; 3]>> = vec![Vec::new(); slots];
    let mut scratch = vec![0u32; mesh.max_face_triangles * 3];
    for (face_index, face) in mesh.faces.iter().enumerate() {
        if face.num_indices < 3 {
            continue;
        }
        let slot = mesh
            .face_material
            .get(face_index)
            .map_or(0, |m| *m as usize)
            .min(slots - 1);
        let count = if face.num_indices == 4 {
            let b = face.index_begin;
            scratch[..6].copy_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
            2
        } else {
            mesh.triangulate_face(&mut scratch, *face) as usize
        };
        for triangle in scratch[..count * 3].chunks(3) {
            let [a, b, c] = [triangle[0], triangle[1], triangle[2]].map(|i| corner(i as usize));
            triangles[slot].push(match space.mirrored() {
                true => [a, c, b],
                false => [a, b, c],
            });
        }
    }
    if settings.normals == Source::Calculate
        || (settings.normals == Source::Import && imported_normals.is_none())
    {
        calculate_normals(&mut triangles, settings);
    }
    let has_uv = triangles
        .iter()
        .flatten()
        .next()
        .is_some_and(|t| !t[0].uvs.is_empty());
    if settings.tangents != Source::None
        && settings.normals != Source::None
        && triangles
            .iter()
            .flatten()
            .next()
            .is_some_and(|t| t[0].normal.is_some())
    {
        if settings.tangents == Source::Import && mesh.vertex_tangent.exists {
            bail!("{name}: imported tangents");
        }
        calculate_tangents(&mut triangles, has_uv);
    }
    // Welded corners, numbered in order of first use, per submesh.
    let mut vertices: Vec<Corner> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut submeshes = Vec::new();
    for slot in &triangles {
        let first_index = indices.len();
        let first_vertex = vertices.len();
        let mut known: HashMap<Vec<u32>, u32> = HashMap::new();
        for triangle in slot {
            for corner in triangle {
                let id = match settings.weld_vertices {
                    true => *known.entry(corner.key()).or_insert_with(|| {
                        vertices.push(corner.clone());
                        vertices.len() as u32 - 1
                    }),
                    false => {
                        vertices.push(corner.clone());
                        vertices.len() as u32 - 1
                    }
                };
                indices.push(id);
            }
        }
        submeshes.push((
            first_index,
            indices.len() - first_index,
            first_vertex,
            vertices.len() - first_vertex,
        ));
    }
    let wide = match settings.index_format {
        1 => false,
        2 => true,
        _ => vertices.len() > u16::MAX as usize,
    };
    let mut index_bytes = Vec::with_capacity(indices.len() * if wide { 4 } else { 2 });
    for &index in &indices {
        if wide {
            index_bytes.extend_from_slice(&index.to_le_bytes());
        } else {
            index_bytes.extend_from_slice(&(index as u16).to_le_bytes());
        }
    }
    let index_size = if wide { 4 } else { 2 };
    let submesh_values: Vec<Val> = submeshes
        .iter()
        .map(|&(first_index, count, first_vertex, vertex_count)| {
            let range = &vertices[first_vertex..first_vertex + vertex_count];
            Val::map([
                ("firstByte", Val::U((first_index * index_size) as u64)),
                ("indexCount", Val::U(count as u64)),
                ("topology", Val::I(0)),
                ("baseVertex", Val::U(0)),
                ("firstVertex", Val::U(first_vertex as u64)),
                ("vertexCount", Val::U(vertex_count as u64)),
                ("localAABB", bounds(aabb(range.iter().map(|v| v.position)))),
            ])
        })
        .collect();

    // The layout: what moves with the skin in the first stream, what stays
    // in the second, the skin's weights in the third; a mesh without a
    // skin has one stream.
    let skinned = skin.is_some();
    let first = vertices.first().cloned().unwrap_or_default();
    let uv_count = first.uvs.len();
    let bones_per_vertex = vertices.iter().map(|v| v.weights.len()).max().unwrap_or(0);
    let mut channels = [Channel::default(); 14];
    let mut strides = [0u64; 4];
    let mut place = |channel: usize, stream: u64, format: u64, dimension: u64, size: u64| {
        channels[channel] = Channel {
            stream,
            offset: strides[stream as usize],
            format,
            dimension,
        };
        strides[stream as usize] += size * dimension;
    };
    place(0, 0, FLOAT, 3, 4);
    if first.normal.is_some() {
        place(1, 0, FLOAT, 3, 4);
    }
    if first.tangent.is_some() {
        place(2, 0, FLOAT, 4, 4);
    }
    let rest = if skinned { 1 } else { 0 };
    if first.color.is_some() {
        place(3, rest, UNORM8, 4, 1);
    }
    for uv in 0..uv_count {
        place(4 + uv, rest, FLOAT, 2, 4);
    }
    if skinned && bones_per_vertex > 0 {
        if bones_per_vertex > 1 {
            place(12, 2, FLOAT, bones_per_vertex as u64, 4);
        }
        place(13, 2, UINT32, bones_per_vertex as u64, 4);
    }
    let mut data: Vec<u8> = Vec::new();
    for (stream, &stride) in strides.iter().enumerate() {
        if stride == 0 {
            continue;
        }
        while !data.len().is_multiple_of(16) {
            data.push(0);
        }
        let start = data.len();
        data.resize(start + stride as usize * vertices.len(), 0);
        for (index, vertex) in vertices.iter().enumerate() {
            let at = start + index * stride as usize;
            let mut put = |channel: usize, bytes: &[u8]| {
                let c = channels[channel];
                if c.dimension == 0 || c.stream != stream as u64 {
                    return;
                }
                let offset = at + c.offset as usize;
                data[offset..offset + bytes.len()].copy_from_slice(bytes);
            };
            let floats = |values: &[f32]| -> Vec<u8> {
                values.iter().flat_map(|v| v.to_le_bytes()).collect()
            };
            put(0, &floats(&vertex.position));
            if let Some(normal) = vertex.normal {
                put(1, &floats(&normal));
            }
            if let Some(tangent) = vertex.tangent {
                put(2, &floats(&tangent));
            }
            if let Some(color) = vertex.color {
                put(3, &color);
            }
            for (uv, value) in vertex.uvs.iter().enumerate() {
                put(4 + uv, &floats(value));
            }
            if skinned && bones_per_vertex > 0 {
                let mut bones = vec![0u32; bones_per_vertex];
                let mut amounts = vec![0f32; bones_per_vertex];
                for (slot, (bone, weight)) in vertex.weights.iter().enumerate() {
                    bones[slot] = *bone;
                    amounts[slot] = *weight;
                }
                put(12, &floats(&amounts));
                put(
                    13,
                    &bones
                        .iter()
                        .flat_map(|b| b.to_le_bytes())
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    let channel_values: Vec<Val> = channels
        .iter()
        .map(|c| {
            Val::map([
                ("stream", Val::U(c.stream)),
                ("offset", Val::U(c.offset)),
                ("format", Val::U(c.format)),
                ("dimension", Val::U(c.dimension)),
            ])
        })
        .collect();

    let local = aabb(vertices.iter().map(|v| v.position));
    let mut fields: Vec<(&'static str, Val)> = vec![
        ("m_Name", Val::Str(name.to_string())),
        ("m_SubMeshes", Val::Seq(submesh_values)),
    ];
    if let Some(skin) = skin {
        let hashes = skin
            .paths
            .iter()
            .map(|path| Val::U(crate::playerbake::crc32(path) as u64))
            .collect();
        // Each bone's box: the vertices it moves, in its own space.
        let boxes = (0..skin.paths.len())
            .map(|bone| {
                let points: Vec<[f32; 3]> = vertices
                    .iter()
                    .filter(|v| v.weights.iter().any(|(b, _)| *b as usize == bone))
                    .map(|v| transform(&skin.bind_poses[bone], v.position))
                    .collect();
                let (min, max) = points.iter().fold(
                    ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]),
                    |(mut min, mut max), p| {
                        for a in 0..3 {
                            min[a] = min[a].min(p[a]);
                            max[a] = max[a].max(p[a]);
                        }
                        (min, max)
                    },
                );
                let (min, max) = match points.is_empty() {
                    true => ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]),
                    false => (min, max),
                };
                Val::map([("m_Min", vector(min)), ("m_Max", vector(max))])
            })
            .collect();
        fields.push((
            "m_BindPose",
            Val::Seq(skin.bind_poses.iter().map(matrix).collect()),
        ));
        fields.push(("m_BoneNameHashes", Val::Seq(hashes)));
        fields.push((
            "m_RootBoneNameHash",
            Val::U(crate::playerbake::crc32(&skin.root) as u64),
        ));
        fields.push(("m_BonesAABB", Val::Seq(boxes)));
    }
    let keep = settings.readable || vertices.len() <= 300;
    fields.extend([
        ("m_MeshCompression", Val::U(0)),
        ("m_IsReadable", Val::Bool(settings.readable)),
        ("m_KeepVertices", Val::Bool(keep)),
        ("m_KeepIndices", Val::Bool(keep)),
        ("m_IndexFormat", Val::I(wide as i64)),
        ("m_IndexBuffer", Val::Bytes(index_bytes)),
        (
            "m_VertexData",
            Val::map([
                ("m_VertexCount", Val::U(vertices.len() as u64)),
                ("m_Channels", Val::Seq(channel_values)),
                ("m_DataSize", Val::Bytes(data)),
            ]),
        ),
        ("m_LocalAABB", bounds(local)),
        ("m_MeshUsageFlags", Val::I(skinned as i64)),
        ("m_CookingOptions", Val::I(30)),
        (
            "m_MeshMetrics[0]",
            Val::F(uv_metric(&vertices, &indices, 0)),
        ),
        (
            "m_MeshMetrics[1]",
            Val::F(uv_metric(&vertices, &indices, 1)),
        ),
    ]);
    Ok(Val::Map(
        fields
            .into_iter()
            .map(|(name, value)| (std::borrow::Cow::Borrowed(name), value))
            .collect(),
    ))
}

/// How much of the mesh's surface a unit of a UV set covers, for texture
/// streaming, as the importer approximates it: over the triangles that are
/// neither slivers (an angle under 5 degrees) nor among the smallest (under
/// the mean area of all but the largest tenth, less the spread of those
/// below that mean), the mean ratio of surface to UV area plus the spread
/// of the ratios above that mean. 1 without the set or a result.
fn uv_metric(vertices: &[Corner], indices: &[u32], set: usize) -> f32 {
    if vertices.first().is_none_or(|v| v.uvs.len() <= set) || indices.len() < 3 {
        return 1.0;
    }
    let corners = |t: &[u32]| [t[0], t[1], t[2]].map(|i| &vertices[i as usize]);
    let sub = |a: [f32; 3], b: [f32; 3]| [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let area = |p: [&Corner; 3]| {
        let d1 = sub(p[0].position, p[1].position);
        let d2 = sub(p[0].position, p[2].position);
        let x = d1[1] * d2[2] - d1[2] * d2[1];
        let y = d1[2] * d2[0] - d1[0] * d2[2];
        let z = d1[0] * d2[1] - d1[1] * d2[0];
        (z * z + y * y + x * x).sqrt() * 0.5
    };
    let mut areas: Vec<f32> = indices.chunks(3).map(|t| area(corners(t))).collect();
    areas.sort_by(f32::total_cmp);
    let kept = &areas[..areas.len() - indices.len() / 30];
    // The mean and the spread below it.
    let spread = |values: &[f32], below: bool| -> (f32, f32) {
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let (mut sum, mut count) = (0f32, 0u32);
        for &value in values {
            if (below && value < mean) || (!below && mean < value) {
                sum += (value - mean) * (value - mean);
                count += 1;
            }
        }
        let deviation = if count == 0 {
            0.0
        } else {
            (sum / count as f32).sqrt()
        };
        (mean, deviation)
    };
    let smallest = match kept.is_empty() {
        true => 0.001,
        false => {
            let (mean, deviation) = spread(kept, true);
            if (deviation as f64) < 0.1 {
                0.001
            } else {
                (mean - deviation).max(0.001)
            }
        }
    };
    let length = |v: [f32; 3]| (v[2] * v[2] + v[1] * v[1] + v[0] * v[0]).sqrt();
    let sliver = 5f32.to_radians();
    let mut ratios = Vec::new();
    for triangle in indices.chunks(3) {
        let p = corners(triangle);
        let a = length(sub(p[0].position, p[1].position));
        let b = length(sub(p[0].position, p[2].position));
        let c = length(sub(p[1].position, p[2].position));
        let angle = |x: f32, y: f32, opposite: f32| {
            (((x * x + y * y) - opposite * opposite) / ((x + x) * y)).acos()
        };
        if angle(a, b, c) < sliver || angle(b, c, a) < sliver || angle(a, c, b) < sliver {
            continue;
        }
        let world = area(p);
        let [u0, u1, u2] = p.map(|v| v.uvs[set]);
        let uv =
            ((u2[1] - u0[1]) * (u1[0] - u0[0]) - (u1[1] - u0[1]) * (u2[0] - u0[0])).abs() * 0.5;
        if smallest <= world && 1e-9 <= uv {
            let ratio = world / uv;
            if 0.001 < ratio {
                ratios.push(ratio);
            }
        }
    }
    if ratios.is_empty() {
        return 1.0;
    }
    let (mean, deviation) = spread(&ratios, false);
    let metric = mean + deviation;
    if metric <= 0.001 { 1.0 } else { metric }
}

/// Normals from the faces: a corner takes the normals of the faces around
/// its position that turn from its own face by no more than the smoothing
/// angle, weighed as the import says.
fn calculate_normals(triangles: &mut [Vec<[Corner; 3]>], settings: &Settings) {
    let face = |t: &[Corner; 3]| {
        let d1 = [0, 1, 2].map(|k| t[1].position[k] - t[0].position[k]);
        let d2 = [0, 1, 2].map(|k| t[2].position[k] - t[0].position[k]);
        [
            d1[1] * d2[2] - d1[2] * d2[1],
            d1[2] * d2[0] - d1[0] * d2[2],
            d1[0] * d2[1] - d1[1] * d2[0],
        ]
    };
    let unit = |v: [f32; 3]| {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if l > 0.0 { v.map(|c| c / l) } else { v }
    };
    let angle_at = |t: &[Corner; 3], i: usize| {
        let p = t[i].position;
        let a = unit([0, 1, 2].map(|k| t[(i + 1) % 3].position[k] - p[k]));
        let b = unit([0, 1, 2].map(|k| t[(i + 2) % 3].position[k] - p[k]));
        (a[0] * b[0] + a[1] * b[1] + a[2] * b[2])
            .clamp(-1.0, 1.0)
            .acos()
    };
    let all: Vec<(usize, usize)> = triangles
        .iter()
        .enumerate()
        .flat_map(|(s, list)| (0..list.len()).map(move |i| (s, i)))
        .collect();
    let normals: Vec<[f32; 3]> = all.iter().map(|&(s, i)| face(&triangles[s][i])).collect();
    let mut around: HashMap<[u32; 3], Vec<(usize, usize)>> = HashMap::new();
    for (n, &(s, i)) in all.iter().enumerate() {
        for (corner, point) in triangles[s][i].iter().enumerate() {
            let key = point.position.map(f32::to_bits);
            around.entry(key).or_default().push((n, corner));
        }
    }
    let threshold = settings.normal_smoothing_angle.to_radians().cos();
    let mut results = vec![[[0f32; 3]; 3]; all.len()];
    for (n, &(s, i)) in all.iter().enumerate() {
        let own = unit(normals[n]);
        for corner in 0..3 {
            let key = triangles[s][i][corner].position.map(f32::to_bits);
            let mut sum = [0f32; 3];
            for &(other, at) in &around[&key] {
                let theirs = unit(normals[other]);
                let cos = own[0] * theirs[0] + own[1] * theirs[1] + own[2] * theirs[2];
                if other != n && cos < threshold {
                    continue;
                }
                let (os, oi) = all[other];
                let area = match settings.normal_weighting {
                    2 | 4 => {
                        let f = normals[other];
                        (f[0] * f[0] + f[1] * f[1] + f[2] * f[2]).sqrt()
                    }
                    _ => 1.0,
                };
                let angle = match settings.normal_weighting {
                    3 | 4 => angle_at(&triangles[os][oi], at),
                    _ => 1.0,
                };
                let weight = area * angle;
                for k in 0..3 {
                    sum[k] += theirs[k] * weight;
                }
            }
            results[n][corner] = unit(sum);
        }
    }
    for (n, &(s, i)) in all.iter().enumerate() {
        for corner in 0..3 {
            triangles[s][i][corner].normal = Some(results[n][corner]);
        }
    }
}

/// Tangents by MikkTSpace over the first UV set; without one, along X.
fn calculate_tangents(triangles: &mut [Vec<[Corner; 3]>], has_uv: bool) {
    struct Faces<'a> {
        faces: Vec<&'a mut [Corner; 3]>,
    }
    impl bevy_mikktspace::Geometry for Faces<'_> {
        fn num_faces(&self) -> usize {
            self.faces.len()
        }
        fn num_vertices_of_face(&self, _face: usize) -> usize {
            3
        }
        fn position(&self, face: usize, vert: usize) -> [f32; 3] {
            self.faces[face][vert].position
        }
        fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
            self.faces[face][vert].normal.unwrap_or([0.0, 1.0, 0.0])
        }
        fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
            self.faces[face][vert]
                .uvs
                .first()
                .copied()
                .unwrap_or([0.0; 2])
        }
        fn set_tangent(
            &mut self,
            space: Option<bevy_mikktspace::TangentSpace>,
            face: usize,
            vert: usize,
        ) {
            let space = space.unwrap_or_default();
            let t = space.tangent();
            let w = if space.is_orientation_preserving() {
                1.0
            } else {
                -1.0
            };
            self.faces[face][vert].tangent = Some([t[0], t[1], t[2], w]);
        }
    }
    if !has_uv {
        for corner in triangles.iter_mut().flatten().flatten() {
            corner.tangent = Some([1.0, 0.0, 0.0, 1.0]);
        }
        return;
    }
    let mut faces = Faces {
        faces: triangles.iter_mut().flatten().collect(),
    };
    let _ = bevy_mikktspace::generate_tangents(&mut faces);
}
