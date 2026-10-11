//! What the engine corrects after it has read a native-format source. The
//! Editor runs a class's `CheckConsistency` on the loaded object before a
//! build writes it, so the Player holds the corrected values. Each function
//! restates one of them from the pinned Editor, in its order.
use super::{
    native::NativeSources,
    source::{get, text},
    typetree::Val,
    unsupported,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// Corrects a transcoded object. The source fields are at hand for what the
/// engine derives from a field the Player does not keep.
pub(super) type Settle = fn(&Map<String, Value>, &mut Val, &dyn NativeSources) -> Result<()>;

/// For a class the engine reads without correcting anything.
pub(super) fn as_authored(
    _: &Map<String, Value>,
    _: &mut Val,
    _: &dyn NativeSources,
) -> Result<()> {
    Ok(())
}

fn int(value: &Val, name: &str) -> Result<i64> {
    Ok(match value.field(name) {
        Some(Val::I(number)) => *number,
        Some(Val::U(number)) => *number as i64,
        Some(Val::Bool(flag)) => *flag as i64,
        _ => bail!("{name} is not an integer field"),
    })
}

fn real(value: &Val, name: &str) -> Result<f32> {
    match value.field(name) {
        Some(Val::F(number)) => Ok(*number),
        _ => bail!("{name} is not a float field"),
    }
}

fn set(value: &mut Val, name: &str, new: Val) -> Result<()> {
    match value.field_mut(name) {
        Some(field) => {
            *field = new;
            Ok(())
        }
        None => bail!("{name} is not a field"),
    }
}

/// The least below it, the most above it. Negative zero is not below zero.
fn within(number: f32, least: f32, most: f32) -> f32 {
    if number < least {
        least
    } else if most < number {
        most
    } else {
        number
    }
}

fn range(value: &mut Val, name: &str, least: f32, most: f32) -> Result<()> {
    let number = within(real(value, name)?, least, most);
    set(value, name, Val::F(number))
}

fn limit(value: &mut Val, name: &str, most: f32) -> Result<()> {
    range(value, name, 0.0, most)
}

pub(super) fn physics_material(
    _: &Map<String, Value>,
    value: &mut Val,
    _: &dyn NativeSources,
) -> Result<()> {
    // One step below the largest single, as the engine has it.
    let most = f32::from_bits(0x7f7f_fffe);
    limit(value, "m_DynamicFriction", most)?;
    limit(value, "m_StaticFriction", most)?;
    limit(value, "m_Bounciness", 1.0)
}

pub(super) fn physics_material_2d(
    _: &Map<String, Value>,
    value: &mut Val,
    _: &dyn NativeSources,
) -> Result<()> {
    limit(value, "friction", 100_000.0)?;
    limit(value, "bounciness", 100_000.0)?;
    // An unknown way of combining falls back to the default of its kind.
    for (name, fallback) in [("m_FrictionCombine", 1), ("m_BounceCombine", 4)] {
        if !(0..5).contains(&int(value, name)?) {
            set(value, name, Val::I(fallback))?;
        }
    }
    Ok(())
}

/// `GraphicsFormat` values with a depth component.
const DEPTH_FORMATS: [i64; 6] = [90, 91, 92, 93, 94, 151];
/// `GraphicsFormat` values the engine resolves when it creates the texture.
const AUTO_FORMATS: std::ops::Range<i64> = 142..145;
const FORMATS: std::ops::Range<i64> = 0..152;
/// Each sRGB `GraphicsFormat` and its linear counterpart.
const SRGB_FORMATS: [(i64, i64); 27] = [
    (1, 5),
    (2, 6),
    (3, 7),
    (4, 8),
    (56, 58),
    (57, 59),
    (81, 82),
    (83, 84),
    (85, 86),
    (87, 88),
    (96, 97),
    (98, 99),
    (100, 101),
    (108, 109),
    (110, 111),
    (112, 113),
    (114, 115),
    (116, 117),
    (119, 120),
    (121, 122),
    (123, 124),
    (129, 130),
    (131, 132),
    (133, 134),
    (135, 136),
    (137, 138),
    (139, 140),
];
/// A linear format the engine pairs with the sRGB format of its neighbour.
const ONE_WAY_SRGB: (i64, i64) = (118, 119);
const DIMENSION_3D: i64 = 3;
const DIMENSION_CUBE: i64 = 4;
const NO_SHADOW_SAMPLING: i64 = 2;

fn linear_format(format: i64) -> i64 {
    SRGB_FORMATS
        .iter()
        .find(|pair| pair.0 == format)
        .map_or(format, |pair| pair.1)
}

fn srgb_format(format: i64) -> i64 {
    if format == ONE_WAY_SRGB.0 {
        return ONE_WAY_SRGB.1;
    }
    SRGB_FORMATS
        .iter()
        .find(|pair| pair.1 == format)
        .map_or(format, |pair| pair.0)
}

/// The engine reads a color format as the sRGB one in a project that works
/// in linear color space when the texture is marked sRGB, as the linear one
/// otherwise.
fn color_format(value: &Val, sources: &dyn NativeSources) -> Result<i64> {
    let format = int(value, "m_ColorFormat")?;
    if AUTO_FORMATS.contains(&format) {
        return Ok(format);
    }
    if !FORMATS.contains(&format) {
        return Ok(0);
    }
    let (linear, srgb) = (linear_format(format), srgb_format(format));
    if linear == srgb || int(value, "m_SRGB")? == 0 {
        return Ok(linear);
    }
    match sources.linear_color_space() {
        Some(true) => Ok(srgb),
        Some(false) => Ok(linear),
        None => Err(unsupported(
            "sRGB render texture in a project whose color space is not known",
        )),
    }
}

pub(super) fn render_texture(
    _: &Map<String, Value>,
    value: &mut Val,
    sources: &dyn NativeSources,
) -> Result<()> {
    let color = color_format(value, sources)?;
    set(value, "m_ColorFormat", Val::I(color))?;
    let Some(settings) = value.field_mut("m_TextureSettings") else {
        bail!("m_TextureSettings is not a field");
    };
    for (name, most) in [
        ("m_FilterMode", 2),
        ("m_WrapU", 3),
        ("m_WrapV", 3),
        ("m_WrapW", 3),
    ] {
        let number = int(settings, name)?.clamp(0, most);
        set(settings, name, Val::I(number))?;
    }
    for name in ["m_Width", "m_Height", "m_VolumeDepth"] {
        let number = int(value, name)?.max(1);
        set(value, name, Val::I(number))?;
    }
    let samples = int(value, "m_AntiAliasing")?.clamp(1, 32);
    set(value, "m_AntiAliasing", Val::I(samples))?;
    let mut depth = int(value, "m_DepthStencilFormat")?;
    if color == 0 && DEPTH_FORMATS.contains(&depth) {
        set(value, "m_MipMap", Val::Bool(false))?;
    }
    let dimension = int(value, "m_Dimension")?;
    if dimension == DIMENSION_CUBE {
        let width = int(value, "m_Width")?;
        set(value, "m_Height", Val::I(width))?;
    }
    if int(value, "m_MipCount")? == 0 {
        set(value, "m_MipCount", Val::I(-1))?;
    }
    if dimension == DIMENSION_3D && depth != 0 {
        depth = 0;
        set(value, "m_DepthStencilFormat", Val::I(0))?;
    }
    if color != 0 && depth == 0 {
        set(value, "m_ShadowSamplingMode", Val::I(NO_SHADOW_SAMPLING))
    } else {
        // A texture with a depth buffer is never filtered anisotropically.
        let Some(settings) = value.field_mut("m_TextureSettings") else {
            bail!("m_TextureSettings is not a field");
        };
        set(settings, "m_Aniso", Val::I(0))
    }
}

pub(super) fn custom_render_texture(
    fields: &Map<String, Value>,
    value: &mut Val,
    sources: &dyn NativeSources,
) -> Result<()> {
    render_texture(fields, value, sources)?;
    let period = real(value, "m_UpdatePeriod")?;
    set(
        value,
        "m_UpdatePeriod",
        Val::F(if period > 0.0 { period } else { 0.0 }),
    )?;
    if int(value, "m_UpdateZoneSpace")? != int(value, "m_CurrentUpdateZoneSpace")? {
        return Err(unsupported(
            "custom render texture whose update zones are stored in another space than it uses",
        ));
    }
    Ok(())
}

pub(super) fn lighting_settings(
    fields: &Map<String, Value>,
    value: &mut Val,
    sources: &dyn NativeSources,
) -> Result<()> {
    limit(value, "m_BounceScale", 10.0)?;
    range(value, "m_AlbedoBoost", 1.0, 10.0)?;
    limit(value, "m_IndirectOutputScale", 5.0)?;
    // The Player keeps whether mixed lights use a shadowmask, not the mode.
    let Ok(mode) = text(get(fields, "m_MixedBakeMode"))?.parse::<i64>() else {
        return Err(unsupported("lighting settings without a mixed bake mode"));
    };
    set(value, "m_UsingShadowmask", Val::Bool(mode == 2))?;
    let baked = int(value, "m_EnableBakedLightmaps")? != 0;
    let realtime = int(value, "m_EnableRealtimeLightmaps")? != 0;
    // A build keeps a kind of lightmap only when the render pipeline of the
    // project lights with it.
    let (baked, realtime) = if baked || realtime {
        let Some(lit) = sources.lightmaps() else {
            return Err(unsupported(
                "lighting settings with lightmaps in a project whose render pipeline is not known",
            ));
        };
        (baked && lit.baked, realtime && lit.realtime)
    } else {
        (baked, realtime)
    };
    let environment = int(value, "m_RealtimeEnvironmentLighting")? != 0;
    set(value, "m_EnableBakedLightmaps", Val::Bool(baked))?;
    set(value, "m_EnableRealtimeLightmaps", Val::Bool(realtime))?;
    set(
        value,
        "m_RealtimeEnvironmentLighting",
        Val::Bool(!baked || (realtime && environment)),
    )
}
