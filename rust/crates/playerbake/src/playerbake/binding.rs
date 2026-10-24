//! The binding the engine creates for a curve on a component. A build has no
//! object to bind to: the engine derives the binding from the class and the
//! attribute alone, by asking the custom bindings registered for the class
//! and its base classes. The rules below are those of the pinned engine.
use super::{crc32, humanoid, unsupported};
use anyhow::Result;

/// The custom type of a binding that drives a curve of the humanoid rig.
pub(super) const HUMAN_CURVE: u8 = 8;

const TRANSFORM: i64 = 4;
const CAMERA: i64 = 20;
const MESH_FILTER: i64 = 33;
const ANIMATOR: i64 = 95;
const TRAIL_RENDERER: i64 = 96;
const LIGHT: i64 = 108;
pub(super) const MONO_BEHAVIOUR: i64 = 114;
const LINE_RENDERER: i64 = 120;
const SKINNED_MESH_RENDERER: i64 = 137;
const SPRITE_RENDERER: i64 = 212;
const RECT_TRANSFORM: i64 = 224;

/// `Renderer` and the classes derived from it, except the two below.
const RENDERERS: [i64; 13] = [
    23, 25, 96, 120, 137, 199, 212, 227, 331, 73398921, 483693784, 1120581460, 1971053207,
];

/// Classes with a custom binding whose attribute encoding is not reproduced:
/// the particle system and its force field, the six constraints, the visual
/// effect and the two UI Toolkit renderers.
const UNSUPPORTED: [i64; 11] = [
    198, 330, 895512359, 1183024399, 1773428102, 1818360608, 1818360609, 1818360610, 2083052967,
    1931382933, 1931382934,
];

const LIGHT_PROPERTIES: [&str; 17] = [
    "m_Color.r",
    "m_Color.g",
    "m_Color.b",
    "m_Color.a",
    "m_CookieSize2D.x",
    "m_CookieSize2D.y",
    "m_DrawHalo",
    "m_Intensity",
    "m_BounceIntensity",
    "m_Range",
    "m_Shadows.m_Strength",
    "m_Shadows.m_Bias",
    "m_Shadows.m_NormalBias",
    "m_Shadows.m_NearPlane",
    "m_SpotAngle",
    "m_InnerSpotAngle",
    "m_ColorTemperature",
];

const RECT_TRANSFORM_PROPERTIES: [&str; 11] = [
    "m_LocalPosition.z",
    "m_AnchoredPosition.x",
    "m_AnchoredPosition.y",
    "m_AnchorMin.x",
    "m_AnchorMin.y",
    "m_AnchorMax.x",
    "m_AnchorMax.y",
    "m_SizeDelta.x",
    "m_SizeDelta.y",
    "m_Pivot.x",
    "m_Pivot.y",
];

/// What the engine makes of a curve's class and attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Generic {
    pub attribute: u32,
    pub custom_type: u8,
}

/// The binding of a float curve, or `None` where the engine leaves the curve
/// out of the muscle clip: on a `Transform`, whose properties only the
/// position, rotation and scale curves animate. A class the engine does not
/// know is bound like one without custom bindings.
pub(super) fn float(class_id: i64, attribute: &str) -> Result<Option<Generic>> {
    supported(class_id, attribute)?;
    if class_id == TRANSFORM {
        return Ok(None);
    }
    let hash = crc32(attribute);
    let bound = |custom_type| Generic {
        attribute: hash,
        custom_type,
    };
    // An Animator float named exactly like a curve of the humanoid rig
    // drives that curve, whatever its path; any other name is a parameter.
    if class_id == ANIMATOR {
        return Ok(Some(match humanoid::curve_index(hash) {
            Some(index) => Generic {
                attribute: index,
                custom_type: HUMAN_CURVE,
            },
            None => bound(0),
        }));
    }
    if RENDERERS.contains(&class_id) {
        if let Some(attribute) = material_property(attribute) {
            return Ok(Some(Generic {
                attribute,
                custom_type: 22,
            }));
        }
        if matches!(attribute, "m_ReceiveShadows" | "m_SortingOrder") {
            return Ok(Some(bound(26)));
        }
    }
    let width = attribute == "m_Parameters.widthMultiplier";
    Ok(Some(match class_id {
        // The name of a blend shape is hashed without its prefix.
        SKINNED_MESH_RENDERER | SPRITE_RENDERER if attribute.starts_with("blendShape.") => {
            Generic {
                attribute: crc32(&attribute["blendShape.".len()..]),
                custom_type: if class_id == SPRITE_RENDERER { 23 } else { 20 },
            }
        }
        MONO_BEHAVIOUR if attribute == "m_Enabled" => bound(24),
        LIGHT if LIGHT_PROPERTIES.contains(&attribute) => bound(25),
        RECT_TRANSFORM if RECT_TRANSFORM_PROPERTIES.contains(&attribute) => bound(28),
        LINE_RENDERER if width => bound(29),
        TRAIL_RENDERER if width => bound(30),
        CAMERA if matches!(attribute, "field of view" | "m_FocalLength") => bound(37),
        _ => bound(0),
    }))
}

/// The binding of an object reference curve.
pub(super) fn object(class_id: i64, attribute: &str) -> Result<Generic> {
    supported(class_id, attribute)?;
    let bound = |attribute, custom_type| {
        Ok(Generic {
            attribute,
            custom_type,
        })
    };
    if RENDERERS.contains(&class_id)
        && let Some(index) = material_index(attribute)?
    {
        return bound(index, 21);
    }
    match (class_id, attribute) {
        (SPRITE_RENDERER, "m_Sprite") => bound(0, 23),
        (MESH_FILTER, "m_Mesh") => bound(crc32(attribute), 41),
        _ => bound(crc32(attribute), 0),
    }
}

fn supported(class_id: i64, attribute: &str) -> Result<()> {
    // A class ID outside the range of a type has no reference to compare.
    if UNSUPPORTED.contains(&class_id) || !(0..=i32::MAX as i64).contains(&class_id) {
        return Err(unsupported(format!(
            "curve binding of class {class_id}: {attribute}"
        )));
    }
    Ok(())
}

/// The attribute of `material.<property>` or `material.<property>.<channel>`:
/// 28 bits of the property name's hash under the channel and, for a property
/// without a channel, the top bit.
fn material_property(attribute: &str) -> Option<u32> {
    let name = attribute.strip_prefix("material.")?;
    let Some((property, channel)) = name.split_once('.') else {
        return Some(crc32(name) & 0x0fff_ffff | 0x8000_0000);
    };
    let hash = crc32(property) & 0x0fff_ffff;
    // A colour channel carries one more bit than the same vector channel.
    Some(match channel {
        "x" => hash,
        "y" => hash | 0x1000_0000,
        "z" => hash | 0x2000_0000,
        "w" => hash | 0x3000_0000,
        "r" => hash | 0x4000_0000,
        "g" => hash | 0x5000_0000,
        "b" => hash | 0x6000_0000,
        "a" => hash | 0x7000_0000,
        _ => return None,
    })
}

/// The index of `m_Materials.Array.data[<index>]`.
fn material_index(attribute: &str) -> Result<Option<u32>> {
    let Some(rest) = attribute.strip_prefix("m_Materials.Array.data[") else {
        return Ok(None);
    };
    let digits = rest.strip_suffix(']').unwrap_or("");
    match digits.parse::<u32>() {
        Ok(index) if digits.bytes().all(|byte| byte.is_ascii_digit()) && index < 1 << 31 => {
            Ok(Some(index))
        }
        // The engine reads whatever number the text starts with.
        _ => Err(unsupported(format!(
            "object reference curve {attribute} with a malformed index"
        ))),
    }
}
