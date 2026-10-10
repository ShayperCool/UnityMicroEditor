//! Compares baked Player objects with the pinned engine's own build output.
//! Each `<name>.anim` fixture is authored here; `<name>.clip.bin` is the
//! `AnimationClip` object Unity 6000.6.3f1 wrote for it into an AssetBundle.
use std::{
    fs,
    path::{Path, PathBuf},
};
use unity_micro_editor::{
    playerbake::{self, Unsupported, Val},
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
                    eprintln!("refused {}: {error}", source.display());
                    *refused.entry(reason).or_default() += 1;
                }
                Err(error) => mismatched.push(format!("{}: {error:#}", source.display())),
            }
        }
    }
    eprintln!("matched {matched}, refused {refused:#?}");
    assert!(mismatched.is_empty(), "{}", mismatched.join("\n"));
}

/// Sources of a directory tree, found by the GUID in their `.meta` files.
struct Tree(std::collections::HashMap<String, (PathBuf, String)>);

impl Tree {
    fn scan(roots: &[PathBuf]) -> Self {
        let mut found = std::collections::HashMap::new();
        // A root stands for the project's `Assets` directory.
        let mut pending: Vec<(PathBuf, String)> = roots
            .iter()
            .map(|root| (root.clone(), "Assets".to_string()))
            .collect();
        while let Some((directory, logical)) = pending.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    pending.push((path, format!("{logical}/{name}")));
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "meta")
                    && let Some(guid) =
                        unity_micro_editor::yaml::metadata_guid(&fs::read(&path).unwrap())
                {
                    let source = name.strip_suffix(".meta").unwrap();
                    found.insert(
                        guid,
                        (path.with_extension(""), format!("{logical}/{source}")),
                    );
                }
            }
        }
        Self(found)
    }
    fn read(&self, guid: &str) -> anyhow::Result<(Vec<u8>, &str)> {
        match self.0.get(guid) {
            Some((path, logical)) => Ok((fs::read(path)?, logical)),
            None => Err(
                Unsupported(format!("reference to a source outside the fixture: {guid}")).into(),
            ),
        }
    }
}

impl playerbake::ControllerSources for Tree {
    fn clip(&self, guid: &str, _file_id: &str) -> anyhow::Result<playerbake::ClipFacts> {
        let (source, path) = self.read(guid)?;
        playerbake::read_clip_facts(&source, path)
    }
    fn mask(&self, guid: &str) -> anyhow::Result<playerbake::Mask> {
        playerbake::read_mask(&self.read(guid)?.0)
    }
}

/// Puts the parts of a controller that have no fixed order or identity into
/// a comparable form: the name table by hash, clip references by first use.
fn normalize(mut value: Val) -> Val {
    // Compared separately: the baker reserves an upper bound.
    if let Some(size) = value.field_mut("m_ControllerSize") {
        *size = Val::Zero;
    }
    if let Some(Val::Seq(names)) = value.field_mut("m_TOS") {
        names.sort_by_key(|pair| match pair.field("first") {
            Some(Val::U(hash)) => *hash,
            _ => 0,
        });
    }
    if let Some(Val::Seq(clips)) = value.field_mut("m_AnimationClips") {
        let mut seen: Vec<Val> = Vec::new();
        for clip in clips.iter_mut() {
            let position = seen
                .iter()
                .position(|known| known == clip)
                .unwrap_or_else(|| {
                    seen.push(clip.clone());
                    seen.len() - 1
                });
            *clip = Val::U(position as u64);
        }
    }
    value
}

fn differences(path: &str, actual: &Val, expected: &Val, out: &mut Vec<String>) {
    if out.len() >= 40 {
        return;
    }
    match (actual, expected) {
        (Val::Map(a), Val::Map(b)) => {
            for ((name, a), (_, b)) in a.iter().zip(b) {
                differences(&format!("{path}.{name}"), a, b, out);
            }
        }
        (Val::Seq(a), Val::Seq(b)) if a.len() == b.len() => {
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                differences(&format!("{path}[{index}]"), a, b, out);
            }
        }
        (Val::Seq(a), Val::Seq(b)) => {
            out.push(format!("{path}: {} items, engine has {}", a.len(), b.len()));
        }
        (Val::F(a), Val::F(b)) if a.to_bits() == b.to_bits() => {}
        (a, b) if a == b && !matches!(a, Val::F(_)) => {}
        (a, b) => out.push(format!("{path}: {a:?}, engine has {b:?}")),
    }
}

/// Bakes every controller of the roots and compares it with the engine's
/// object beside it. Returns how many matched and which were refused.
fn compare_controllers(roots: &[PathBuf], tree: &Tree) -> (usize, Vec<String>) {
    let (mut matched, mut failed, mut refused) = (0, 0, Vec::new());
    for root in roots {
        let mut controllers: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "controller")
            })
            .collect();
        controllers.sort();
        for source in controllers {
            let expected = fs::read(source.with_extension("controller.bin")).unwrap();
            let documents = parse_unity_yaml_lexical(&fs::read(&source).unwrap()).unwrap();
            let name = source.file_stem().unwrap().to_string_lossy().into_owned();
            match playerbake::bake_controller_object(&documents, tree) {
                Ok((actual, _)) => {
                    let layout = playerbake::controller_tree().unwrap();
                    let (actual, expected) = (
                        layout.decode(&actual).unwrap(),
                        layout.decode(&expected).unwrap(),
                    );
                    let size = |value: &Val| match value.field("m_ControllerSize") {
                        Some(Val::U(size)) => *size,
                        _ => panic!("controller has no size"),
                    };
                    let mut out = Vec::new();
                    if size(&actual) < size(&expected)
                        || (size(&expected) == 0) != (size(&actual) == 0)
                    {
                        out.push(format!(
                            "reserves {}, engine needs {}",
                            size(&actual),
                            size(&expected)
                        ));
                    }
                    let (actual, expected) = (normalize(actual), normalize(expected));
                    differences("", &actual, &expected, &mut out);
                    if out.is_empty() {
                        matched += 1;
                    } else {
                        failed += 1;
                        eprintln!("MISMATCH {name}:\n  {}", out.join("\n  "));
                    }
                }
                Err(error) if error.is::<Unsupported>() => {
                    eprintln!("refused {name}: {error}");
                    refused.push(name);
                }
                Err(error) => {
                    failed += 1;
                    eprintln!("ERROR {name}: {error:#}");
                }
            }
        }
    }
    eprintln!("matched {matched}");
    assert_eq!(failed, 0);
    (matched, refused)
}

/// Controllers authored through the Editor API and built by the engine. The
/// name table and the reserved size are compared as described in `normalize`.
#[test]
fn controllers_match_the_engine_build() {
    let assets = fixture("controllers/Assets");
    let tree = Tree::scan(std::slice::from_ref(&assets));
    let (matched, refused) = compare_controllers(&[assets.join("Ctl")], &tree);
    assert_eq!(refused, ["C04_BlendTrees"]);
    assert_eq!(matched, 8);
}

/// Compares every `<name>.controller` / `<name>.controller.bin` pair of the
/// given directories; `UNITY_PLAYERBAKE_SOURCES` lists the roots that hold the
/// clips and masks they refer to.
#[test]
fn controller_corpus_matches_the_engine_build() {
    let Some(roots) = std::env::var("UNITY_PLAYERBAKE_CONTROLLER_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_CONTROLLER_CORPUS");
        return;
    };
    let sources: Vec<PathBuf> = std::env::var("UNITY_PLAYERBAKE_SOURCES")
        .unwrap_or_default()
        .split(':')
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .collect();
    let roots: Vec<PathBuf> = roots.split(':').map(PathBuf::from).collect();
    compare_controllers(&roots, &Tree::scan(&sources));
}

/// Replaces each object reference with the order of its first appearance, so
/// references of two builds compare by which objects are the same.
fn label_references(value: &mut Val, seen: &mut Vec<Val>) {
    match value {
        Val::Map(fields) if fields.iter().any(|(name, _)| name == "m_PathID") => {
            if matches!(value.field("m_PathID"), Some(Val::I(0))) {
                *value = Val::Zero;
                return;
            }
            let position = seen
                .iter()
                .position(|known| known == value)
                .unwrap_or_else(|| {
                    seen.push(value.clone());
                    seen.len() - 1
                });
            *value = Val::U(position as u64 + 1);
        }
        Val::Map(fields) => fields
            .iter_mut()
            .for_each(|(_, field)| label_references(field, seen)),
        Val::Seq(items) => items
            .iter_mut()
            .for_each(|item| label_references(item, seen)),
        _ => {}
    }
}

/// Compares every `<name>.overrideController` / `<name>.override.bin` pair of
/// the given directories. An override clip stored in a model is absent from
/// the reference build, so such a pair differs and is only counted.
#[test]
fn override_corpus_matches_the_engine_build() {
    let Some(roots) = std::env::var("UNITY_PLAYERBAKE_OVERRIDE_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_OVERRIDE_CORPUS");
        return;
    };
    let (mut matched, mut differing) = (0, 0);
    for root in roots.split(':') {
        for entry in fs::read_dir(root).unwrap() {
            let source = entry.unwrap().path();
            if !source
                .extension()
                .is_some_and(|extension| extension == "overrideController")
            {
                continue;
            }
            let expected = fs::read(source.with_extension("override.bin")).unwrap();
            let text = fs::read(&source).unwrap();
            let documents = parse_unity_yaml_lexical(&text).unwrap();
            let (actual, _) = playerbake::bake_override_object(&documents[0].data).unwrap();
            let layout = playerbake::override_tree().unwrap();
            let (mut actual, mut expected) = (
                layout.decode(&actual).unwrap(),
                layout.decode(&expected).unwrap(),
            );
            label_references(&mut actual, &mut Vec::new());
            label_references(&mut expected, &mut Vec::new());
            if actual == expected {
                matched += 1;
            } else if String::from_utf8_lossy(&text).contains("type: 3}") {
                differing += 1;
            } else {
                panic!("{}: differs from the engine build", source.display());
            }
        }
    }
    eprintln!("matched {matched}, {differing} refer to clips inside models");
    assert!(matched > 0);
}

#[test]
fn override_controller_matches_the_engine_build() {
    let source = fixture("controllers/Assets/Ctl/O01_Override.overrideController");
    let documents = parse_unity_yaml_lexical(&fs::read(&source).unwrap()).unwrap();
    let (actual, externals) = playerbake::bake_override_object(&documents[0].data).unwrap();
    let layout = playerbake::override_tree().unwrap();
    let expected = fs::read(source.with_extension("override.bin")).unwrap();
    let (mut actual, mut expected) = (
        layout.decode(&actual).unwrap(),
        layout.decode(&expected).unwrap(),
    );
    label_references(&mut actual, &mut Vec::new());
    label_references(&mut expected, &mut Vec::new());
    assert_eq!(actual, expected);
    // The controller, then four distinct clips.
    assert_eq!(externals.len(), 5);
}
