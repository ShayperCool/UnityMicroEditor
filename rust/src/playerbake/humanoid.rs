//! What a build derives from a clip's human curves: the root transform at the
//! start and stop of the clip, the pose difference between them and the
//! average speed of the root.
//!
//! The engine computes these in single precision with four-lane vector code.
//! Every function here performs the same operations in the same order, one
//! rounding each, because a reordered sum already changes the last bit. Lane
//! sums that look needlessly shuffled, and multiplications by one or zero,
//! are the engine's.
use super::{crc32, unsupported};
use anyhow::Result;
use std::sync::LazyLock;

/// Float curves the engine binds to the rig instead of to a parameter.
pub(crate) const CURVE_COUNT: usize = 200;

// Positions in the curve table. A transform takes seven curves: its
// translation, then its rotation as x, y, z, w.
const MOTION: usize = 0;
const ROOT: usize = 7;
const GOALS: usize = 14;
const GOAL_COUNT: usize = 4;
const MUSCLES: usize = 42;
const MUSCLE_COUNT: usize = 55;
const LEFT_FINGERS: usize = 97;
const RIGHT_FINGERS: usize = 117;
const FINGER_COUNT: usize = 20;
const TRANSLATIONS: usize = 137;
const TRANSLATION_COUNT: usize = 21;

const MUSCLE_NAMES: [&str; MUSCLE_COUNT] = [
    "Spine Front-Back",
    "Spine Left-Right",
    "Spine Twist Left-Right",
    "Chest Front-Back",
    "Chest Left-Right",
    "Chest Twist Left-Right",
    "UpperChest Front-Back",
    "UpperChest Left-Right",
    "UpperChest Twist Left-Right",
    "Neck Nod Down-Up",
    "Neck Tilt Left-Right",
    "Neck Turn Left-Right",
    "Head Nod Down-Up",
    "Head Tilt Left-Right",
    "Head Turn Left-Right",
    "Left Eye Down-Up",
    "Left Eye In-Out",
    "Right Eye Down-Up",
    "Right Eye In-Out",
    "Jaw Close",
    "Jaw Left-Right",
    "Left Upper Leg Front-Back",
    "Left Upper Leg In-Out",
    "Left Upper Leg Twist In-Out",
    "Left Lower Leg Stretch",
    "Left Lower Leg Twist In-Out",
    "Left Foot Up-Down",
    "Left Foot Twist In-Out",
    "Left Toes Up-Down",
    "Right Upper Leg Front-Back",
    "Right Upper Leg In-Out",
    "Right Upper Leg Twist In-Out",
    "Right Lower Leg Stretch",
    "Right Lower Leg Twist In-Out",
    "Right Foot Up-Down",
    "Right Foot Twist In-Out",
    "Right Toes Up-Down",
    "Left Shoulder Down-Up",
    "Left Shoulder Front-Back",
    "Left Arm Down-Up",
    "Left Arm Front-Back",
    "Left Arm Twist In-Out",
    "Left Forearm Stretch",
    "Left Forearm Twist In-Out",
    "Left Hand Down-Up",
    "Left Hand In-Out",
    "Right Shoulder Down-Up",
    "Right Shoulder Front-Back",
    "Right Arm Down-Up",
    "Right Arm Front-Back",
    "Right Arm Twist In-Out",
    "Right Forearm Stretch",
    "Right Forearm Twist In-Out",
    "Right Hand Down-Up",
    "Right Hand In-Out",
];

/// Bones with translation curves, in the order of their curves.
const TRANSLATION_BONES: [&str; TRANSLATION_COUNT] = [
    "Spine",
    "Chest",
    "UpperChest",
    "Neck",
    "Head",
    "LeftUpperLeg",
    "LeftLowerLeg",
    "LeftFoot",
    "LeftToes",
    "RightUpperLeg",
    "RightLowerLeg",
    "RightFoot",
    "RightToes",
    "LeftShoulder",
    "LeftUpperArm",
    "LeftLowerArm",
    "LeftHand",
    "RightShoulder",
    "RightUpperArm",
    "RightLowerArm",
    "RightHand",
];

/// The attribute name of every human curve, by its position in the table.
fn curve_names() -> Vec<String> {
    let mut names = Vec::with_capacity(CURVE_COUNT);
    for transform in [
        "Motion",
        "Root",
        "LeftFoot",
        "RightFoot",
        "LeftHand",
        "RightHand",
    ] {
        names.extend(["x", "y", "z"].map(|axis| format!("{transform}T.{axis}")));
        names.extend(["x", "y", "z", "w"].map(|axis| format!("{transform}Q.{axis}")));
    }
    names.extend(MUSCLE_NAMES.map(str::to_string));
    for hand in ["LeftHand", "RightHand"] {
        for finger in ["Thumb", "Index", "Middle", "Ring", "Little"] {
            for part in ["1 Stretched", "Spread", "2 Stretched", "3 Stretched"] {
                names.push(format!("{hand}.{finger}.{part}"));
            }
        }
    }
    for bone in TRANSLATION_BONES {
        names.extend(["x", "y", "z"].map(|axis| format!("{bone}TDOF.{axis}")));
    }
    names
}

static CURVE_HASHES: LazyLock<Vec<u32>> =
    LazyLock::new(|| curve_names().iter().map(|name| crc32(name)).collect());

/// The human curve an attribute of this name hash drives. The engine looks
/// the curve up by hash as well, so the name has to match exactly.
pub(crate) fn curve_index(hash: u32) -> Option<u32> {
    CURVE_HASHES
        .iter()
        .position(|&known| known == hash)
        .map(|index| index as u32)
}

/// Whether a human curve belongs to the motion transform, which makes the
/// clip one with motion float curves.
pub(crate) fn motion_curve(index: u32) -> bool {
    (index as usize) < ROOT
}

type Vector = [f32; 3];
type Quaternion = [f32; 4];

/// A translation and a rotation as x, y, z, w.
pub(crate) type Transform = (Vector, Quaternion);

pub(crate) const IDENTITY: Transform = ([0.0; 3], [0.0, 0.0, 0.0, 1.0]);

#[derive(Debug, Clone)]
pub(crate) struct Pose {
    pub root: Transform,
    pub goals: [Transform; GOAL_COUNT],
    pub muscles: [f32; MUSCLE_COUNT],
    pub left_fingers: [f32; FINGER_COUNT],
    pub right_fingers: [f32; FINGER_COUNT],
    pub translations: [Vector; TRANSLATION_COUNT],
}

impl Pose {
    pub(crate) fn identity() -> Self {
        Self {
            root: IDENTITY,
            goals: [IDENTITY; GOAL_COUNT],
            muscles: [0.0; MUSCLE_COUNT],
            left_fingers: [0.0; FINGER_COUNT],
            right_fingers: [0.0; FINGER_COUNT],
            translations: [[0.0; 3]; TRANSLATION_COUNT],
        }
    }

    fn values(&self) -> impl Iterator<Item = f32> + '_ {
        transform_values(&self.root)
            .chain(self.goals.iter().flat_map(transform_values))
            .chain(self.muscles.iter().copied())
            .chain(self.left_fingers.iter().copied())
            .chain(self.right_fingers.iter().copied())
            .chain(self.translations.iter().flatten().copied())
    }
}

fn transform_values(transform: &Transform) -> impl Iterator<Item = f32> + '_ {
    transform.0.iter().chain(&transform.1).copied()
}

/// The clip settings the derived values depend on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Settings {
    pub start: f32,
    pub stop: f32,
    pub orientation_offset_y: f32,
    pub mirror: bool,
    pub loop_blend_orientation: bool,
    pub loop_blend_position_y: bool,
    pub loop_blend_position_xz: bool,
    pub keep_original_orientation: bool,
}

/// The muscle clip fields a build fills from the human curves.
#[derive(Debug, Clone)]
pub(crate) struct Derived {
    pub start: Transform,
    pub stop: Transform,
    pub left_foot_start: Transform,
    pub right_foot_start: Transform,
    pub delta_pose: Pose,
    pub average_speed: Vector,
    pub average_angular_speed: f32,
}

/// Where each human curve sits in the clip's value array, or -1.
pub(crate) type Index = [i32; CURVE_COUNT];

fn conjugate(q: Quaternion) -> Quaternion {
    [-q[0], -q[1], -q[2], q[3]]
}

/// Each lane is the reciprocal root of its own sum of squares times the
/// component; the sums pair the lanes differently in even and odd lanes.
fn normalize(q: Quaternion) -> Quaternion {
    let s = q.map(|part| part * part);
    let t = [s[1] + s[0], s[2] + s[1], s[3] + s[2], s[0] + s[3]];
    let u = [t[2] + t[0], t[3] + t[1], t[0] + t[2], t[1] + t[3]];
    std::array::from_fn(|lane| (1.0 / u[lane].sqrt()) * q[lane])
}

/// Sum of squares per lane of a product still in its lane order before the
/// final shuffle.
fn lane_sums(lanes: [f32; 4]) -> [f32; 4] {
    let s = lanes.map(|part| part * part);
    let t = [s[3] + s[2], s[0] + s[3], s[1] + s[0], s[2] + s[1]];
    [t[2] + t[0], t[3] + t[1], t[0] + t[2], t[1] + t[3]]
}

/// The lanes of the product `c * a` before signs and shuffle are applied:
/// the product is `[-m2, -m3, -m0, m1]`.
fn multiply_lanes(c: Quaternion, a: Quaternion) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [cx, cy, cz, cw] = c;
    [
        ax * cy - az * cw - aw * cz - ay * cx,
        aw * cw - ax * cx - az * cz - ay * cy,
        ay * cz - az * cy - ax * cw - aw * cx,
        az * cx - ax * cz - ay * cw - aw * cy,
    ]
}

fn multiply(c: Quaternion, a: Quaternion) -> Quaternion {
    let [m0, m1, m2, m3] = multiply_lanes(c, a);
    [-m2, -m3, -m0, m1]
}

fn multiply_normalized(c: Quaternion, a: Quaternion) -> Quaternion {
    let [m0, m1, m2, m3] = multiply_lanes(c, a);
    let lanes = [-m0, m1, -m2, -m3];
    let total = lane_sums(lanes);
    let product = [lanes[2], lanes[3], lanes[0], lanes[1]];
    std::array::from_fn(|lane| (1.0 / total[lane].sqrt()) * product[lane])
}

/// Rotates `d` by `c`. The columns are the rotation matrix less the
/// identity, and the vector itself is added last.
fn rotate(c: Quaternion, d: Vector) -> Vector {
    let [cx, cy, cz, cw] = c;
    let (k1, k2, k3) = ([-2.0, 2.0, -2.0], [2.0, -2.0, -2.0], [-2.0, -2.0, 2.0]);
    let (s1, s2, s3) = ([cy, cx, cw], [cz, cw, cx], [cw, cz, cy]);
    std::array::from_fn(|i| {
        let column0 = cy * k1[i] * s1[i] - cz * k2[i] * s2[i];
        let column1 = cz * k3[i] * s3[i] - cx * k1[i] * s1[i];
        let column2 = cx * k2[i] * s2[i] - cy * k3[i] * s3[i];
        (d[2] * column2 + d[1] * column1) + (d[0] * column0 + d[i])
    })
}

/// `a` relative to `b`.
fn relative(a: &Transform, b: &Transform) -> Transform {
    let delta = std::array::from_fn(|i| a.0[i] - b.0[i]);
    let inverse = conjugate(b.1);
    (rotate(inverse, delta), multiply(inverse, a.1))
}

/// The rotation about the vertical axis that remains once the rotated Y axis
/// is turned back upright; identity when nothing remains.
fn project_on_y_plane(q: Quaternion) -> Quaternion {
    let [x, y, z, w] = q;
    // Twice the rotated Y axis.
    let v = [
        -(w * z) + y * x,
        w * w + -0.5 + y * y,
        w * x + y * z,
        -(w * y) + y * w,
    ]
    .map(|part| part + part);
    let zero = 0.0f32;
    // Its cross and dot products with the Y axis.
    let cross = [
        v[0] * 1.0 - v[1] * zero,
        v[1] * zero - v[2] * 1.0,
        v[2] * zero - v[0] * zero,
    ];
    let products = [v[0] * zero, v[1] * 1.0, v[2] * zero, zero * zero];
    let pairs = [
        products[0] + products[1],
        products[1] + products[2],
        products[2] + products[3],
        products[3] + products[0],
    ];
    let dot = pairs[2] + pairs[0];
    let s = [v[0] * v[0], v[1] * v[1], v[2] * v[2], 0.0];
    let t = [s[0] + s[1], s[1] + s[2], s[2] + s[3], s[3] + s[0]];
    let length = (t[0] + t[2]).sqrt() + dot;
    let upright = normalize([cross[1], cross[2], cross[0], length]);
    let [m0, m1, m2, m3] = multiply_lanes(upright, q);
    // Only the Y and W lanes of the product are kept.
    let lanes = [-m0 * zero, m1 * 1.0, -m2 * zero, -m3 * 1.0];
    let total = lane_sums(lanes);
    let product = [lanes[2], lanes[3], lanes[0], lanes[1]];
    std::array::from_fn(|lane| {
        if 1e-30 < total[lane] {
            product[lane] / total[lane].sqrt()
        } else {
            IDENTITY.1[lane]
        }
    })
}

/// The rotation about Y by the orientation offset in degrees, through the
/// engine's polynomial sine and cosine of half the angle.
fn orientation_offset(degrees: f32) -> Quaternion {
    let constant = f32::from_bits;
    let (pi, two_pi) = (constant(0x40490fdb), constant(0x40c90fdb));
    let radians = degrees * constant(0x3c8efa35);
    // Wrapped into a turn around zero.
    let shifted = radians.abs() + pi;
    let turns = (shifted / two_pi).trunc();
    let mut wrapped = (shifted - turns * two_pi) + -pi;
    if radians.is_sign_negative() {
        wrapped = -wrapped;
    }
    let mut half = wrapped * 0.5;
    let (low, high) = (constant(0xbfc90fd3), constant(0x3fc90fd3));
    half = if half > low { half } else { low };
    half = if half < high { half } else { high };
    // As a fraction of a turn.
    let x = half * constant(0x3e22f983);
    let polynomial = |v: f32| {
        let fraction = v - v.round_ties_even();
        let p = 0.25 - fraction.abs();
        let p2 = p * p;
        let p4 = p2 * p2;
        let high = p2 * constant(0xc2992322) + constant(0x42a33422);
        let low = p2 * constant(0xc2255ddc) + constant(0x40c90fda);
        let sum = high * p4 + low;
        (p4 * p4 * constant(0x421ea0cd) + sum) * p
    };
    let sine = polynomial(x + -0.25);
    let cosine = polynomial(x + 0.0);
    normalize([0.0, sine / cosine, 0.0, 1.0])
}

fn value(values: &[f32], index: &Index, curve: usize, missing: f32) -> f32 {
    match index[curve] {
        -1 => missing,
        position => values[position as usize],
    }
}

/// A transform from seven consecutive curves. A missing curve reads zero and
/// a missing rotation w reads one.
fn transform(values: &[f32], index: &Index, first: usize) -> Transform {
    let read = |offset: usize, missing: f32| value(values, index, first + offset, missing);
    (
        [read(0, 0.0), read(1, 0.0), read(2, 0.0)],
        normalize([read(3, 0.0), read(4, 0.0), read(5, 0.0), read(6, 1.0)]),
    )
}

fn pose(values: &[f32], index: &Index) -> Pose {
    let read = |curve: usize| value(values, index, curve, 0.0);
    Pose {
        root: transform(values, index, ROOT),
        goals: std::array::from_fn(|goal| transform(values, index, GOALS + 7 * goal)),
        muscles: std::array::from_fn(|i| read(MUSCLES + i)),
        left_fingers: std::array::from_fn(|i| read(LEFT_FINGERS + i)),
        right_fingers: std::array::from_fn(|i| read(RIGHT_FINGERS + i)),
        translations: std::array::from_fn(|bone| {
            std::array::from_fn(|axis| read(TRANSLATIONS + 3 * bone + axis))
        }),
    }
}

fn pose_difference(a: &Pose, b: &Pose) -> Pose {
    Pose {
        root: relative(&a.root, &b.root),
        goals: std::array::from_fn(|goal| relative(&a.goals[goal], &b.goals[goal])),
        muscles: std::array::from_fn(|i| a.muscles[i] - b.muscles[i]),
        left_fingers: std::array::from_fn(|i| a.left_fingers[i] - b.left_fingers[i]),
        right_fingers: std::array::from_fn(|i| a.right_fingers[i] - b.right_fingers[i]),
        translations: std::array::from_fn(|bone| {
            std::array::from_fn(|axis| a.translations[bone][axis] - b.translations[bone][axis])
        }),
    }
}

/// The root's average velocity and turning speed over twenty equal steps of
/// the clip. Motion curves give the path when the clip has any; otherwise it
/// is the root transform with its rotation reduced to a heading.
fn average_speed(
    sample: &dyn Fn(f32) -> Vec<f32>,
    index: &Index,
    settings: &Settings,
    start_rotation: Quaternion,
) -> (Vector, f32) {
    const STEPS: usize = 20;
    let count = STEPS as f32;
    if settings.stop.partial_cmp(&settings.start) != Some(std::cmp::Ordering::Greater) {
        return ([0.0; 3], 0.0 / count);
    }
    let period = (settings.stop - settings.start) / count;
    let has_motion = index[MOTION..ROOT].iter().any(|&position| position != -1);
    let mut offset = orientation_offset(settings.orientation_offset_y);
    if settings.keep_original_orientation {
        offset = multiply_normalized(offset, conjugate(start_rotation));
    }
    let mut velocity = [0.0f32; 3];
    let mut angular = 0.0f32;
    let mut previous: Option<Transform> = None;
    // The time is accumulated, not computed from the step number.
    let mut time = settings.start;
    for _ in 0..=STEPS {
        let values = sample(time);
        let current = if has_motion {
            transform(&values, index, MOTION)
        } else {
            let (translation, rotation) = transform(&values, index, ROOT);
            let heading = if settings.loop_blend_orientation {
                start_rotation
            } else {
                rotation
            };
            (
                translation,
                multiply_normalized(offset, project_on_y_plane(heading)),
            )
        };
        if let Some(previous) = previous {
            let (mut moved, mut turned) = relative(&current, &previous);
            if settings.mirror {
                // A reflection in the YZ plane.
                moved = [-moved[0], moved[1], moved[2]];
                turned = [turned[0], -turned[1], -turned[2], turned[3]];
            }
            let tangent = if 1e-6 > turned[3].abs() {
                turned[1] / 1e-6
            } else {
                turned[1] / turned[3]
            };
            // The arc tangent is the system C library's, as in the engine;
            // a library that rounds it differently moves the result by one
            // unit in the last place.
            let angle = tangent.atan();
            angular += (angle + angle) / period;
            for (sum, moved) in velocity.iter_mut().zip(moved) {
                *sum += moved / period;
            }
        }
        previous = Some(current);
        time += period;
    }
    let mut average = velocity.map(|sum| sum / count);
    if settings.loop_blend_position_xz {
        average[0] = 0.0;
        average[2] = 0.0;
    }
    if settings.loop_blend_position_y {
        average[1] = 0.0;
    }
    (average, angular / count)
}

/// Derives the fields from the clip's curves. `sample` returns the value
/// array at a time: every streamed curve, then every constant.
pub(crate) fn derive(
    index: &Index,
    settings: &Settings,
    sample: &dyn Fn(f32) -> Vec<f32>,
) -> Result<Derived> {
    let (at_start, at_stop) = (sample(settings.start), sample(settings.stop));
    let start = transform(&at_start, index, ROOT);
    let stop = transform(&at_stop, index, ROOT);
    // A pose is taken only when a goal, muscle or translation curve exists.
    let posed = index[GOALS..].iter().any(|&position| position != -1);
    let (delta_pose, left_foot_start, right_foot_start) = if posed {
        let (first, last) = (pose(&at_start, index), pose(&at_stop, index));
        (
            pose_difference(&first, &last),
            first.goals[0],
            first.goals[1],
        )
    } else {
        (Pose::identity(), IDENTITY, IDENTITY)
    };
    let (average_speed, average_angular_speed) = average_speed(sample, index, settings, start.1);
    let derived = Derived {
        start,
        stop,
        left_foot_start,
        right_foot_start,
        delta_pose,
        average_speed,
        average_angular_speed,
    };
    // A rotation of zero length normalizes to no number at all, and which of
    // the possible encodings the engine ends up with is not reproduced.
    let undefined = transform_values(&derived.start)
        .chain(transform_values(&derived.stop))
        .chain(transform_values(&derived.left_foot_start))
        .chain(transform_values(&derived.right_foot_start))
        .chain(derived.delta_pose.values())
        .chain(derived.average_speed)
        .chain([derived.average_angular_speed])
        .any(f32::is_nan);
    if undefined {
        return Err(unsupported(
            "human curves with a rotation of zero length or another value that is not a number",
        ));
    }
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_table_has_the_engine_layout() {
        let names = curve_names();
        assert_eq!(names.len(), CURVE_COUNT);
        for (index, name) in [
            (MOTION, "MotionT.x"),
            (ROOT + 6, "RootQ.w"),
            (GOALS, "LeftFootT.x"),
            (MUSCLES, "Spine Front-Back"),
            (LEFT_FINGERS, "LeftHand.Thumb.1 Stretched"),
            (RIGHT_FINGERS + 19, "RightHand.Little.3 Stretched"),
            (TRANSLATIONS, "SpineTDOF.x"),
            (CURVE_COUNT - 1, "RightHandTDOF.z"),
        ] {
            assert_eq!(names[index], name);
            assert_eq!(curve_index(crc32(name)), Some(index as u32));
        }
        // The lookup is exact: neither case nor a near name binds to the rig.
        assert_eq!(curve_index(crc32("rootT.x")), None);
        assert_eq!(curve_index(crc32("RootT.w")), None);
    }
}
