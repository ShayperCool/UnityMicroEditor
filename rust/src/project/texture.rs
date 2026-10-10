//! Raw textures the public decoder profile accepts without a native import.
use super::{
    image_config::decode_config,
    session::{Inner, needs_import},
    value::{self, Map, Value, array, get, number, object},
};
use anyhow::{Result, anyhow, bail};

impl Inner {
    pub(crate) fn refresh_authored_texture(&mut self, asset: &mut Map) -> Result<()> {
        let path = value::text(asset, "path").into_owned();
        let physical = self.source(&path)?;
        self.depend(&path)?;
        let (content, _) = self.read_text(&physical)?;
        let (width, height) =
            decode_config(&content).map_err(|e| e.context(format!("raw texture {path}")))?;
        let (meta, _) = self.read_text(&format!("{physical}.meta"))?;
        let text = std::str::from_utf8(&meta).map_err(|_| anyhow!("yaml: invalid UTF-8"))?;
        let parsed = match crate::yaml::parse_yaml_value(text) {
            Err(error) if error.is::<crate::yaml::EmptyDocument>() => {
                bail!("raw texture {path} has invalid meta")
            }
            other => Value::from_yaml(&other?),
        };
        let importer = object(parsed.field("TextureImporter"));
        let stale = || needs_import(&path);
        if importer.is_empty()
            || number(get(importer, "textureType")) != 0
            || number(get(importer, "textureShape")) != 1
            || number(get(importer, "alphaUsage")) != 1
        {
            return Err(stale());
        }
        for key in [
            "alphaIsTransparency",
            "grayScaleToAlpha",
            "streamingMipmaps",
            "vTOnly",
            "flipGreenChannel",
            "ignorePngGamma",
            "applyGammaDecoding",
        ] {
            if number(get(importer, key)) != 0 {
                return Err(stale());
            }
        }
        if importer
            .get("swizzle")
            .is_some_and(|swizzle| number(swizzle) != 50462976)
        {
            return Err(stale());
        }
        static NONE: Map = Map::new();
        let mut settings = &NONE;
        let platforms = array(get(importer, "platformSettings"));
        for item in platforms {
            let platform = object(item);
            if value::text(platform, "buildTarget") == "DefaultTexturePlatform" {
                settings = platform;
            }
        }
        for item in platforms {
            let platform = object(item);
            if value::text(platform, "buildTarget") == "Standalone"
                && number(get(platform, "overridden")) != 0
            {
                settings = platform;
            }
        }
        if settings.is_empty()
            || number(get(settings, "textureCompression")) != 0
            || number(get(settings, "crunchedCompression")) != 0
            || number(get(settings, "textureFormat")) != -1
        {
            return Err(stale());
        }
        let maximum = number(get(settings, "maxTextureSize"));
        if maximum <= 0
            || width > maximum
            || height > maximum
            || width <= 0
            || height <= 0
            || width * height > 16 * 1024 * 1024
        {
            return Err(stale());
        }
        if number(get(importer, "nPOTScale")) != 0
            && (width & (width - 1) != 0 || height & (height - 1) != 0)
        {
            return Err(stale());
        }
        let mipmap = object(get(importer, "mipmaps"));
        for key in [
            "mipMapMode",
            "fadeOut",
            "borderMipMap",
            "mipMapsPreserveCoverage",
        ] {
            if number(get(mipmap, key)) != 0 {
                return Err(stale());
            }
        }
        let sampler = object(get(importer, "textureSettings"));
        if number(get(sampler, "mipBias")) != 0 {
            return Err(stale());
        }
        let normalized = value::map([
            ("textureType", Value::Int(0)),
            ("textureShape", Value::Int(1)),
            ("sRGBTexture", get(mipmap, "sRGBTexture").clone()),
            ("mipmapEnabled", get(mipmap, "enableMipMap").clone()),
            ("isReadable", get(importer, "isReadable").clone()),
            ("filterMode", get(sampler, "filterMode").clone()),
            ("wrapU", get(sampler, "wrapU").clone()),
            ("wrapV", get(sampler, "wrapV").clone()),
            ("aniso", get(sampler, "aniso").clone()),
        ]);
        asset.insert("fileId".into(), "2800000".into());
        asset.insert("classId".into(), Value::Int(28));
        asset.insert("typeName".into(), "UnityEngine.Texture2D".into());
        let encoded = value::compact_map(&normalized);
        asset.insert(
            "importerJson".into(),
            encoded.as_deref().unwrap_or_default().into(),
        );
        encoded.map(|_| ())
    }
}
