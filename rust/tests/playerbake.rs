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
    // A session cannot bake a clip again when its reference clip changes.
    let additive = fs::read_to_string(fixture("humanoid/additive.anim")).unwrap();
    refused(&additive, &playerbake::NoScripts, "additive reference pose");
    // A root rotation of zero length has no direction.
    let zero = edited("root_flipped_up", "value: 1\n", "value: 0\n");
    refused(&zero, &FixtureScripts, "zero length");
}

#[test]
fn baked_file_is_a_content_file_with_the_authored_identity() {
    let source = fs::read(fixture("PosSlopes.anim")).unwrap();
    let baked = playerbake::bake_clip(&source, &playerbake::NoScripts).unwrap();
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

/// Accepts any script, for counting what a corpus without scripts would bake.
struct AnyScript;
impl playerbake::ClipSources for AnyScript {
    fn script(&self, guid: &str) -> anyhow::Result<playerbake::ScriptIdentity> {
        Ok(playerbake::ScriptIdentity {
            class_name: "Unknown".into(),
            namespace: String::new(),
            assembly: "Assembly-CSharp".into(),
            execution_order: 0,
            local_id: playerbake::script_local_id(guid),
        })
    }
}

/// The identities the engine gave the assets a corpus refers to, from the
/// optional `ids.tsv` of its directory: `<guid> <file ID> <path ID>` for an
/// object in the bundle the corpus was taken from, a script's `MonoScript`
/// among them, and `discrete <script guid> <attribute>` for a field its
/// class makes discrete.
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
        let mut identity = AnyScript.script(guid)?;
        if let Some(path_id) = self.objects.get(&(guid.to_string(), 11500000)) {
            identity.local_id = *path_id;
        }
        Ok(identity)
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
/// curves a build leaves out, compressed clips among them. The objects and the script they refer to have
/// the identities of the reference bundle, listed in `ids.tsv`.
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
        let ids = CorpusIds::read(Path::new(root));
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
            let Ok(documents) = unity_micro_editor::yaml::parse_unity_yaml_lexical_within(
                &fs::read(&source).unwrap(),
                playerbake::MAX_SOURCE_BYTES,
            ) else {
                *refused
                    .entry("source exceeds the text bound".into())
                    .or_default() += 1;
                continue;
            };
            // A corpus is built without its project's scripts, so a clip that
            // refers to one cannot be compared unless the corpus lists the
            // script's identity; it is only counted.
            let unlisted = |script: &playerbake::ScriptIdentity| {
                !ids.objects.values().any(|id| *id == script.local_id)
            };
            let baked = match playerbake::bake_clip_parts(&documents[0].data, &ids) {
                Ok(baked) if baked.scripts.iter().any(unlisted) => {
                    *refused
                        .entry("baked with scripts, not compared".into())
                        .or_default() += 1;
                    continue;
                }
                other => other.map(|baked| baked.data),
            };
            match baked {
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
                Ok(playerbake::Controller { data: actual, .. }) => {
                    let layout = playerbake::controller_tree().unwrap();
                    let (actual, expected) = (
                        layout.decode(&actual).unwrap(),
                        layout.decode(&expected).unwrap(),
                    );
                    let mut out = Vec::new();
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
/// name table is compared as described in `normalize`.
#[test]
fn controllers_match_the_engine_build() {
    let assets = fixture("controllers/Assets");
    let tree = Tree::scan(std::slice::from_ref(&assets));
    let (matched, refused) = compare_controllers(&[assets.join("Ctl")], &tree);
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(matched, 9);
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

/// The scripts of the behaviour fixtures: every class with the ID the engine
/// gave its `MonoScript` object, and the fields of the classes that the
/// metadata reader lays out.
struct BehaviourFixtures {
    root: PathBuf,
    scripts: std::collections::HashMap<String, FixtureScript>,
    /// The clips and masks the controllers refer to.
    tree: Tree,
    targets: Targets,
    /// Whether a reference to an asset that is not a clip is left to the
    /// caller to bind, as a session does, or held as in a build.
    defer: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureScript {
    class_name: String,
    local_id: i64,
}

/// What the engine built for the behaviours of one controller.
#[derive(serde::Deserialize)]
struct BuiltBehaviours {
    /// The controller's behaviour list, as authored file IDs.
    #[serde(default)]
    list: Vec<String>,
    /// The object of each behaviour as hex, by authored file ID.
    objects: std::collections::BTreeMap<String, String>,
    /// The ID the build gave each object a field refers to, by
    /// `<guid>:<file ID>` of the authored reference. A reference that is
    /// not listed leads to nothing the build holds.
    pointers: std::collections::HashMap<String, i64>,
    /// The position, counted from one, of the build's external reference to
    /// the file of such an object, where it is another file.
    #[serde(default)]
    files: std::collections::HashMap<String, i64>,
}

/// The classes of the objects that fields of the fixtures refer to from a
/// field of another class, by `<guid>:<file ID>`: the class and what it
/// derives from. The engine leaves such a field null.
type Targets = std::collections::HashMap<String, Vec<String>>;

/// Whether a field may hold the object it refers to.
fn fits(targets: &Targets, reference: &playerbake::ObjectReference<'_>) -> bool {
    let declared = match reference.namespace {
        "" => reference.class_name.to_string(),
        namespace => format!("{namespace}.{}", reference.class_name),
    };
    let key = format!("{}:{}", reference.guid, reference.file_id);
    targets
        .get(&key)
        .is_none_or(|classes| classes.contains(&declared))
}

/// The reply of the code runner to a request, as recorded under
/// `<root>/code` by the hash of the request. The SDK helper gave these
/// replies for the classes under `Scripts`; `compiler_behaviour_objects_sdk`
/// checks that it still does. With `UNITY_PLAYERBAKE_CODE_RUNNER` set to a
/// command that takes the path of a request and prints the reply, a request
/// without a record is run and recorded.
fn recorded_code(
    root: &Path,
    request: &serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    let key = unity_micro_editor::sha256(&serde_json::to_vec(request)?);
    let path = root.join("code").join(format!("{key}.json"));
    if let Ok(record) = fs::read(&path) {
        let record: serde_json::Value = serde_json::from_slice(&record)?;
        assert_eq!(&record["request"], request);
        return Ok(Some(record["reply"].clone()));
    }
    let Some(runner) = std::env::var_os("UNITY_PLAYERBAKE_CODE_RUNNER") else {
        return Ok(None);
    };
    fs::create_dir_all(root.join("code"))?;
    let asked = root.join("code").join(format!("{key}.request"));
    fs::write(&asked, serde_json::to_vec(request)?)?;
    let output = std::process::Command::new(runner).arg(&asked).output()?;
    fs::remove_file(&asked)?;
    anyhow::ensure!(
        output.status.success(),
        "code runner failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let record = serde_json::json!({ "request": request, "reply": reply });
    fs::write(&path, serde_json::to_vec(&record)?)?;
    Ok(Some(reply))
}

/// Stands in for the bundles the fixtures were built into.
struct BundlePointers<'a>(&'a BuiltBehaviours, &'a Targets, &'a Path);

impl playerbake::References for BundlePointers<'_> {
    fn run_object(
        &mut self,
        request: &serde_json::Value,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        recorded_code(self.2, request)
    }

    fn pointer(
        &mut self,
        reference: &playerbake::ObjectReference<'_>,
    ) -> anyhow::Result<playerbake::Pointer> {
        let key = format!("{}:{}", reference.guid, reference.file_id);
        Ok(match self.0.pointers.get(&key) {
            Some(id) if fits(self.1, reference) => playerbake::Pointer::Object {
                file: self.0.files.get(&key).copied().unwrap_or(0),
                path_id: *id,
            },
            _ => playerbake::Pointer::Null,
        })
    }
}

impl BehaviourFixtures {
    fn open(root: PathBuf) -> Self {
        let scripts =
            serde_json::from_slice(&fs::read(root.join("scripts.json")).unwrap()).unwrap();
        let tree = Tree::scan(&[root.join("Assets")]);
        let targets = fs::read(root.join("targets.json"))
            .map(|targets| serde_json::from_slice(&targets).unwrap())
            .unwrap_or_default();
        Self {
            root,
            scripts,
            tree,
            targets,
            defer: false,
        }
    }
}

impl playerbake::ControllerSources for BehaviourFixtures {
    fn clip(&self, guid: &str, file_id: &str) -> anyhow::Result<playerbake::ClipFacts> {
        self.tree.clip(guid, file_id)
    }
    fn mask(&self, guid: &str) -> anyhow::Result<playerbake::Mask> {
        self.tree.mask(guid)
    }
    fn behaviour(&self, guid: &str) -> anyhow::Result<Option<playerbake::BehaviourScript>> {
        let Some(script) = self.scripts.get(guid) else {
            return Ok(None);
        };
        let path = self
            .root
            .join("layouts")
            .join(format!("{}.json", script.class_name));
        let Ok(layout) = fs::read(path) else {
            return Err(Unsupported(format!("script {} has no layout", script.class_name)).into());
        };
        let layout: playerbake::ScriptLayout = serde_json::from_slice(&layout)?;
        Ok(Some(playerbake::BehaviourScript {
            identity: playerbake::ScriptIdentity {
                class_name: layout.class_name.clone(),
                namespace: layout.namespace.clone(),
                assembly: layout.assembly.clone(),
                execution_order: 0,
                local_id: script.local_id,
            },
            layout,
        }))
    }
    fn run_object(&self, request: &serde_json::Value) -> anyhow::Result<Option<serde_json::Value>> {
        recorded_code(&self.root, request)
    }
    fn reference(
        &self,
        reference: &playerbake::ObjectReference<'_>,
    ) -> anyhow::Result<playerbake::Resolution> {
        use playerbake::Resolution;
        if !fits(&self.targets, reference) {
            return Ok(Resolution::Null);
        }
        let clip = self
            .tree
            .clip(reference.guid, &reference.file_id.to_string())
            .is_ok();
        Ok(if self.defer && !clip {
            Resolution::Deferred
        } else {
            Resolution::External {
                guid: reference.guid.to_string(),
                local_id: reference.file_id,
            }
        })
    }
}

/// The sources of an extension below `<root>/Assets`, one directory deep.
fn sources_of(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    for directory in fs::read_dir(root.join("Assets")).unwrap() {
        for entry in fs::read_dir(directory.unwrap().path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|found| found == extension) {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}

fn controller_sources(root: &Path) -> Vec<PathBuf> {
    sources_of(root, "controller")
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap())
        .collect()
}

/// Where two hex dumps part, with what follows in each.
fn first_difference(actual: &str, expected: &str) -> String {
    let at = actual
        .bytes()
        .zip(expected.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or(actual.len().min(expected.len()))
        & !1;
    let window = |text: &str| text[at.min(text.len())..(at + 48).min(text.len())].to_string();
    format!(
        "{} bytes, engine has {}; first difference at {}: {} vs {}",
        actual.len() / 2,
        expected.len() / 2,
        at / 2,
        window(actual),
        window(expected)
    )
}

/// The type table entry of a script class without what it ends in: the
/// positions, in a table of their file, of the types that objects of the
/// class hold by managed reference.
fn class_entry(entry: &[u8]) -> &[u8] {
    let size = u32::from_le_bytes(entry[55..59].try_into().unwrap()) as usize;
    &entry[..59 + size]
}

/// A node as a type table entry holds it: the names the tree reader resolves
/// and the version, level, flags, size and meta flags.
type RawNode = (String, String, [u8; 12]);

/// The fields of a script class in which two type table entries differ.
fn entry_differences(actual: &[u8], expected: &[u8]) -> Vec<String> {
    let nodes = |entry: &[u8]| -> Option<Vec<RawNode>> {
        let tree = playerbake::TypeTree::parse(entry.to_vec()).ok()?;
        let records = entry.get(59 + 16..)?;
        Some(
            tree.nodes
                .iter()
                .zip(records.chunks(32))
                .map(|(node, record)| {
                    let mut raw = [0; 12];
                    raw[..4].copy_from_slice(&record[..4]);
                    raw[4..8].copy_from_slice(&record[12..16]);
                    raw[8..].copy_from_slice(&record[20..24]);
                    (node.type_name.clone(), node.name.clone(), raw)
                })
                .collect(),
        )
    };
    let (Some(actual), Some(expected)) = (nodes(actual), nodes(expected)) else {
        return vec!["an entry the reader does not parse".into()];
    };
    let fields = |nodes: &[RawNode]| {
        let mut fields: Vec<(String, Vec<RawNode>)> = Vec::new();
        for node in nodes.iter().skip(1) {
            if node.2[2] == 1 {
                fields.push((node.1.clone(), Vec::new()));
            }
            fields.last_mut().unwrap().1.push(node.clone());
        }
        fields
    };
    let (actual, expected) = (fields(&actual), fields(&expected));
    let mut out = Vec::new();
    for (name, nodes) in &expected {
        match actual.iter().find(|(known, _)| known == name) {
            Some((_, found)) if found == nodes => {}
            Some(_) => out.push(name.clone()),
            None => out.push(format!("{name} (missing)")),
        }
    }
    for (name, _) in &actual {
        if !expected.iter().any(|(known, _)| known == name) {
            out.push(format!("{name} (not in the engine's)"));
        }
    }
    if out.is_empty() && actual != expected {
        out.push("the order of the fields".into());
    }
    out
}

/// The fields in which two objects of a script class differ, each decoded
/// with its own type tree.
fn field_differences(actual: &Val, expected: &Val) -> Vec<String> {
    let (Val::Map(actual), Val::Map(expected)) = (actual, expected) else {
        return Vec::new();
    };
    expected
        .iter()
        .filter(|(name, value)| {
            actual
                .iter()
                .find(|(known, _)| known == name)
                .is_none_or(|(_, found)| format!("{found:?}") != format!("{value:?}"))
        })
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Bakes every `<name>.controller` below `<root>/Assets` and compares it with
/// the engine's build beside it: the controller object field by field, each
/// behaviour object and each script's type table entry byte for byte. Returns
/// how many controllers matched, how many differ and which were refused, with
/// the reason.
fn compare_behaviours(root: &Path) -> (usize, usize, Vec<(String, String)>) {
    let fixtures = BehaviourFixtures::open(root.to_path_buf());
    let (mut matched, mut failed, mut refused) = (0, 0, Vec::new());
    for source in controller_sources(root) {
        let name = source.file_stem().unwrap().to_string_lossy().into_owned();
        let documents = match parse_unity_yaml_lexical(&fs::read(&source).unwrap()) {
            Ok(documents) => documents,
            Err(error) => {
                refused.push((name, format!("unreadable: {error:#}")));
                continue;
            }
        };
        let baked = match playerbake::bake_controller_object(&documents, &fixtures) {
            Ok(baked) => baked,
            Err(error) if error.is::<Unsupported>() => {
                refused.push((name, error.to_string()));
                continue;
            }
            Err(error) => {
                failed += 1;
                eprintln!("ERROR {name}: {error:#}");
                continue;
            }
        };
        let built: BuiltBehaviours =
            serde_json::from_slice(&fs::read(source.with_extension("behaviours.json")).unwrap())
                .unwrap();
        let layout = playerbake::controller_tree().unwrap();
        let (mut actual, mut expected) = (
            layout.decode(&baked.data).unwrap(),
            layout
                .decode(&fs::read(source.with_extension("controller.bin")).unwrap())
                .unwrap(),
        );
        let mut out = Vec::new();
        // The engine gives the objects of a bundle other IDs, so the list is
        // compared through the authored file IDs.
        let key = "m_StateMachineBehaviours";
        let listed: Vec<String> = match actual.field(key) {
            Some(Val::Seq(pointers)) => pointers
                .iter()
                .map(|pointer| match pointer.field("m_PathID") {
                    Some(Val::I(id)) => id.to_string(),
                    _ => panic!("behaviour pointer has no ID"),
                })
                .collect(),
            _ => panic!("controller has no behaviour list"),
        };
        if listed != built.list {
            out.push(format!("lists {listed:?}, engine has {:?}", built.list));
        }
        for value in [&mut actual, &mut expected] {
            *value.field_mut(key).unwrap() = Val::Zero;
        }
        differences("", &normalize(actual), &normalize(expected), &mut out);
        if baked.behaviours.len() != built.objects.len() {
            out.push(format!(
                "{} behaviour objects, engine has {}",
                baked.behaviours.len(),
                built.objects.len()
            ));
        }
        for behaviour in &baked.behaviours {
            let class = &baked.scripts[behaviour.script].0.class_name;
            let actual = hex(&behaviour.data);
            // A file of its own numbers the objects it refers to otherwise
            // than the bundle; such objects are compared by which of their
            // references are the same, and byte for byte in
            // `compare_behaviour_objects`.
            // An object of the engine's that the class's tree does not
            // describe compares with nothing.
            let relabelled = |data: &[u8]| {
                let mut value = baked.scripts[behaviour.script].1.tree.decode(data).ok()?;
                label_references(&mut value, &mut Vec::new());
                Some(value)
            };
            // An object with a registry of managed references is not
            // decoded by the tree of its class alone; it is encoded again
            // with the bundle's numbering instead.
            let renumbered = || {
                let (script, layout) = &baked.scripts[behaviour.script];
                let document = documents
                    .iter()
                    .find(|document| document.file_id == behaviour.file_id)?;
                let object = layout.object(
                    &document.data,
                    script.local_id,
                    &mut BundlePointers(&built, &fixtures.targets, root),
                );
                object.ok().map(|object| hex(&object.data))
            };
            match built.objects.get(&behaviour.file_id) {
                Some(expected) if *expected == actual => {}
                Some(expected)
                    if !behaviour.managed.is_empty() && renumbered().as_ref() == Some(expected) => {
                }
                Some(expected)
                    if relabelled(&unhex(expected))
                        .is_some_and(|engine| relabelled(&behaviour.data) == Some(engine)) => {}
                Some(expected) => out.push(format!(
                    "{class} {}: {}",
                    behaviour.file_id,
                    first_difference(&actual, expected)
                )),
                None => out.push(format!(
                    "{class} {}: engine has no such object",
                    behaviour.file_id
                )),
            }
        }
        for (identity, script) in &baked.scripts {
            let entry = root.join(format!("types/{}.entry.bin", identity.class_name));
            let entry = fs::read(entry).unwrap();
            if class_entry(&entry) != class_entry(&script.tree.entry) {
                out.push(format!(
                    "type table entry of {}: {}",
                    identity.class_name,
                    entry_differences(&script.tree.entry, &entry).join(", ")
                ));
            }
        }
        if out.is_empty() {
            matched += 1;
        } else {
            failed += 1;
            eprintln!("MISMATCH {name}:\n  {}", out.join("\n  "));
        }
    }
    eprintln!("matched {matched}");
    (matched, failed, refused)
}

/// Encodes every behaviour document below `<root>/Assets` on its own and
/// compares it with the engine's object, object references included, and
/// the type table entry of its class with the engine's. Returns how many
/// matched, how many differ and which were refused, with the reason.
fn compare_behaviour_objects(root: &Path) -> (usize, usize, Vec<(String, String)>) {
    use playerbake::ControllerSources;
    let fixtures = BehaviourFixtures::open(root.to_path_buf());
    let (mut matched, mut failed, mut refused) = (0, 0, Vec::new());
    let mut mistyped: Vec<String> = Vec::new();
    let mut sources = controller_sources(root);
    sources.extend(sources_of(root, "asset"));
    for source in sources {
        let name = source.file_stem().unwrap().to_string_lossy().into_owned();
        let Ok(documents) = parse_unity_yaml_lexical(&fs::read(&source).unwrap()) else {
            refused.push((name, "unreadable".to_string()));
            continue;
        };
        let scripted = source.extension().is_some_and(|found| found == "asset");
        let built = if scripted {
            "objects.json"
        } else {
            "behaviours.json"
        };
        let Ok(built) = fs::read(source.with_extension(built)) else {
            // An asset that only stands for what others refer to.
            continue;
        };
        let built: BuiltBehaviours = serde_json::from_slice(&built).unwrap();
        for document in documents.iter().filter(|document| document.class_id == 114) {
            // A document nothing refers to is not in the build.
            let Some(expected) = built.objects.get(&document.file_id) else {
                continue;
            };
            let guid = document.data["m_Script"]["guid"].as_str().unwrap();
            let label = format!("{name} {}", document.file_id);
            let script = match fixtures.behaviour(guid) {
                Ok(script) => script.unwrap(),
                Err(error) => {
                    refused.push((label, error.to_string()));
                    continue;
                }
            };
            let class = &script.layout.class_name;
            let layout = playerbake::ScriptType::new(&script.layout);
            // The engine's entry of the class, where the directory has it.
            let entry = fs::read(root.join(format!("types/{class}.entry.bin"))).ok();
            if let (Ok(layout), Some(entry)) = (&layout, &entry)
                && class_entry(entry) != class_entry(&layout.tree.entry)
                && !mistyped.contains(class)
            {
                mistyped.push(class.clone());
                failed += 1;
                eprintln!(
                    "MISMATCH type table entry of {class}: {}",
                    entry_differences(&layout.tree.entry, entry).join(", ")
                );
            }
            let object = layout.and_then(|layout| {
                let object = layout.object(
                    &document.data,
                    script.identity.local_id,
                    &mut BundlePointers(&built, &fixtures.targets, root),
                )?;
                Ok((layout, object))
            });
            match object {
                Ok((_, actual)) if hex(&actual.data) == *expected => matched += 1,
                Ok((layout, actual)) => {
                    failed += 1;
                    let engine = entry
                        .and_then(|entry| playerbake::TypeTree::parse(entry).ok())
                        .and_then(|tree| tree.decode(&unhex(expected)).ok());
                    let fields = match (layout.tree.decode(&actual.data), engine) {
                        (Ok(actual), Some(engine)) => field_differences(&actual, &engine),
                        _ => Vec::new(),
                    };
                    eprintln!(
                        "MISMATCH {label} {class}: {}; in {}",
                        first_difference(&hex(&actual.data), expected),
                        fields.join(", ")
                    );
                }
                Err(error) if error.is::<Unsupported>() => {
                    refused.push((label, error.to_string()));
                }
                Err(error) => {
                    failed += 1;
                    eprintln!("ERROR {label}: {error:#}");
                }
            }
        }
    }
    for (label, reason) in &refused {
        eprintln!("refused object {label}: {reason}");
    }
    eprintln!("matched {matched} objects");
    (matched, failed, refused)
}

/// Controllers with state machine behaviours, authored through the Editor API
/// or edited by hand, and built by the engine. `layouts` holds what the
/// metadata reader reports for the classes under `Scripts`. The controllers
/// of `GRef` refer to objects of another bundle, of their own file and, in
/// the one edited by hand, to objects that are missing or of another class
/// than the field, which the engine leaves null. Two layers of
/// `M01_Embedded` take their masks from the controller's own file. The
/// documents of `F08_KnownMissing` to `F10_KnownBase` lack fields of their
/// classes, down to all of them, and hold fields their classes do not have.
/// The classes of `C01_Cut` nest serialized types past the engine's depth
/// limit, where it keeps the numbers and strings of a type and nothing else.
/// `E02_Engine` holds engine types as fields: the ones the engine stores in
/// a form of its own, among them an ID, a property name and a GUI style, and
/// a choice of those it stores by their C# fields. The behaviours of
/// `N03_Typed` and `T01_Filled` hold objects by managed reference: in plain
/// fields and lists, shared, null, in a cycle, from a class stored inline
/// and from one another.
///
/// The controllers of `GRest` hold objects that are what the code of their
/// classes leaves, which the SDK helper ran (`code`, see `recorded_code`).
/// `K01_Count` and `K01h_CountHand` count the serialization callbacks a build
/// runs: on the behaviour, on classes and structs it stores inline, in lists,
/// through a base class, and on objects it holds by managed reference; the
/// second lacks some of those objects, which then never hear that they were
/// read. `E04_CallbackHand` is the first fixture that showed the callbacks.
/// `K05h_NullsHand` records what a callback finds in fields the text lacks.
/// `K06_Wrap` and its hand-edited twin hold a dictionary that is a class of
/// its own, a wrapper that upgrades old data and an event whose call names
/// its types by full assembly names, which the engine's own callback
/// shortens. `K02h_Missing` and `K02p_Partial` lack managed reference fields:
/// a constructor's object gets an ID of the build's, renumbered in the
/// fixture from 0x7000000000000000 as the baker numbers them, since no two
/// builds agree on those IDs. `K03h_Missing` and `K03p_Partial` lack fields
/// that constructors set through calls, branches, loops, shared objects,
/// private constructors and constructors with arguments.
#[test]
fn behaviours_match_the_engine_build() {
    let (matched, failed, refused) = compare_behaviours(&fixture("behaviours"));
    assert_eq!(failed, 0);
    assert_eq!(matched, 41);
    // The documents of these lack fields of a class whose constructor makes
    // a curve or a gradient, which only the engine does.
    let engine = "needs the engine: a call into the engine in UnityEngine.AnimationCurve";
    let reasons: Vec<(&str, &str)> = refused
        .iter()
        .map(|(name, reason)| {
            let known = reason.contains(engine);
            (name.as_str(), if known { engine } else { "another reason" })
        })
        .collect();
    assert_eq!(
        reasons,
        [
            ("D01_Stripped", engine),
            ("F02_Missing", engine),
            ("K03h_Missing", engine),
            ("K03p_Partial", engine),
        ],
        "{refused:#?}"
    );
}

/// The avatar masks stored in a controller's file are objects of the baked
/// file, published beside the controller, the one hidden in the Project
/// window as well: the Editor's importer holds both, and a session checks
/// what a baked file publishes against it. The reference bundle holds the
/// visible one, whose bytes are compared.
#[test]
fn baked_controller_carries_the_masks_of_its_file() {
    let root = fixture("behaviours");
    let fixtures = BehaviourFixtures::open(root.clone());
    let source = root.join("Assets/GMask/M01_Embedded.controller");
    let baked = playerbake::bake_controller(&fs::read(&source).unwrap(), &fixtures)
        .unwrap()
        .baked;
    let masks: Vec<&str> = baked
        .objects
        .iter()
        .filter(|(_, class)| class == "UnityEngine.AvatarMask")
        .map(|(file_id, _)| file_id.as_str())
        .collect();
    assert_eq!(masks, ["3158504355398234554", "7453294008069960692"]);
    let built = fs::read(source.with_extension("behaviours.json")).unwrap();
    let built: serde_json::Value = serde_json::from_slice(&built).unwrap();
    let objects = playerbake::read_serialized(&baked.file).unwrap();
    let extra = built["extra"].as_object().unwrap();
    assert_eq!(extra.len(), 1);
    for (file_id, engine) in extra {
        let object = objects
            .iter()
            .find(|object| object.local_id.to_string() == *file_id)
            .unwrap();
        assert_eq!(object.tree.class_id, 319);
        assert_eq!(hex(&object.data), engine["data"].as_str().unwrap());
    }
}

/// A caller that cannot run the code of the project gets the reason why the
/// text alone does not tell the object.
#[test]
fn behaviours_that_need_their_code_are_refused_without_it() {
    let root = fixture("behaviours");
    for (class, reason) in [
        (
            "SmbCount",
            "class SmbCount has serialization callbacks, which a build runs",
        ),
        (
            "SmbEvent",
            "class SmbEvent has serialization callbacks, which a build runs",
        ),
        (
            "SmbRefPlain",
            "behaviour field r is a managed reference that is not in the authored document",
        ),
        (
            "SmbCtor",
            "behaviour field baseCall is not in the authored document, and what its class constructs it with is not known",
        ),
    ] {
        let layout = fs::read(root.join(format!("layouts/{class}.json"))).unwrap();
        let layout: playerbake::ScriptLayout = serde_json::from_slice(&layout).unwrap();
        let refused = playerbake::ScriptType::new(&layout)
            .unwrap()
            .object(&serde_json::Map::new(), 1, &mut NoReferences)
            .unwrap_err();
        assert_eq!(
            refused.downcast::<Unsupported>().unwrap().to_string(),
            reason
        );
    }
}

/// Every behaviour document of the fixtures on its own, and every object of
/// the scriptable object assets, whose classes the same reader lays out.
/// `T01_Filled` sets
/// every field of every class to a value of its own. It is the Editor's file
/// but for four lines of one document: three closing quotes the Editor wrote
/// at the start of a line are indented, and `a #b` is quoted. YAML reads
/// both forms as the same strings; the reader used here fails on the first
/// and takes the `#` of the second for a comment.
#[test]
fn behaviour_objects_match_the_engine_build() {
    let (matched, failed, refused) = compare_behaviour_objects(&fixture("behaviours"));
    assert_eq!(failed, 0);
    assert_eq!(matched, 205);
    // The documents of `D01_Stripped` and `F02_Missing` lack fields their
    // classes serialize. Twelve of them are filled in with the constants the
    // constructors store, three more with what running the constructors
    // leaves. The constructors of three classes make a curve or a gradient,
    // which takes the engine, and five objects of them stay refused.
    let engine = "needs the engine: a call into the engine in UnityEngine.AnimationCurve";
    assert_eq!(refused.len(), 5, "{refused:#?}");
    assert!(refused.iter().all(|(_, reason)| reason.contains(engine)));
}

#[test]
fn baked_controller_carries_its_behaviours_and_their_scripts() {
    let root = fixture("behaviours");
    let fixtures = BehaviourFixtures::open(root.clone());
    let source = fs::read(root.join("Assets/One/S01_Single.controller")).unwrap();
    let baked = playerbake::bake_controller(&source, &fixtures)
        .unwrap()
        .baked;
    // The controller and its behaviours are published, each behaviour as an
    // object of its class; the script objects only travel with them.
    let published: Vec<(&str, &str)> = baked
        .objects
        .iter()
        .map(|(file_id, class)| (file_id.as_str(), class.as_str()))
        .collect();
    assert_eq!(
        published,
        [
            ("9100000", "UnityEngine.RuntimeAnimatorController"),
            ("4871019363250582183", "SmbTag"),
            ("2735788593747126708", "Second.Deep.Ns.SmbSecond"),
            ("-9034250506258404624", "SmbTag"),
        ]
    );
    let objects = playerbake::read_serialized(&baked.file).unwrap();
    let classes: Vec<i32> = objects.iter().map(|object| object.tree.class_id).collect();
    assert_eq!(classes.iter().filter(|&&class| class == 91).count(), 1);
    assert_eq!(classes.iter().filter(|&&class| class == 114).count(), 3);
    assert_eq!(classes.iter().filter(|&&class| class == 115).count(), 2);
    // The scripts lead, so the Player has read the script of a behaviour
    // before the behaviour, wherever its ID sorts: one of these is negative.
    assert!(objects.is_sorted_by_key(|object| (object.tree.class_id != 115, object.local_id)));
    assert!(objects.iter().any(|object| object.local_id < 0));
    for object in &objects {
        // The Player finds the class of a behaviour through the script type
        // table, which has to name a `MonoScript` object of the file.
        let script = objects
            .iter()
            .find(|other| Some(other.local_id) == object.script);
        assert_eq!(object.tree.class_id == 114, script.is_some());
        if let Some(script) = script {
            assert_eq!(script.tree.class_id, 115);
            let name = script.tree.decode(&script.data).unwrap();
            let class = match name.field("m_ClassName") {
                Some(Val::Str(class)) => class.clone(),
                _ => panic!("script object has no class name"),
            };
            let entry = fs::read(root.join(format!("types/{class}.entry.bin"))).unwrap();
            // The file numbers its script types; the fixture's entry has none.
            let mut written = object.tree.entry.to_vec();
            written[5..7].fill(0);
            assert_eq!(written, entry, "{class}");
        }
    }
    // A source that cannot name behaviour scripts refuses the controller.
    let tree = Tree::scan(&[root.join("Assets")]);
    let refused = playerbake::bake_controller(&source, &tree).unwrap_err();
    assert!(refused.is::<Unsupported>());
}

/// Nulls every reference to an object of a file other than `keep`, zero
/// being the object's own file, and returns how many there were.
fn null_references_to_other_files(value: &mut Val, keep: i64) -> usize {
    match value {
        Val::Map(fields) if fields.iter().any(|(name, _)| name == "m_PathID") => {
            match value.field("m_FileID") {
                Some(Val::I(file)) if *file != 0 && *file != keep => {
                    *value = Val::Zero;
                    1
                }
                _ => 0,
            }
        }
        Val::Map(fields) => fields
            .iter_mut()
            .map(|(_, field)| null_references_to_other_files(field, keep))
            .sum(),
        Val::Seq(items) => items
            .iter_mut()
            .map(|item| null_references_to_other_files(item, keep))
            .sum(),
        _ => 0,
    }
}

/// The two answers a source has for a behaviour field that refers to another
/// asset. Held, the object is the engine's, which `behaviours_match_the_engine_build`
/// compares. Deferred, as a session answers for all but clips, the same
/// object has a null in each such place, a list keeps its length, and every
/// null is reported with the field's path.
#[test]
fn behaviour_references_are_held_or_deferred() {
    let root = fixture("behaviours");
    let mut fixtures = BehaviourFixtures::open(root.clone());
    let source = fs::read(root.join("Assets/GRef/R01_External.controller")).unwrap();
    let clip = "219a6312c29f375719c3273b6b86ada8";
    let held = playerbake::bake_controller(&source, &fixtures).unwrap();
    assert!(held.deferred.is_empty());
    assert!(held.baked.externals.len() > 5);
    fixtures.defer = true;
    let deferred = playerbake::bake_controller(&source, &fixtures).unwrap();
    // Only the clip is left to refer to.
    let externals: Vec<&str> = deferred
        .baked
        .externals
        .iter()
        .map(|external| external.guid.as_str())
        .collect();
    assert_eq!(externals, [clip]);
    let kept = held
        .baked
        .externals
        .iter()
        .position(|external| external.guid == clip)
        .unwrap() as i64
        + 1;
    let held = playerbake::read_serialized(&held.baked.file).unwrap();
    let (mut nulled, mut behaviours) = (0, 0);
    for object in playerbake::read_serialized(&deferred.baked.file).unwrap() {
        let other = held
            .iter()
            .find(|other| other.local_id == object.local_id)
            .unwrap();
        let mut expected = other.tree.decode(&other.data).unwrap();
        let mut actual = object.tree.decode(&object.data).unwrap();
        if object.tree.class_id != 114 {
            continue;
        }
        behaviours += 1;
        let count = null_references_to_other_files(&mut expected, kept);
        nulled += count;
        assert_eq!(null_references_to_other_files(&mut actual, 1), 0);
        label_references(&mut expected, &mut Vec::new());
        label_references(&mut actual, &mut Vec::new());
        assert_eq!(actual, expected, "{}", object.local_id);
        let owner = object.local_id.to_string();
        let reported = deferred
            .deferred
            .iter()
            .filter(|binding| binding.owner == owner)
            .count();
        assert_eq!(reported, count, "{owner}");
    }
    assert_eq!((behaviours, nulled), (3, deferred.deferred.len()));
    let binding = |owner: &str, path: &str| {
        deferred
            .deferred
            .iter()
            .find(|binding| binding.owner == owner && binding.path == path)
            .map(|binding| (binding.guid.as_str(), binding.file_id, binding.kind))
    };
    let (refs, nested) = ("1088311419948901774", "-5257619603817630436");
    let prefab = "6e22b73078ae28e63b7beaa0dafe42c3";
    let scriptable = ("e686ac87a9de98f34afba5382b737167", 11400000, 2);
    let material = ("6d3ff677adf6b5815a331deafb745410", 2100000, 2);
    assert_eq!(binding(refs, "go"), Some((prefab, 5811007898720732970, 3)));
    assert_eq!(binding(refs, "sos[0]"), Some(scriptable));
    // The list keeps its null entry, which is not reported.
    assert_eq!(binding(refs, "sos[1]"), None);
    assert_eq!(binding(refs, "sos[2]"), Some(scriptable));
    assert_eq!(binding(refs, "mats[0]"), Some(material));
    assert_eq!(binding(refs, "clip"), None);
    assert_eq!(binding(nested, "h.inner.mats[0]"), Some(material));
    assert_eq!(binding(nested, "hs[2].inners[1].mats[0]"), Some(material));
    assert_eq!(
        binding(nested, "hs[2].inners[1].any"),
        Some(("d4a7b458bb74ee61684a4cf8f427d00f", 11400000, 2))
    );
    assert_eq!(binding(nested, "hl[0].so"), Some(scriptable));
}

/// Never asked: the layouts of the tests that use it hold no references.
struct NoReferences;
impl playerbake::References for NoReferences {
    fn pointer(
        &mut self,
        reference: &playerbake::ObjectReference<'_>,
    ) -> anyhow::Result<playerbake::Pointer> {
        panic!("unexpected reference from {}", reference.path)
    }
}

/// What the engine made of text that is not what a field's type is written
/// as, recorded from a build of a controller edited by hand: it reads a
/// number as far as the text is one, keeps what the constructor set where
/// the text is a structure, and reads no string out of one.
#[test]
fn text_of_another_type_reads_as_the_engine_reads_it() {
    use serde_json::json;
    let field = |name: &str, kind: serde_json::Value, initial: Option<&str>| {
        let mut field = json!({"name": name, "type": kind});
        if let Some(initial) = initial {
            field["initial"] = json!(initial);
        }
        field
    };
    let primitive = |name: &str| json!({"kind": "primitive", "name": name});
    let layout: playerbake::ScriptLayout = serde_json::from_value(json!({
        "className": "Probe", "assembly": "Assembly-CSharp", "baseKind": "StateMachineBehaviour",
        "fields": [
            field("i0", primitive("int"), None),
            field("i5", primitive("int"), Some("5")),
            field("u", primitive("uint"), None),
            field("sh", primitive("short"), None),
            field("by", primitive("byte"), None),
            field("sb", primitive("sbyte"), None),
            field("us", primitive("ushort"), None),
            field("l", primitive("long"), None),
            field("ul", primitive("ulong"), None),
            field("on", primitive("bool"), Some("1")),
            field("c", primitive("char"), None),
            field("e", json!({"kind": "enum", "underlying": "int"}), None),
            field("f", primitive("float"), Some("1.5")),
            field("d", primitive("double"), Some("2.5")),
            field("s", json!({"kind": "string"}), Some("hello")),
        ],
    }))
    .unwrap();
    let layout = playerbake::ScriptType::new(&layout).unwrap();
    let read = |name: &str, authored: serde_json::Value| {
        let mut document = serde_json::Map::new();
        document.insert(name.to_string(), authored);
        let object = layout.object(&document, 1, &mut NoReferences).unwrap();
        let value = layout.tree.decode(&object.data).unwrap();
        value.field(name).unwrap().clone()
    };
    let structure = || json!({"x": "1"});
    let sequence = || json!(["1", "2"]);
    let integers: [(&str, serde_json::Value, i128); 26] = [
        ("i5", json!("1.5"), 1),
        ("i5", json!("abc"), 0),
        ("i5", structure(), 5),
        ("i5", sequence(), 5),
        ("i0", json!("0x10"), 0),
        ("i0", json!("+5"), 5),
        ("i0", json!(" 7 "), 7),
        ("i0", json!("1e2"), 1),
        ("i0", json!("12abc"), 12),
        ("i0", json!("true"), 0),
        ("u", json!("-1"), 4294967295),
        ("sh", json!("70000"), 4464),
        ("by", json!("300"), 44),
        ("sb", json!("200"), -56),
        ("us", json!("-1"), 65535),
        ("l", json!("1e3"), 1),
        ("l", json!("99999999999999999999"), 7766279631452241919),
        ("ul", json!("-1"), 18446744073709551615),
        ("on", json!("true"), 0),
        ("on", json!("false"), 0),
        ("on", json!("2"), 2),
        ("c", json!("x"), 0),
        ("c", json!("65"), 65),
        ("c", json!("xy"), 0),
        ("e", json!("C"), 0),
        ("e", json!("99"), 99),
    ];
    for (name, authored, expected) in integers {
        let actual = match read(name, authored.clone()) {
            Val::I(value) => value as i128,
            Val::U(value) => value as i128,
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(actual, expected, "{name}: {authored}");
    }
    let nan = f32::NAN.to_bits();
    let floats: [(serde_json::Value, u32); 10] = [
        (json!("abc"), nan),
        (json!("inf"), nan),
        (json!("-inf"), nan),
        (json!("nan"), nan),
        (structure(), 1.5f32.to_bits()),
        (json!("1,5"), 1f32.to_bits()),
        (json!(".5"), 0.5f32.to_bits()),
        (json!("5."), 5f32.to_bits()),
        (json!("1e-3"), 0x3a83126f),
        (json!("1.5abc"), 1.5f32.to_bits()),
    ];
    for (authored, expected) in floats {
        match read("f", authored.clone()) {
            Val::F(value) => assert_eq!(value.to_bits(), expected, "{authored}"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(read("d", json!("1e3")), Val::D(1000.0));
    assert_eq!(read("d", sequence()), Val::D(2.5));
    assert_eq!(read("s", structure()), Val::Str(String::new()));
    assert_eq!(read("s", sequence()), Val::Str(String::new()));
    // A field the text lacks is as the constructor left it.
    assert_eq!(read("i0", json!("3")), Val::I(3));
    let object = layout
        .object(&serde_json::Map::new(), 1, &mut NoReferences)
        .unwrap();
    let made = layout.tree.decode(&object.data).unwrap();
    assert_eq!(made.field("i5"), Some(&Val::I(5)));
    assert_eq!(made.field("s"), Some(&Val::Str("hello".into())));
    assert_eq!(made.field("m_Enabled"), Some(&Val::U(1)));
}

/// A behaviour that holds objects by managed reference ends in a registry
/// of them, and its file in a table of their classes, which the type of the
/// behaviour names by position. The entries of that table are the engine's;
/// the objects themselves are compared in `behaviours_match_the_engine_build`.
#[test]
fn managed_references_travel_with_their_classes() {
    let root = fixture("behaviours");
    let fixtures = BehaviourFixtures::open(root.clone());
    let source = fs::read(root.join("Assets/GMan/N03_Typed.controller")).unwrap();
    let documents = parse_unity_yaml_lexical(&source).unwrap();
    let controller = playerbake::bake_controller_object(&documents, &fixtures).unwrap();
    let file = playerbake::bake_controller(&source, &fixtures)
        .unwrap()
        .baked
        .file;
    let mut held = Vec::new();
    for behaviour in &controller.behaviours {
        let (_, layout) = &controller.scripts[behaviour.script];
        for &position in &behaviour.managed {
            let kind = &layout.managed_types()[position];
            let name = (&kind.assembly, &kind.namespace, &kind.class_name);
            if !held.contains(&name) {
                held.push(name);
            }
            let reference = playerbake::ReferenceType {
                class_name: &kind.class_name,
                namespace: &kind.namespace,
                assembly: &kind.assembly,
                blob: &kind.blob,
            };
            let engine = root.join(format!(
                "reftypes/{}.{}.{}.entry.bin",
                kind.assembly,
                kind.namespace,
                kind.class_name.replace('/', "+")
            ));
            assert_eq!(
                reference.entry(0),
                fs::read(engine).unwrap(),
                "{}",
                kind.class_name
            );
            // The file holds the entry at some position of its table.
            let written = (0..16).any(|position| {
                let entry = reference.entry(position);
                file.windows(entry.len()).any(|window| window == entry)
            });
            assert!(written, "{}", kind.class_name);
        }
    }
    let mut names: Vec<String> = held
        .iter()
        .map(|(_, namespace, class)| format!("{namespace}.{class}"))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ".Box",
            ".Circle",
            ".GlobalNode",
            "Gap.Man.Branch",
            "Gap.Man.EmptyNode",
            "Gap.Man.Leaf",
            "Gap.Man.Outer/Inner",
            "Gap.Man.SNode"
        ]
    );
    // The file reads back with the objects it was written with.
    let objects = playerbake::read_serialized(&file).unwrap();
    for behaviour in &controller.behaviours {
        let id: i64 = behaviour.file_id.parse().unwrap();
        let object = objects.iter().find(|object| object.local_id == id).unwrap();
        assert!(object.data == behaviour.data);
    }
    // A registry has to hold exactly the objects the fields refer to, and
    // only objects of classes the layout knows.
    let text = String::from_utf8(source).unwrap();
    let refusal = |edited: String| {
        assert_ne!(edited, text);
        let error = playerbake::bake_controller(edited.as_bytes(), &fixtures)
            .map(|_| ())
            .unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
        error.to_string()
    };
    let dangling = refusal(text.replacen("  nullRef:\n    rid: -2", "  nullRef:\n    rid: 77", 1));
    assert!(dangling.contains("does not hold exactly"), "{dangling}");
    let unknown = refusal(text.replacen("class: EmptyNode,", "class: Missing,", 1));
    assert!(
        unknown.contains("a class the layout does not hold"),
        "{unknown}"
    );
}

/// A layer reads a mask stored inside the controller, which the baked file
/// does not hold. A behaviour field that refers to such a mask would need
/// the object itself.
#[test]
fn mask_inside_the_controller_is_read_but_not_referred_to() {
    let root = fixture("behaviours");
    let fixtures = BehaviourFixtures::open(root.clone());
    let source = fs::read(root.join("Assets/GRef/R03_Local.controller")).unwrap();
    let source = String::from_utf8(source).unwrap();
    assert!(playerbake::bake_controller(source.as_bytes(), &fixtures).is_ok());
    let mask =
        "--- !u!319 &777\nAvatarMask:\n  m_Name: Inside\n  m_Mask: 01000000u\n  m_Elements: []\n";
    let referred = source.replacen("avatarMask: {fileID: 0}", "avatarMask: {fileID: 777}", 1);
    assert_ne!(referred, source);
    let refused = playerbake::bake_controller(format!("{referred}{mask}").as_bytes(), &fixtures)
        .map(|_| ())
        .unwrap_err();
    assert!(refused.is::<Unsupported>());
    assert!(
        refused
            .to_string()
            .contains("avatarMask refers to an avatar mask stored inside the controller"),
        "{refused}"
    );
}

/// The Editor writes `Attack #2` without quotes and the engine reads it back
/// whole, where YAML sees a comment. The reader keeps such a value.
#[test]
fn text_yaml_reads_as_a_comment_is_kept() {
    let root = fixture("behaviours");
    let fixtures = BehaviourFixtures::open(root.clone());
    let source = fs::read(root.join("Assets/One/S01_Single.controller")).unwrap();
    let source = String::from_utf8(source).unwrap();
    assert!(source.contains("tag: single"));
    let plain = source.replace("tag: single", "tag: single #2");
    let quoted = source.replace("tag: single", "tag: 'single #2'");
    let plain = playerbake::bake_controller(plain.as_bytes(), &fixtures).unwrap();
    let quoted = playerbake::bake_controller(quoted.as_bytes(), &fixtures).unwrap();
    assert!(plain.baked.file == quoted.baked.file);
    let whole = playerbake::bake_controller(source.as_bytes(), &fixtures).unwrap();
    assert!(plain.baked.file != whole.baked.file);
}

/// Compares the controllers of a directory laid out like the behaviour
/// fixtures, such as the full set the fixtures were taken from.
#[test]
fn behaviour_corpus_matches_the_engine_build() {
    let Some(root) = std::env::var("UNITY_PLAYERBAKE_BEHAVIOUR_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_BEHAVIOUR_CORPUS");
        return;
    };
    let (_, failed, refused) = compare_behaviours(Path::new(&root));
    for (name, reason) in refused {
        eprintln!("refused {name}: {reason}");
    }
    let (_, failed_objects, refused) = compare_behaviour_objects(Path::new(&root));
    for (name, reason) in refused {
        eprintln!("refused {name}: {reason}");
    }
    assert_eq!((failed, failed_objects), (0, 0));
}

/// Bakes every `<name>.mask` of a directory and compares the object with the
/// engine's `<name>.mask.bin`. Returns how many matched.
fn compare_masks(root: &Path) -> usize {
    let (mut matched, mut failed) = (0, 0);
    let mut sources: Vec<PathBuf> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "mask")
        })
        .collect();
    sources.sort();
    for source in sources {
        let name = source.file_stem().unwrap().to_string_lossy().into_owned();
        let baked = match playerbake::bake_mask(&fs::read(&source).unwrap()) {
            Ok(baked) => baked,
            Err(error) => {
                failed += 1;
                eprintln!("ERROR {name}: {error:#}");
                continue;
            }
        };
        assert_eq!(
            baked.objects,
            [("31900000".to_string(), "UnityEngine.AvatarMask".to_string())]
        );
        assert!(baked.externals.is_empty());
        let objects = playerbake::read_serialized(&baked.file).unwrap();
        let [object] = objects.as_slice() else {
            panic!("{name}: a baked mask is one object");
        };
        assert_eq!((object.local_id, object.tree.class_id), (31900000, 319));
        let expected = fs::read(source.with_extension("mask.bin")).unwrap();
        if object.data == expected {
            matched += 1;
        } else {
            failed += 1;
            eprintln!(
                "MISMATCH {name}: {}",
                first_difference(&hex(&object.data), &hex(&expected))
            );
        }
    }
    assert_eq!(failed, 0);
    matched
}

/// Avatar masks built by the engine: body parts alone, transform paths with
/// their weights, and a mask as the Editor creates it.
#[test]
fn masks_match_the_engine_build() {
    assert_eq!(compare_masks(&fixture("masks")), 3);
}

/// Compares the masks of the directories in `UNITY_PLAYERBAKE_MASK_CORPUS`.
#[test]
fn mask_corpus_matches_the_engine_build() {
    let Some(roots) = std::env::var("UNITY_PLAYERBAKE_MASK_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_MASK_CORPUS");
        return;
    };
    let matched: usize = roots
        .split(':')
        .map(|root| compare_masks(Path::new(root)))
        .sum();
    eprintln!("matched {matched}");
    assert!(matched > 0);
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

#[test]
fn clip_with_scripts_carries_their_objects() {
    let source = fs::read(fixture("S_ScriptCurves.anim")).unwrap();
    let baked = playerbake::bake_clip(&source, &FixtureScripts).unwrap();
    // Only the clip is published; the MonoScript object travels with it.
    assert_eq!(baked.objects.len(), 1);
    let name = b"Probe.Space";
    assert!(baked.file.windows(name.len()).any(|window| window == name));
    let refused = playerbake::bake_clip(&source, &playerbake::NoScripts).unwrap_err();
    assert!(refused.is::<Unsupported>());
}

/// Blend spaces built by the engine: positions and the tables it derived,
/// all as single-precision bit patterns. `UNITY_PLAYERBAKE_FREEFORM_VECTORS`
/// names a larger file of the same form.
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
    let path = std::env::var("UNITY_PLAYERBAKE_FREEFORM_VECTORS")
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(|| fixture("freeform-vectors.json"), PathBuf::from);
    let cases: Vec<Case> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
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

/// Bakes the clips of import worker artifacts (`<guid>.artifact.bin`) and
/// compares each with the engine's object of the same clip name
/// (`<clip name>.clip.bin`) from a build of the same models.
#[test]
fn model_clip_corpus_matches_the_engine_build() {
    let Some(root) = std::env::var("UNITY_PLAYERBAKE_MODEL_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_MODEL_CORPUS");
        return;
    };
    let (mut matched, mut failed) = (0, 0);
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if !path.to_string_lossy().ends_with(".artifact.bin") {
            continue;
        }
        let clips = playerbake::bake_model_clips(&fs::read(&path).unwrap()).unwrap();
        for (file_id, reason) in &clips.refused {
            eprintln!("refused {file_id} of {}: {reason}", path.display());
        }
        let Some(baked) = clips.baked else { continue };
        for object in playerbake::read_serialized(&baked.file).unwrap() {
            let name = clips
                .facts
                .iter()
                .find(|(file_id, _, _)| *file_id == object.local_id.to_string())
                .map(|(_, name, _)| name.clone())
                .unwrap();
            let expected = fs::read(Path::new(&root).join(format!("{name}.clip.bin"))).unwrap();
            if object.data == expected {
                matched += 1;
            } else {
                failed += 1;
                let at = object
                    .data
                    .iter()
                    .zip(&expected)
                    .position(|(a, b)| a != b)
                    .unwrap_or(object.data.len().min(expected.len()));
                eprintln!(
                    "MISMATCH {name}: {} vs {} bytes, first difference at {at}",
                    object.data.len(),
                    expected.len()
                );
                if let Ok(directory) = std::env::var("UNITY_PLAYERBAKE_DUMP") {
                    fs::write(
                        Path::new(&directory).join(format!("{name}.baked.bin")),
                        &object.data,
                    )
                    .unwrap();
                }
            }
        }
    }
    eprintln!("matched {matched}");
    assert_eq!(failed, 0);
    assert!(matched > 0);
}

/// What of a reference project reaches its native assets.
struct Project {
    linear: bool,
    lightmaps: playerbake::Lightmaps,
    /// The shaders the build wrote, by GUID and file ID.
    shaders: Vec<((String, i64), playerbake::ShaderFacts)>,
    /// The spellings of shader tags in the materials the build wrote.
    tags: Vec<String>,
    /// The material sources of the project by GUID.
    materials: Vec<(String, PathBuf)>,
}

const LIT: playerbake::Lightmaps = playerbake::Lightmaps {
    baked: true,
    realtime: true,
};

impl Project {
    fn new(linear: bool, lightmaps: playerbake::Lightmaps) -> Project {
        Project {
            linear,
            lightmaps,
            shaders: Vec::new(),
            tags: Vec::new(),
            materials: Vec::new(),
        }
    }

    /// Reads `shaders/<guid>_<file ID>.bin`, the tags of the engine-built
    /// materials of a directory, as a session reads its catalog, and the
    /// GUID of every material that has a `.meta`.
    fn read(mut self, root: &Path) -> Project {
        if let Ok(entries) = fs::read_dir(root.join("shaders")) {
            for entry in entries {
                let path = entry.unwrap().path();
                let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                let (guid, file_id) = stem.split_once('_').unwrap();
                let facts = playerbake::read_shader_facts(&fs::read(&path).unwrap()).unwrap();
                self.shaders
                    .push(((guid.to_string(), file_id.parse().unwrap()), facts));
            }
        }
        let layout = playerbake::native_tree(21).unwrap();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if let Some(source) = path.to_string_lossy().strip_suffix(".mat.meta") {
                let meta = fs::read_to_string(&path).unwrap();
                let guid = meta.lines().find_map(|line| line.strip_prefix("guid: "));
                self.materials.push((
                    guid.unwrap().trim().to_string(),
                    PathBuf::from(format!("{source}.mat")),
                ));
            }
            if !path.to_string_lossy().ends_with(".mat.bin") {
                continue;
            }
            let material = layout.decode(&fs::read(&path).unwrap()).unwrap();
            let mut keep = |value: &Val| {
                if let Val::Str(tag) = value {
                    self.tags.push(tag.clone());
                }
            };
            if let Some(Val::Seq(pairs)) = material.field("stringTagMap") {
                for pair in pairs {
                    pair.field("first").into_iter().for_each(&mut keep);
                    pair.field("second").into_iter().for_each(&mut keep);
                }
            }
            if let Some(Val::Seq(passes)) = material.field("disabledShaderPasses") {
                passes.iter().for_each(&mut keep);
            }
        }
        self
    }
}

impl playerbake::NativeSources for Project {
    fn linear_color_space(&self) -> Option<bool> {
        Some(self.linear)
    }

    fn lightmaps(&self) -> Option<playerbake::Lightmaps> {
        Some(self.lightmaps)
    }

    fn shader(&self, guid: &str, file_id: i64) -> Option<playerbake::ShaderFacts> {
        self.shaders
            .iter()
            .find(|(key, _)| key.0 == guid && key.1 == file_id)
            .map(|(_, facts)| facts.clone())
    }

    fn tag_spelling(&self, tag: &str) -> Option<String> {
        self.tags
            .iter()
            .find(|known| known.eq_ignore_ascii_case(tag))
            .cloned()
    }

    fn material(&self, guid: &str) -> Option<Vec<u8>> {
        let (_, path) = self.materials.iter().find(|(known, _)| known == guid)?;
        fs::read(path).ok()
    }
}

/// Compares every `<source>` / `<source>.bin` pair of a directory: a
/// native-format source and the object the engine built from it. References
/// compare by identity, since a build numbers its objects its own way.
/// Returns how many matched, the sources the baker refused and the ones it
/// baked differently.
fn compare_native(root: &Path, project: &Project) -> (usize, Vec<String>, Vec<String>) {
    let (mut matched, mut refused, mut differing) = (0, Vec::new(), Vec::new());
    let mut sources: Vec<PathBuf> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension != "bin" && extension != "meta")
        })
        .collect();
    sources.sort();
    for source in sources {
        let name = source.file_name().unwrap().to_string_lossy().into_owned();
        // A parent the build did not write is there for its variants only.
        let Ok(expected) = fs::read(root.join(format!("{name}.bin"))) else {
            continue;
        };
        let baked = playerbake::bake_native_objects(&fs::read(&source).unwrap(), project);
        let (objects, _) = match baked {
            Ok(baked) => baked,
            Err(error) if error.is::<Unsupported>() => {
                refused.push(format!("{name}: {error}"));
                continue;
            }
            Err(error) => panic!("{name}: {error:#}"),
        };
        let object = &objects[0];
        if object.data != expected {
            let layout = playerbake::native_tree(object.class_id as i64).unwrap();
            let (mut actual, mut expected) = (
                layout.decode(&object.data).unwrap(),
                layout.decode(&expected).unwrap(),
            );
            label_references(&mut actual, &mut Vec::new());
            label_references(&mut expected, &mut Vec::new());
            let mut found = Vec::new();
            differences("", &actual, &expected, &mut found);
            if !found.is_empty() {
                differing.push(format!("{name}: {found:?}"));
                continue;
            }
        }
        matched += 1;
    }
    (matched, refused, differing)
}

#[test]
fn native_assets_match_the_engine_build() {
    let lit = |baked, realtime| playerbake::Lightmaps { baked, realtime };
    for (directory, linear, lightmaps, least) in [
        ("native", false, lit(true, true), 138),
        ("native/linear", true, lit(true, true), 62),
        ("native/linear-baked", true, lit(true, false), 10),
        ("native/linear-realtime", true, lit(false, true), 10),
        ("native/gamma-mixed", false, lit(false, false), 10),
    ] {
        let root = fixture(directory);
        let project = Project::new(linear, lightmaps).read(&root);
        let (matched, refused, differing) = compare_native(&root, &project);
        assert!(refused.is_empty(), "{refused:?}");
        assert!(differing.is_empty(), "{differing:?}");
        assert!(matched >= least, "fixtures of {directory} are missing");
    }
}

/// An asset whose Player form depends on a project setting is refused when
/// the project is not known, and baked when the setting cannot matter.
#[test]
fn native_assets_wait_for_their_project() {
    let bake = |name: &str| {
        playerbake::bake_native(
            &fs::read(fixture(name)).unwrap(),
            &playerbake::NoNativeSources,
        )
    };
    for name in ["native/RC_4_s1.renderTexture", "native/L_Default.lighting"] {
        let error = bake(name).unwrap_err();
        assert!(error.is::<Unsupported>(), "{name}: {error:#}");
    }
    for name in [
        "native/RC_4_s0.renderTexture",
        "native/R_Hdr.renderTexture",
        "native/P_Ice.physicMaterial",
    ] {
        bake(name).unwrap();
    }
}

/// `UNITY_PLAYERBAKE_NATIVE_CORPUS` names directories of such pairs. A
/// directory built by another project than the default one says so after
/// `@`: `l` for linear color space, `b` and `r` for the only kinds of
/// lightmap its render pipeline lights with.
#[test]
fn native_corpus_matches_the_engine_build() {
    let Some(roots) = std::env::var("UNITY_PLAYERBAKE_NATIVE_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_NATIVE_CORPUS");
        return;
    };
    let mut failed = 0;
    for root in roots.split(':') {
        let (root, project) = match root.split_once('@') {
            Some((root, flags)) => (
                root,
                Project::new(
                    flags.contains('l'),
                    playerbake::Lightmaps {
                        baked: flags.contains('b'),
                        realtime: flags.contains('r'),
                    },
                ),
            ),
            None => (root, Project::new(false, LIT)),
        };
        let project = project.read(Path::new(root));
        let (matched, refused, differing) = compare_native(Path::new(root), &project);
        eprintln!(
            "{root}: matched {matched}, refused {}, differing {}",
            refused.len(),
            differing.len()
        );
        for reason in refused.iter().chain(&differing) {
            eprintln!("  {reason}");
        }
        failed += differing.len();
    }
    assert_eq!(failed, 0);
}

/// A material is published alone: the Editor-only script objects a render
/// pipeline keeps next to it are left out, and its shader and textures are
/// named in order of first use.
#[test]
fn baked_material_keeps_its_identity() {
    let root = fixture("native");
    let project = Project::new(false, LIT).read(&root);
    let source = fs::read_to_string(root.join("GA003_lit.mat")).unwrap();
    let (head, material) = source.split_once("--- !u!21 ").unwrap();
    let with_record = format!(
        "{head}--- !u!114 &-7218417781224609008\nMonoBehaviour:\n  m_ObjectHideFlags: 11\n  \
         m_Script: {{fileID: 11500000, guid: d0353a89b1f911e48b9e16bdc9f2e058, type: 3}}\n  \
         version: 10\n--- !u!21 {material}"
    );
    let expected = fs::read(root.join("GA003_lit.mat.bin")).unwrap();
    for source in [source.as_str(), with_record.as_str()] {
        let baked = playerbake::bake_native(source.as_bytes(), &project).unwrap();
        assert_eq!(
            baked.objects,
            [("2100000".to_string(), "UnityEngine.Material".to_string())]
        );
        assert_eq!(
            baked.externals[0].guid, "73dcab8d6bcd67bd996f010a4f58f089",
            "the shader comes first"
        );
        assert!(baked.externals.len() > 1, "textures follow");
        let objects = playerbake::read_serialized(&baked.file).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].data.len(), expected.len());
    }
}

/// What the baker does not reproduce stays with the Editor.
#[test]
fn materials_beyond_the_baker_are_refused() {
    let root = fixture("native");
    let project = Project::new(false, LIT).read(&root);
    let source = fs::read_to_string(root.join("GA003_lit.mat")).unwrap();
    let refused = |source: &str, sources: &dyn playerbake::NativeSources| {
        let error = playerbake::bake_native(source.as_bytes(), sources).unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
        error.to_string()
    };
    assert!(refused(&source, &playerbake::NoNativeSources).contains("shader is not known"));
    let variant = source.replace(
        "m_Parent: {fileID: 0}",
        "m_Parent: {fileID: 2100000, guid: 7ce7eb525fddc12529a8908a725a914e, type: 2}",
    );
    assert!(
        refused(&variant, &project)
            .contains("parent 7ce7eb525fddc12529a8908a725a914e is not known")
    );
    let older = source.replace("  serializedVersion: 8\n", "  serializedVersion: 6\n");
    assert!(refused(&older, &project).contains("serialized version 6"));
    // The engine writes a tag the way it first met it. One it
    // registers late, or the shader spells otherwise, could have been met
    // either way.
    let mut unsure = Project::new(false, LIT).read(&root);
    unsure.tags.clear();
    let respelled = source.replace("stringTagMap: {}", "stringTagMap:\n    Custom: opaque");
    assert_ne!(respelled, source);
    assert!(refused(&respelled, &unsure).contains("spelled"));
    // One the engine registers itself is always written its way.
    let engine_tag = source.replace("  - FORWARDBASE\n", "  - ForwardBase\n");
    assert_ne!(engine_tag, source);
    let bake = |source: &str| {
        let (objects, _) = playerbake::bake_native_objects(source.as_bytes(), &unsure).unwrap();
        objects.into_iter().next().unwrap().data
    };
    assert_eq!(bake(&engine_tag), bake(&source));
}

/// A variant is published as a whole material: its parent is read, not
/// referred to, and the shader is the one at the top of the chain.
#[test]
fn baked_variant_stands_alone() {
    let root = fixture("native");
    let project = Project::new(false, LIT).read(&root);
    let read = |name: &str| fs::read_to_string(root.join(name)).unwrap();
    // The variant names another shader than its parent's parent.
    let source = read("VA_othershader2.mat");
    assert!(source.contains("guid: 33333333aaaa4bbb8ccc0123456789ab"));
    let baked = playerbake::bake_native(source.as_bytes(), &project).unwrap();
    assert_eq!(
        baked.objects,
        [("2100000".to_string(), "UnityEngine.Material".to_string())]
    );
    assert_eq!(baked.externals[0].guid, "73dcab8d6bcd67bd996f010a4f58f089");
    let parents = ["VA_othershader.mat.meta", "VA_root.mat.meta"].map(|meta| {
        let meta = read(meta);
        let guid = meta.lines().find_map(|line| line.strip_prefix("guid: "));
        guid.unwrap().trim().to_string()
    });
    for external in &baked.externals {
        assert!(!parents.contains(&external.guid), "{}", external.guid);
    }
    // A change of the parent reaches the variant.
    struct Edited<'a>(&'a Project, String, Vec<u8>);
    impl playerbake::NativeSources for Edited<'_> {
        fn linear_color_space(&self) -> Option<bool> {
            self.0.linear_color_space()
        }
        fn shader(&self, guid: &str, file_id: i64) -> Option<playerbake::ShaderFacts> {
            self.0.shader(guid, file_id)
        }
        fn tag_spelling(&self, tag: &str) -> Option<String> {
            self.0.tag_spelling(tag)
        }
        fn material(&self, guid: &str) -> Option<Vec<u8>> {
            if guid == self.1 {
                Some(self.2.clone())
            } else {
                self.0.material(guid)
            }
        }
    }
    let bake = |sources: &dyn playerbake::NativeSources| {
        let source = read("VA_plain.mat");
        let (objects, _) = playerbake::bake_native_objects(source.as_bytes(), sources).unwrap();
        objects.into_iter().next().unwrap().data
    };
    let parent = read("VA_root.mat");
    let edited = parent.replace("m_CustomRenderQueue: 2450", "m_CustomRenderQueue: 2451");
    assert_ne!(edited, parent);
    let edited = Edited(&project, parents[1].clone(), edited.into_bytes());
    assert_ne!(bake(&edited), bake(&project));
}

/// What a build does to a variant beyond the merge stays with the Editor.
#[test]
fn variants_beyond_the_baker_are_refused() {
    let root = fixture("native");
    let project = Project::new(false, LIT).read(&root);
    let read = |name: &str| fs::read_to_string(root.join(name)).unwrap();
    let refused = |source: &str, sources: &dyn playerbake::NativeSources| {
        let error = playerbake::bake_native(source.as_bytes(), sources).unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
        format!("{error:#}")
    };
    assert!(refused(&read("VD_orphan.mat"), &project).contains("is not known"));
    assert!(refused(&read("VD_wrongid.mat"), &project).contains("holds no material 2100002"));
    assert!(refused(&read("VA_plain.mat"), &playerbake::NoNativeSources).contains("is not known"));
    // A chain that returns to itself.
    let guid = |name: &str| {
        let meta = read(&format!("{name}.meta"));
        let guid = meta.lines().find_map(|line| line.strip_prefix("guid: "));
        guid.unwrap().trim().to_string()
    };
    let own = guid("VA_plain.mat");
    let itself = read("VA_plain.mat").replace(&guid("VA_root.mat"), &own);
    struct Itself(Vec<u8>);
    impl playerbake::NativeSources for Itself {
        fn material(&self, _guid: &str) -> Option<Vec<u8>> {
            Some(self.0.clone())
        }
    }
    assert!(refused(&itself, &Itself(itself.clone().into_bytes())).contains("cycle"));
    // The inspector a shader names validates a variant in its own code, and
    // a drawer may be the project's.
    let mut edited = Project::new(false, LIT).read(&root);
    for (_, facts) in &mut edited.shaders {
        facts.custom_editors.push("Some.ShaderGUI".to_string());
    }
    assert!(refused(&read("VA_plain.mat"), &edited).contains("Some.ShaderGUI validates"));
    playerbake::bake_native(read("VA_root.mat").as_bytes(), &edited).unwrap();
    let mut edited = Project::new(false, LIT).read(&root);
    for (_, facts) in &mut edited.shaders {
        for property in &mut facts.properties {
            if property.name == "_Gloss" {
                property.attributes.push("Fancy(1)".to_string());
            }
        }
    }
    assert!(refused(&read("VA_plain.mat"), &edited).contains("drawer of the project"));
    playerbake::bake_native(read("VA_root.mat").as_bytes(), &edited).unwrap();
}

#[test]
fn baked_native_asset_keeps_its_identity() {
    let source = fs::read(fixture("native/K_Upper.mask")).unwrap();
    let baked = playerbake::bake_native(&source, &playerbake::NoNativeSources).unwrap();
    assert_eq!(
        baked.objects,
        [("31900000".to_string(), "UnityEngine.AvatarMask".to_string())]
    );
    assert!(baked.externals.is_empty());
    let objects = playerbake::read_serialized(&baked.file).unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].local_id, 31900000);
    assert_eq!(
        objects[0].data,
        fs::read(fixture("native/K_Upper.mask.bin")).unwrap()
    );
}

/// Compares every `<name>.mixer` / `<name>.mixer.bin` pair of a directory.
/// The second holds the objects the engine built from the first, each as
/// file ID (64 bits), class and length (32 bits each) and bytes, with the
/// references a build renumbers written back as authored file IDs. Returns
/// how many objects matched, the sources the baker refused and the ones it
/// baked differently, and how many values a build leaves undefined were
/// passed over.
///
/// A build writes memory nothing has set for a parameter its effect declares
/// but does not keep. Exactly those values, which the baker names, compare
/// as equal whatever the build wrote; every other byte has to match.
fn compare_mixers(root: &Path) -> (usize, Vec<String>, Vec<String>, usize) {
    let (mut matched, mut refused, mut differing) = (0, Vec::new(), Vec::new());
    let mut passed_over = 0;
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
            expected.push((id, class, packed[at + 16..at + 16 + length].to_vec()));
            at += 16 + length;
        }
        let baked = playerbake::bake_native_objects(
            &fs::read(&source).unwrap(),
            &playerbake::NoNativeSources,
        );
        let (mut objects, _) = match baked {
            Ok(baked) => baked,
            Err(error) if error.is::<Unsupported>() => {
                refused.push(format!("{name}: {error}"));
                continue;
            }
            Err(error) => panic!("{name}: {error:#}"),
        };
        objects.sort_by_key(|object| object.file_id.parse::<i64>().unwrap());
        let undefined = playerbake::mixer_undefined_values(&fs::read(&source).unwrap()).unwrap();
        if !undefined.is_empty() {
            let layout = playerbake::native_tree(241).unwrap();
            let baked = objects.iter().find(|object| object.class_id == 241);
            let mut baked = layout.decode(&baked.unwrap().data).unwrap();
            let built = expected.iter_mut().find(|object| object.1 == 241).unwrap();
            let mut decoded = layout.decode(&built.2).unwrap();
            // Rewriting the object changes nothing but the values set here.
            assert_eq!(layout.encode(&decoded).unwrap(), built.2, "{name}");
            for &(snapshot, index) in &undefined {
                *value(&mut decoded, snapshot, index) = value(&mut baked, snapshot, index).clone();
            }
            built.2 = layout.encode(&decoded).unwrap();
            passed_over += undefined.len();
        }
        let same = objects.len() == expected.len()
            && objects.iter().zip(&expected).all(|(object, expected)| {
                (
                    object.file_id.parse::<i64>().unwrap(),
                    object.class_id,
                    &object.data,
                ) == (expected.0, expected.1, &expected.2)
            });
        if !same {
            let mut found = Vec::new();
            for (object, expected) in objects.iter().zip(&expected) {
                if object.class_id == expected.1 && object.data != expected.2 {
                    let layout = playerbake::native_tree(object.class_id as i64).unwrap();
                    differences(
                        &object.file_id,
                        &layout.decode(&object.data).unwrap(),
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
            continue;
        }
        matched += objects.len();
    }
    (matched, refused, differing, passed_over)
}

/// Mixers authored through the Editor (groups several deep, every built-in
/// effect, sends and ducking, snapshots, transitions, exposed parameters, a
/// mixer playing into another) and hand-written ones the Editor would not
/// author, each with the objects of an engine build.
#[test]
fn mixers_match_the_engine_build() {
    let (matched, refused, differing, passed_over) = compare_mixers(&fixture("mixers"));
    assert!(refused.is_empty(), "{refused:?}");
    assert!(differing.is_empty(), "{differing:?}");
    assert!(matched >= 272, "mixer fixtures are missing: {matched}");
    // Three of them hold a chorus with its feedback left to the default.
    assert_eq!(passed_over, 5);
}

/// `UNITY_PLAYERBAKE_MIXER_CORPUS` names directories of such pairs.
#[test]
fn mixer_corpus_matches_the_engine_build() {
    let Some(roots) = std::env::var("UNITY_PLAYERBAKE_MIXER_CORPUS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_MIXER_CORPUS");
        return;
    };
    let mut failed = 0;
    for root in roots.split(':') {
        let (matched, refused, differing, passed_over) = compare_mixers(Path::new(root));
        eprintln!(
            "{root}: matched {matched} objects, refused {}, differing {}, undefined values passed over {passed_over}",
            refused.len(),
            differing.len()
        );
        for reason in refused.iter().chain(&differing) {
            eprintln!("  {reason}");
        }
        failed += differing.len();
    }
    assert_eq!(failed, 0);
}

/// What a build cannot be followed in is refused with its reason.
#[test]
fn mixers_the_engine_builds_otherwise_are_refused() {
    let read = |name: &str| fs::read_to_string(fixture(name)).unwrap();
    let refused = |source: &str| {
        let error = playerbake::bake_native(source.as_bytes(), &playerbake::NoNativeSources)
            .expect_err("the mixer is refused");
        assert!(error.is::<Unsupported>(), "{error:#}");
        error.to_string()
    };
    let changed = |source: &str, from: &str, to: &str| {
        let changed = source.replacen(from, to, 1);
        assert_ne!(changed, source, "{from:?} is not in the fixture");
        changed
    };
    let default = read("mixers/A_Default.mixer");
    let flags = read("mixers/X_06AttFlags.mixer");

    // A build leaves the feedback of a chorus undefined in a snapshot that
    // does not store it. That is baked, as the default the effect declares.
    assert!(
        playerbake::mixer_undefined_values(flags.as_bytes())
            .unwrap()
            .is_empty()
    );
    let unstored = changed(&flags, "    52c0599e41537ce330924f84d9b78c99: -0.25\n", "");
    let undefined = playerbake::mixer_undefined_values(unstored.as_bytes()).unwrap();
    let [(snapshot, index)] = undefined[..] else {
        panic!("one value is undefined: {undefined:?}");
    };
    let (objects, _) =
        playerbake::bake_native_objects(unstored.as_bytes(), &playerbake::NoNativeSources).unwrap();
    let mixer = objects
        .iter()
        .find(|object| object.class_id == 241)
        .unwrap();
    let mixer = playerbake::native_tree(241)
        .unwrap()
        .decode(&mixer.data)
        .unwrap();
    let values = |snapshot: usize| {
        let snapshots = mixer
            .field("m_MixerConstant")
            .and_then(|constant| constant.field("snapshots"));
        match snapshots {
            Some(Val::Seq(snapshots)) => snapshots[snapshot].field("values").cloned(),
            _ => None,
        }
    };
    let Some(Val::Seq(baked)) = values(snapshot) else {
        panic!("the snapshot has no values");
    };
    assert_eq!(baked[index], Val::F(0.0));
    // The other snapshot still stores its own.
    let Some(Val::Seq(other)) = values(1 - snapshot) else {
        panic!("the snapshot has no values");
    };
    assert_eq!(other[index], Val::F(0.5));
    // An effect of a native plugin.
    let plugin = changed(
        &flags,
        "m_EffectName: Lowpass",
        "m_EffectName: Vendor Limiter",
    );
    assert!(refused(&plugin).contains("not built into the engine"));
    // A parameter list of another engine.
    let renamed = changed(
        &flags,
        "m_ParameterName: Resonance",
        "m_ParameterName: Slope",
    );
    assert!(refused(&renamed).contains("without its parameter \"Resonance\""));
    // The engine reads a transition type it does not know as another.
    let transition = changed(
        &default,
        "m_TransitionOverrides: {}",
        "m_TransitionOverrides:\n    9a9afe3e66bd4a56a990dc8ce370365b: 9",
    );
    assert!(refused(&transition).contains("unknown transition type"));
    let silent = changed(
        &default,
        "m_Snapshots:\n  - {fileID: 24500006}",
        "m_Snapshots: []",
    );
    assert!(refused(&silent).contains("without snapshots"));
    let shared = changed(
        &default,
        "m_Pitch: ad25f5b2c33c36863b09448c27423b56",
        "m_Pitch: 9a9afe3e66bd4a56a990dc8ce370365b",
    );
    assert!(refused(&shared).contains("shares its GUID"));
    let foreign = format!("{default}--- !u!21 &5\nMaterial:\n  m_Name: Stray\n");
    assert!(refused(&foreign).contains("object of class 21"));
    // A group without its mixer.
    let alone = changed(
        &default,
        "--- !u!241 &24100000\nAudioMixerController:",
        "--- !u!243 &24100000\nAudioMixerGroupController:",
    );
    assert!(refused(&alone).contains("object of class 244"));
}

/// A baked mixer publishes the mixer, its groups and its snapshots under
/// their authored file IDs, and names the mixer it plays into.
#[test]
fn baked_mixer_keeps_its_identities() {
    let bake = |name: &str| {
        playerbake::bake_native(
            &fs::read(fixture(name)).unwrap(),
            &playerbake::NoNativeSources,
        )
        .unwrap()
    };
    let parent = bake("mixers/I_Parent.mixer");
    assert!(parent.externals.is_empty());
    let mut types: Vec<&str> = parent
        .objects
        .iter()
        .map(|(_, name)| name.as_str())
        .collect();
    types.sort();
    assert_eq!(
        types,
        [
            "UnityEngine.Audio.AudioMixer",
            "UnityEngine.Audio.AudioMixerGroup",
            "UnityEngine.Audio.AudioMixerGroup",
            "UnityEngine.Audio.AudioMixerSnapshot",
        ]
    );
    assert!(
        parent
            .objects
            .contains(&("24100000".to_string(), types[0].to_string()))
    );
    let objects = playerbake::read_serialized(&parent.file).unwrap();
    assert_eq!(objects.len(), parent.objects.len());
    assert!(objects.iter().any(|object| object.local_id == 24100000));

    let child = bake("mixers/J_Child.mixer");
    assert_eq!(child.externals.len(), 1);
    let source = fs::read_to_string(fixture("mixers/J_Child.mixer")).unwrap();
    assert!(source.contains(&format!("guid: {}", child.externals[0].guid)));
}
