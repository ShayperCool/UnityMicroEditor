//! The tight mesh of a sprite as Unity's importer makes it (6000.0): the
//! pixels whose alpha is above a tolerance, grown by the extrusion, traced
//! over their corners into outlines (holes apart), each outline moved a pixel
//! outward, simplified by removing the vertices that cost least area down to
//! the count the detail asks for, kept within its bounds, rounded to whole
//! pixels and tessellated by libtess2. `docs/OWN_IMPORTER.md` states the
//! rules; every step computes in `f32` in the order the engine does, so the
//! vertices are the engine's to the bit.
use std::os::raw::{c_int, c_void};

/// What the importer passes the generator for a tight sprite mesh.
pub struct Settings {
    /// `spriteTessellationDetail`: from 0 to 1, or negative to let the area
    /// of the outlines decide.
    pub detail: f32,
    /// `spriteExtrude`: pixels the opaque area grows by.
    pub extrude: u32,
    pub alpha_tolerance: u8,
    pub hole_detection: bool,
}

/// A mesh in the pixels of the image it was made from.
pub struct Mesh {
    pub vertices: Vec<[f32; 2]>,
    pub indices: Vec<u16>,
    /// The rectangle the outlines span, `[x, y, width, height]`.
    pub bounds: Option<[f32; 4]>,
}

const EPSILON: f32 = 1e-5;

#[derive(Clone, Copy, Default, Debug)]
struct Vertex {
    pos: [f32; 2],
    normal: [f32; 2],
    /// 1 where the outline turns left (outward of a counter-clockwise
    /// outline), -1 where it turns right.
    convex: i32,
    /// Area already lost around the vertex.
    error: f32,
    /// Area lost by removing the vertex, negative when it cannot go.
    cost: f32,
    /// How far a concave vertex's neighbours move along their normals.
    shift: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Outer,
    Hole,
}

struct Path {
    vertices: Vec<Vertex>,
    /// The outline before simplification.
    original: Vec<Vertex>,
    kind: Kind,
    /// `[min x, min y, max x, max y]` of the outline, within the image.
    bounds: [f32; 4],
    /// Vertices whose normal and cost are to be computed again.
    dirty: Vec<i32>,
}

#[derive(Clone)]
struct Grid {
    width: i32,
    height: i32,
    bits: Vec<bool>,
}

impl Grid {
    fn inside(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width && y < self.height
    }

    fn get(&self, x: i32, y: i32) -> bool {
        self.inside(x, y) && self.bits[(y * self.width + x) as usize]
    }

    fn set(&mut self, x: i32, y: i32, value: bool) {
        if self.inside(x, y) {
            self.bits[(y * self.width + x) as usize] = value;
        }
    }

    fn toggle(&mut self, x: i32, y: i32) {
        if self.inside(x, y) {
            let bit = &mut self.bits[(y * self.width + x) as usize];
            *bit = !*bit;
        }
    }
}

fn wrap(index: i32, count: usize) -> usize {
    index.rem_euclid(count as i32) as usize
}

/// The eight neighbours of a corner, counter-clockwise from the left.
const NEIGHBOURS: [(i32, i32); 8] = [
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
];

/// By the signs of a step (`direction_code`), the neighbour one past it.
const NEXT_AFTER: [i32; 9] = [0, 1, 2, 7, -1, 3, 6, 5, 4];

fn direction_code(dx: f32, dy: f32) -> usize {
    let x = if dx > 0.0 {
        7
    } else if dx < 0.0 {
        1
    } else {
        4
    };
    let y = if dy > 0.0 {
        1
    } else if dy < 0.0 {
        -1
    } else {
        0
    };
    (x - y) as usize
}

/// The corners of the pixels whose alpha is above the tolerance, after the
/// area grows by `extrude` pixels of Manhattan distance; `alpha` has the
/// image's rows from the bottom.
fn corners(alpha: &[u8], width: i32, height: i32, tolerance: u8, extrude: u32) -> Grid {
    let (w, h) = (width as usize, height as usize);
    let mut mask: Vec<bool> = alpha.iter().map(|&a| a > tolerance).collect();
    if extrude != 0 && w != 0 && h != 0 {
        let mut distance = vec![0u32; w * h];
        for y in 0..h {
            for x in 0..w {
                let index = y * w + x;
                let mut d = 0;
                if !mask[index] {
                    d = (h + w) as u32;
                    if y != 0 {
                        d = d.min(distance[index - w] + 1);
                    }
                    if x != 0 {
                        d = d.min(distance[index - 1] + 1);
                    }
                }
                distance[index] = d;
            }
        }
        for y in (0..h).rev() {
            for x in (0..w).rev() {
                let index = y * w + x;
                if y + 1 < h {
                    distance[index] = (distance[index + w] + 1).min(distance[index]);
                }
                if x + 1 < w {
                    distance[index] = distance[index].min(distance[index + 1] + 1);
                }
            }
        }
        for (set, d) in mask.iter_mut().zip(&distance) {
            if *d <= extrude {
                *set = true;
            }
        }
    }
    let mut grid = Grid {
        width: width + 1,
        height: height + 1,
        bits: vec![false; (w + 1) * (h + 1)],
    };
    for y in 0..height {
        for x in 0..width {
            if mask[(y * width + x) as usize] {
                grid.set(x, y, true);
                grid.set(x + 1, y + 1, true);
                grid.set(x, y + 1, true);
                grid.set(x + 1, y, true);
            }
        }
    }
    grid
}

/// The next outline among the corners left: traced from the first corner in
/// row order, counter-clockwise around the area, its corners then cleared
/// with what they enclose. An outline enclosing less than 4 pixels is passed.
fn next_outline(original: &Grid, working: &mut Grid) -> Option<(Vec<[f32; 2]>, Kind, f32)> {
    loop {
        let start = working.bits.iter().position(|&b| b)? as i32;
        let (sx, sy) = (start % working.width, start / working.width);
        let kind = if original.get(sx, sy) {
            Kind::Outer
        } else {
            Kind::Hole
        };
        let (start_x, start_y) = (sx as f32, sy as f32);
        let mut trace = Vec::new();
        let mut area = 0.0f32;
        let (mut x, mut y) = (start_x, start_y);
        let (mut previous_x, mut previous_y) = (start_x + -1.0, start_y);
        loop {
            trace.push([x, y]);
            let code = direction_code(previous_x - x, previous_y - y);
            if code == 4 {
                break;
            }
            let first = NEXT_AFTER[code];
            let found = (0..8).find_map(|k| {
                let (dx, dy) = NEIGHBOURS[((first + k) % 8) as usize];
                let (nx, ny) = (dx + x as i32, dy + y as i32);
                working.get(nx, ny).then_some((nx as f32, ny as f32))
            });
            let Some((nx, ny)) = found else {
                break;
            };
            area += (x * ny - y * nx) * 0.5;
            (previous_x, previous_y) = (x, y);
            (x, y) = (nx, ny);
            if x == start_x && y == start_y {
                break;
            }
        }
        erase(working, &trace);
        if area.abs() >= 4.0 {
            if (area < 0.0 && kind == Kind::Outer) || (area > 0.0 && kind == Kind::Hole) {
                trace.reverse();
            }
            return Some((trace, kind, area.abs()));
        }
    }
}

/// Clears a traced outline from the corners: each row's crossings toggle
/// the corners between them and the first vertex's column, so what the
/// outline encloses flips (a hole's corners appear), and then its own
/// corners are cleared.
fn erase(grid: &mut Grid, trace: &[[f32; 2]]) {
    let n = trace.len();
    if n == 0 {
        return;
    }
    let reference = trace[0][0] as i32;
    let mut previous_y = trace[n - 1][1];
    let mut i = 0;
    loop {
        let mut x = trace[i][0];
        let y = trace[i][1];
        let mut next = i + 1;
        let mut more = false;
        if next < n {
            if trace[next][1] != y {
                more = true;
            } else {
                // A horizontal run: the crossing takes the end the outline
                // leaves the row by.
                let mut k = next;
                loop {
                    match NEXT_AFTER[direction_code(x - trace[k][0], y - trace[k][1])] {
                        5 if y < previous_y => x = trace[k][0],
                        1 if previous_y < y => x = trace[k][0],
                        _ => {}
                    }
                    if k == n - 1 {
                        next = n;
                        break;
                    }
                    if trace[k + 1][1] != y {
                        next = k + 1;
                        more = true;
                        break;
                    }
                    k += 1;
                }
            }
        }
        let (row, column) = (y as i32, x as i32);
        let (low, high) = if column <= reference {
            (column, reference)
        } else {
            (reference, column)
        };
        for c in low..high {
            grid.toggle(c, row);
        }
        // A row the outline turns back on is not crossed.
        if more && y != previous_y && trace[next][1] == previous_y {
            for c in low..high {
                grid.toggle(c, row);
            }
        }
        if next >= n {
            for corner in trace {
                grid.set(corner[0] as i32, corner[1] as i32, false);
            }
            return;
        }
        previous_y = y;
        i = next;
    }
}

/// The outward normal at a vertex: of the two edges' normals summed.
fn normal(vertices: &mut [Vertex], index: i32) {
    let n = vertices.len();
    if n < 3 {
        return;
    }
    let previous = vertices[wrap(index - 1, n)].pos;
    let current = vertices[wrap(index, n)].pos;
    let next = vertices[wrap(index + 1, n)].pos;
    let unit = |x: f32, y: f32| {
        let length = (x * x + y * y).sqrt();
        if length <= EPSILON {
            [0.0, 0.0]
        } else {
            [x / length, y / length]
        }
    };
    let a = [previous[0] - current[0], previous[1] - current[1]];
    let first = unit(-a[1], a[0]);
    let b = [current[0] - next[0], current[1] - next[1]];
    let second = unit(-b[1], b[0]);
    let sum = [first[0] + second[0], first[1] + second[1]];
    let length = (sum[1] * sum[1] + sum[0] * sum[0]).sqrt();
    vertices[wrap(index, n)].normal = if EPSILON < length {
        [sum[0] / length, sum[1] / length]
    } else {
        [0.0, 0.0]
    };
}

fn convexity(vertices: &mut [Vertex], index: i32) {
    let n = vertices.len();
    if n < 3 {
        return;
    }
    let p = vertices[wrap(index - 1, n)].pos;
    let c = vertices[wrap(index, n)].pos;
    let q = vertices[wrap(index + 1, n)].pos;
    let fy = p[1] - q[1];
    let fx = q[0] - p[0];
    let tie = if fy != 0.0 { fy < 0.0 } else { fx < 0.0 };
    let cross = (c[1] * fx + c[0] * fy) + (-(p[0] * fy) - p[1] * fx);
    vertices[wrap(index, n)].convex = if cross > 0.0 {
        1
    } else if cross == 0.0 {
        if tie { 1 } else { -1 }
    } else {
        -1
    };
}

/// A convex vertex costs the area of the triangle it and its neighbours make.
fn convex_cost(vertices: &mut [Vertex], index: usize) {
    let n = vertices.len();
    if n < 3 {
        return;
    }
    let p = vertices[wrap(index as i32 - 1, n)].pos;
    let q = vertices[wrap(index as i32 + 1, n)].pos;
    let vertex = &mut vertices[index];
    let c = vertex.pos;
    let area = (c[1] - p[1]) * (q[0] - p[0]) - (p[1] - q[1]) * (p[0] - c[0]);
    vertex.cost = if area >= 0.0 { area } else { -1.0 };
    vertex.shift = 0.0;
}

/// Whether `value` is within `ulps` units in the last place of `of`.
fn near(value: f32, of: f32, ulps: i32) -> bool {
    (value.to_bits() as i32 - of.to_bits() as i32).abs() <= ulps
}

/// A concave vertex goes by moving its neighbours along their normals until
/// the area it took is covered again; it costs the area the move adds.
fn concave_cost(vertices: &mut [Vertex], index: usize) {
    let n = vertices.len();
    if n < 5 {
        return;
    }
    let reject = |vertices: &mut [Vertex]| {
        vertices[index].cost = -1.0;
        vertices[index].shift = 0.0;
    };
    let previous = vertices[wrap(index as i32 - 1, n)];
    let next = vertices[wrap(index as i32 + 1, n)];
    let (np, nn) = (previous.normal, next.normal);
    let dot = np[1] * nn[1] + np[0] * nn[0];
    // Parallel or perpendicular normals give no move.
    if dot < 0.0 || dot.is_nan() || dot.to_bits() as i32 <= 10 || near(dot, 1.0, 10) {
        return reject(vertices);
    }
    let (p, q, c) = (previous.pos, next.pos, vertices[index].pos);
    let a = np[1] * nn[0] - nn[1] * np[0];
    let constant = (p[0] - q[0]) * (c[1] - p[1]) - (p[1] - q[1]) * (c[0] - p[0]);
    let b = (np[1] * (p[0] - q[0]) - (p[1] - q[1]) * np[0])
        + ((nn[0] - np[0]) * (c[1] - p[1]) - (nn[1] - np[1]) * (c[0] - p[0]));
    let (r1, r2);
    if EPSILON <= a.abs() {
        let discriminant = -4.0 * a * constant + b * b;
        if discriminant < 0.0 {
            return reject(vertices);
        }
        let scale = 0.5 / a;
        let root = discriminant.sqrt();
        r1 = (b + root) * scale;
        r2 = (b - root) * scale;
    } else {
        if b.abs() <= EPSILON {
            return reject(vertices);
        }
        r1 = constant / b;
        r2 = -1.0;
    }
    let high = if r1 <= r2 { r2 } else { r1 };
    let low = if r2 <= r1 { r2 } else { r1 };
    let t = if 0.0 < r2 {
        if 0.0 < r1 { low } else { high }
    } else {
        high
    };
    if t < 0.0 {
        return reject(vertices);
    }
    let pp = vertices[wrap(index as i32 - 2, n)].pos;
    let qq = vertices[wrap(index as i32 + 2, n)].pos;
    let moved_p = [np[0] * t + p[0], np[1] * t + p[1]];
    let moved_q = [nn[0] * t + q[0], nn[1] * t + q[1]];
    // The area of the four triangles, summed from the one before.
    let after =
        (q[1] - moved_q[1]) * (qq[0] - moved_q[0]) - (moved_q[1] - qq[1]) * (moved_q[0] - q[0]);
    let next_side = (q[1] - c[1]) * (moved_q[0] - c[0]) - (c[1] - moved_q[1]) * (c[0] - q[0]);
    let previous_side =
        (p[1] - moved_p[1]) * (c[0] - moved_p[0]) - (moved_p[1] - c[1]) * (moved_p[0] - p[0]);
    let before = (p[1] - pp[1]) * (moved_p[0] - pp[0]) - (pp[1] - moved_p[1]) * (pp[0] - p[0]);
    let cost = after + (next_side + (previous_side + before));
    vertices[index].cost = cost;
    vertices[index].shift = t;
}

/// Whether the segment from `a` to `b` crosses an edge of the outline that
/// does not end in one of them.
fn crosses(vertices: &[Vertex], a: [f32; 2], b: [f32; 2]) -> bool {
    let n = vertices.len();
    for k in 0..n {
        let c = vertices[k].pos;
        let d = vertices[(k + 1) % n].pos;
        if c == a || d == b || b == c || a == d {
            continue;
        }
        let denominator = ((d[1] - c[1]) * (b[0] - a[0]) - (d[0] - c[0]) * (b[1] - a[1])) as f64;
        if denominator.abs() < EPSILON as f64 {
            continue;
        }
        let ua =
            ((b[0] - a[0]) * (a[1] - c[1]) - (b[1] - a[1]) * (a[0] - c[0])) as f64 / denominator;
        if (0.0..=1.0).contains(&ua) {
            let ub = ((d[0] - c[0]) * (a[1] - c[1]) - (d[1] - c[1]) * (a[0] - c[0])) as f64
                / denominator;
            if (0.0..=1.0).contains(&ub) {
                return true;
            }
        }
    }
    false
}

/// Removes the vertex that costs least, if the outline stays clear of
/// itself and keeps spanning its bounds.
fn collapse(path: &mut Path) -> bool {
    let n = path.vertices.len();
    if n <= 4 {
        return false;
    }
    let mut tries = 0;
    loop {
        let vertices = &mut path.vertices;
        let mut best = None;
        let mut least = f32::MAX;
        for (i, vertex) in vertices.iter().enumerate() {
            if vertex.cost >= 0.0 && vertex.cost + vertex.error < least {
                least = vertex.cost + vertex.error;
                best = Some(i);
            }
        }
        let Some(i) = best else {
            return false;
        };
        let previous = wrap(i as i32 - 1, n);
        let next = (i + 1) % n;
        let cost = vertices[i].cost;
        let removable = if vertices[i].convex < 1 {
            let t = vertices[i].shift;
            let pp = vertices[wrap(i as i32 - 2, n)].pos;
            let qq = vertices[wrap(i as i32 + 2, n)].pos;
            let (p, q) = (vertices[previous], vertices[next]);
            let moved_p = [p.normal[0] * t + p.pos[0], p.normal[1] * t + p.pos[1]];
            let moved_q = [q.normal[0] * t + q.pos[0], t * q.normal[1] + q.pos[1]];
            let first = crosses(vertices, pp, moved_p);
            let second = crosses(vertices, moved_p, moved_q);
            let third = crosses(vertices, moved_q, qq);
            if !first && !second && !third {
                vertices[previous].pos = moved_p;
                vertices[next].pos = moved_q;
                vertices[previous].error += cost;
                vertices[next].error += cost;
                path.dirty.extend([
                    wrap(i as i32 - 2, n) as i32,
                    previous as i32,
                    next as i32,
                    wrap(i as i32 + 2, n) as i32,
                ]);
                true
            } else {
                false
            }
        } else if !crosses(vertices, vertices[previous].pos, vertices[next].pos) {
            vertices[previous].error += cost;
            vertices[next].error += cost;
            path.dirty.extend([(i % n) as i32, next as i32]);
            true
        } else {
            false
        };
        if !removable {
            path.vertices[i].cost = -1.0;
            if tries == n {
                return false;
            }
            tries += 1;
            continue;
        }
        let (mut min, mut max) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
        for (k, vertex) in path.vertices.iter().enumerate() {
            if k == i {
                continue;
            }
            let [x, y] = vertex.pos;
            min = [
                if min[0] <= x { min[0] } else { x },
                if min[1] <= y { min[1] } else { y },
            ];
            max = [
                if x <= max[0] { max[0] } else { x },
                if y <= max[1] { max[1] } else { y },
            ];
        }
        let b = path.bounds;
        if min[0] <= b[0] && min[1] <= b[1] && b[2] <= max[0] && b[3] <= max[1] {
            path.vertices.remove(i);
            for d in &mut path.dirty {
                if *d > i as i32 {
                    *d -= 1;
                }
            }
            return true;
        }
        return false;
    }
}

/// Keeps the outline within its bounds (Sutherland–Hodgman), against one
/// side: 1 left, 2 right, 4 top, 8 bottom.
fn clip(path: &mut Path, side: u8) {
    let b = path.bounds;
    let n = path.vertices.len();
    let mut out: Vec<Vertex> = Vec::new();
    let point = |x: f64, y: f64| Vertex {
        pos: [x as f32, y as f32],
        ..Vertex::default()
    };
    for i in 0..n {
        let c = path.vertices[i].pos;
        let d = path.vertices[(i + 1) % n].pos;
        let (next_inside, current_outside) = match side {
            1 => (b[0] <= d[0], c[0] < b[0]),
            2 => (d[0] <= b[2], b[2] < c[0]),
            4 => (d[1] <= b[3], b[3] < c[1]),
            _ => (b[1] <= d[1], c[1] < b[1]),
        };
        // The crossing on the line through the outside end.
        let crossing = |from: [f32; 2], to: [f32; 2]| {
            let slope = ((to[1] - from[1]) / (to[0] - from[0])) as f64;
            let intercept = from[1] as f64 - from[0] as f64 * slope;
            let on_x = |x: f32| point(x as f64, slope * x as f64 + intercept);
            let on_y = |y: f32| {
                // A vertical edge (an infinite or undefined slope) keeps its x.
                let x = if slope.is_finite() {
                    (y as f64 - intercept) / slope
                } else {
                    from[0] as f64
                };
                point(x, y as f64)
            };
            match side {
                1 => on_x(b[0]),
                2 => on_x(b[2]),
                4 => on_y(b[3]),
                _ => on_y(b[1]),
            }
        };
        if next_inside {
            if current_outside {
                out.push(crossing(d, c));
            }
            out.push(point(d[0] as f64, d[1] as f64));
        } else if !current_outside {
            out.push(crossing(c, d));
        }
    }
    path.vertices = out;
}

/// The outline as the trace gives it, reduced to the corners where its
/// direction turns and moved a pixel outward.
fn path_of(trace: &[[f32; 2]], width: i32, height: i32, kind: Kind) -> Path {
    let mut vertices: Vec<Vertex> = trace
        .iter()
        .map(|&pos| Vertex {
            pos,
            ..Vertex::default()
        })
        .collect();
    smooth(&mut vertices, 1.0);
    let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN_POSITIVE; 2]);
    for vertex in &vertices {
        let [x, y] = vertex.pos;
        min = [
            if min[0] <= x { min[0] } else { x },
            if min[1] <= y { min[1] } else { y },
        ];
        max = [
            if x <= max[0] { max[0] } else { x },
            if y <= max[1] { max[1] } else { y },
        ];
    }
    let (w, h) = (width as f32, height as f32);
    let clamp = |v: f32, limit: f32| {
        if v < 0.0 {
            0.0
        } else if v <= limit {
            v
        } else {
            limit
        }
    };
    let bounds = [
        clamp(min[0], w),
        clamp(min[1], h),
        clamp(max[0], w),
        clamp(max[1], h),
    ];
    Path {
        original: vertices.clone(),
        vertices,
        kind,
        bounds,
        dirty: Vec::new(),
    }
}

/// Keeps the corners where a run of steps in at most two neighbouring
/// directions ends, adds back the corners farthest off the lines between
/// them, and moves every vertex `offset` along its normal.
fn smooth(vertices: &mut Vec<Vertex>, offset: f32) {
    let n = vertices.len();
    if n < 3 {
        return;
    }
    let mut keys = vec![0i32];
    let mut seen = [0u32; 8];
    let mut run: i32 = -1;
    for i in 0..n {
        let next = if i + 1 < n { i + 1 } else { 0 };
        let code = direction_code(
            vertices[i].pos[0] - vertices[next].pos[0],
            vertices[i].pos[1] - vertices[next].pos[1],
        );
        if code == 4 {
            continue;
        }
        let direction = NEXT_AFTER[code];
        seen[direction as usize] = 1;
        if run < 0 {
            run = direction;
            continue;
        }
        let left = if run == 0 { 7 } else { run - 1 };
        let right = if run + 1 < 8 { run + 1 } else { (run + 1) & 7 };
        let along = direction == left || direction == run || direction == right;
        if along && seen.iter().sum::<u32>() < 3 {
            continue;
        }
        seen = [0; 8];
        keys.push(i as i32);
        run = -1;
    }
    let mut chosen = Vec::new();
    for k in 0..keys.len() {
        split(vertices, &mut chosen, keys[k], keys[(k + 1) % keys.len()]);
    }
    let original = std::mem::take(vertices);
    *vertices = chosen.iter().map(|&i| original[i as usize]).collect();
    for i in 0..vertices.len() {
        normal(vertices, i as i32);
    }
    for vertex in vertices.iter_mut() {
        vertex.pos = [
            vertex.normal[0] * offset + vertex.pos[0],
            vertex.normal[1] * offset + vertex.pos[1],
        ];
    }
}

/// Adds to `chosen` the vertices from after `a` to `b` that lie farther than
/// half a pixel (along the larger axis) off the line from `a` to `b`, the
/// farthest first, and `b`.
fn split(vertices: &[Vertex], chosen: &mut Vec<i32>, mut a: i32, b: i32) {
    let n = vertices.len();
    loop {
        let next = wrap(a + 1, n) as i32;
        if a == b || next == b {
            break;
        }
        let p = vertices[a as usize].pos;
        let q = vertices[b as usize].pos;
        let ly = p[1] - q[1];
        let lx = q[0] - p[0];
        let px = p[0] * ly;
        let py = p[1] * lx;
        let constant = -px - py;
        let own = (py + px + constant).abs();
        let mut farthest = if own <= -1.0 { -1.0 } else { own };
        let mut best = if -1.0 < own { a } else { -1 };
        let mut u = a;
        loop {
            u = wrap(u + 1, n) as i32;
            let v = vertices[u as usize].pos;
            let distance = (v[1] * lx + v[0] * ly + constant).abs();
            if farthest < distance {
                best = u;
                farthest = distance;
            }
            if u == b {
                break;
            }
        }
        let threshold = (ly.abs() as f64 * 0.5).max(lx.abs() as f64 * 0.5);
        if farthest <= threshold as f32 || best < 0 {
            break;
        }
        split(vertices, chosen, a, best);
        a = best;
    }
    chosen.push(b);
}

/// Removes the vertices that cost least until the count the detail leaves
/// (at least five, less one), then keeps an outer outline within its bounds.
fn simplify(path: &mut Path, detail: f32) {
    path.vertices = path.original.clone();
    let n = path.vertices.len();
    if n > 4 {
        let fraction = if detail < 0.0 {
            1.0
        } else {
            1.0 - if detail <= 1.0 { detail } else { 1.0 }
        };
        let target = (fraction * n as f32) as i32;
        let least = if target > 4 { target } else { 5 };
        path.dirty = (0..n as i32).collect();
        loop {
            let count = path.vertices.len() as i32;
            let dirty = std::mem::take(&mut path.dirty);
            for &d in &dirty {
                normal(&mut path.vertices, d);
                convexity(&mut path.vertices, d);
            }
            for &d in &dirty {
                let index = wrap(d, path.vertices.len());
                if path.vertices[index].convex < 1 {
                    concave_cost(&mut path.vertices, index);
                } else {
                    convex_cost(&mut path.vertices, index);
                }
            }
            let removed = collapse(path);
            if !(least < count && removed) {
                break;
            }
        }
    }
    if path.kind == Kind::Outer {
        for side in [1, 2, 8, 4] {
            clip(path, side);
        }
    }
}

/// Removes the vertices within a ten-thousandth of a pixel of the segment
/// between their neighbours.
fn without_straight_vertices(points: &mut Vec<[f32; 2]>) {
    if points.len() < 3 {
        return;
    }
    let off_segment = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
        let e = [c[0] - a[0], c[1] - a[1]];
        let f = [b[0] - a[0], b[1] - a[1]];
        let along = e[1] * f[1] + e[0] * f[0];
        let (dx, dy) = if along <= 0.0 {
            (f[0], f[1])
        } else {
            let length = e[1] * e[1] + e[0] * e[0];
            let mut foot = c;
            if along < length {
                let r = along / length;
                foot = [a[0] + e[0] * r, a[1] + e[1] * r];
            }
            (b[0] - foot[0], b[1] - foot[1])
        };
        (dy * dy + dx * dx).sqrt() <= 1e-4
    };
    let mut i = 0;
    loop {
        if off_segment(points[i], points[i + 1], points[i + 2]) {
            points.remove(i + 1);
        } else {
            i += 1;
        }
        if i + 2 >= points.len() {
            break;
        }
    }
    let n = points.len();
    if n > 2 && off_segment(points[n - 1], points[0], points[1]) {
        points.remove(0);
    }
    let n = points.len();
    if n > 2 && off_segment(points[n - 2], points[n - 1], points[0]) {
        points.pop();
    }
}

#[repr(C)]
struct Tesselator {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn tessNewTess(alloc: *mut c_void) -> *mut Tesselator;
    fn tessDeleteTess(tess: *mut Tesselator);
    fn tessAddContour(
        tess: *mut Tesselator,
        size: c_int,
        pointer: *const c_void,
        stride: c_int,
        count: c_int,
    );
    fn tessTesselate(
        tess: *mut Tesselator,
        winding_rule: c_int,
        element_type: c_int,
        poly_size: c_int,
        vertex_size: c_int,
        normal: *const f32,
    ) -> c_int;
    fn tessGetElementCount(tess: *mut Tesselator) -> c_int;
    fn tessGetElements(tess: *mut Tesselator) -> *const c_int;
    fn tessGetVertices(tess: *mut Tesselator) -> *const f32;
}

/// Triangles of the outlines by the non-zero winding rule, their vertices in
/// the order the triangles first use them.
fn tessellate(outlines: &[Vec<[f32; 2]>]) -> (Vec<[f32; 2]>, Vec<u16>) {
    let mut vertices: Vec<[f32; 2]> = Vec::new();
    let mut indices = Vec::new();
    // SAFETY: libtess2 owns the tesselator until it is deleted below; the
    // contours outlive the calls that read them.
    unsafe {
        let tess = tessNewTess(std::ptr::null_mut());
        if tess.is_null() {
            return (vertices, indices);
        }
        for outline in outlines {
            if !outline.is_empty() {
                tessAddContour(tess, 2, outline.as_ptr().cast(), 8, outline.len() as c_int);
            }
        }
        if tessTesselate(tess, 1, 0, 3, 2, std::ptr::null()) == 1 {
            let count = tessGetElementCount(tess).max(0) as usize;
            let elements = std::slice::from_raw_parts(tessGetElements(tess), count * 3);
            let points = tessGetVertices(tess);
            for &element in elements {
                if element < 0 {
                    continue;
                }
                let p = [
                    *points.add(element as usize * 2),
                    *points.add(element as usize * 2 + 1),
                ];
                let known = vertices
                    .iter()
                    .position(|v| (v[0] - p[0]).abs() <= EPSILON && (v[1] - p[1]).abs() <= EPSILON);
                match known {
                    Some(index) => indices.push(index as u16),
                    None => {
                        indices.push(vertices.len() as u16);
                        vertices.push(p);
                    }
                }
            }
        }
        tessDeleteTess(tess);
    }
    (vertices, indices)
}

/// The outlines of an image's alpha (`width` × `height`, rows from the
/// bottom), in its pixels, and the rectangle they span.
pub struct Outlines {
    pub outlines: Vec<Vec<[f32; 2]>>,
    pub bounds: Option<[f32; 4]>,
}

/// The tight mesh of an image's alpha, in its pixels.
pub fn tight_mesh(alpha: &[u8], width: i32, height: i32, settings: &Settings) -> Mesh {
    let Outlines { outlines, bounds } = outlines(alpha, width, height, settings);
    let (mut vertices, indices) = if outlines.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        tessellate(&outlines)
    };
    // Where the tessellator added a vertex (outlines crossing), it is rounded
    // to a whole pixel too.
    for v in &mut vertices {
        *v = [(v[0] + 0.5).floor(), (v[1] + 0.5).floor()];
    }
    Mesh {
        vertices,
        indices,
        bounds,
    }
}

/// The outlines the generator makes of an image's alpha, as a sprite's mesh
/// and physics shape take them.
pub fn outlines(alpha: &[u8], width: i32, height: i32, settings: &Settings) -> Outlines {
    let detail = if settings.detail >= 0.0 {
        1.0 - if 1.0 < settings.detail {
            1.0
        } else {
            settings.detail
        }
    } else {
        settings.detail
    };
    let extrude = if settings.extrude == 0 {
        1
    } else {
        settings.extrude.min(32)
    };
    let original = corners(alpha, width, height, settings.alpha_tolerance, extrude);
    let mut working = original.clone();
    let mut paths = Vec::new();
    let mut total = 0.0f32;
    let quarter = (width * height) as f32 * 0.25;
    while let Some((trace, kind, area)) = next_outline(&original, &mut working) {
        if kind == Kind::Hole && !settings.hole_detection {
            continue;
        }
        if detail < 0.0 && kind == Kind::Hole && area < quarter {
            continue;
        }
        total += if kind == Kind::Outer { area } else { -area };
        paths.push(path_of(&trace, width, height, kind));
    }
    let mut detail = detail;
    if detail < 0.0 {
        let triangles: i32 = paths
            .iter()
            .map(|p| match p.kind {
                Kind::Hole => 2,
                Kind::Outer => p.original.len() as i32 - 2,
            })
            .sum();
        let auto = (total * f32::from_bits(0xb9d5_5555)) / triangles as f32 + 1.0;
        detail = if auto < 0.0 {
            0.0
        } else if auto <= 1.0 {
            auto
        } else {
            1.0
        };
    }
    for path in &mut paths {
        simplify(path, detail);
        for vertex in &mut path.vertices {
            vertex.pos = [(vertex.pos[0] + 0.5).floor(), (vertex.pos[1] + 0.5).floor()];
        }
    }
    let bounds = (!paths.is_empty()).then(|| {
        let side = |i: usize| paths.iter().map(move |p| p.bounds[i]);
        let min = |i: usize| side(i).fold(f32::INFINITY, |a, b| if b < a { b } else { a });
        let max = |i: usize| side(i).fold(f32::NEG_INFINITY, |a, b| if a < b { b } else { a });
        let (min_x, min_y, max_x, max_y) = (min(0), min(1), max(2), max(3));
        [min_x, min_y, max_x - min_x, max_y - min_y]
    });
    let outlines: Vec<Vec<[f32; 2]>> = paths
        .iter()
        .map(|p| {
            let mut points: Vec<[f32; 2]> = p.vertices.iter().map(|v| v.pos).collect();
            without_straight_vertices(&mut points);
            points
        })
        .collect();
    Outlines { outlines, bounds }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            detail: -1.0,
            extrude: 1,
            alpha_tolerance: 0,
            hole_detection: true,
        }
    }

    #[test]
    fn an_opaque_image_is_its_rectangle_in_two_triangles() {
        let alpha = vec![255u8; 40 * 36];
        let mesh = tight_mesh(&alpha, 40, 36, &settings());
        let mut corners = mesh.vertices.clone();
        corners.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            corners,
            [[0.0, 0.0], [0.0, 36.0], [40.0, 0.0], [40.0, 36.0]]
        );
        assert_eq!(mesh.indices.len(), 6);
        assert_eq!(mesh.bounds, Some([0.0, 0.0, 40.0, 36.0]));
    }

    #[test]
    fn transparent_pixels_leave_no_outline() {
        let mesh = tight_mesh(&[0u8; 32 * 32], 32, 32, &settings());
        assert!(mesh.vertices.is_empty() && mesh.bounds.is_none());
    }

    #[test]
    fn an_outline_moves_out_a_pixel_and_keeps_within_the_image() {
        // A 20×20 opaque square in a 40×40 image, grown a pixel by the
        // extrusion and moved out a pixel along its corners' normals.
        let mut alpha = vec![0u8; 40 * 40];
        for y in 10..30 {
            for x in 10..30 {
                alpha[y * 40 + x] = 255;
            }
        }
        let outlines = outlines(&alpha, 40, 40, &settings());
        assert_eq!(outlines.outlines.len(), 1);
        let [x, y, w, h] = outlines.bounds.unwrap();
        // The extrusion cuts the corners; the corner of the cut moves out
        // along a normal at 22.5 degrees.
        let out = (std::f32::consts::PI / 8.0).cos();
        assert!((x - (9.0 - out)).abs() < 1e-5, "{x}");
        assert!((y - x).abs() < 1e-5 && (w - h).abs() < 1e-5);
        for point in &outlines.outlines[0] {
            assert!(point[0] == point[0].floor() && point[1] == point[1].floor());
        }
    }

    #[test]
    fn a_hole_is_cut_out_of_the_mesh() {
        let mut alpha = vec![255u8; 64 * 64];
        // Larger than a quarter of the image after the extrusion narrows it:
        // with the detail left to the area, smaller holes are filled.
        for y in 8..56 {
            for x in 8..56 {
                alpha[y * 64 + x] = 0;
            }
        }
        let mesh = tight_mesh(&alpha, 64, 64, &settings());
        // No triangle covers the middle of the hole.
        let inside = |p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
            let side = |u: [f32; 2], v: [f32; 2]| {
                (v[0] - u[0]) * (p[1] - u[1]) - (v[1] - u[1]) * (p[0] - u[0])
            };
            let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
            (d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0) || (d1 <= 0.0 && d2 <= 0.0 && d3 <= 0.0)
        };
        for triangle in mesh.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|k| mesh.vertices[triangle[k] as usize]);
            assert!(!inside([32.0, 32.0], a, b, c));
        }
        assert!(mesh.vertices.len() >= 8);
    }
}
