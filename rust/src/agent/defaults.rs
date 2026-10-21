//! The version-bound native component defaults cache behind `component_schema`,
//! `component_add` and prefab `add_component`.
use super::{
    Any, AnyMap, Service, actions::request, error_text, fail, fail_with, join, join_errors,
    map_reply, marshal, not_exist, nz, obj, read_json, store_error, text, to_serde, typed,
    unity_version,
};
use crate::{
    ENGINE_PROFILE,
    authoring::{
        AuthoringError, NativeDefaultsRequest, NativeDefaultsTemplate, Store,
        native_component_types,
    },
    cancel::Context,
    compiler::{NilMap, glob},
    editorbridge::{self, Client},
    mcp::HandlerValue,
    packages,
    project::{is_owned_cache_path, parse_unity_yaml, value},
    sha256, sysutil,
    util::{
        agent_canonical, agent_id, agent_immutable_json, file_hash, micro_editor_root,
        read_agent_file,
    },
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, path::Path, time::Duration};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DefaultsProfile {
    #[serde(deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub unity_version: String,
    #[serde(deserialize_with = "nz")]
    pub unity_revision: String,
    pub engine_inputs: NilMap<String>,
    pub context_inputs: NilMap<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DefaultsCatalog {
    #[serde(rename = "schemaVersion", deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub profile: DefaultsProfile,
    #[serde(rename = "profileSha256", deserialize_with = "nz")]
    pub profile_sha256: String,
    pub templates: NilMap<NativeDefaultsTemplate>,
    #[serde(rename = "captureReceipts")]
    pub receipts: NilMap<String>,
    #[serde(rename = "sourceSnapshotSha256", deserialize_with = "nz")]
    pub source_sha256: String,
}

/// The qualified name of a pinned native component.
pub(crate) fn component_defaults_name(name: &str) -> Result<String> {
    let name = name.strip_prefix("UnityEngine.").unwrap_or(name);
    if native_component_types()
        .values()
        .any(|candidate| *candidate == name)
    {
        return Ok(format!("UnityEngine.{name}"));
    }
    Err(fail(
        "unsupported_component",
        "component type is outside the pinned native profile",
    ))
}

const SETTINGS: [&str; 5] = [
    "GraphicsSettings.asset",
    "QualitySettings.asset",
    "PhysicsManager.asset",
    "Physics2DSettings.asset",
    "ProjectSettings.asset",
];
pub(crate) fn settings_names() -> &'static [&'static str] {
    &SETTINGS
}

/// Built-in defaults depend on the native engine
/// and its rendering/settings context. Gameplay and IDE package changes do not
/// invalidate these templates. Custom Editor Reset/default extensions are
/// outside this native-only contract.
pub fn component_defaults_profile(
    ctx: &Context,
    project: &str,
    editor: &str,
) -> Result<(DefaultsProfile, String)> {
    let mut profile = DefaultsProfile {
        schema_version: 1,
        unity_version: unity_version(project)?.into(),
        unity_revision: String::new(),
        engine_inputs: NilMap::make(),
        context_inputs: NilMap::make(),
    };
    #[derive(Deserialize)]
    struct Embedded {
        #[serde(default, rename = "unityRevision")]
        unity_revision: String,
    }
    let release = crate::release::of_project(project).ok();
    profile.unity_revision = match release {
        Some(release) if release != crate::release::verified() => release.revision.to_string(),
        _ => serde_json::from_str::<Embedded>(ENGINE_PROFILE)?.unity_revision,
    };
    let version = read_agent_file(
        &join(&[project, "ProjectSettings/ProjectVersion.txt"]),
        64 << 10,
    );
    let version_text = version
        .as_deref()
        .map(|data| String::from_utf8_lossy(data).into_owned())
        .unwrap_or_default();
    let valid_version = release.is_some();
    if version.is_err() || !valid_version {
        return Err(fail(
            "unsupported_unity_version",
            "component defaults require the pinned project version",
        ));
    }
    if version_text.contains("m_EditorVersionWithRevision:")
        && !version_text.contains(&format!("({})", profile.unity_revision))
    {
        return Err(fail(
            "unsupported_unity_revision",
            "component defaults require the exact pinned engine revision",
        ));
    }
    let editor_directory = sysutil::dir(editor);
    let modules = glob(&join(&[
        &editor_directory,
        "Data/Managed/UnityEngine/UnityEngine.*Module.dll",
    ]))
    .unwrap_or_default();
    if modules.is_empty() {
        return Err(fail(
            "invalid_engine_profile",
            "installed SDK native module inventory is missing",
        ));
    }
    for path in std::iter::once(editor.to_string()).chain(modules) {
        ctx.err()?;
        agent_canonical(&path)?;
        let hash = file_hash(&path)?;
        let logical = sysutil::rel(&editor_directory, &path).unwrap_or_default();
        profile.engine_inputs.insert(logical, hash);
    }
    for name in SETTINGS {
        let logical = format!("ProjectSettings/{name}");
        let path = join(&[project, &logical]);
        agent_canonical(&path)?;
        let hash = match file_hash(&path) {
            Ok(hash) => hash,
            Err(error) if not_exist(&error) => "absent".into(),
            Err(error) => return Err(error),
        };
        profile.context_inputs.insert(logical, hash);
    }
    // Resolve exact native package roots rather than interpreting PackageCache
    // directory names. The read path is offline and never executes an Editor.
    let resolved = packages::resolve(
        ctx,
        project,
        editor,
        packages::Options {
            offline: true,
            ..Default::default()
        },
    )?;
    for package in resolved.packages.iter() {
        if package.name.starts_with("com.unity.render-pipelines.")
            || package.name == "com.unity.ugui"
        {
            profile.context_inputs.insert(
                format!("Packages/{}", package.name),
                package.content_hash.clone(),
            );
        }
    }
    super::defaults_closure::reference_closure(
        ctx,
        project,
        Some(&resolved),
        &mut profile.context_inputs,
    )?;
    let tool = micro_editor_root()?;
    let producer = file_hash(&join(&[
        &tool,
        "Packages/com.unitymicroeditor.runtime/Editor/AgentEditorAuthoring.cs",
    ]))?;
    profile.context_inputs.insert("capturePolicy", producer);
    let hash = sha256(&marshal(&profile)?);
    Ok((profile, hash))
}

pub(crate) fn defaults_directory(project: &str, profile: &str) -> String {
    join(&[
        project,
        "Library/UnityMicroEditor/component-defaults",
        profile,
    ])
}
pub(crate) fn candidate_template_name(name: &str) -> String {
    format!(
        "{}.unity",
        name.strip_prefix("UnityEngine.").unwrap_or(name)
    )
}

/// The cached catalog of exactly this profile.
pub(crate) fn read_defaults_catalog(
    project: &str,
    profile: &DefaultsProfile,
    hash: &str,
) -> Result<DefaultsCatalog> {
    let directory = defaults_directory(project, hash);
    let path = join(&[&directory, "catalog.json"]);
    agent_canonical(&path)?;
    let catalog: DefaultsCatalog = match read_json(&path, 64 << 20) {
        Ok(catalog) => catalog,
        Err(error) if not_exist(&error) => {
            return Err(fail_with(
                "component_defaults_missing",
                "capture the version-bound native defaults once before adding components",
                obj([
                    ("profileSha256", hash.into()),
                    (
                        "captureCommand",
                        format!("unity-micro-editor cache-component-defaults --project {project}")
                            .into(),
                    ),
                ]),
            ));
        }
        Err(error) => return Err(error),
    };
    let types = native_component_types();
    if catalog.schema_version != 1
        || catalog.profile_sha256 != hash
        || catalog.profile != *profile
        || catalog.templates.len() != types.len()
    {
        return Err(fail(
            "invalid_component_defaults",
            "cached native default catalog differs from its exact engine/context profile",
        ));
    }
    // Visited in class order.
    for candidate in types.values() {
        let name = format!("UnityEngine.{candidate}");
        let valid = catalog.templates.get(&name).filter(|template| {
            template.schema_version == 1
                && template.component_type == name
                && template.profile == hash
                && template.unity_version == profile.unity_version
                && template.unity_revision == profile.unity_revision
                && template.sha256 == sha256(template.content.as_bytes())
        });
        let Some(template) = valid else {
            return Err(fail(
                "invalid_component_defaults",
                format!("cached native default template identity or contents changed: {name}"),
            ));
        };
        let path = join(&[&directory, &candidate_template_name(&name)]);
        agent_canonical(&path)?;
        if !read_agent_file(&path, 32 << 20)
            .is_ok_and(|contents| sha256(&contents) == template.sha256)
        {
            return Err(fail(
                "invalid_component_defaults",
                format!("cached native default file changed: {name}"),
            ));
        }
    }
    Ok(catalog)
}

/// Publishes bytes once; identical bytes are accepted.
pub(crate) fn immutable_file(path: &str, contents: &[u8]) -> Result<()> {
    agent_canonical(path)?;
    let parent = sysutil::dir(path);
    sysutil::mkdir_all(&parent)?;
    match read_agent_file(path, 32 << 20) {
        Ok(previous) => {
            if previous != contents {
                return Err(fail(
                    "immutable_cache_conflict",
                    "existing component default template differs",
                ));
            }
            return Ok(());
        }
        Err(error) if !not_exist(&error) => return Err(error),
        Err(_) => {}
    }
    let mut file = tempfile::Builder::new()
        .prefix(".component-template-")
        .tempfile_in(&parent)
        .map_err(|error| {
            sysutil::Error::os("open", &join(&[&parent, ".component-template-"]), &error)
        })?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    if let Err(error) = fs::hard_link(file.path(), path) {
        if error.kind() == std::io::ErrorKind::AlreadyExists
            && read_agent_file(path, 32 << 20).is_ok_and(|previous| previous == contents)
        {
            return Ok(());
        }
        return Err(anyhow!(
            "link {} {path}: {}",
            file.path().display(),
            sysutil::errno_text(&error)
        ));
    }
    fs::File::open(&parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| sysutil::Error::os("open", &parent, &error))?;
    Ok(())
}

impl Service {
    fn defaults(
        &self,
        ctx: &Context,
        args: &AnyMap,
    ) -> Result<(String, DefaultsProfile, String, DefaultsCatalog)> {
        let name = component_defaults_name(&text(args, "componentType"))?;
        let (profile, hash) = component_defaults_profile(ctx, &self.0.project, &self.0.editor)?;
        let catalog = read_defaults_catalog(&self.0.project, &profile, &hash)?;
        Ok((name, profile, hash, catalog))
    }

    /// The cached default fields of a native component.
    pub(crate) fn component_schema(&self, ctx: &Context, args: &AnyMap) -> Result<HandlerValue> {
        let (name, profile, hash, catalog) = self.defaults(ctx, args)?;
        let template = catalog.templates.get(&name).cloned().unwrap_or_default();
        let documents = parse_unity_yaml(template.content.as_bytes())?;
        let types = native_component_types();
        let mut components = Vec::new();
        for document in documents {
            if types.contains_key(&document.class_id) || document.class_id == 224 {
                components.push(Any::Object(obj([
                    ("fileId", document.file_id.clone().into()),
                    ("classId", document.class_id.into()),
                    ("typeName", document.type_name.clone().into()),
                    ("fields", document.data.map_or(Any::Null, Any::Object)),
                ])));
            }
        }
        map_reply(obj([
            ("backend", "versioned_component_cache".into()),
            ("originBackend", "unity_editor_public_api".into()),
            ("componentType", name.clone().into()),
            ("requestId", text(args, "requestId").into()),
            ("profileSha256", hash.clone().into()),
            ("unityVersion", profile.unity_version.into()),
            ("unityRevision", profile.unity_revision.into()),
            ("templateSha256", template.sha256.into()),
            ("components", components.into()),
            ("editorStarted", false.into()),
            (
                "templatePath",
                join(&[
                    &defaults_directory(&self.0.project, &hash),
                    &candidate_template_name(&name),
                ])
                .into(),
            ),
        ]))
    }

    /// Adds a native component with its cached defaults.
    pub(crate) fn component_add(&self, ctx: &Context, args: &AnyMap) -> Result<HandlerValue> {
        let (name, _, hash, catalog) = self.defaults(ctx, args)?;
        let store = Store::new(Path::new(&self.0.project)).map_err(store_error)?;
        let mut fields = BTreeMap::new();
        if let Some(Any::Object(map)) = args.get("fields") {
            for (key, field) in map {
                fields.insert(key.clone(), to_serde(field));
            }
        }
        let request = NativeDefaultsRequest {
            path: text(args, "path"),
            expected_revision: text(args, "expectedRevision"),
            operation_id: text(args, "operationId"),
            file_id: text(args, "fileId"),
            target_path: text(args, "targetPath"),
            component_type: name.clone(),
            fields,
            template: catalog.templates.get(&name).cloned().unwrap_or_default(),
        };
        let guard = self.component_defaults_guard(ctx, hash.clone());
        let result = store
            .add_native_defaults(&request, Some(&guard))
            .map_err(store_error)?;
        map_reply(obj([
            ("backend", "raw_source".into()),
            ("defaultsBackend", "versioned_component_cache".into()),
            ("profileSha256", hash.into()),
            ("editorStarted", false.into()),
            ("operationId", text(args, "operationId").into()),
            ("publication", value::raw(&result)?),
        ]))
    }

    /// Rechecks the profile right before publication.
    pub(crate) fn component_defaults_guard(
        &self,
        ctx: &Context,
        expected: String,
    ) -> impl Fn() -> std::result::Result<(), AuthoringError> + use<> {
        let (service, ctx) = (self.clone(), ctx.clone());
        move || {
            let check = || -> Result<()> {
                let (profile, actual) =
                    component_defaults_profile(&ctx, &service.0.project, &service.0.editor)?;
                if actual != expected {
                    return Err(fail(
                        "component_defaults_changed",
                        "native engine/settings/package defaults profile changed before source publication",
                    ));
                }
                read_defaults_catalog(&service.0.project, &profile, &actual).map(drop)
            };
            check().map_err(|error| match typed(&error) {
                Some(detail) => detail.clone(),
                // Untyped errors travel through the authoring layer under this code.
                None => AuthoringError {
                    code: "internal_error".into(),
                    message: error_text(&error),
                    details: None,
                },
            })
        }
    }

    /// Cache bootstrap is explicit. Ordinary
    /// component_schema/component_add never execute this path, including when
    /// the version/context cache is missing.
    pub(crate) fn capture_component_defaults(&self, ctx: &Context) -> Result<DefaultsCatalog> {
        let (project, editor) = (self.0.project.as_str(), self.0.editor.as_str());
        if self.0.no_editor {
            return Err(fail(
                "editor_disabled",
                "native default capture is an explicit Editor bootstrap; this service forbids Editor execution",
            ));
        }
        packages::resolve(ctx, project, editor, packages::Options::default())?;
        let (profile, hash) = component_defaults_profile(ctx, project, editor)?;
        match read_defaults_catalog(project, &profile, &hash) {
            Ok(cached) => return Ok(cached),
            Err(error) => {
                if typed(&error).is_none_or(|detail| detail.code != "component_defaults_missing") {
                    return Err(error);
                }
            }
        }
        let directory = defaults_directory(project, &hash);
        agent_canonical(&directory)?;
        // Failed captures cannot pin all future attempts to stale gameplay source.
        // Profile identity stays version/context-bound; each capture owns a fresh
        // authored snapshot and independently preserved receipts.
        let (stage, snapshot) = super::clone_staging(
            ctx,
            project,
            &join(&[&directory, "capture-attempts", &agent_id()]),
        )?;
        let client = Client::new(
            &self.lifetime(),
            editorbridge::Options {
                project: stage.clone(),
                editor: editor.into(),
                startup_timeout: Duration::from_secs(600),
                ..Default::default()
            },
        )?;
        self.stage(
            "capture_component_defaults",
            "capturing native component defaults in an owned staging copy",
            &join(&[client.mailbox(), "editor.log"]),
        );
        struct Close(Client);
        impl Drop for Close {
            fn drop(&mut self) {
                let _ = self.0.close();
            }
        }
        let _close = Close(client.clone());
        let mut result = DefaultsCatalog {
            schema_version: 1,
            profile: profile.clone(),
            profile_sha256: hash.clone(),
            templates: NilMap::make(),
            receipts: NilMap::make(),
            source_sha256: sha256(&marshal(&snapshot.files)?),
        };
        let mut names: Vec<String> = native_component_types()
            .values()
            .map(|name| format!("UnityEngine.{name}"))
            .collect();
        names.sort();
        for (index, name) in names.iter().enumerate() {
            let arguments = marshal(&Any::Object(obj([("componentType", name.clone().into())])))?;
            let id = format!("{}-{index:02}", &hash[..32]);
            let (response, data) = request(&client, ctx, &id, "component_schema", &arguments);
            let data = data?;
            #[derive(Deserialize, Default)]
            #[serde(default, rename_all = "camelCase")]
            struct Captured {
                #[serde(deserialize_with = "nz")]
                template_path: String,
                #[serde(deserialize_with = "nz")]
                unity_version: String,
            }
            let captured = serde_json::from_slice::<Option<Captured>>(&data)
                .ok()
                .map(Option::unwrap_or_default)
                .filter(|captured| {
                    captured.unity_version == profile.unity_version
                        && is_owned_cache_path(&stage, &captured.template_path)
                })
                .ok_or_else(|| {
                    anyhow!("component capture lacks exact native version/owned template identity")
                })?;
            agent_canonical(&captured.template_path)?;
            let content = read_agent_file(&captured.template_path, 32 << 20)?;
            result.templates.insert(
                name.clone(),
                NativeDefaultsTemplate {
                    schema_version: 1,
                    profile: hash.clone(),
                    unity_version: profile.unity_version.clone(),
                    unity_revision: profile.unity_revision.clone(),
                    component_type: name.clone(),
                    content: String::from_utf8_lossy(&content).into_owned(),
                    sha256: sha256(&content),
                },
            );
            result.receipts.insert(name.clone(), response.receipt);
            immutable_file(
                &join(&[&directory, &candidate_template_name(name)]),
                &content,
            )?;
        }
        client.close()?;
        super::verify_snapshot(ctx, &snapshot)?;
        match component_defaults_profile(ctx, project, editor) {
            Ok((current, current_hash)) if current_hash == hash && current == profile => {}
            Ok(_) => return Err(anyhow!("native default profile changed during capture")),
            Err(error) => {
                return Err(join_errors(
                    "native default profile changed during capture",
                    Some(&error),
                ));
            }
        }
        agent_immutable_json(&join(&[&directory, "catalog.json"]), &result)?;
        read_defaults_catalog(project, &profile, &hash)
    }
}
