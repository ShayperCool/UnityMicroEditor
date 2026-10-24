//! Compares baked animation objects with the ones Unity 6000.6.3f1 wrote
//! into AssetBundles. Each `<name>.anim` fixture is authored here;
//! `<name>.clip.bin` is the `AnimationClip` object the engine wrote for it,
//! `<name>.controller.bin` the `AnimatorController` of a `.controller`,
//! `<name>.mask.bin` the `AvatarMask` of a `.mask`.
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

/// The script of the fixtures, with the local ID the engine gave its
/// `MonoScript` object in the reference bundle.
struct FixtureScripts;
impl playerbake::ClipSources for FixtureScripts {
    fn script(&self, guid: &str) -> anyhow::Result<playerbake::ScriptIdentity> {
        assert_eq!(guid, "a01ac09a24c8493b862564ce94c80366");
        Ok(playerbake::ScriptIdentity {
            class_name: "ProbeBehaviour".into(),
            namespace: "Probe.Space".into(),
            assembly: "Assembly-CSharp".into(),
            execution_order: 0,
            local_id: -7906036524527734719,
        })
    }
    /// The clips the humanoid fixtures take an additive reference pose from,
    /// by the GUID their `.meta` files had in the reference project.
    fn clip(&self, guid: &str) -> anyhow::Result<Vec<u8>> {
        let name = match guid {
            "31f1006fa0677c73246324b8155c4168" => "additive_ref",
            "825f6d7b69cd06c329c0d1223711de42" => "additive_self",
            _ => panic!("unexpected reference to clip {guid}"),
        };
        Ok(fs::read(fixture(&format!("humanoid/{name}.anim")))?)
    }
}

fn bake(name: &str) -> anyhow::Result<Vec<u8>> {
    let source = fs::read(fixture(&format!("{name}.anim"))).unwrap();
    let documents = parse_unity_yaml_lexical(&source).unwrap();
    Ok(playerbake::bake_clip_object(&documents[0].data, &FixtureScripts)?.0)
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

/// The clip fixtures of a directory under the fixture root, by name.
fn clip_fixtures(directory: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixture(directory))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "anim")
        })
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn clips_match_the_engine_build() {
    let names = clip_fixtures("");
    assert!(names.len() >= 30, "fixtures are missing: {names:?}");
    for name in names {
        assert_matches_engine(&name);
    }
}

/// One small clip per rule of the humanoid bake: the curve table, binding
/// order, the pose difference, the root's start, stop and average speed under
/// each setting that affects them, motion curves, generic root motion and the
/// additive reference pose.
#[test]
fn humanoid_clips_match_the_engine_build() {
    let names = clip_fixtures("humanoid");
    assert_eq!(names.len(), 28, "fixtures are missing: {names:?}");
    for name in names {
        assert_matches_engine(&format!("humanoid/{name}"));
    }
}

fn clip_fields(yaml: &str) -> serde_json::Map<String, serde_json::Value> {
    parse_unity_yaml_lexical(yaml.as_bytes()).unwrap()[0]
        .data
        .clone()
}

/// A humanoid fixture with part of its source replaced.
fn edited(name: &str, from: &str, to: &str) -> String {
    let source = fs::read_to_string(fixture(&format!("humanoid/{name}.anim"))).unwrap();
    assert!(source.contains(from), "{name} has no {from:?}");
    source.replace(from, to)
}

#[test]
fn unreproduced_humanoid_features_are_refused() {
    let refused = |yaml: &str, sources: &dyn playerbake::ClipSources, reason: &str| {
        let error = playerbake::bake_clip_object(&clip_fields(yaml), sources).expect_err(reason);
        assert!(error.is::<Unsupported>(), "{reason}: {error:#}");
        assert!(error.to_string().contains(reason), "{reason}: {error}");
    };
    let additive = fs::read_to_string(fixture("humanoid/additive.anim")).unwrap();
    refused(&additive, &playerbake::NoScripts, "additive reference pose");
    // A root rotation of zero length has no direction.
    let zero = edited("root_flipped_up", "value: 1\n", "value: 0\n");
    refused(&zero, &FixtureScripts, "zero length");
}

/// The identities the engine gave the assets a corpus refers to, from the
/// `ids.tsv` of its directory: `<guid> <file ID> <path ID>` for an object in
/// the bundle the corpus was taken from, a script's `MonoScript` among them,
/// and `discrete <script guid> <attribute>` for a field its class makes
/// discrete.
#[derive(Default)]
struct CorpusIds {
    objects: std::collections::HashMap<(String, i64), i64>,
    discrete: std::collections::HashSet<(String, String)>,
    listed: bool,
}

impl CorpusIds {
    fn read(root: &Path) -> Self {
        let mut ids = Self::default();
        let Ok(text) = fs::read_to_string(root.join("ids.tsv")) else {
            return ids;
        };
        ids.listed = true;
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields[..] {
                ["discrete", guid, attribute] => {
                    ids.discrete.insert((guid.into(), attribute.into()));
                }
                [guid, file_id, path_id] => {
                    let key = (guid.to_string(), file_id.parse().unwrap());
                    ids.objects.insert(key, path_id.parse().unwrap());
                }
                _ => panic!("malformed ids.tsv line {line:?}"),
            }
        }
        ids
    }
}

impl playerbake::ClipSources for CorpusIds {
    fn script(&self, guid: &str) -> anyhow::Result<playerbake::ScriptIdentity> {
        let Some(path_id) = self.objects.get(&(guid.to_string(), 11500000)) else {
            return Err(Unsupported(format!("script {guid} without a listed identity")).into());
        };
        Ok(playerbake::ScriptIdentity {
            class_name: "Unknown".into(),
            namespace: String::new(),
            assembly: "Assembly-CSharp".into(),
            execution_order: 0,
            local_id: *path_id,
        })
    }
    fn object(
        &self,
        target: &playerbake::ObjectTarget,
    ) -> anyhow::Result<playerbake::ObjectPointer> {
        let key = (target.guid.clone(), target.file_id);
        match self.objects.get(&key) {
            Some(path_id) => Ok(playerbake::ObjectPointer {
                external: false,
                path_id: *path_id,
            }),
            None => Err(Unsupported(format!("object {key:?} without a listed identity")).into()),
        }
    }
    fn discrete(&self, script: &str, attribute: &str) -> anyhow::Result<Option<bool>> {
        let key = (script.to_string(), attribute.to_string());
        Ok(self.listed.then(|| self.discrete.contains(&key)))
    }
}

/// Clips of `curves` the baker refuses instead of approximating: a particle
/// force field, whose binding is encoded differently, a script curve without
/// a script, a material index that is not a number, and a weighted key that
/// stays weighted where the dense clip would sample it.
const REFUSED: [&str; 6] = [
    "N_ForceField",
    "N_Unknown",
    "P_Generic",
    "U_NoScript",
    "W_KeptLow",
    "W_KeptSpanLow",
];

/// One clip per rule of the dense clip, of bindings on components, of
/// discrete and object reference curves, of the clip's time range and of the
/// curves a build leaves out, compressed clips among them.
#[test]
fn curve_clips_match_the_engine_build() {
    let ids = CorpusIds::read(&fixture("curves"));
    let names = clip_fixtures("curves");
    assert_eq!(names.len(), 107, "fixtures are missing: {names:?}");
    for name in names {
        let source = fs::read(fixture(&format!("curves/{name}.anim"))).unwrap();
        let documents = parse_unity_yaml_lexical(&source).unwrap();
        let baked = playerbake::bake_clip_parts(&documents[0].data, &ids);
        if REFUSED.contains(&name.as_str()) {
            let error = baked.expect_err(&name);
            assert!(error.is::<Unsupported>(), "{name}: {error:#}");
            continue;
        }
        let baked = baked.unwrap_or_else(|error| panic!("{name}: {error:#}"));
        let expected = fs::read(fixture(&format!("curves/{name}.clip.bin"))).unwrap();
        assert!(
            baked.data == expected,
            "{name} differs from the engine build"
        );
    }
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

/// Controllers authored through the Editor API and built by the engine. The
/// name table is compared as described in `normalize`.
#[test]
fn controllers_match_the_engine_build() {
    let assets = fixture("controllers/Assets");
    let tree = Tree::scan(std::slice::from_ref(&assets));
    let mut controllers: Vec<_> = fs::read_dir(assets.join("Ctl"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "controller")
        })
        .collect();
    controllers.sort();
    let mut mismatched = Vec::new();
    for source in &controllers {
        let expected = fs::read(source.with_extension("controller.bin")).unwrap();
        let documents = parse_unity_yaml_lexical(&fs::read(source).unwrap()).unwrap();
        let name = source.file_stem().unwrap().to_string_lossy().into_owned();
        let baked = playerbake::bake_controller_object(&documents, &tree)
            .unwrap_or_else(|error| panic!("{name}: {error:#}"));
        let layout = playerbake::controller_tree().unwrap();
        assert_eq!(layout.encode(&baked.value).unwrap(), baked.data);
        let (actual, expected) = (
            normalize(layout.decode(&baked.data).unwrap()),
            normalize(layout.decode(&expected).unwrap()),
        );
        let mut out = Vec::new();
        differences("", &actual, &expected, &mut out);
        if !out.is_empty() {
            mismatched.push(format!("{name}:\n  {}", out.join("\n  ")));
        }
    }
    assert_eq!(controllers.len(), 9);
    assert!(mismatched.is_empty(), "{}", mismatched.join("\n"));
}

/// Avatar masks built by the engine: body parts alone, transform paths with
/// their weights, and a mask as the Editor creates it.
#[test]
fn masks_match_the_engine_build() {
    let mut sources: Vec<PathBuf> = fs::read_dir(fixture("masks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "mask")
        })
        .collect();
    sources.sort();
    assert_eq!(sources.len(), 3);
    for source in sources {
        let documents = parse_unity_yaml_lexical(&fs::read(&source).unwrap()).unwrap();
        let [document] = documents.as_slice() else {
            panic!("{}: a mask source is one object", source.display());
        };
        let actual = playerbake::bake_mask_object(&document.data).unwrap();
        let expected = fs::read(source.with_extension("mask.bin")).unwrap();
        assert!(actual == expected, "{} differs", source.display());
    }
}

/// Replaces each pointer by its position among the distinct pointers met.
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

/// Blend spaces built by the engine: positions and the tables it derived,
/// all as single-precision bit patterns.
#[test]
fn freeform_tables_match_the_engine_build() {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        blend_type: u32,
        positions: Vec<[u32; 2]>,
        magnitudes: Vec<u32>,
        pair_vectors: Vec<[u32; 2]>,
        pair_avg_mag_inv: Vec<u32>,
        neighbors: Vec<Vec<u32>>,
    }
    let cases: Vec<Case> =
        serde_json::from_slice(&fs::read(fixture("freeform-vectors.json")).unwrap()).unwrap();
    assert!(cases.len() >= 100);
    for (index, case) in cases.iter().enumerate() {
        let positions: Vec<[f32; 2]> = case
            .positions
            .iter()
            .map(|p| p.map(f32::from_bits))
            .collect();
        let table = playerbake::precompute_freeform(case.blend_type == 2, &positions);
        let bits = |values: &[f32]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        let pairs: Vec<[u32; 2]> = table
            .pair_vectors
            .iter()
            .map(|p| p.map(f32::to_bits))
            .collect();
        assert_eq!(bits(&table.magnitudes), case.magnitudes, "case {index}");
        assert_eq!(pairs, case.pair_vectors, "case {index}");
        assert_eq!(
            bits(&table.pair_scales),
            case.pair_avg_mag_inv,
            "case {index}"
        );
        assert_eq!(table.neighbors, case.neighbors, "case {index}");
    }
}

/// Bakes the mixers of a directory and compares their objects with the
/// engine's (`<name>.mixer.bin`: per object its ID, class, length and data).
/// A value a build leaves undefined is taken from the bake on both sides.
fn compare_mixers(root: &Path) -> (usize, Vec<String>, usize) {
    fn value(mixer: &mut Val, snapshot: usize, index: usize) -> &mut Val {
        let snapshots = mixer
            .field_mut("m_MixerConstant")
            .and_then(|constant| constant.field_mut("snapshots"));
        let Some(Val::Seq(snapshots)) = snapshots else {
            panic!("the mixer has no snapshots");
        };
        match snapshots[snapshot].field_mut("values") {
            Some(Val::Seq(values)) => &mut values[index],
            _ => panic!("the snapshot has no values"),
        }
    }
    let (mut matched, mut differing, mut passed_over) = (0, Vec::new(), 0);
    let mut sources: Vec<PathBuf> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "mixer")
        })
        .collect();
    sources.sort();
    for source in sources {
        let name = source.file_name().unwrap().to_string_lossy().into_owned();
        let packed = fs::read(root.join(format!("{name}.bin"))).unwrap();
        let mut expected = Vec::new();
        let mut at = 0;
        while at < packed.len() {
            let id = i64::from_le_bytes(packed[at..at + 8].try_into().unwrap());
            let class = i32::from_le_bytes(packed[at + 8..at + 12].try_into().unwrap());
            let length = u32::from_le_bytes(packed[at + 12..at + 16].try_into().unwrap()) as usize;
            expected.push((id, class as i64, packed[at + 16..at + 16 + length].to_vec()));
            at += 16 + length;
        }
        let text = fs::read(&source).unwrap();
        let mut objects = playerbake::bake_mixer_objects(&text)
            .unwrap_or_else(|error| panic!("{name}: {error:#}"));
        objects.sort_by_key(|object| object.0);
        let undefined = playerbake::mixer_undefined_values(&text).unwrap();
        if !undefined.is_empty() {
            let layout = playerbake::mixer_tree(241).unwrap();
            let baked = objects.iter().find(|object| object.1 == 241).unwrap();
            let mut baked = layout.decode(&baked.2).unwrap();
            let built = expected.iter_mut().find(|object| object.1 == 241).unwrap();
            let mut decoded = layout.decode(&built.2).unwrap();
            assert_eq!(layout.encode(&decoded).unwrap(), built.2, "{name}");
            for &(snapshot, index) in &undefined {
                *value(&mut decoded, snapshot, index) = value(&mut baked, snapshot, index).clone();
            }
            built.2 = layout.encode(&decoded).unwrap();
            passed_over += undefined.len();
        }
        if objects == expected {
            matched += objects.len();
            continue;
        }
        let mut found = Vec::new();
        for (object, expected) in objects.iter().zip(&expected) {
            if object.1 == expected.1 && object.2 != expected.2 {
                let layout = playerbake::mixer_tree(object.1).unwrap();
                differences(
                    &object.0.to_string(),
                    &layout.decode(&object.2).unwrap(),
                    &layout.decode(&expected.2).unwrap(),
                    &mut found,
                );
            }
        }
        differing.push(format!(
            "{name}: {} objects for {} {found:?}",
            objects.len(),
            expected.len()
        ));
    }
    (matched, differing, passed_over)
}

/// Mixers authored through the Editor (groups several deep, every built-in
/// effect, sends and ducking, snapshots, transitions, exposed parameters, a
/// mixer playing into another) and hand-written ones the Editor would not
/// author, each with the objects of an engine build.
#[test]
fn mixers_match_the_engine_build() {
    let (matched, differing, passed_over) = compare_mixers(&fixture("mixers"));
    assert!(differing.is_empty(), "{differing:?}");
    assert!(matched >= 272, "mixer fixtures are missing: {matched}");
    // Three of them hold a chorus with its feedback left to the default.
    assert_eq!(passed_over, 5);
}
