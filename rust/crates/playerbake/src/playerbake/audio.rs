//! An audio file as the importer makes it an `AudioClip`: its samples
//! encoded by the Editor's `FSBTool` (FMOD's sound bank tool, which the
//! Editor runs for each import) into an FSB5 bank that the clip names in the
//! `.resource` file beside the written one, and the importer's settings for
//! the platform. The tool of the installation encodes, so the banks are its
//! own: Vorbis at a quality of 100 gives the worker's bytes.
use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

pub const AUDIO_CLIP_FILE_ID: i64 = 8_300_000;

static TOOL: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The Unity installation (its `Editor` directory) whose `FSBTool` encodes.
pub fn use_editor(editor: &Path) {
    *TOOL.lock().unwrap() = Some(editor.join("Data/Tools/FSBTool/x64"));
}

/// What the importer makes of a clip for one platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Decompress on load (0), compressed in memory (1), streaming (2).
    pub load_type: i64,
    /// PCM (0), Vorbis (1), ADPCM (2).
    pub compression: i64,
    /// From 0 to 1.
    pub quality: f64,
    /// Preserve (0), optimize (1) or override (2) the sample rate.
    pub sample_rate_setting: i64,
    pub sample_rate_override: i64,
    pub force_to_mono: bool,
    pub normalize: bool,
    pub preload: bool,
    pub load_in_background: bool,
    pub ambisonic: bool,
    pub legacy_3d: bool,
}

/// The build target group a standalone Player's settings are kept under.
const STANDALONE: &str = "1";

fn number(fields: &Map<String, Value>, name: &str, default: f64) -> f64 {
    match fields.get(name) {
        Some(Value::String(text)) => text.trim().parse().unwrap_or(default),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        _ => default,
    }
}

/// The importer's settings of a `.meta` for a standalone Player: its
/// override for the platform, else its defaults.
pub fn settings(meta: &[u8]) -> Result<Settings> {
    let importer = super::font::importer_settings(meta, "AudioImporter")?;
    let empty = Map::new();
    let defaults = importer
        .get("defaultSettings")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let sample = importer
        .get("platformSettingOverrides")
        .and_then(Value::as_object)
        .and_then(|overrides| overrides.get(STANDALONE))
        .and_then(Value::as_object)
        .unwrap_or(defaults);
    let flag = |name: &str, default: bool| number(&importer, name, default as i64 as f64) != 0.0;
    Ok(Settings {
        load_type: number(sample, "loadType", 0.0) as i64,
        compression: number(sample, "compressionFormat", 1.0) as i64,
        quality: number(sample, "quality", 1.0),
        sample_rate_setting: number(sample, "sampleRateSetting", 0.0) as i64,
        sample_rate_override: number(sample, "sampleRateOverride", 44100.0) as i64,
        force_to_mono: flag("forceToMono", false),
        normalize: flag("normalize", true),
        preload: flag("preloadAudioData", true),
        load_in_background: flag("loadInBackground", false),
        ambisonic: flag("ambisonic", false),
        legacy_3d: flag("3D", true),
    })
}

/// An encoded clip: the bank and what its header says of the samples.
pub struct Encoded {
    pub data: Vec<u8>,
    pub channels: u32,
    pub frequency: u32,
    pub samples: u64,
}

/// Encodes an audio file as the importer does for `settings`.
pub fn encode(path: &Path, settings: &Settings) -> Result<Encoded> {
    let tool = TOOL
        .lock()
        .unwrap()
        .clone()
        .context("no Unity installation to take FSBTool from")?;
    let format = match settings.compression {
        0 => "pcm",
        1 => "vorbis",
        2 => "adpcm",
        other => bail!("compression format {other} is left to the worker"),
    };
    let scratch = tempfile::tempdir()?;
    let output = scratch.path().join("clip.fsb");
    let cache = scratch.path().join("cache");
    std::fs::create_dir_all(&cache)?;
    let quality = (settings.quality * 100.0).round().clamp(0.0, 100.0) as i64;
    let mut command = Command::new(tool.join("FSBTool"));
    command
        .env("LD_LIBRARY_PATH", &tool)
        .arg("-i")
        .arg(path)
        .arg("-o")
        .arg(&output)
        .arg("-l")
        .arg(&tool)
        .arg("-h")
        .arg(&cache)
        .args(["-C", "fsb", "-c", format, "-q", &quality.to_string()]);
    if settings.force_to_mono {
        command.arg("-m");
        if settings.normalize {
            command.arg("-n");
        }
    }
    match settings.sample_rate_setting {
        1 => {
            command.arg("-r");
        }
        2 => {
            command.args(["-s", &settings.sample_rate_override.to_string()]);
        }
        _ => {}
    }
    let run = command.output().context("run FSBTool")?;
    if !run.status.success() {
        bail!(
            "FSBTool failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&run.stderr).trim()
        );
    }
    let data = std::fs::read(&output).context("FSBTool wrote no bank")?;
    let (channels, frequency, samples) = header(&data)?;
    Ok(Encoded {
        data,
        channels,
        frequency,
        samples,
    })
}

/// The channels, frequency and sample count of an FSB5 bank's one sample.
fn header(data: &[u8]) -> Result<(u32, u32, u64)> {
    let u32_at = |at: usize| -> Result<u32> {
        Ok(u32::from_le_bytes(
            data.get(at..at + 4).context("bank cut short")?.try_into()?,
        ))
    };
    if !data.starts_with(b"FSB5") {
        bail!("FSBTool wrote no FSB5 bank");
    }
    if u32_at(8)? != 1 {
        bail!("an FSB5 bank of more than one sample");
    }
    let start = match u32_at(4)? {
        0 => 0x40,
        _ => 0x3c,
    };
    let raw = u64::from_le_bytes(
        data.get(start..start + 8)
            .context("bank cut short")?
            .try_into()?,
    );
    let mut frequency = match (raw >> 1) & 0xf {
        1 => 8000,
        2 => 11000,
        3 => 11025,
        4 => 16000,
        5 => 22050,
        6 => 24000,
        7 => 32000,
        8 => 44100,
        9 => 48000,
        10 => 96000,
        _ => 0,
    };
    let mut channels = ((raw >> 5) & 1) as u32 + 1;
    let samples = (raw >> 34) & 0x3fff_ffff;
    // Chunks after the sample header give what the header cannot.
    let mut more = raw & 1 != 0;
    let mut at = start + 8;
    while more {
        let chunk = u32_at(at)?;
        more = chunk & 1 != 0;
        let size = ((chunk >> 1) & 0xff_ffff) as usize;
        match chunk >> 25 {
            1 => channels = *data.get(at + 4).context("bank cut short")? as u32,
            2 => frequency = u32_at(at + 4)?,
            _ => {}
        }
        at += 4 + size;
    }
    if frequency == 0 {
        bail!("an FSB5 sample of an unknown frequency");
    }
    Ok((channels, frequency, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bank_header_gives_the_clip_s_samples() {
        // The header of a mono 44.1 kHz Vorbis bank FSBTool wrote.
        let mut bank = b"FSB5".to_vec();
        for word in [1u32, 1, 0x24, 0, 0x1ea0, 15, 1, 0] {
            bank.extend_from_slice(&word.to_le_bytes());
        }
        bank.extend_from_slice(&[0; 24]);
        bank.extend_from_slice(&0x0002_324c_0000_0011u64.to_le_bytes());
        // The chunk flagged after the header: Vorbis data (type 11) of 8 bytes.
        bank.extend_from_slice(&0x1600_0010u32.to_le_bytes());
        bank.extend_from_slice(&[0; 8]);
        let (channels, frequency, samples) = header(&bank).unwrap();
        assert_eq!((channels, frequency, samples), (1, 44100, 35987));
        assert_eq!(
            (samples as f32 / frequency as f32).to_string(),
            "0.81603175"
        );
    }

    #[test]
    fn a_platform_override_comes_before_the_defaults() {
        let meta = b"fileFormatVersion: 2
guid: 1bf70e901a36e4eda903d686ae76b5ac
AudioImporter:
  serializedVersion: 6
  defaultSettings:
    loadType: 0
    sampleRateSetting: 0
    sampleRateOverride: 44100
    compressionFormat: 1
    quality: 1
    conversionMode: 0
  platformSettingOverrides:
    4:
      loadType: 0
      compressionFormat: 2
      quality: 1
    1:
      loadType: 2
      compressionFormat: 0
      quality: 0.5
  forceToMono: 1
  normalize: 1
  preloadAudioData: 1
  loadInBackground: 0
  ambisonic: 0
  3D: 1
";
        let settings = settings(meta).unwrap();
        assert_eq!(settings.load_type, 2);
        assert_eq!(settings.compression, 0);
        assert_eq!(settings.quality, 0.5);
        assert!(settings.force_to_mono && settings.normalize && settings.legacy_3d);
    }
}
