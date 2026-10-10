//! `parity-compare-png` measures screenshot differences; it does not modify either image.
use super::image::Image;
use crate::flags::FlagSet;
use serde::Serialize;

const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PIXELS: u64 = 16_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metrics {
    pixels: u64,
    changed_pixels: u64,
    max_channel_difference: u32,
    mean_absolute_channel_difference: f64,
}

#[derive(Serialize)]
struct Report<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    reference: &'a str,
    actual: &'a str,
    #[serde(rename = "referenceSHA256")]
    reference_sha256: String,
    #[serde(rename = "actualSHA256")]
    actual_sha256: String,
    width: u32,
    height: u32,
    #[serde(rename = "fullFrame")]
    full_frame: Metrics,
    #[serde(rename = "ignoredBottomRows")]
    ignored_bottom_rows: u32,
    #[serde(
        rename = "viewportWithoutBottomRows",
        skip_serializing_if = "Option::is_none"
    )]
    viewport_without_bottom_rows: Option<Metrics>,
}

/// Compares the top `rows` rows of two images of the same width, per 8-bit channel.
fn measure(reference: &Image, actual: &Image, rows: u32) -> Metrics {
    let pixels = u64::from(reference.width) * u64::from(rows);
    let mut result = Metrics {
        pixels,
        changed_pixels: 0,
        max_channel_difference: 0,
        mean_absolute_channel_difference: 0.0,
    };
    let mut total = 0u64;
    for y in 0..rows {
        for x in 0..reference.width {
            let (a, b) = (reference.rgba(x, y), actual.rgba(x, y));
            let mut changed = false;
            for channel in 0..4 {
                let difference = (a[channel] >> 8).abs_diff(b[channel] >> 8);
                total += u64::from(difference);
                result.max_channel_difference = result.max_channel_difference.max(difference);
                changed |= difference != 0;
            }
            result.changed_pixels += u64::from(changed);
        }
    }
    if pixels != 0 {
        result.mean_absolute_channel_difference = total as f64 / (4 * pixels) as f64;
    }
    result
}

fn read(path: &str) -> Result<(Image, String), String> {
    let data = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    if data.len() > MAX_FILE_BYTES {
        return Err("PNG exceeds 32 MiB".into());
    }
    let image =
        Image::decode(&data, Some(MAX_PIXELS)).map_err(|error| format!("{path}: {error}"))?;
    Ok((image, crate::sha256(&data)))
}

fn run(flags: &FlagSet) -> Result<(), String> {
    let (reference_path, actual_path) = (flags.get_string("reference"), flags.get_string("actual"));
    let (reference, reference_sha256) = read(&reference_path)?;
    let (actual, actual_sha256) = read(&actual_path)?;
    if (reference.width, reference.height) != (actual.width, actual.height) {
        return Err(
            "screenshots have different dimensions; capture the same dimensions before comparing"
                .into(),
        );
    }
    let ignored = u32::try_from(flags.get_int("ignore-bottom"))
        .ok()
        .filter(|rows| *rows < reference.height)
        .ok_or("invalid ignore-bottom row count")?;
    let report = Report {
        schema_version: 1,
        reference: &reference_path,
        actual: &actual_path,
        reference_sha256,
        actual_sha256,
        width: reference.width,
        height: reference.height,
        full_frame: measure(&reference, &actual, reference.height),
        ignored_bottom_rows: ignored,
        viewport_without_bottom_rows: (ignored > 0)
            .then(|| measure(&reference, &actual, reference.height - ignored)),
    };
    super::emit(&flags.get_string("output"), &super::pretty(&report)?)
        .map_err(|error| error.to_string())
}

/// Exit status 0 after a measurement, 1 when the images cannot be measured.
pub fn main(args: Vec<String>) -> i32 {
    let mut flags = FlagSet::new("unity-micro-editor parity-compare-png");
    flags.string("reference", "", "Editor PNG");
    flags.string("actual", "", "Player PNG");
    flags.int(
        "ignore-bottom",
        0,
        "explicit bottom rows excluded from an additional viewport metric; full-frame differences remain reported",
    );
    flags.string("output", "", "optional report path");
    if let Err(status) = super::parse(&mut flags, args) {
        return status;
    }
    match run(&flags) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_frame_always_reports_excluded_overlay() {
        let black = [0, 0, 0, 255];
        let reference = Image::from_rgba8(2, 2, black.repeat(4));
        let mut pixels = black.repeat(4);
        pixels[12] = 100; // red of the pixel at (1, 1)
        let actual = Image::from_rgba8(2, 2, pixels);
        let full = measure(&reference, &actual, 2);
        let viewport = measure(&reference, &actual, 1);
        assert_eq!(
            full,
            Metrics {
                pixels: 4,
                changed_pixels: 1,
                max_channel_difference: 100,
                mean_absolute_channel_difference: 6.25,
            }
        );
        assert_eq!(viewport.changed_pixels, 0);
        assert_eq!(viewport.pixels, 2);
    }

    #[test]
    fn transparent_pixels_compare_by_visible_color() {
        let reference = Image::from_rgba8(1, 1, vec![255, 0, 0, 0]);
        let actual = Image::from_rgba8(1, 1, vec![0, 255, 0, 0]);
        assert_eq!(measure(&reference, &actual, 1).changed_pixels, 0);
        let half = Image::from_rgba8(1, 1, vec![255, 255, 255, 128]);
        assert_eq!(half.rgba(0, 0), [0x8080, 0x8080, 0x8080, 0x8080]);
    }
}
