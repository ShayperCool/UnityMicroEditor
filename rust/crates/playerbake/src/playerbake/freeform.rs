//! The tables the engine precomputes for a freeform 2D blend tree
//! (`mecanim::animation::PrecomputeFreeform` of the pinned Editor): per pair
//! of children a vector and a scale, and per child the neighbors that can
//! limit its weight somewhere in the blend space.
//!
//! Every step is a single-precision operation in the engine's order. Element
//! `[j * n + i]` of a pair table describes the ordered pair from child `i` to
//! child `j`.

/// The NaN an invalid operation yields on the engine's hardware, sign set.
const INDEFINITE: f32 = f32::from_bits(0xffc0_0000);
const STEP: f32 = f32::from_bits(0x3c23_d70a); // 0.01
const NEGATIVE_INVERSE_PI: f32 = f32::from_bits(0xbea2_f983);
/// Samples per axis of the grid that finds neighbors.
const GRID: usize = 101;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Freeform {
    /// Empty for the Cartesian kind.
    pub magnitudes: Vec<f32>,
    pub pair_vectors: Vec<[f32; 2]>,
    pub pair_scales: Vec<f32>,
    pub neighbors: Vec<Vec<u32>>,
}

/// Keeps the engine's NaN bit pattern whatever the compiler folds.
fn exact(value: f32) -> f32 {
    if value.is_nan() { INDEFINITE } else { value }
}

fn magnitude(p: [f32; 2]) -> f32 {
    exact((p[1] * p[1] + p[0] * p[0]).sqrt())
}

fn is_origin(p: [f32; 2]) -> bool {
    p[0] == 0.0 && p[1] == 0.0
}

/// The signed angle from `a` to `b`; neither may be the origin. The arc
/// cosine comes from the system library, as it does in the engine.
fn signed_angle(a: [f32; 2], b: [f32; 2]) -> f32 {
    let scale = exact(magnitude(a) * magnitude(b));
    let dot = exact(a[1] * b[1] + a[0] * b[0]);
    let mut cosine = exact(dot / scale);
    // Written as the engine's comparisons, which send a NaN to -1.
    cosine = if cosine > -1.0 { cosine } else { -1.0 };
    cosine = if cosine < 1.0 { cosine } else { 1.0 };
    let angle = cosine.acos();
    let cross = exact(a[0] * b[1] - a[1] * b[0]);
    if cross < 0.0 { -angle } else { angle }
}

fn directional_crop(table: &Freeform, positions: &[[f32; 2]], sample: [f32; 2], crop: &mut [i32]) {
    let n = positions.len();
    let sample_magnitude = magnitude(sample);
    let at_origin = is_origin(sample);
    let offsets: Vec<[f32; 2]> = (0..n)
        .map(|child| {
            let angle = if at_origin || is_origin(positions[child]) {
                0.0
            } else {
                signed_angle(positions[child], sample)
            };
            [angle, exact(sample_magnitude - table.magnitudes[child])]
        })
        .collect();
    for child in 0..n {
        let mut value = exact(offsets[child][0].abs() * NEGATIVE_INVERSE_PI + 1.0);
        let mut best = -1;
        for other in 0..n {
            if other == child {
                continue;
            }
            let pair = n * other + child;
            let [mut pair_x, pair_y] = table.pair_vectors[pair];
            let mut offset_x = offsets[child][0];
            if is_origin(positions[child]) {
                pair_x = offsets[other][0];
            } else if is_origin(positions[other]) {
                pair_x = offset_x;
            } else if pair_x == 0.0 || at_origin {
                offset_x = pair_x;
            }
            let numerator = exact(
                exact(exact(table.pair_scales[pair] * offsets[child][1]) * pair_y)
                    + exact(offset_x * pair_x),
            );
            let denominator = exact(pair_y * pair_y + pair_x * pair_x);
            let height = exact(1.0 - exact(numerator / denominator));
            if 0.0 >= height {
                best = -1;
                break;
            }
            if value > height {
                best = other as i32;
                value = height;
            }
        }
        crop[child] = best;
    }
}

fn cartesian_crop(table: &Freeform, positions: &[[f32; 2]], sample: [f32; 2], crop: &mut [i32]) {
    let n = positions.len();
    for child in 0..n {
        let x = exact(sample[0] - positions[child][0]);
        let y = exact(sample[1] - positions[child][1]);
        let mut value = 1.0f32;
        let mut best = -1;
        for other in 0..n {
            if other == child {
                continue;
            }
            let pair = n * other + child;
            let [pair_x, pair_y] = table.pair_vectors[pair];
            let height = exact(
                1.0 - exact(exact(exact(pair_y * y) + exact(pair_x * x)) * table.pair_scales[pair]),
            );
            if 0.0 >= height {
                best = -1;
                break;
            }
            if value > height {
                best = other as i32;
                value = height;
            }
        }
        crop[child] = best;
    }
}

/// The tables of a freeform directional (`directional`) or Cartesian blend
/// tree over the positions of the children its node keeps.
pub fn precompute(directional: bool, positions: &[[f32; 2]]) -> Freeform {
    let n = positions.len();
    let mut table = Freeform {
        pair_vectors: vec![[0.0; 2]; n * n],
        pair_scales: vec![0.0; n * n],
        ..Default::default()
    };
    if directional {
        table.magnitudes = positions.iter().map(|&p| magnitude(p)).collect();
    }
    for i in 0..n {
        for j in 0..n {
            let pair = i + j * n;
            if directional {
                let (from, to) = (table.magnitudes[i], table.magnitudes[j]);
                let scale = exact(2.0 / exact(to + from));
                let angle = if to == 0.0 || from == 0.0 {
                    0.0
                } else {
                    signed_angle(positions[i], positions[j])
                };
                table.pair_scales[pair] = scale;
                table.pair_vectors[pair] = [angle, exact(exact(to - from) * scale)];
            } else {
                let x = exact(positions[j][0] - positions[i][0]);
                let y = exact(positions[j][1] - positions[i][1]);
                table.pair_scales[pair] = exact(1.0 / exact(y * y + x * x));
                table.pair_vectors[pair] = [x, y];
            }
        }
    }
    // The grid covers the bounding box enlarged by half its size on each
    // side. A coordinate beyond the initial bounds leaves them in place.
    let (mut low, mut high) = ([10000.0f32; 2], [-10000.0f32; 2]);
    for position in positions {
        for axis in 0..2 {
            if position[axis] < low[axis] {
                low[axis] = position[axis];
            }
            if high[axis] < position[axis] {
                high[axis] = position[axis];
            }
        }
    }
    let mut from = [0.0f32; 2];
    let mut to = [0.0f32; 2];
    for axis in 0..2 {
        let extent = exact(exact(high[axis] - low[axis]) * 0.5);
        from[axis] = exact(low[axis] - extent);
        to[axis] = exact(extent + high[axis]);
    }
    let coordinate = |axis: usize, index: usize| {
        let t = exact(index as f32 * STEP);
        exact(exact(t * to[axis]) + exact(exact(1.0 - t) * from[axis]))
    };
    let mut seen = vec![false; n * n];
    let mut crop = vec![-1i32; n];
    if n != 0 {
        let ys: Vec<f32> = (0..GRID).map(|index| coordinate(1, index)).collect();
        for index in 0..GRID {
            let x = coordinate(0, index);
            for &y in &ys {
                if directional {
                    directional_crop(&table, positions, [x, y], &mut crop);
                } else {
                    cartesian_crop(&table, positions, [x, y], &mut crop);
                }
                for (child, &limit) in crop.iter().enumerate() {
                    if limit >= 0 {
                        seen[child * n + limit as usize] = true;
                    }
                }
            }
        }
    }
    table.neighbors = (0..n)
        .map(|child| {
            (0..n)
                .filter(|&other| seen[child * n + other])
                .map(|other| other as u32)
                .collect()
        })
        .collect();
    table
}
