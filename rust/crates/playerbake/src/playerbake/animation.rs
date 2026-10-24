//! Animation and audio mixer sources in a closure: `.anim`, `.controller`,
//! `.overrideController`, `.mask` and `.mixer`. The Editor objects of these
//! sources are not what a Player holds; a build derives its objects from
//! them (`clip`, `controller`, `mask`, `override_controller`, `mixer`). Here each Player
//! object stands in the source's documents as a document of its class and
//! file ID without fields, with the object made beside it; what stays an
//! object of its own (a controller's state machine behaviours) keeps its
//! document.
use super::{
    External, Unsupported,
    authored::Sources,
    clip::{self, ClipSources},
    controller::{self, BehaviourScript, ClipFacts, ControllerSources},
    mask::{self, Mask},
    mixer, override_controller,
    typetree::Val,
};
use crate::yaml::Document;
use anyhow::{Context, Result, bail};
use serde_json::Map;
use std::path::Path;

/// A Player object a build derives from a source, with pointers into the
/// source itself (file 0, by authored file ID) and into the sources of
/// `externals` (file `n` is `externals[n - 1]`).
pub(super) struct Prebuilt {
    pub value: Val,
    pub externals: Vec<External>,
}

/// The documents an animation source stands for in a closure, and the
/// objects made for the documents without fields, by file ID.
pub(super) struct Animation {
    pub documents: Vec<Document>,
    pub prebuilt: Vec<(i64, Prebuilt)>,
}

const CLIP: i64 = 74;
const CONTROLLER: i64 = 91;
const OVERRIDE: i64 = 221;
const MASK: i64 = 319;
const BEHAVIOUR: i64 = 114;

/// A document that names a Player object made beside it.
fn standing(class_id: i64, file_id: &str, type_name: &str) -> Document {
    Document {
        class_id,
        file_id: file_id.to_string(),
        type_name: type_name.to_string(),
        data: Map::new(),
        stripped: false,
    }
}

fn file_id(document: &Document) -> Result<i64> {
    document
        .file_id
        .parse()
        .with_context(|| format!("{} has an invalid file ID", document.type_name))
}

/// The objects of an animation source, or `None` for another kind.
pub(super) fn load(path: &Path, sources: &Sources) -> Result<Option<Animation>> {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("fbx") {
        return model(path).map(Some);
    }
    if !matches!(
        extension.as_str(),
        "anim" | "controller" | "overrideController" | "mask" | "mixer"
    ) {
        return Ok(None);
    }
    let text = std::fs::read(path)?;
    if text.len() > clip::MAX_SOURCE_BYTES {
        return Err(Unsupported("animation source larger than the baker reads".into()).into());
    }
    let documents = crate::yaml::parse_unity_yaml_lexical_within(&text, clip::MAX_SOURCE_BYTES)?;
    let mut animation = Animation {
        documents: Vec::new(),
        prebuilt: Vec::new(),
    };
    match extension.as_str() {
        "anim" => {
            let [document] = documents.as_slice() else {
                return Err(Unsupported("animation source with several objects".into()).into());
            };
            if document.class_id != CLIP || document.stripped {
                bail!("animation source does not hold an AnimationClip");
            }
            let baked = clip::bake_clip_parts(&document.data, &Closure { sources })?;
            animation
                .documents
                .push(standing(CLIP, &document.file_id, "AnimationClip"));
            animation.prebuilt.push((
                file_id(document)?,
                Prebuilt {
                    value: baked.value,
                    externals: baked.externals,
                },
            ));
        }
        "controller" => {
            let Some(document) = documents.iter().find(|d| d.class_id == CONTROLLER) else {
                bail!("controller source does not hold an AnimatorController");
            };
            let baked = controller::bake_controller_object(&documents, &Closure { sources })?;
            animation.documents.push(standing(
                CONTROLLER,
                &document.file_id,
                "AnimatorController",
            ));
            animation.prebuilt.push((
                file_id(document)?,
                Prebuilt {
                    value: baked.value,
                    externals: baked.externals,
                },
            ));
            // The behaviours the controller runs stay objects of their
            // classes, and a mask stored in the controller's file is
            // published beside it as well, though no layer points at it.
            for other in &documents {
                if other.class_id == BEHAVIOUR && baked.behaviours.contains(&other.file_id) {
                    animation.documents.push(other.clone());
                } else if other.class_id == MASK && !other.stripped {
                    animation
                        .documents
                        .push(standing(MASK, &other.file_id, "AvatarMask"));
                    animation.prebuilt.push((
                        file_id(other)?,
                        Prebuilt {
                            value: mask::mask_value(&other.data)?,
                            externals: Vec::new(),
                        },
                    ));
                }
            }
        }
        "mixer" => {
            // The mixer, its groups and its snapshots; the effects live on
            // in the mixer's constant.
            let (objects, externals) = mixer::mixer_objects(&documents)?;
            for object in objects {
                let Some(document) = documents
                    .iter()
                    .find(|d| d.file_id == object.file_id.to_string())
                else {
                    continue;
                };
                animation.documents.push(standing(
                    object.class_id,
                    &document.file_id,
                    &document.type_name,
                ));
                animation.prebuilt.push((
                    object.file_id,
                    Prebuilt {
                        value: object.value,
                        externals: externals.clone(),
                    },
                ));
            }
        }
        "overrideController" => {
            let [document] = documents.as_slice() else {
                return Err(
                    Unsupported("override controller source with several objects".into()).into(),
                );
            };
            if document.class_id != OVERRIDE || document.stripped {
                bail!("source does not hold an AnimatorOverrideController");
            }
            let (value, externals) = override_controller::override_value(&document.data)?;
            animation.documents.push(standing(
                OVERRIDE,
                &document.file_id,
                "AnimatorOverrideController",
            ));
            animation
                .prebuilt
                .push((file_id(document)?, Prebuilt { value, externals }));
        }
        _ => {
            let [document] = documents.as_slice() else {
                return Err(Unsupported("avatar mask source with several objects".into()).into());
            };
            if document.class_id != MASK || document.stripped {
                bail!("source does not hold an AvatarMask");
            }
            animation
                .documents
                .push(standing(MASK, &document.file_id, "AvatarMask"));
            animation.prebuilt.push((
                file_id(document)?,
                Prebuilt {
                    value: mask::mask_value(&document.data)?,
                    externals: Vec::new(),
                },
            ));
        }
    }
    Ok(Some(animation))
}

/// The objects of a model the tool imports: its meshes.
fn model(path: &Path) -> Result<Animation> {
    let meta = std::fs::read_to_string(format!("{}.meta", path.display()))?;
    let settings = super::model::Settings::read(&meta)?;
    let imported = super::model::import(path, &settings)?;
    let mut animation = Animation {
        documents: Vec::new(),
        prebuilt: Vec::new(),
    };
    for (file_id, class_id, value) in imported.objects {
        animation.documents.push(standing(
            class_id,
            &file_id.to_string(),
            match class_id {
                43 => "Mesh",
                74 => "AnimationClip",
                90 => "Avatar",
                _ => "Object",
            },
        ));
        animation.prebuilt.push((
            file_id,
            Prebuilt {
                value,
                externals: Vec::new(),
            },
        ));
    }
    Ok(animation)
}

/// The project's path of a source, as the Editor names it: under `Assets`,
/// or under `Packages/<name>` for a package, wherever its files lie.
fn project_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(at) = text.find("/Library/PackageCache/") {
        let rest = &text[at + "/Library/PackageCache/".len()..];
        let (package, inner) = rest.split_once('/').unwrap_or((rest, ""));
        let name = package.split('@').next().unwrap_or(package);
        return format!("Packages/{name}/{inner}");
    }
    for root in ["/Assets/", "/Packages/"] {
        if let Some(at) = text.rfind(root) {
            return text[at + 1..].to_string();
        }
    }
    text
}

/// The project's other sources as a clip and a controller need them.
struct Closure<'a> {
    sources: &'a Sources,
}

impl Closure<'_> {
    fn source(&self, guid: &str) -> Result<(&Path, Vec<u8>)> {
        let Some(path) = self.sources.path(guid) else {
            return Err(Unsupported(format!("no source has the GUID {guid}")).into());
        };
        Ok((path, std::fs::read(path)?))
    }
}

impl ClipSources for Closure<'_> {
    fn script(&self, guid: &str) -> Result<clip::ScriptIdentity> {
        Err(Unsupported(format!("reference to script {guid} inside the clip's file")).into())
    }
    fn clip(&self, guid: &str) -> Result<Vec<u8>> {
        Ok(self.source(guid)?.1)
    }
    fn scripts_external(&self) -> bool {
        true
    }
}

impl ControllerSources for Closure<'_> {
    fn clip(&self, guid: &str, file_id: &str) -> Result<ClipFacts> {
        let (path, text) = self.source(guid)?;
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("fbx"))
        {
            let meta = std::fs::read_to_string(format!("{}.meta", path.display()))?;
            let settings = super::model::Settings::read(&meta)?;
            let wanted: i64 = file_id.parse().unwrap_or(0);
            let Some((_, name, looping)) = super::model::clips(path, &settings)?
                .into_iter()
                .find(|(id, ..)| *id == wanted)
            else {
                return Err(Unsupported(format!(
                    "no clip of {} has file ID {file_id}",
                    path.display()
                ))
                .into());
            };
            return Ok(ClipFacts {
                looping,
                path: project_path(path),
                name,
            });
        }
        if path.extension().is_none_or(|e| e != "anim") || file_id != "7400000" {
            return Err(Unsupported(format!(
                "a clip the tool does not import ({}, file ID {file_id})",
                path.display()
            ))
            .into());
        }
        controller::read_clip_facts(&text, &project_path(path))
    }
    fn mask(&self, guid: &str) -> Result<Mask> {
        mask::read_mask(&self.source(guid)?.1)
    }
    /// A script's class overrides a callback of its state machine when its
    /// source declares one.
    fn behaviour(&self, guid: &str) -> Result<Option<BehaviourScript>> {
        if self.sources.script_class(guid).is_none() {
            return Ok(None);
        }
        let text = match self.sources.path(guid) {
            Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
            None => String::new(),
        };
        let overrides = text.lines().any(|line| {
            line.contains("override")
                && (line.contains("OnStateMachineEnter") || line.contains("OnStateMachineExit"))
        });
        Ok(Some(BehaviourScript {
            overrides_state_machine_callbacks: overrides,
        }))
    }
}
