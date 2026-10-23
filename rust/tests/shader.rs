//! Compares baked `Shader` objects with the ones Unity 6000.6.3f1 wrote into
//! AssetBundles for the same sources.
//!
//! A fixture under `testdata/playerbake/shaders` is the `Shader` object of
//! the import worker's artifact (`<name>.editor.bin`, laid out by
//! `editor-type-48.bin`), the materials of the reference build
//! (`<name>.materials`), the replies `UnityShaderCompiler` gave to the
//! requests of the bake (`<name>.replies.bin`, each under the hash of its
//! request) and the engine's object (`<name>.shader.bin`). A request the
//! recording lacks fails the test, so the fixtures also pin the requests.
use sha2::Digest;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};
use unity_micro_editor::playerbake::{
    self, ShaderDependency, ShaderMaterialUse, ShaderPointer, ShaderProject, ShaderSettings,
    ShaderSources, SnippetCompiler, SnippetReply, SnippetRequest, TypeTree, Unsupported, Val,
    WIRE_MARKER,
};

/// The engine's objects of a set of reference builds, by shader name: what a
/// shader that falls back on another or uses its passes is baked against.
/// The references to them are given the values the engine's object has,
/// which depend on how its build was laid out.
#[derive(Default)]
struct References {
    objects: HashMap<String, Vec<u8>>,
    pointers: RefCell<VecDeque<(i32, i64)>>,
}

fn decoded(object: &[u8]) -> Val {
    playerbake::shader_tree().unwrap().decode(object).unwrap()
}

impl References {
    fn add(&mut self, directory: &Path) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if !path.to_str().unwrap().ends_with(".shader.bin") {
                continue;
            }
            let object = fs::read(&path).unwrap();
            let value = decoded(&object);
            let Some(Val::Str(name)) = value.field("m_ParsedForm").unwrap().field("m_Name") else {
                panic!("{} has no name", path.display());
            };
            self.objects.insert(name.clone(), object);
        }
    }

    /// Takes the references of the engine's object of a shader, to hand
    /// them out in the order it lists them.
    fn expect(&self, expected: &[u8]) {
        let value = decoded(expected);
        let Some(Val::Seq(listed)) = value.field("m_Dependencies") else {
            panic!("object lists no dependencies");
        };
        *self.pointers.borrow_mut() = listed
            .iter()
            .map(
                |pointer| match (pointer.field("m_FileID"), pointer.field("m_PathID")) {
                    (Some(Val::I(file)), Some(Val::I(path))) => (*file as i32, *path),
                    _ => panic!("dependency is not a pointer"),
                },
            )
            .collect();
    }
}

impl ShaderSources for References {
    fn shader(&self, name: &str) -> anyhow::Result<Option<ShaderDependency>> {
        let Some(object) = self.objects.get(name) else {
            return Ok(None);
        };
        let (file_id, path_id) = self
            .pointers
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("the engine's object lists fewer shaders"))?;
        Ok(Some(ShaderDependency {
            pointer: ShaderPointer::Raw { file_id, path_id },
            links: playerbake::read_shader_links(object)?,
        }))
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rust/tests/testdata/playerbake/shaders")
}

/// The materials of a reference build: per line the instancing switch and
/// the keywords.
fn materials(path: &Path) -> Vec<ShaderMaterialUse> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .map(|line| {
            let mut fields = line.split_ascii_whitespace();
            ShaderMaterialUse {
                instancing: fields.next() == Some("1"),
                keywords: fields.map(str::to_string).collect(),
            }
        })
        .collect()
}

fn settings(directory: &Path) -> anyhow::Result<ShaderSettings> {
    let read = |name: &str| fs::read_to_string(directory.join(name)).unwrap_or_default();
    ShaderSettings::read(
        &read("ProjectSettings/ProjectSettings.asset"),
        &read("ProjectSettings/GraphicsSettings.asset"),
        &read("ProjectSettings/QualitySettings.asset"),
        &read("manifest.json"),
    )
}

fn refusal(error: &anyhow::Error) -> Option<String> {
    error
        .chain()
        .any(|cause| cause.is::<Unsupported>())
        .then(|| format!("{error:#}"))
}

fn first_difference(actual: &[u8], expected: &[u8]) -> String {
    let at = actual
        .iter()
        .zip(expected)
        .position(|(a, b)| a != b)
        .unwrap_or(actual.len().min(expected.len()));
    format!(
        "{} bytes against {}, first difference at {at}",
        actual.len(),
        expected.len()
    )
}

/// Replies recorded from the compiler, by the SHA-256 of the request.
struct Recorded(HashMap<[u8; 32], SnippetReply>);

fn digest(request: &SnippetRequest) -> [u8; 32] {
    let encoded = request.encode();
    assert_eq!(encoded[..4], WIRE_MARKER);
    sha2::Sha256::digest(&encoded).into()
}

fn take<'a>(data: &mut &'a [u8], count: usize) -> &'a [u8] {
    let (head, rest) = data.split_at(count);
    *data = rest;
    head
}
fn take_u32(data: &mut &[u8]) -> u32 {
    u32::from_le_bytes(take(data, 4).try_into().unwrap())
}

impl Recorded {
    /// A recording is a sequence of requests, each the hash of its wire
    /// form, followed by its reply: the lines, the verdict, the program's
    /// kind and the program.
    fn read(path: &Path) -> Self {
        let file = fs::read(path).unwrap_or_default();
        let mut data = &file[..];
        let mut replies = HashMap::new();
        while !data.is_empty() {
            let request: [u8; 32] = take(&mut data, 32).try_into().unwrap();
            let mut lines = Vec::new();
            for _ in 0..take_u32(&mut data) {
                let length = take_u32(&mut data) as usize;
                lines.push(take(&mut data, length).to_vec());
            }
            let compiled = take(&mut data, 1)[0] != 0;
            let program_type = take_u32(&mut data) as i32;
            let length = take_u32(&mut data) as usize;
            let program = take(&mut data, length).to_vec();
            replies.insert(
                request,
                SnippetReply {
                    lines,
                    compiled,
                    program_type,
                    program,
                },
            );
        }
        Self(replies)
    }
}

impl SnippetCompiler for Recorded {
    fn compile(&mut self, request: &SnippetRequest) -> anyhow::Result<SnippetReply> {
        self.0.get(&digest(request)).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "no recorded reply for pass {:?}, platform {}, keywords {:?}",
                request.pass_name,
                request.platform,
                request.keywords
            )
        })
    }
}

/// A compiler that also keeps what it was asked and what it answered.
struct Recording<C> {
    compiler: C,
    out: Vec<u8>,
}

impl<C: SnippetCompiler> SnippetCompiler for Recording<C> {
    fn compile(&mut self, request: &SnippetRequest) -> anyhow::Result<SnippetReply> {
        let reply = self.compiler.compile(request)?;
        let out = &mut self.out;
        out.extend_from_slice(&digest(request));
        out.extend_from_slice(&(reply.lines.len() as u32).to_le_bytes());
        for line in &reply.lines {
            out.extend_from_slice(&(line.len() as u32).to_le_bytes());
            out.extend_from_slice(line);
        }
        out.push(reply.compiled as u8);
        out.extend_from_slice(&reply.program_type.to_le_bytes());
        out.extend_from_slice(&(reply.program.len() as u32).to_le_bytes());
        out.extend_from_slice(&reply.program);
        Ok(reply)
    }
}

/// The worker's artifact of a fixture: its `Shader` object in a file of its
/// own, with the Editor's layout of the class.
fn artifact(name: &str) -> Vec<u8> {
    let tree = TypeTree::parse(fs::read(fixtures().join("editor-type-48.bin")).unwrap()).unwrap();
    let data = fs::read(fixtures().join(format!("{name}.editor.bin"))).unwrap();
    playerbake::write(
        &[playerbake::Object {
            local_id: playerbake::SHADER_LOCAL_ID,
            tree: &tree,
            data,
            script: None,
            script_file: 0,
        }],
        &[],
    )
    .unwrap()
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixtures())
        .unwrap()
        .filter_map(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_str()?
                .strip_suffix(".editor.bin")
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

fn bake_fixture(name: &str, settings: ShaderSettings) -> anyhow::Result<Vec<u8>> {
    let project = ShaderProject {
        platforms: vec![15, 18],
        settings,
        materials: materials(&fixtures().join(format!("{name}.materials"))),
    };
    let mut references = References::default();
    references.add(&fixtures());
    references.expect(&fs::read(fixtures().join(format!("{name}.shader.bin"))).unwrap());
    let mut compiler = Recorded::read(&fixtures().join(format!("{name}.replies.bin")));
    let (object, externals) =
        playerbake::bake_shader_object(&artifact(name), &project, &references, &mut compiler)?;
    assert!(externals.is_empty());
    Ok(object)
}

#[test]
fn shaders_match_the_engine_build() {
    let names = fixture_names();
    assert!(names.len() >= 8, "shader fixtures are missing");
    for name in names {
        let expected = fs::read(fixtures().join(format!("{name}.shader.bin"))).unwrap();
        let baked = bake_fixture(&name, ShaderSettings::default())
            .unwrap_or_else(|error| panic!("{name}: {error:#}"));
        assert!(
            baked == expected,
            "{name}: {}",
            first_difference(&baked, &expected)
        );
    }
}

/// Shaders published under GUIDs, as a session finds them.
struct Published(Vec<(&'static str, &'static str, &'static str)>);
impl ShaderSources for Published {
    fn shader(&self, name: &str) -> anyhow::Result<Option<ShaderDependency>> {
        let Some((_, guid, fixture)) = self.0.iter().find(|(known, _, _)| *known == name) else {
            return Ok(None);
        };
        let object = fs::read(fixtures().join(format!("{fixture}.shader.bin")))?;
        Ok(Some(ShaderDependency {
            pointer: ShaderPointer::External {
                guid: guid.to_string(),
            },
            links: playerbake::read_shader_links(&object)?,
        }))
    }
}

fn bake_linked(name: &str, sources: &dyn ShaderSources) -> anyhow::Result<playerbake::Baked> {
    let project = ShaderProject {
        platforms: vec![15, 18],
        settings: ShaderSettings::default(),
        materials: materials(&fixtures().join(format!("{name}.materials"))),
    };
    let mut compiler = Recorded::read(&fixtures().join(format!("{name}.replies.bin")));
    playerbake::bake_shader(&artifact(name), &project, sources, &mut compiler)
}

#[test]
fn fallback_and_used_passes_refer_to_other_published_shaders() {
    let fallback = "5ade5ade5ade5ade5ade5ade00000001";
    let used = "5ade5ade5ade5ade5ade5ade00000002";
    let sources = Published(vec![
        ("Corpus/S01", fallback, "s01_unlit"),
        ("Corpus/S02", used, "s02_variants"),
    ]);
    for (name, guid) in [("s11_fallback", fallback), ("s12_usepass", used)] {
        let baked = bake_linked(name, &sources).unwrap();
        assert_eq!(
            baked.externals,
            [playerbake::External {
                guid: guid.to_string(),
                path: String::new(),
            }],
            "{name}"
        );
        // The object lists the other shader as the first external file's
        // Shader; everything else is the engine's object.
        let objects = playerbake::read_serialized(&baked.file).unwrap();
        let mut value = decoded(&objects[0].data);
        let listed = value.field_mut("m_Dependencies").unwrap();
        assert_eq!(
            *listed,
            Val::Seq(vec![Val::map([
                ("m_FileID", Val::I(1)),
                ("m_PathID", Val::I(4_800_000))
            ])]),
            "{name}"
        );
        let mut expected =
            decoded(&fs::read(fixtures().join(format!("{name}.shader.bin"))).unwrap());
        *expected.field_mut("m_Dependencies").unwrap() = listed.clone();
        assert!(value == expected, "{name}");
    }
    // A name the project has no source for is refused, whichever way it is
    // named.
    for name in ["s11_fallback", "s12_usepass"] {
        let error = bake_linked(name, &playerbake::NoShaders).unwrap_err();
        let reason = refusal(&error).unwrap_or_else(|| panic!("{name}: {error:#}"));
        assert!(reason.contains("does not hold as a source"), "{reason}");
    }
}

/// A compiler that answers every request with an error, or with a warning
/// beside the program it was recorded to give.
struct Complaining(Option<Recorded>);
impl SnippetCompiler for Complaining {
    fn compile(&mut self, request: &SnippetRequest) -> anyhow::Result<SnippetReply> {
        let complaint = [
            b"err: 2 15 16".to_vec(),
            b"Assets/Corpus/s01_unlit.shader".to_vec(),
            b"undeclared identifier 'colour'".to_vec(),
        ];
        match &mut self.0 {
            Some(recorded) => {
                let mut reply = recorded.compile(request)?;
                reply.lines.extend(complaint);
                Ok(reply)
            }
            None => Ok(SnippetReply {
                lines: complaint.to_vec(),
                compiled: false,
                program_type: 0,
                program: Vec::new(),
            }),
        }
    }
}

#[test]
fn a_program_the_compiler_complains_about_is_refused() {
    let name = "s01_unlit";
    let project = ShaderProject {
        platforms: vec![15, 18],
        settings: ShaderSettings::default(),
        materials: Vec::new(),
    };
    let bake = |mut compiler: Complaining| {
        let error = playerbake::bake_shader(
            &artifact(name),
            &project,
            &playerbake::NoShaders,
            &mut compiler,
        )
        .unwrap_err();
        refusal(&error).unwrap_or_else(|| panic!("not a refusal: {error:#}"))
    };
    assert!(
        bake(Complaining(None))
            .contains("Assets/Corpus/s01_unlit.shader(16): undeclared identifier 'colour'")
    );
    let recorded = Recorded::read(&fixtures().join(format!("{name}.replies.bin")));
    assert!(bake(Complaining(Some(recorded))).contains("warns about"));
}

#[test]
fn baked_shader_is_a_content_file_with_the_authored_identity() {
    let name = &fixture_names()[0];
    let project = ShaderProject {
        platforms: vec![15, 18],
        settings: ShaderSettings::default(),
        materials: materials(&fixtures().join(format!("{name}.materials"))),
    };
    let mut compiler = Recorded::read(&fixtures().join(format!("{name}.replies.bin")));
    let baked = playerbake::bake_shader(
        &artifact(name),
        &project,
        &playerbake::NoShaders,
        &mut compiler,
    )
    .unwrap();
    assert_eq!(
        baked.objects,
        [("4800000".to_string(), "UnityEngine.Shader".to_string())]
    );
    assert!(baked.externals.is_empty());
    let objects = playerbake::read_serialized(&baked.file).unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].local_id, 4_800_000);
    assert_eq!(
        objects[0].data,
        fs::read(fixtures().join(format!("{name}.shader.bin"))).unwrap()
    );
}

#[test]
fn unreproduced_shader_settings_are_refused() {
    let refused = |graphics: &str, quality: &str, player: &str| {
        let error = ShaderSettings::read(player, graphics, quality, "").unwrap_err();
        refusal(&error).unwrap_or_else(|| panic!("not a refusal: {error:#}"))
    };
    assert!(
        refused(
            "  m_CustomRenderPipeline: {fileID: 11400000, guid: 4b83569d67af61e458304325a23e5dfd, type: 2}\n",
            "",
            ""
        )
        .contains("scriptable render pipeline")
    );
    assert!(
        refused("", "    customRenderPipeline: {fileID: 11400000, guid: 4b83569d67af61e458304325a23e5dfd, type: 2}\n", "")
            .contains("scriptable render pipeline")
    );
    assert!(
        refused(
            "  m_ShaderBuildSettings:\n    keywordDeclarationOverrides: []\n    numInternalDefines: 0\n    defines:\n    - A\n    compilerSettings: []\n  m_LightsUseLinearIntensity: 0\n",
            "",
            ""
        )
        .contains("shader build settings")
    );
    assert!(refused("", "", "  enable360StereoCapture: 1\n").contains("360"));
    // The settings of a new project are the defaults, and so is no file.
    let defaults = ShaderSettings::read(
        "  m_ActiveColorSpace: 0\n  m_DefaultShaderChunkSizeInMB: 16\n  m_DefaultShaderChunkCount: 0\n",
        "  m_CustomRenderPipeline: {fileID: 0}\n  m_ShaderBuildSettings:\n    keywordDeclarationOverrides: []\n    numInternalDefines: 0\n    defines: []\n    compilerSettings: []\n  m_TierSettings: []\n",
        "    customRenderPipeline: {fileID: 0}\n    enableLODCrossFade: 0\n",
        "{\"dependencies\":{}}",
    )
    .unwrap();
    assert_eq!(defaults, ShaderSettings::default());
    assert_eq!(ShaderSettings::read("", "", "", "").unwrap(), defaults);
    let listed = |apis: &str| {
        playerbake::shader_platforms(&format!(
            "  m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: {apis}\n    m_Automatic: 0\n"
        ))
    };
    assert_eq!(listed("1500000011000000").unwrap(), [18, 15]);
    assert_eq!(listed("11000000").unwrap(), [15]);
    // Another renderer, a repeated one and a malformed list.
    for apis in ["0b000000", "1100000011000000", "110000", ""] {
        assert!(refusal(&listed(apis).unwrap_err()).is_some(), "{apis}");
    }
    assert_eq!(
        playerbake::shader_platforms("  m_BuildTargetGraphicsAPIs: []\n").unwrap(),
        [15]
    );
}

#[test]
fn settings_change_what_a_shader_is_compiled_with() {
    let linear = ShaderSettings::read(
        "  m_ActiveColorSpace: 1\n  shaderPrecisionModel: 1\n  m_BuildTargetGroupLightmapEncodingQuality:\n  - serializedVersion: 2\n    m_BuildTarget: Standalone\n    m_EncodingQuality: 1\n  m_DefaultShaderChunkSizeInMB: 4\n",
        "  m_DeferredReflections:\n    m_Mode: 0\n    m_Shader: {fileID: 0}\n  m_TierSettings:\n  - serializedVersion: 5\n    m_BuildTarget: 1\n    m_Tier: 1\n    m_Settings:\n      standardShaderQuality: 0\n      useReflectionProbeBoxProjection: 0\n      useReflectionProbeBlending: 1\n      useDetailNormalMap: 0\n      useCascadedShadowMaps: 0\n      enableLPPV: 0\n      useDitherMaskForAlphaBlendedShadows: 1\n    m_Automatic: 0\n  m_LightmapStripping: 1\n  m_FogStripping: 1\n  m_InstancingStripping: 2\n  m_LightmapKeepPlain: 0\n  m_FogKeepExp: 0\n",
        "  - enableLODCrossFade: 1\n",
        "{\"dependencies\":{\"com.unity.entities.graphics\":\"1.0.0\"}}",
    )
    .unwrap();
    assert!(linear.linear && linear.unified_precision && !linear.reflection_buffers);
    assert_eq!((linear.lightmap_encoding, linear.chunk_megabytes), (1, 4));
    assert_eq!(linear.tiers[0], playerbake::ShaderTier::default());
    assert_eq!(linear.tiers[1].quality, 0);
    assert!(!linear.tiers[1].cascaded_shadows && linear.tiers[1].probe_blending);
    assert!(!linear.lightmap_keep.unwrap().plain && linear.lightmap_keep.unwrap().directional);
    assert!(!linear.fog_keep.unwrap().exponential && linear.fog_keep.unwrap().linear);
    assert_eq!(linear.instancing_stripping, 2);
    assert!(linear.lod_cross_fade && linear.hybrid_package);
    // A recording made with the default settings does not answer a request
    // made with others.
    let name = &fixture_names()[0];
    let error = bake_fixture(name, linear).unwrap_err();
    assert!(format!("{error:#}").contains("no recorded reply"));
}

/// Compares every shader of the directories `UNITY_PLAYERBAKE_SHADER_CORPUS`
/// lists (separated by `:`) with the engine's object, compiling with the
/// `UnityShaderCompiler` of the Editor at `UNITY_PLAYERBAKE_EDITOR`. A
/// directory holds `<name>.artifact.bin`, `<name>.shader.bin` and
/// `<name>.materials`, the settings of the reference build under
/// `ProjectSettings` with `manifest.json`, and a file `project` naming the
/// project whose files the shaders include. With
/// `UNITY_PLAYERBAKE_SHADER_RECORD` set to a directory, the artifact's object
/// and the compiler's replies of every match are written there as fixtures.
#[test]
fn shader_corpus_matches_the_engine_build() {
    let (Some(roots), Some(editor)) = (
        std::env::var("UNITY_PLAYERBAKE_SHADER_CORPUS")
            .ok()
            .filter(|roots| !roots.is_empty()),
        std::env::var("UNITY_PLAYERBAKE_EDITOR").ok(),
    ) else {
        eprintln!("skipped: set UNITY_PLAYERBAKE_SHADER_CORPUS and UNITY_PLAYERBAKE_EDITOR");
        return;
    };
    let record = std::env::var("UNITY_PLAYERBAKE_SHADER_RECORD").ok();
    let only = std::env::var("UNITY_PLAYERBAKE_SHADER_ONLY").ok();
    let (mut matched, mut refused, mut different) = (0, Vec::new(), Vec::new());
    let mut references = References::default();
    for root in roots.split(':') {
        references.add(Path::new(root));
    }
    for root in roots.split(':').map(PathBuf::from) {
        let project = fs::read_to_string(root.join("project")).unwrap();
        let platforms = playerbake::shader_platforms(
            &fs::read_to_string(root.join("ProjectSettings/ProjectSettings.asset"))
                .unwrap_or_default(),
        )
        .unwrap();
        let settings = match settings(&root) {
            Ok(settings) => settings,
            Err(error) => panic!("{}: {error:#}", root.display()),
        };
        let logs = tempfile::tempdir().unwrap();
        let context = unity_micro_editor::cancel::Context::background();
        let mut compiler = Recording {
            compiler: unity_micro_editor::native::ShaderCompiler::start(
                &context,
                &editor,
                project.trim(),
                logs.path().to_str().unwrap(),
            )
            .unwrap(),
            out: Vec::new(),
        };
        let mut names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_str()?
                    .strip_suffix(".artifact.bin")
                    .map(str::to_string)
            })
            .collect();
        names.sort();
        for name in names {
            if only.as_ref().is_some_and(|only| !name.contains(only)) {
                continue;
            }
            let label = format!("{}/{name}", root.display());
            let artifact = fs::read(root.join(format!("{name}.artifact.bin"))).unwrap();
            let expected = fs::read(root.join(format!("{name}.shader.bin"))).unwrap();
            let project = ShaderProject {
                platforms: platforms.clone(),
                settings: settings.clone(),
                materials: materials(&root.join(format!("{name}.materials"))),
            };
            compiler.out.clear();
            references.expect(&expected);
            let baked =
                playerbake::bake_shader_object(&artifact, &project, &references, &mut compiler)
                    .map(|(object, _)| object);
            match baked {
                Ok(baked) if baked == expected => {
                    matched += 1;
                    eprintln!("match     {label}");
                    if let Some(record) = &record {
                        let record = Path::new(record);
                        let objects = playerbake::read_serialized(&artifact).unwrap();
                        let shader = objects
                            .iter()
                            .find(|object| object.tree.class_id == 48)
                            .unwrap();
                        fs::write(record.join(format!("{name}.editor.bin")), &shader.data).unwrap();
                        fs::write(record.join("editor-type-48.bin"), &shader.tree.entry).unwrap();
                        fs::write(record.join(format!("{name}.replies.bin")), &compiler.out)
                            .unwrap();
                        fs::write(record.join(format!("{name}.shader.bin")), &expected).unwrap();
                        fs::copy(
                            root.join(format!("{name}.materials")),
                            record.join(format!("{name}.materials")),
                        )
                        .unwrap();
                    }
                }
                Ok(baked) => {
                    eprintln!("DIFFERENT {label}: {}", first_difference(&baked, &expected));
                    if let Ok(directory) = std::env::var("UNITY_PLAYERBAKE_DUMP") {
                        fs::write(
                            Path::new(&directory).join(format!("{name}.baked.bin")),
                            &baked,
                        )
                        .unwrap();
                    }
                    different.push(label);
                }
                Err(error) => match refusal(&error) {
                    Some(reason) => {
                        eprintln!("refused   {label}: {reason}");
                        refused.push(label);
                    }
                    None => {
                        eprintln!("FAILED    {label}: {error:#}");
                        different.push(label);
                    }
                },
            }
        }
    }
    eprintln!(
        "{matched} match, {} refused, {} differ",
        refused.len(),
        different.len()
    );
    assert!(different.is_empty(), "{different:?}");
}
