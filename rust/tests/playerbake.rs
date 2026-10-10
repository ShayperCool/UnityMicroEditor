//! Compares baked Player objects with the pinned engine's own build output.
//! Each `<name>.anim` fixture is authored here; `<name>.clip.bin` is the
//! `AnimationClip` object Unity 6000.6.3f1 wrote for it into an AssetBundle.
use std::{
    fs,
    path::{Path, PathBuf},
};
use unity_micro_editor::{
    playerbake::{self, Unsupported},
    yaml::parse_unity_yaml_lexical,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("rust/tests/testdata/playerbake")
        .join(name)
}

fn bake(name: &str) -> anyhow::Result<Vec<u8>> {
    let source = fs::read(fixture(&format!("{name}.anim"))).unwrap();
    let documents = parse_unity_yaml_lexical(&source).unwrap();
    playerbake::bake_clip_object(&documents[0].data)
}

fn assert_matches_engine(name: &str) {
    let expected = fs::read(fixture(&format!("{name}.clip.bin"))).unwrap();
    let actual = bake(name).unwrap_or_else(|error| panic!("{name}: {error:#}"));
    if actual != expected {
        let at = actual
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(actual.len().min(expected.len()));
        panic!(
            "{name}: baked {} bytes, engine wrote {}; first difference at {at}: {:02x?} vs {:02x?}",
            actual.len(),
            expected.len(),
            &actual[at.min(actual.len())..(at + 16).min(actual.len())],
            &expected[at.min(expected.len())..(at + 16).min(expected.len())],
        );
    }
}

/// Fixtures whose features the baker refuses instead of approximating.
const REFUSED: [&str; 3] = ["Mixed", "Ordering", "Tangents"];

#[test]
fn clips_match_the_engine_build() {
    let mut names: Vec<String> = fs::read_dir(fixture(""))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "anim")
        })
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .filter(|name| !REFUSED.contains(&name.as_str()))
        .collect();
    names.sort();
    assert!(names.len() >= 28, "fixtures are missing: {names:?}");
    for name in names {
        assert_matches_engine(&name);
    }
}

#[test]
fn unreproduced_features_are_refused() {
    for name in REFUSED {
        let error = bake(name).expect_err(name);
        assert!(error.is::<Unsupported>(), "{name}: {error:#}");
    }
}

#[test]
fn baked_file_is_a_content_file_with_the_authored_identity() {
    let source = fs::read(fixture("PosSlopes.anim")).unwrap();
    let baked = playerbake::bake_clip(&source).unwrap();
    assert_eq!(
        baked.objects,
        [(
            "7400000".to_string(),
            "UnityEngine.AnimationClip".to_string()
        )]
    );
    assert!(baked.externals.is_empty());
    // Version 23, little-endian, and sizes that describe the whole file.
    assert_eq!(&baked.file[8..12], &23u32.to_be_bytes());
    assert_eq!(
        u64::from_be_bytes(baked.file[24..32].try_into().unwrap()),
        baked.file.len() as u64
    );
}

/// Compares every `<name>.anim` / `<name>.clip.bin` pair of a directory built
/// from a real project. Refused clips are counted, mismatches fail the test.
#[test]
fn clip_corpus_matches_the_engine_build() {
    let Some(roots) = std::env::var("UNITY_PLAYERBAKE_CLIP_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_CLIP_CORPUS");
        return;
    };
    let (mut matched, mut mismatched) = (0, Vec::new());
    let mut refused: std::collections::BTreeMap<String, usize> = Default::default();
    for root in roots.split(':') {
        let mut sources: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "anim")
            })
            .collect();
        sources.sort();
        for source in sources {
            let expected = fs::read(source.with_extension("clip.bin")).unwrap();
            let Ok(documents) = parse_unity_yaml_lexical(&fs::read(&source).unwrap()) else {
                *refused
                    .entry("source exceeds the text bound".into())
                    .or_default() += 1;
                continue;
            };
            match playerbake::bake_clip_object(&documents[0].data) {
                Ok(actual) if actual == expected => matched += 1,
                Ok(actual) => {
                    if let Ok(directory) = std::env::var("UNITY_PLAYERBAKE_DUMP") {
                        let name = source.with_extension("baked.bin");
                        fs::write(
                            Path::new(&directory).join(name.file_name().unwrap()),
                            &actual,
                        )
                        .unwrap();
                    }
                    let at = actual
                        .iter()
                        .zip(&expected)
                        .position(|(a, b)| a != b)
                        .unwrap_or(actual.len().min(expected.len()));
                    mismatched.push(format!(
                        "{}: {} vs {} bytes, first difference at {at}",
                        source.display(),
                        actual.len(),
                        expected.len()
                    ));
                }
                Err(error) if error.is::<Unsupported>() => {
                    let reason: String = error
                        .to_string()
                        .split([':', '.'])
                        .next()
                        .unwrap()
                        .chars()
                        .filter(|c| !c.is_ascii_digit())
                        .collect();
                    *refused.entry(reason).or_default() += 1;
                }
                Err(error) => mismatched.push(format!("{}: {error:#}", source.display())),
            }
        }
    }
    eprintln!("matched {matched}, refused {refused:#?}");
    assert!(mismatched.is_empty(), "{}", mismatched.join("\n"));
}
