//! Weighted tangents (`ConvertToHermiteCurve` of the pinned Editor). A build
//! does not keep a weighted curve: before it classifies the curve it samples
//! every weighted segment once per frame of the clip's sample rate and joins
//! the samples with straight tangents, so the Player only ever sees plain
//! keys. The samples come from the Editor's own Bézier evaluation.
//!
//! Every step is a single-precision operation in the engine's order, and the
//! logarithm, exponential, arc tangent and cosine come from the system
//! library, as they do in the engine.
use super::unsupported;
use anyhow::Result;

/// One component of an authored key.
#[derive(Debug, Clone, Copy)]
pub(super) struct Key {
    pub time: f32,
    pub value: f32,
    pub in_slope: f32,
    pub out_slope: f32,
    pub in_weight: f32,
    pub out_weight: f32,
}

/// A converted key of one component: time, value, in and out tangent.
pub(super) type Plain = [f32; 4];

/// The bits of `weightedMode`.
const IN: i64 = 1;
const OUT: i64 = 2;
/// The weight of a tangent that is not weighted.
const THIRD: f32 = 1.0 / 3.0;
/// A third of a turn, as the engine rounds it.
const TURN_THIRD: f32 = f32::from_bits(0x4006_0a92);
/// Below this a coefficient of the cubic counts as absent.
const DEGENERATE: f32 = 0.001;
/// A curve is sampled into at most this many keys. The engine has no bound;
/// this one keeps a clip of hours from exhausting memory.
const MAX_KEYS: usize = 1 << 20;

/// The engine's cube root: through the logarithm in single precision and the
/// exponential in double.
fn cube_root(value: f32) -> f32 {
    let root = |value: f32| (f64::from(value.ln()) / 3.0).exp();
    if 0.0 > value {
        -root(-value) as f32
    } else {
        root(value) as f32
    }
}

/// The Bézier parameter at which a segment reaches the fraction `t` of its
/// duration (`BezierExtractU`): a root within the unit interval of
/// `a u^3 + b u^2 + c u = t`, where the control points sit at `out` and
/// `into` of the duration. Without such a root the nearer end is taken.
fn parameter(t: f32, out: f32, into: f32) -> f32 {
    let c = out * 3.0;
    let scaled = into * 3.0;
    let a = c - scaled + 1.0;
    let b = out * -6.0 + scaled;
    let end = if t < 0.5 { 0.0 } else { 1.0 };
    // Written as the engine's comparisons, which a NaN fails.
    let within = |u: f32| (0.0..=1.0).contains(&u);
    if a.abs() > DEGENERATE {
        let triple = a * 3.0;
        let shift = -b / triple;
        let square = shift * shift;
        let q = (c * b + t * triple) / (a * (a * 6.0)) + shift * square;
        let p = c / triple - square;
        let discriminant = p * p * p + q * q;
        if 0.0 > discriminant {
            let angle = (-discriminant).sqrt().atan2(q) / 3.0;
            let radius = cube_root((q * q - discriminant).sqrt());
            let double = radius + radius;
            let roots = [
                angle.cos(),
                (TURN_THIRD + angle).cos(),
                (angle + -TURN_THIRD).cos(),
            ];
            // Of several roots within the interval the first in this order.
            return roots
                .into_iter()
                .map(|cosine| cosine * double + shift)
                .find(|&u| within(u))
                .unwrap_or(end);
        }
        let root = discriminant.sqrt();
        let u = cube_root(q + root) + cube_root(q - root) + shift;
        if within(u) { u } else { end }
    } else if b.abs() > DEGENERATE {
        let root = (4.0 * b * t + c * c).sqrt();
        let double = b + b;
        [(-c - root) / double, (root - c) / double]
            .into_iter()
            .find(|&u| within(u))
            .unwrap_or(end)
    } else if c.abs() > DEGENERATE {
        t / c
    } else {
        0.0
    }
}

/// The value of a weighted segment at a time inside it (`BezierInterpolate`).
/// An unweighted end of the segment weighs a third.
fn bezier(time: f32, lhs: &Key, rhs: &Key, modes: [i64; 2]) -> f32 {
    let duration = rhs.time - lhs.time;
    let into = if modes[1] & IN != 0 {
        rhs.in_weight
    } else {
        THIRD
    };
    let out = if modes[0] & OUT != 0 {
        lhs.out_weight
    } else {
        THIRD
    };
    let leave = lhs.out_slope * duration;
    let arrive = duration * rhs.in_slope;
    let u = parameter((time - lhs.time) / duration, out, 1.0 - into);
    let first = leave * out + lhs.value;
    let second = rhs.value - arrive * into;
    let square = u * u;
    let rest = 1.0 - u;
    let rest_square = rest * rest;
    u * square * rhs.value
        + (square * 3.0 * rest * second
            + (u * 3.0 * rest_square * first + rest * rest_square * lhs.value))
}

/// Which end a stepped segment holds: the left one for a positive infinite
/// tangent on either side, the right one for a negative.
#[derive(Clone, Copy, PartialEq)]
enum Held {
    Left,
    Right,
}

fn held(lhs: &Key, rhs: &Key) -> Option<Held> {
    let either = |slope: f32| lhs.out_slope == slope || rhs.in_slope == slope;
    if either(f32::INFINITY) {
        Some(Held::Left)
    } else if either(f32::NEG_INFINITY) {
        Some(Held::Right)
    } else {
        None
    }
}

/// `FloorfToInt(time * rate + 0.5)`: the frame a key belongs to.
fn frame(time: f32, rate: f32, what: &str) -> Result<i32> {
    let scaled = time * rate + 0.5;
    let floored = if 0.0 <= scaled {
        scaled
    } else {
        scaled + -0.999_999_94
    };
    // The engine's conversion of anything else is not a frame.
    if !(floored > -2_147_483_648.0 && floored < 2_147_483_648.0) {
        return Err(unsupported(format!(
            "{what} has weighted keys at times the sample rate does not count"
        )));
    }
    Ok(floored as i32)
}

/// The plain keys a build makes of a curve with weighted tangents, per
/// component, or `None` for a curve the build takes as authored. Every
/// component has a key at the same times, and `modes` holds the
/// `weightedMode` of each. Keys are in increasing time order.
///
/// A segment is weighted when its left key weighs the outgoing tangent or its
/// right key the incoming one. Keys before the first weighted segment are
/// kept; from there on every weighted segment is sampled at the frames
/// strictly between its keys. A curve with weighted keys and no weighted
/// segment comes out empty, and the build drops it.
///
/// A key kept after the first weighted segment keeps its mode in the engine.
/// The streamed clip is built from tangents alone, but the dense clip would
/// evaluate the converted curve, weights included; `streamed` tells that no
/// curve of the clip can end up there.
pub(super) fn convert(
    components: &[Vec<Key>],
    modes: &[i64],
    sample_rate: f32,
    streamed: bool,
    what: &str,
) -> Result<Option<Vec<Vec<Plain>>>> {
    let count = modes.len();
    if count < 2 || modes.iter().all(|&mode| mode == 0) {
        return Ok(None);
    }
    if let Some(mode) = modes.iter().find(|mode| !(0..=(IN | OUT)).contains(*mode)) {
        return Err(unsupported(format!("{what} has weighted mode {mode}")));
    }
    // A quaternion is stepped as a whole, by any of its components.
    let whole = components.len() == 4;
    let plain = |key: &Key| [key.time, key.value, key.in_slope, key.out_slope];
    let mut result: Vec<Vec<Plain>> = vec![Vec::new(); components.len()];
    let mut started = false;
    for index in 0..count - 1 {
        let pair = [modes[index], modes[index + 1]];
        if pair[0] & OUT == 0 && pair[1] & IN == 0 {
            if started {
                if pair[1] & OUT != 0 && !streamed {
                    return Err(unsupported(format!(
                        "{what} keeps a weighted key the dense clip could sample"
                    )));
                }
                for (keys, component) in result.iter_mut().zip(components) {
                    keys.push(plain(&component[index + 1]));
                }
            }
            continue;
        }
        let ends: Vec<(&Key, &Key)> = components
            .iter()
            .map(|component| (&component[index], &component[index + 1]))
            .collect();
        let weights = |(lhs, rhs): &(&Key, &Key)| {
            (pair[0] & OUT == 0 || lhs.out_weight.is_finite())
                && (pair[1] & IN == 0 || rhs.in_weight.is_finite())
        };
        if !ends.iter().all(weights) {
            return Err(unsupported(format!("{what} has a non-finite weight")));
        }
        if !started {
            for (keys, component) in result.iter_mut().zip(components) {
                keys.extend(component[..index].iter().map(plain));
                let lhs = &component[index];
                keys.push([lhs.time, lhs.value, lhs.in_slope, 0.0]);
            }
        }
        started = true;
        let stepped: Vec<Option<Held>> = ends.iter().map(|(lhs, rhs)| held(lhs, rhs)).collect();
        let all = [Held::Left, Held::Right]
            .into_iter()
            .find(|side| stepped.contains(&Some(*side)));
        // Joins the last key to the next one with the tangent of the line
        // between them, or with the infinite tangent of a stepped segment.
        let mut join = |time: f32, values: &[f32], out_slopes: Option<&[f32]>| -> Result<()> {
            for (lane, keys) in result.iter_mut().enumerate() {
                let last = keys.len() - 1;
                let [before, value, _, _] = keys[last];
                let slope = match stepped[lane] {
                    Some(Held::Left) => f32::INFINITY,
                    Some(Held::Right) => f32::NEG_INFINITY,
                    None => (value - values[lane]) / (before - time),
                };
                if !values[lane].is_finite() || (stepped[lane].is_none() && !slope.is_finite()) {
                    return Err(unsupported(format!(
                        "{what} has weighted tangents that sample to a non-finite key"
                    )));
                }
                if keys.len() >= MAX_KEYS {
                    return Err(unsupported(format!(
                        "{what} has weighted tangents that sample to too many keys"
                    )));
                }
                keys[last][3] = slope;
                let out_slope = out_slopes.map_or(0.0, |slopes| slopes[lane]);
                keys.push([time, values[lane], slope, out_slope]);
            }
            Ok(())
        };
        let (begin, end) = (ends[0].0.time, ends[0].1.time);
        let mut previous = begin;
        for number in frame(begin, sample_rate, what)? + 1..frame(end, sample_rate, what)? {
            let time = number as f32 / sample_rate;
            // Rounding can put a frame on or past a key, where the engine
            // divides by zero or samples a neighboring segment.
            if !(previous < time && time < end) {
                return Err(unsupported(format!(
                    "{what} has weighted keys the sample rate does not tell apart"
                )));
            }
            previous = time;
            let values: Vec<f32> = ends
                .iter()
                .zip(&stepped)
                .map(|((lhs, rhs), own)| match if whole { all } else { *own } {
                    Some(Held::Left) => lhs.value,
                    Some(Held::Right) => rhs.value,
                    None => bezier(time, lhs, rhs, pair),
                })
                .collect();
            join(time, &values, None)?;
        }
        let values: Vec<f32> = ends.iter().map(|(_, rhs)| rhs.value).collect();
        let out_slopes: Vec<f32> = ends.iter().map(|(_, rhs)| rhs.out_slope).collect();
        join(end, &values, Some(&out_slopes))?;
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(time: f32, value: f32, weight: f32) -> Key {
        Key {
            time,
            value,
            in_slope: 0.0,
            out_slope: 0.0,
            in_weight: weight,
            out_weight: weight,
        }
    }

    #[test]
    fn thirds_are_the_unweighted_curve() {
        // With both weights at a third the parameter is the time fraction.
        for step in 0..=8 {
            let t = step as f32 / 8.0;
            assert!((parameter(t, THIRD, 1.0 - THIRD) - t).abs() < 1e-6, "{t}");
        }
    }

    #[test]
    fn only_weighted_segments_are_sampled() {
        let curve = [vec![
            key(0.0, 0.0, 0.5),
            key(0.5, 1.0, 0.5),
            key(1.0, 0.0, 0.5),
            key(1.5, 1.0, 0.5),
        ]];
        // No key before the last weighs the tangent of a segment.
        assert_eq!(
            convert(&curve, &[IN, 0, 0, OUT], 4.0, true, "curve").unwrap(),
            Some(vec![Vec::new()])
        );
        assert_eq!(convert(&curve, &[0; 4], 4.0, true, "curve").unwrap(), None);
        // A key kept behind a weighted segment still weighs its tangent.
        let kept = [OUT, 0, OUT, 0];
        assert!(convert(&curve, &kept, 4.0, false, "curve").is_err());
        assert!(convert(&curve, &kept, 4.0, true, "curve").is_ok());
        // The middle segment: one frame between its keys, then the rest.
        let converted = convert(&curve, &[0, OUT, 0, 0], 4.0, true, "curve")
            .unwrap()
            .unwrap();
        let times: Vec<f32> = converted[0].iter().map(|key| key[0]).collect();
        assert_eq!(times, [0.0, 0.5, 0.75, 1.0, 1.5]);
        let [_, middle, slope, _] = converted[0][2];
        assert_eq!(converted[0][1][3], slope);
        assert_eq!(slope, (1.0 - middle) / (0.5 - 0.75));
    }

    #[test]
    fn a_quaternion_is_stepped_as_a_whole() {
        let mut stepped = vec![key(0.0, 1.0, 0.5), key(1.0, 2.0, 0.5)];
        stepped[0].out_slope = f32::INFINITY;
        let smooth = vec![key(0.0, 3.0, 0.5), key(1.0, 4.0, 0.5)];
        let sample = |width: usize| {
            let mut components = vec![smooth.clone(); width];
            components[0] = stepped.clone();
            let converted = convert(&components, &[OUT, IN], 2.0, true, "curve").unwrap();
            converted.unwrap()[1][1]
        };
        // A vector holds only the stepped component.
        assert_ne!(sample(3)[1], 3.0);
        assert_eq!(sample(4)[1], 3.0);
        // The tangents stay those of the component itself.
        assert_eq!(sample(4)[2], 0.0);
    }
}
