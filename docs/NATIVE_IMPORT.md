# Native import

Scenes and everything they refer to are imported by Unity's own AssetImportWorker, driven by this tool's coordinator, and written in the Player format by the Editor's build code inside that worker ([Scenes in the Player format](#scenes-in-the-player-format)). The Player loads the written scene with the engine's own loader. Shaders are built on their own and published beside the scene ([Shaders](#shaders)). The main Editor process, its GUI and a pre-imported `Library` are not needed for a worker run; the worker does need files of an Editor compilation of the project ([Scripted importers](#scripted-importers)).

Only Unity `6000.6.3f1` on Linux x64 is supported. The coordinator starts `Unity` in AssetImportWorker mode and a separate `Unity.DataStore` from the installed engine. The importers themselves stay inside Unity's executable.

## Commands

Import selected files into a new owned context:

```bash
unity-micro-editor project-import \
  --project /absolute/project \
  --output /absolute/project/Library/UnityMicroEditor/imports/manual-01 \
  --asset Assets/Audio/door.wav \
  --timeout 45s
```

`--asset` can be repeated. `--output` must be a new directory under the project's `Library/UnityMicroEditor` or `Temp`. `--timeout` covers the whole import and is capped at 5 minutes. The coordinator reads the real source and `.meta` bytes and their dependencies, builds its own SourceAssetDB, and extracts the worker's reply from its own DataStore. It checks native content hashes, SHA-256 and that the source inputs did not change. The result is `<output>/result.json`.

This imports the chosen files. It does not bootstrap a project, and it does not create the baseline that `run` needs.

Print the current import environment fingerprint without starting any Unity process:

```bash
unity-micro-editor native-environment --project /absolute/project
```

The hidden `scene-objects --project P --editor E --scene S --output DIR [--with-dependencies 1]` writes a scene in the Player format into `DIR/scene` (and `DIR/deps`) without a session, for diagnostics.

## During a run

A session does not import assets one by one. It asks the scene worker for its scene ([Scenes in the Player format](#scenes-in-the-player-format)), which imports the scene and everything it refers to in one context and writes the scene and its dependencies as a build does. The one kind of source a session builds and publishes on its own is a shader (`.shader`, `.shadergraph`, `.compute`, see [Shaders](#shaders)); any other source that the session reports as changed is refused with `the session builds only shaders`.

Each published shader records an `environmentSha256`. The fingerprint covers the pinned protocol revision, the Unity path, the exact SHA-256 of `ProjectSettings.asset`, the parsed settings and the wire environment. An index with no fingerprint, or a different one, is skipped and its shaders are built again. Changing this environment while a runner is live requires restarting the runner.

## How an import works

1. The coordinator creates an isolated context with its own `Assets`, `ProjectSettings`, `Packages`, `Library` and `Logs`, and copies the exact source and `.meta` bytes into it.
2. The coordinator writes a minimal `SourceAssetDB` directly: GUIDs, paths, importer settings, the children of every folder and XXH3-128 file hashes taken from the sources. It also holds the register of the engine's scripts, read from the installed Editor's `unity default resources` ([UI and fonts](#ui-and-fonts)). Nothing is copied from an Editor run, except for the two kinds of import that need a piece of one: [models that make their own material](#obj-and-collada-models) and [scripted importers](#scripted-importers).
3. It starts a DataStore with a unique namespace, accepts the worker's local TCP connection, and sends `Initialize`, `Prepare` and `ImportRequest`.
4. Requests use the primary importer ID `-1`, so the native pipeline picks the importer and reads its settings from `.meta`. Explicit importer IDs run an extra importer with default settings and are rejected by content validation.
5. The worker is stopped through the protocol, then the DataStore, whose shared memory segments in `/dev/shm` are removed: the service is ended by a signal and leaves about 180 MB a run there. It reads only the stopped store, following `ImportResponse.ArtifactBlobs[1]` → `ArtifactMetaInfo.Files[].Hash` → the stored file.

Worker parameters come from an exact copy of `ProjectSettings.asset`: color space, shader precision and the Linux graphics API override. The profile is limited to the Linux standalone target, with OpenGLCore and Vulkan as the verified API set. A project that lists no Linux graphics APIs is imported and built for OpenGLCore alone, the API the Player runs on: the context's copy of `ProjectSettings.asset` gets that one entry, since a build inside a worker would otherwise compile every shader variant for Vulkan as well. A list of the project's own is kept as written. Unsupported or ambiguous settings are refused. The applied values and the settings hash are written to `environment` in the report.

### What an importer asks of the project

An importer does not read project settings itself. It asks its import context for a named value, a *custom dependency*, and the Editor registers those values when it refreshes its database (`RefreshInternalV2`) and sends them to each worker with `Initialize` and `Prepare`. A worker has only what it was sent, and an importer that asks for a key the message lacks ends the worker with `std::bad_optional_access`. That is why a standalone worker used to abort on BC7: `CompressorGlobalSettingsProviderFromImportContext::GetBc7Compressor` asks for `graphics/bc7-compressor`. Nothing outside the Editor binary is involved; the BC7 compressors (`libcompress_bc7e.so`, `libispc_texcomp.so`) are libraries the worker loads like the Editor does.

The coordinator sends the keys a texture, model or sprite atlas import asks for, each with the value the pinned Editor registers. The values were read back from an Editor's ArtifactDB, through a scripted importer that depends on every key:

| Key | Value | From |
| --- | --- | --- |
| `graphics/shader-precision-model` | setting + 1 | `shaderPrecisionModel` |
| `graphics/metal-needs-renderpass-fbfetch-fallback` | 1 | not on Linux |
| `graphics/texture-import-overrides` | 1 | no size or compression override |
| `graphics/default-texture-compression-format` | format + 1 | DXTC is the verified default |
| `graphics/texture-platform-settings` | 1 | the Linux standalone target |
| `graphics/active-color-space` | color space + 2 | `m_ActiveColorSpace` |
| `graphics/normal-map-encoding` | 2 | DXT5nm, the standalone default |
| `graphics/lightmap-encoding` | 3 | high quality, the standalone default |
| `graphics/hdr-cubemap-encoding` | 3 | high quality, the standalone default |
| `graphics/bc7-compressor` | 1, then the compressor | `m_Bc7TextureCompressor` of `EditorSettings.asset`; the default is ISPC (2) |
| `2d/spriteatlas-import-mode` | 1, or 2 for importer-based atlases | `m_SpritePackerMode` of `EditorSettings.asset` is 5 |

A project without `EditorSettings.asset` gets the values the Editor writes into a new one. A project that sets a normal map, lightmap or HDR cubemap encoding for the standalone target is refused, because no artifact was compared under such a setting; an entry for another target is ignored. The file's hash is part of the environment fingerprint.

For each linked file the coordinator checks length, storage bounds and the native XXH3-128 content key, and records SHA-256. Serialized files are inspected with Unity's standalone `binary2text`, which does not start an Editor. Checked files are saved in `<output>/artifacts`, wire payloads in `<output>/frame-*.bin` and process logs in `<output>/logs`.

The sections below say what the worker makes of each kind of source and what was compared with the Editor's artifacts. In a session these sources are imported by the scene worker in the scene's context, and their objects reach the Player in the file the worker writes beside the scene ([Scenes in the Player format](#scenes-in-the-player-format)); `project-import` imports them on their own. The tool does not check an importer's settings against a list of its own any more: what the worker imports, the engine writes as a build writes it.

## Textures

TextureImporter receives the project's default texture compression format and an explicit record that no user overrides exist. DXTC is the verified default.

Every source format TextureImporter reads goes through one route: `.png`, `.jpg`, `.jpeg`, `.tga`, `.tif`, `.tiff`, `.bmp`, `.psd`, `.gif`, `.exr` and `.hdr`. `.dds` and other container formats are not covered.

A standalone worker imports a texture with the importer's resize and mip chain under the settings below. For each of them the worker's artifact was compared with the artifact the Editor writes for the same source and `.meta`, and is byte-identical (54 fixtures, written through the Editor's own importer API):

- The automatic format (`textureFormat: -1`) at every compression quality, crunched or not: uncompressed, normal and low quality (DXT1, DXT5, BC6H for the HDR formats) and high quality, which selects BC7.
- An explicit format: Alpha8, ARGB16, RGB24, RGBA32, RGB16, R16, RG16, R8, RGBA64, RGBAHalf, RGBAFloat, RGB9E5, DXT1, DXT5, their crunched forms, BC4, BC5, BC6H and BC7. A format of another platform family (ETC, ASTC, PVRTC) was not compared.
- A `Standalone` platform override of size, format or quality. An override for another platform is ignored, as the engine ignores it.
- The texture types Default, NormalMap (from a normal map and from a height map), GUI, Cursor, Cookie, Lightmap, DirectionalLightmap, Shadowmask, SingleChannel and Sprite.
- The shapes 2D, Cube (automatic and cylindrical mapping, with and without specular or diffuse convolution, LDR and HDR), 2D array and 3D.

A sprite texture yields its Texture2D and one Sprite per sprite, single or multiple, under the local IDs the Editor gives them (`21300000`, or the IDs of the `.meta`'s `nameFileIdTable`); a cube, an array or a volume yields a Cubemap (`8900000`), a Texture2DArray (`18700000`) or a Texture3D (`11700000`) in place of the Texture2D.

A `.meta` that holds a GUID but no `TextureImporter` block is read by the engine as a version 1 importer, which makes an 8x8 cubemap of the file, in the Editor as in a worker; the Editor writes the block when it first sees the file.

## Audio

AudioImporter sources share one route: `.wav`, `.ogg`, `.mp3`, `.aif`, `.aiff` and `.flac`. A clip's data stays in the artifact's resource file. In a scene the worker writes, the data of the clips the scene refers to is in the streamed data file beside the file of the scene's dependencies (`ume-deps.resource`), which the Player reads from that file's directory.

## OBJ and COLLADA models

`.obj` and `.dae` are imported by the model importer, as FBX is.

A model without a material of its own gets one from the importer, which looks up the `Standard` shader by name. A worker fills its table of shaders by name with the built-in shaders when it starts, then replaces it with the `mem://Library/ScriptMapper` record of its source database; with no such record the table ends up empty, the lookup fails, and the importer copies the built-in default material instead, under another name and local ID (`Default-Material`, `3297912226980038121`) than the Editor's (`defaultMat`, `-3033667219593020291`; the ID is XXH64 of `Type:Material->` + name + `0`). The coordinator therefore writes that record for these two formats, with the bytes of `Library/ScriptMapper` from an Editor run of the project: the staging clone a preparation with a host build leaves in `Library/UnityMicroEditor/stage/project`, or else the project's own `Library`. Without either the import is refused. With the record the worker's artifacts for `.obj` and `.dae` are byte-identical to the Editor's. FBX imports are left as they were.

### Generated materials

A material the importer makes (`defaultMat` of an `.obj`, the materials of a `.blend` or of an FBX that are not remapped) names its shader as an object of the Editor's `unity_builtin_extra`: `Standard` is path ID 46 there. The worker writes the file of a scene's dependencies with that reference to `Resources/unity_builtin_extra`, which the Player resolves to its own copy of the file. A build keeps in that file only the shaders the project always includes, and copies any other built-in shader an asset uses into the file of that asset. A Player built by an Editor from the same models drew `Standard` for the `.blend` and the `.obj`, the engine's `Default-Material` for a `.dae` without a material, and in a URP project the pipeline's `Lit` shader, which the Editor sets with the pipeline's importer script.

Not verified on the worker-written scene: whether a generated material finds `Standard` in the host's `unity_builtin_extra`, and what a model of a pipeline project gets when the worker does not run the pipeline's importer script. A model whose materials are remapped to materials of the project is not affected.

### Blender files

The model importer does not read a `.blend`. `FBXImporter::ConvertToFBXFile` starts Blender, and imports the FBX it exports:

```
blender -b <project>/Assets/.../file.blend -P <Editor>/Data/Tools/Unity-BlenderToFBX.py -- <project>/Temp/FBXExport/<guid>.fbx
```

The program is found through `PATH` (`execvp`), in the Editor as in a worker; the script is the Editor's own and runs inside Blender. A worker started by this tool inherits the tool's `PATH`, so nothing else is needed on a machine that has Blender, and the artifact is byte-identical to the Editor's with the shader name table above (compared with Blender 5.2.2). Without a `blender` on `PATH` the importer leaves an asset with an import log and no model, so the tool refuses the import before starting a worker and names the missing program. A sandbox that hides the program from the worker has the same effect: a snap-packaged Blender does not start inside `bwrap`, for one.

## Video

On Linux the engine reads VP8 video in a WebM container and nothing else, in the Editor, in a worker and in a Player: an H.264 `.mp4`, `.mov` or `.m4v` and a VP9 `.webm` import as a `DefaultAsset` with the log `Can't create LinuxVideoMedia, format 0 not supported` or `Unsupported video codec 'VP9'`, and a Theora `.ogv` as a clip without frames. No system library is involved; the codecs are compiled into the engine. So a VP8 `.webm` is the one video source that yields a clip with frames.

A worker's artifact for a VP8 `.webm` is byte-identical to the Editor's, with and without transcoding. Transcoded, the clip's frames lie in the artifact's resource file. Not transcoded (the default), the clip names no resource: its frames are the source file, which a build copies. Neither form was played from a scene the worker wrote.

## Scripted importers

A `.inputactions` file is imported by the Input System package's own `InputActionImporter`, run in the standalone worker. The worker's artifact is byte-identical to the Editor's, so every object has the local ID the Editor gives it.

The worker has no compiler; what it needs comes from an Editor compilation of the project, the staging clone of a preparation with a host build (`Library/UnityMicroEditor/stage/project`) or else the project's own `Library`:

- `Library/Bee/*E-inputdata.json` is the Editor's record of that compilation. It lists every assembly with its sources, the assemblies it refers to and the directory of every package.
- The importer's assembly is the one compiled from the script the asset's `.meta` names, and it is loaded together with the assemblies it refers to, from `Library/ScriptAssemblies`. Only immutable packages (`Library/PackageCache/<name>@<hash>`) qualify. An importer's assembly that uses a precompiled assembly of a package or the project is refused; the check is whether the compiled code names that assembly. (A [build in a worker](#projects-with-a-scriptable-render-pipeline) copies such assemblies next to the others, where the worker loads them; that is verified for the render pipeline packages only.)
- The scripts and assembly definitions those assemblies were compiled from are copied into the context where the Editor compiled them, and registered as script compilation assets. An assembly records each script by its path relative to the project; the worker gives an imported object its `MonoScript` only when it can map that path to a registered asset, and it reads where a package lies from the database's root folders only when the database states a refresh version.

The scene worker loads these assemblies for every scripted importer that an asset of the scene's closure names in its `.meta`, not only for `.inputactions`: Dungeon's TestRoom refers to a `.bt` asset of a project importer and was written that way ([Scenes in the Player format](#scenes-in-the-player-format)). Each such importer runs as the project's code in the worker; only the Input System's was compared with the Editor's artifact.

## Sprite atlases

A `.spriteatlasv2` is packed by an importer, `SpriteAtlasImporter`, when the project's sprite packer mode is Sprite Atlas V2 (`m_SpritePackerMode: 5` in `EditorSettings.asset`); in any other mode the importer leaves an atlas without a packed result. A standalone worker runs that importer, and its artifact is byte-identical to the Editor's (compared for an atlas of five sprites from three textures, one of them a sheet, and for an atlas of one sprite).

### Importing with dependencies

The importer packs the sprites of other assets, so it needs their artifacts. It asks the worker's database for them; a worker that does not find them answers the import with error 6 and the addresses it lacks, and the Editor imports those and asks again. The coordinator does the same with two workers of one run:

1. The first worker imports the textures the atlas packs.
2. The coordinator writes what that worker returned into the `Library/ArtifactDB` the worker left in the context, as the Editor commits an import: the result becomes the current revision of its address (`CurrentRevisions`, `ImportAddressToImportResultIDs`), and the three blobs of the response become the records of the result (`ImportResultIDToArtifactDependencies`, `ImportResultIDToArtifactMetaInfo`, `ImportResultIDToImportStats`). The key of an address is the XXH3-128 of its GUID, importer ID and importer hash (`ArtifactDBWriteTxn::SetCurrentRevision`). The database is written only between workers, when no process has it open.
3. The second worker imports the atlas. It finds the textures' revisions in the database and reads their files from the DataStore, which stays up between the two.

`project-import` does this for a `.spriteatlasv2` given together with its textures.

### In a session

No scene refers to an atlas: a sprite belongs to one because the atlas lists its texture, one of its sprites, or a folder above it. The scene worker imports what the scene reaches by GUID, so an atlas that nothing in that closure names is not imported with the scene, and its packing is not applied. A packed atlas in a scene the worker wrote was not verified.

### Atlases packed by a build

A `.spriteatlas` (the first format) is not packed by its import: the worker, like the Editor, runs `NativeFormatImporter` over it and yields the authored asset. The Editor packs it when it builds a Player or an AssetBundle, in Sprite Atlas V2 mode as well: a bundle built from a sprite texture that such an atlas lists holds the packed atlas and the sprite bound to it. Nothing outside the Editor's build does that packing, and the scene worker runs no packing step, so the tool does not deliver such an atlas. To deliver one, the coordinator would have to give the worker a `.spriteatlasv2` made from the authored asset (the same packables, with the texture, packing and platform settings moved into the importer's `.meta`, under the atlas's GUID), and the result would have to be compared with a build of the first format; neither was done.

## Text assets

TextScriptImporter sources are imported by the worker as the one TextAsset they yield: `.txt`, `.json`, `.bytes`, `.xml`, `.csv`, `.html`, `.htm`, `.yaml` and `.fnt`.

## Shaders

A project without a scriptable render pipeline gets its `.shader` sources as described first. A project with one gets `.shader`, `.shadergraph` and `.compute` sources from the Editor's own build code, run in a worker; see [Projects with a scriptable render pipeline](#projects-with-a-scriptable-render-pipeline).

The worker parses a `.shader` and leaves a `Shader` object with the parsed passes and the source of every program, but no compiled program. A build compiles each program with `UnityShaderCompiler`, a separate program the Editor ships in `Data/Tools`, and stores the results in compressed tables. The tool does the same (`rust/src/playerbake/shader.rs` and `shader/`, `rust/src/native/shader_compiler.rs`): the worker imports the shader, the tool starts the compiler, takes the Editor's side of its local socket protocol, asks for every variant a build compiles and writes the Player's object through the Player's type tree. The main Editor does not run. A new or changed `.shader` is published as a file of its own, and a watched session loads the scene again with it in the running Player. An unlit shader of two passes takes about three seconds, most of it the worker.

What is reproduced is an AssetBundle build for Linux standalone, started from the command line, of the shader and the materials of the project that use it. The result is compared byte for byte with objects built by Unity `6000.6.3f1`: 83 objects of 48 shaders, built with default settings, without materials, with changed settings (linear colour space, another lightmap encoding, the unified precision model, deferred reflections off, hardware tiers that differ, custom lightmap and fog stripping, instancing variants kept, a shader chunk size of 1 MB) and for OpenGL Core alone, Vulkan alone and both listed in either order. 69 are equal and 14 are refused for a reason of the list below; none differs. `rust/tests/testdata/playerbake/shaders` holds 28 of them with the compiler's replies, each stored under the hash of its request, so the test also pins what the compiler is asked. `UNITY_PLAYERBAKE_SHADER_CORPUS` with `UNITY_PLAYERBAKE_EDITOR` compares directories of the same form with the real compiler. The rules below were read out of the Editor binary and then confirmed against those builds.

Covered: `CGPROGRAM` and `HLSLPROGRAM` passes with `HLSLINCLUDE` blocks, vertex, fragment, geometry and tessellation stages, surface shaders (the worker generates their passes), fixed-function passes (the worker generates their programs), grab passes, several subshaders, every property type and attribute, render state given as values or as properties, includes of files under `Assets` and `#include_with_pragmas`, `multi_compile` and `shader_feature` in their global, local and per-stage forms, `dynamic_branch`, `skip_variants`, `only_renderers` and `exclude_renderers`, `multi_compile_instancing`, `multi_compile_fog`, `multi_compile_fwdbase`, `multi_compile_fwdadd_fullshadows`, `multi_compile_shadowcaster`, `hardware_tier_variants`, structures, arrays, integer and matrix constants, constant buffers, every texture dimension, separate and inline samplers, buffers and random-access targets.

Which variants are compiled:

- A `multi_compile` group is compiled in every combination. A `shader_feature` group is compiled in the combinations the materials enable: a material's keywords are reduced to those of the pass, and a material that enables two keywords of one group, or a `dynamic_branch` keyword, adds nothing. Without materials only the first combination remains.
- The built-in keyword sets (`multi_compile_fwdbase` and the like) are stripped as a build strips them: stereo variants always; instancing variants unless a material enables instancing or the project keeps them; lightmap, shadowmask and fog variants by the project's stripping settings. Under automatic stripping a build reads the scene open in the Editor, and a build from the command line has none, so fog and lightmap variants are left out. This is the one point where the result is narrower than an interactive build can be: a shader that needs fog or lightmap variants needs custom stripping in the graphics settings.
- A pass is enumerated by material usage until one graphics API has no more than one combination; every later API of that pass then gets all combinations, unstripped, as in the Editor.
- A pass whose `LightMode` is `Meta` is kept only when the project keeps dynamic lightmaps; `Picking` and `SceneSelectionPass` passes are dropped.
- Hardware tier variants are compiled when the three tiers of the standalone target differ or the pass asks for them.

A shader is compiled for the graphics APIs the project lists for its Linux standalone target, of OpenGL Core and Vulkan, and for OpenGL Core alone when it lists none (a build of such a project compiles Vulkan too, which no session loads); a build visits them by their number, whatever the listed order. The settings read are `ProjectSettings.asset` (graphics APIs, colour space, precision model, virtual texturing, lightmap encoding, shader chunk size), `GraphicsSettings.asset` (deferred reflections, tier settings, stripping), `QualitySettings.asset` (level-of-detail cross-fade) and `Packages/manifest.json`. A file the project lacks stands for the engine's defaults.

The parameters all programs of a pass share move into the pass, as in a build, and each graphics API gets a table of entries cut at the project's chunk size and compressed with LZ4 at high compression level 9. The reference LZ4 library is linked (`lz4-sys`, MIT), since the compressed bytes depend on its match search.

`Fallback` and `UsePass` name another shader. When that shader is a source of the project, it is baked and published first, and the object lists it and takes over its keywords as a build does; the reference to it leads to the other published file, which is the one deliberate difference from a bundle, whose references depend on its layout. Chains of fallbacks are followed.

A baked shader is kept with the hashes of everything it was made from: the files it includes, the three settings files, the materials that use it and the shaders it names. A session bakes it again when one of them changes or a material starts to use it. The programs of a shader are compiled one after another by one compiler process, so a shader of thousands of variants takes minutes; a pass of more than a million keyword combinations is refused.

Refused, never approximated:

- a `Fallback` or `UsePass` that names a shader built into the engine (`VertexLit`, `Diffuse`), a shader the project has no source for or one that several sources declare, a used pass its shader lacks, and a shader whose keywords more than one other shader extends, for which the order was not established;
- a program that does not compile, with the compiler's message, and a program the compiler warns about. A build leaves a failed variant out and goes on;
- a shader with an import error; an include that is not a literal path; an include of a package file, which this route's worker project does not hold;
- a material variant that uses the shader, whose keywords come from its parent;
- a Linux graphics API other than OpenGL Core and Vulkan, non-empty shader build settings, 360 stereo capture, a pass that selects a compiler toolchain, a ray tracing pass, a pass without a vertex program;
- a shader in `Packages`.

Not covered:

- What a build takes from outside the project's files: scriptable stripping (`IPreprocessShaders`), shader variant collections, XR, native shader plugins. A project that has one of these gets the variants described above, not those of its own build.
- A material-less shader used only from code (`Shader.Find`, `new Material`) gets no `shader_feature` variant beyond the first.
- Compute shaders and shader graphs of a project without a scriptable render pipeline: the worker imports a `.compute`, but its kernels are compiled by another request of the compiler and laid out otherwise, and a `.shadergraph` is imported by a package's script. The route below would write both, but it is verified for pipeline projects only.
- A Player loaded shaders with one and two passes, keyword variants, an include, a fallback and a used pass, on OpenGL Core. The Vulkan programs are compared with the engine's but were not run.
- Rules read from the Editor binary that no reference build exercises: virtual texturing, entities graphics, a `stats:` or warning line in a compiler reply.

### Projects with a scriptable render pipeline

When the graphics or quality settings name a render pipeline asset, the variants a build keeps are decided by the pipeline's Editor C#: the keyword filters on the pipeline asset and its renderers, and the `IPreprocessShaders` strippers of the packages. None of that is reproduced. Instead the Editor's own build code writes the Player object, inside a standalone import worker, and the main Editor still does not run (`rust/src/native/player_objects.rs`, `build_settings.rs`, `artifactdb.rs`, `tools/WorkerScripts/PlayerObjectImporter.cs`). This covers `.shader`, `.shadergraph` and `.compute` sources, includes of package files and shaders that name a shader of a package.

How one source is built:

1. The coordinator finds from the sources what a build reads beside the asset: the pipeline assets of the quality levels and of the graphics settings with everything they refer to (renderers, renderer features, their shaders and textures), followed through every text asset by GUID; the settings objects the graphics settings keep per pipeline (the pipeline's global settings); every volume profile under `Assets` with what it refers to; the files the shader includes, through every branch; the shaders it names with `Fallback` or `UsePass`, in the project or in a package; the materials of the project that use it and the parents of those that are variants. For a shader graph the sources it refers to by GUID (sub graphs, custom function files, textures) come first.
2. It takes the script assemblies from an Editor compilation of the project, as for [scripted importers](#scripted-importers): those of the scripts of the objects found in step 1, every assembly that names `IPreprocessShaders` or `IPreprocessComputeShaders`, what they refer to, and the precompiled assemblies those use (`nunit.framework.dll` and the like), copied next to them. Only immutable packages qualify.
3. All of it goes into one import context as one batch, in dependency order, with the project's `GraphicsSettings.asset`, `QualitySettings.asset` and `ProjectSettings.asset` given to the worker as its settings managers. The last asset of the batch is a request file for the tool's own `ScriptedImporter`, which the tool compiles with the SDK's Roslyn into the context's `Library/ScriptAssemblies`; the worker picks it by the file extension.
4. After every import the coordinator records the result in the context's `Library/ArtifactDB` (below). That is what lets a later import of the batch load an earlier one: without it `AssetDatabase.LoadAssetAtPath`, `Shader.Find` and every reference to an imported asset give null in a worker, without an error.
5. The importer loads the asset, the materials and the settings, computes the usage of the materials (`ContentBuildInterface.CalculateBuildUsageTags`), starts the build's shader callbacks and calls `ContentBuildInterface.WriteSerializedFile` for the Linux standalone target. Variant enumeration, keyword filtering, stripping, compilation through the worker's own `UnityShaderCompiler` processes and compression are the engine's and the packages'.
6. The coordinator takes the object out of the written file and publishes it like any baked shader. A shader of a package that the object names (URP's `FallbackError`) is no source a session publishes, so the build writes it into the same file, as a bundle build does for an asset no bundle is named for; a named shader under `Assets` is published as a file of its own.

What is reproduced is the same build as above: an AssetBundle build for Linux standalone, started from the command line, of the asset and the materials of the project that use it. Compared byte for byte with objects built by Unity `6000.6.3f1` and URP 17.6.0 in a reference project with the pipeline assets, shaders and materials of a game: 2 hand-written URP shaders (one of 12 programs and 2 materials, one of 1,176 programs and 280 materials, 78 of them material variants), 4 shader graphs (0, 81, 108 and 108 programs; Built-in and URP targets) and 1 compute shader of two kernels. All 7 equal the Editor's objects in every byte except the references to other files, which depend on the layout of the file (the shader without references and the compute shader are equal outright); the package shaders written along with them equal theirs in the same way. The 6 shaders and graphs were also built in a copy of the game's own project, with its script assemblies and its whole `Assets`, and gave the same objects. `UME_PLAYER_BUILD_PROJECT` with `UME_PLAYER_BUILD_CASES` and `UME_LIFECYCLE_SDK` repeats the comparison (`builds_player_objects_like_the_editor`); no fixture of it is in the repository, since it needs the URP packages and an Editor compilation.

A Player loaded a new URP shader with package includes, a `multi_compile` and a `shader_feature_local` keyword and a package fallback, rendered a material with it under URP on OpenGL Core, and rendered the edited shader and the material's other keyword variant after a re-bake. It also drew a cube with a material of one of the shader graphs (a URP Lit graph) and with a material of the package's `Lit` shader; both gave the pixel the same materials gave with the shaders of a bundle the Editor built.

#### Shaders of packages

The worker names a shader of a package (`Universal Render Pipeline/Lit` and the like) that the scene or its dependencies refer to as it names a source under `Assets`, and the session builds it the same way: with every material of the project that uses it, in a worker. A build without the project's materials keeps the first combination of every `shader_feature` alone, so a material that enables `_EMISSION`, `_NORMALMAP` or `_ALPHATEST_ON` would be drawn with a variant without it, and nothing would say so (measured: an emissive `Lit` material came out black). The object is kept with the hashes of what it was built from and built again when one of them changes, for one when a material's keywords change. A preparation does the same. A session that reuses its output keeps the object while nothing has changed.

Every importer keeps what it built beneath its own output: the scene's importer and the importer of each registered content scene. Each of them used to build the shader for itself, so a cold run of a project whose content scene uses `Lit` built it twice into the same bytes (measured: 444 s, then 43 s with the compiled variants). The built objects are therefore also kept for the project, in `Library/UnityMicroEditor/built-sources`, and an importer that plans the same build takes them from there and reports `native-build-reused` instead of `native-built`. This also serves a session that starts with a new output directory.

- The key of a kept build is the SHA-256 of everything the build is made from: the hash of the tool's build importer, the import environment (engine version and project settings), the identity of the project's compiled Editor code, the source and its `.meta`, the materials it is built for, the request sent to the worker, the assets the worker imports in order, the scripts whose assemblies it loads, and the hash of every authored file among them with `Packages/manifest.json` and `Packages/packages-lock.json` standing for the immutable packages. A build is found under its key only, and used only while its file has the hash recorded with it; anything else is a build.
- The identity of the compiled Editor code changes whenever the Editor compiles the project's scripts, so after a script change the first importer builds again (with the compiled variants of the shader cache) and the others reuse that.
- The most recently used builds are kept, up to 4 GiB of files and 1024 builds; the directory can be deleted at any time.

The store used to keep 48 builds. URP and SRP Core resources are about 85 shaders and compute shaders, so every preparation dropped each build before the next one asked for it again, and none was ever reused.

A pipeline's resources are found one by one: the session stops at the first one it lacks, the importer builds it, and the session tries again. So the first time the importer builds a shader of a pipeline project, it builds the rest side by side beforehand (`native-prebuild`, then `native-prebuilt`). These are the shader sources that the graphics and quality settings reach through settings assets. Sources in an `Editor` folder are skipped. One worker runs for every three cores and every 2 GiB of available memory, whichever allows fewer, between one and six workers, and each result goes into the store under the key of its inputs. A source built for nothing costs time, not correctness. A prebuild that fails is reported (`native-prebuild-failed`), and the importer builds that source again when it asks for it.

A build changes two things that every build reads, and builds running side by side write both safely:
- the list of assets the pipeline loads by path, which each build merges into and replaces whole;
- the shader cache, which gets each copied file under a name of its own before the rename.

Measured on the URP copy with a fresh host: a preparation built 85 sources one after another in about 36 minutes. With the store and five workers, the whole preparation took 11 minutes: 54 sources were prebuilt in about 4.5 minutes and 82 builds were reused.

- Cost: `Lit` with one material took 372 s the first time and 35 to 55 s with the compiled variants kept in `Library/UnityMicroEditor/shader-cache`. A project with many materials on `Lit` compiles every combination they enable. The materials are among what the object is built from, so any saved change of a material that uses the shader, not only of its keywords, is one more worker run before the scene is rebuilt.
- Only shaders that the written scene and its dependencies refer to (and those of registered content scenes and addressed prefabs) are built. A shader a script finds by name (`Shader.Find`) is in no such closure and stays with the host's content.
- When the build is refused (the reasons below), the preparation fails with the reason. Nothing is published in place of the shader.
- The pipeline's own uses of a package shader (its default materials, its renderer features) are not counted as materials of the project.

A Player ran a compute shader delivered by a session of the URP sample: a `.compute` of two kernels over a `RWStructuredBuffer<float>`, dispatched one after the other, gave the expected sixteen values through `ComputeBuffer.GetData` and through `AsyncGPUReadback`, as a new asset and again after its source was edited between two runs.

In a watched session the running Player took both kinds of edit without a restart: an edited `.shader` was built and the same process rendered the new colour 29 s later, an edited `.compute` gave the new values 16 s later.

#### What the worker needs to find imported assets

A worker resolves an asset it loads by reading `Library/ArtifactDB` of its project directly, with a new LMDB read transaction per lookup; no message of the import protocol is involved. Three tables have to hold a record of the import: `ImportAddressToImportResultIDs`, `ImportResultIDToArtifactDependencies` and `ImportResultIDToArtifactMetaInfo`. The key of an import address is XXH3-128 (seed 0) over its GUID, importer ID and importer hash, low half first; the values are the three blobs of the import response as they came. The coordinator also writes `CurrentRevisions`, `ImportResultIDToImportStats` and `ImportResultIDPropertyIDToProperty`, as the Editor does. A reader takes the newest revision from the transaction number in `ArtifactDB-lock`, so a commit writes the pages, then the metadata page, then that number (`lmdb::commit`).

Every commit rewrites the whole database. On the largest batch of the comparison (448 imports, database 3.3 MB at the end) all commits together took 1.7 s of a 649 s run, on the smallest (110 imports) 0.16 s of 25 s, so it is left as it is. The cost grows with the square of the batch, and the file is bounded at 128 MB, which a batch of the 4,000 assets one context holds stays far below. The writer does not take LMDB's writer lock: the worker was only seen to write this database at start-up, which is an observation of the file, not a reading of its code.

An asset search (`AssetDatabase.FindAssets`) walks the folders of the `SourceAssetDB`. The Editor puts the hash of a folder's list of children before the list; the coordinator's database lacked it, so the first child of every folder was taken for the hash and never found. URP searches for volume profiles this way. The list now has the Editor's form, and a build is refused if the worker's search finds other volume profiles than the sources hold.

#### The build callbacks

Two things the importer does are not public API, because no public entry runs a build's shader callbacks short of a whole build:

- It calls `UnityEditor.Build.BuildPipelineInterfaces.InitializeBuildCallbacks` and `CleanupBuildCallbacks`, the entry the scriptable build pipeline package uses as a friend assembly. Without it `WriteSerializedFile` runs no `IPreprocessShaders`.
- URP's `UpdateShaderPrefilteringDataBeforeBuild` prepares every pipeline asset of the build in its constructor and then saves the asset, and `AssetDatabase.SaveAssetIfDirty` is refused inside an import. The exception leaves the Editor's list of callbacks half filled. The importer then fills the two lists itself (`processors.shaderProcessors`, `computeShaderProcessors`), taking that one class without its constructor. This is allowed only for that class, only when the exception is the refused save, only when the class has no instance fields, and only when the build is made with exactly one render pipeline asset (as the public `BuildTargetExtensions.TryGetRenderPipelineAssets` of the pipeline core package lists them): the save is refused after the first asset is prepared, so with one asset everything a build takes from the constructor is in place. Any other callback whose constructor fails, and a build with several pipeline assets, is refused with its name.

#### Costs

One build is one worker run: about 20 s for a shader of a few programs (110 imports, most of them the pipeline's settings closure), 11 minutes for the shader of 1,176 programs when nothing is cached and 85 s when its variants are. The worker's compiled variants (`Library/ShaderCache` of the context) are kept in `Library/UnityMicroEditor/shader-cache` of the project and given to the next worker, as the Editor keeps them in a project's `Library`; a file there is named after everything its variant was compiled from, and the object built from the kept variants was the same. A shader graph takes two or three runs, 60 to 100 s for the graphs above: its importer leaves out an include whose file is not an asset of the context and notes the path, so the graph is imported until nothing is left out, then built. The templates of the graph targets are copied into the context as plain files, which is where the importer reads them.

A baked source is kept with the hashes of the project's files it was built from: the pipeline assets and what they refer to under `Assets`, the volume profiles, the included files, the materials and their parents, the two settings files and the package manifest with its lock file. Files of a cached package are not hashed; the manifest and the lock file stand for them. A session builds the source again when one of these changes.

#### Refused in a pipeline project

- A project without an Editor compilation (`Library/ScriptAssemblies` and `Library/Bee/*E-inputdata.json` in the staging clone or the project): the pipeline's code comes from there.
- An assembly that names a shader callback and is not compiled from an immutable package: the project's own `IPreprocessShaders` in `Assets` or in an embedded package. A script of the project's own code on a pipeline asset, renderer or renderer feature. A class of a precompiled assembly there.
- A 2D renderer among the settings: with 2D variant stripping a build chooses the light variants from its scenes.
- A build made with more than one render pipeline asset for the platform, when a callback saves assets in its constructor (URP does): see above.
- A shader program with a compile error, with the compiler's message; an object whose program tables are empty or hold an empty program although its passes list programs, which is what a compile that did not find an include leaves.
- A shader that names a shader no source of the project or its packages declares (one built into the engine), or one that several sources declare; a reference from the written object to anything but a shader source under `Assets` (a default texture of a shader, for one).
- A material variant whose parent has no source, or whose chain returns to itself or is longer than 32. Other variants are built: the Editor merges them itself.
- An asset the pipeline settings refer to that the worker cannot import, with the worker's message; settings that refer to more than 3,000 assets; a batch of more than 4,000.
- An included file or other asset without a `.meta`; a source in a package that lies outside the project.
- A shader graph or compute shader in a project without a scriptable render pipeline (not verified there).

#### Not covered or not verified

- What the settings objects of the pipeline (URP's global settings asset) refer to is not loaded in the worker: the pipeline's runtime shaders, textures and debug windows, several of which a standalone worker cannot import. A build's shader code reads the settings held inside the object; that this is all it reads is established by the comparisons above, not by a proof.
- One pipeline (URP 17.6.0), one set of pipeline assets, Linux standalone with OpenGL Core and Vulkan. HDRP, a custom pipeline, a project with its own shader strippers, XR and other package versions were not run.
- An asset a pipeline object loads by path when it finds a field empty (`ResourceReloader`) is taken from the worker's message, added to the batch and the run repeated, at most 24 times; the list is kept in `Library/UnityMicroEditor/pipeline-reloads.json`. With the references of the pipeline assets staged this did not occur in any run, so the retry itself is covered by a unit test only.
- A shader graph's default material (an object of the graph asset beside the shader) is not delivered. A graph that refers to an asset its importer loads through a path rather than a GUID is not followed. Of a graph's sources only those the project holds are hashed; a file its custom functions include from `Assets` is one of them only when the graph names it by GUID.
- Of the shader graphs a Player rendered one, with one material. The compute shader a Player ran used a structured buffer on OpenGL Core; textures as kernel targets, several buffers and Vulkan were not run.
- Materials found for a shader are those of the project's sources; a material that only a package holds is not counted, as in the first route.
- The scripts of the packages run in the worker with the state the worker has. A stripper that reads Editor state a worker lacks (the open scene, build profiles, XR settings) gets the worker's state. URP's reads of the quality levels, the graphics settings and the volume profiles were checked against the worker's report; nothing else was.

## Scenes in the Player format

Every scene a session runs is written by a standalone worker as a build writes it, and the Player loads it with the engine's own loader. There is no other scene path: the tool's former loader, which rebuilt a scene from its YAML through the public API, is gone, and so are its per-class component adapters, its templates for components without an adapter, its imported asset packs and its UI and mesh collider retention.

- **What the scene is made of.** The session follows the GUIDs of the scene, of the render pipeline asset and the preloaded assets the baseline names, of the prefabs the game asks for by address, and of every text asset they reach (prefabs, materials, scriptable objects, controllers, clips and the like). Each of those files and its `.meta` is a dependency of the revision (`Inner::native_scene_over`, `rust/src/project/native_scene.rs`). Their hashes are the key of the written scene. A key the session does not hold is asked for with `NeedsNativeScene`, and the importer writes that scene and prepares again (`NativeProjectImporter::prepare_sources`, `rust/src/native/importer.rs`).
- **Writing.** The importer (`rust/src/native/native_scene.rs`) has the project's scene worker import the scene, what it refers to and what a build of the project reads, in one context. The order is the Editor's: textures, sounds and the other assets that refer to nothing first, then materials, then models (a model looks its textures up by path, and a prefab refuses a model imported before them), then everything else, the scene last. A shader graph brings its sub-graphs and included files, and the templates of its targets as plain files. The worker loads the assemblies of the project's Editor compilation that define the scripts of the scene's objects and the scripted importers of its assets (a project's own `.bt` importer, for one), those that refer to an importer's assembly from an immutable package (a graph's targets), and those they refer to. Every script of those assemblies is an asset of the context, since the worker ties a script to its class among the scripts of its assembly and with some missing ties it to another class; an assembly of a package the project holds in place is loaded without its scripts.
- **The write.** The last asset of the context is a request for the tool's `ScriptedImporter` (`tools/WorkerScripts/PlayerObjectImporter.cs`, compiled with the SDK's Roslyn). It calls `ContentBuildInterface.CalculatePlayerDependenciesForScene` and `WriteSceneSerializedFile` for the Linux standalone target: the engine opens the scene from its source, unpacks its prefab instances, leaves out EditorOnly objects and writes the scene's objects with its settings managers. What the scene refers to, but the engine's own resources and the shaders, is written with `WriteSerializedFile` into a second file, `ume-deps` (scripts first), which the scene refers to. The Editor drops the extension of a scene file's name. The build's shader callbacks run as for a [shader of a pipeline project](#the-build-callbacks).
- **What else the file holds.** The render pipeline asset, the preloaded assets and the addressed prefabs are written into the file of what the scene refers to with their closure (`include` statements of the request), and `nativeScene.aliases` of the manifest maps each source (`guid`, `fileId`) to the object the file holds (`file`, `localId`), so that the Player configures the pipeline and holds the preloads before the scene loads. The pipeline's global settings are not written: a build strips them by the pipeline's rules, and their closure holds Editor resources the worker crashes on. The Player keeps the global settings of the host's build.
- **Shaders are built on their own.** A shader of the project or a package is not written into that file: the worker names its objects by the external file `ume-asset/<GUID>`, and the session publishes the shader through the [shader pipeline](#shaders) (built with every material of the project that uses it, with the build's strippers and the kept variants). A shader it lacks, or made from an older source, is a `NeedsImportError`, which builds it. Written into the scene's file, a shader enumerated every variant it declares (16.6 billion for one of Dungeon's) whatever usage was given.
- **Streamed data.** The file's textures and sounds keep their data in files named after it (`ume-deps.resS`, `ume-deps.resource`), which the Player reads from the file's directory: each set is published in a directory of its own under `native-store/baked`.
- **The worker lives on.** One worker serves every session of a project in a process (`rust/src/native/scene_worker.rs`), so the scenes of a session (its own, its content scenes) are written one after the other. When the scene is written again and the same assets with the same `.meta` files, the same assemblies and scripts and the same request make it up, the worker is not started again: the files under `Assets` that changed are copied into its context, their records in the context's `SourceAssetDB` are updated in place (the worker refuses a file whose time or size differs from its record), they are imported again, and a `revision` line in the request has the scene written again. Anything else, or a failure of that (`scene-worker-restarted`), starts a new worker over a new context. A worker nobody asks for stops with its DataStore after `UME_SCENE_WORKER_IDLE_SECONDS` (120 by default; `scene-worker-stopped`), since it holds more than a gigabyte with its shader compilers; it also stops when the session ends. The variants its shader compilers made are copied to `Library/UnityMicroEditor/shader-cache` for the next worker.
- **The project's library.** A worker that stops moves its import results (`ArtifactDB`, `ArtifactDB-lock`) and the store of their artifacts (`DataStore`) to `Library/UnityMicroEditor/native-store/scene-library`, with a `state.json` that records the hashes of the sources and `.meta` files they were made from and a key of the assemblies and scripts they were made with. The next worker moves them into its new context and imports only what is out of date (`scene-library` gives how many imports were kept): a source or `.meta` that changed, and a text asset that refers to such a source by GUID, transitively. Every other text asset (prefabs, materials, scriptable objects, clips) is imported again only when the assemblies or scripts differ; models, images, sounds, videos and fonts are taken whenever their file and `.meta` are the same and the worker loads the same assemblies, since the asset postprocessors of those take part in their import (an edit of a script's body keeps them). A postprocessor that appears in an assembly already loaded is not noticed: the worker then reports the model out of date (importer error 6) and the preparation fails until the library is removed. Whoever takes `state.json` takes the library: while one worker holds it, another starts without it. A worker that fails to start or import puts back what the library held.
- **Keeping.** The written files are kept in `Library/UnityMicroEditor/built-sources` under a key of the sources' key, the importer's code, the import environment and the assemblies and scripts, so a revision whose sources were written before takes them from there without a worker (`native-scene-reused`; otherwise `native-scene-written`).
- **The manifest** holds no object records: `objects`, `assets`, `importedAssetBindings` and `importedAssetPacks` are empty. `nativeScene` names the scene file, its hash, the file of each of its external references in their order (`ume-deps` becomes the dependencies file; the engine's resources are global dependencies) and the aliases. `nativeContentFiles` holds the dependencies file and the published shaders it reaches.
- **Addressables and content scenes.** The prefabs the game asks for by address are named by their roots in `nativeScene.aliases`, where the runtime finds them (`RawProjectContent.TryLoadPrefab`). A content scene is a session of its own, written by the same worker; the Player loads it beside the session's scene, and one built into a scene the Addressables provider loaded gives its roots to that scene once they woke. Only the session's own scene becomes the active scene and configures the pipeline.
- **The Player** (`Packages/com.unitymicroeditor.runtime/Runtime/Raw/NativeSceneRuntime.cs`) loads the content files, maps the aliases, loads the preloaded assets and sets the render pipeline (`GraphicsSettings.defaultRenderPipeline`, `QualitySettings.renderPipeline`), runs the `BeforeSceneLoad` initializers, retires the previous scene's objects and loads the scene with `ContentLoadInterface.LoadSceneAsync` (additive, integrated at the end of the frame). The engine runs `Awake`, `OnEnable` and the rest of the lifecycle itself; the compiler no longer rewrites lifecycle callbacks, `SetActive`, `isActiveAndEnabled`, `Transform.root` or `GetComponentInChildren`. When the engine reports the scene loaded, it becomes the active scene, which gives it its lighting settings, and the `AfterSceneLoad` initializers run. The game's `[RuntimeInitializeOnLoadMethod]` methods are called through the public bridges `__UnityPlay_RuntimeInitialize_*` that the lifecycle compiler (`tools/LifecycleCompiler`) adds to their classes. A reload loads the new scene and unloads the old one with its file at the end of the frame; a scene is never patched in place.

Verified on 2026-10-20 on a copy of `Samples/RawProject` with the AI module (`Temp/e2e/nav`), in the session's host Player: `auto-prepare` took 21 s, of which the worker run took 7 s (dependencies 15 ms, the scene 9 ms; the rest is the worker's start and the imports). The sample's `Awake` checks passed, its proof lines reported the authored material color, texture, prefab override, camera background, asset names and the obstacle's and agent's fields, and 200 frames ran without an error. In a watched session a moved object was in the reloaded scene, and `Awake` and `Start` ran again; a scene the store held was in the Player 0.3 s after the edit. With the worker that lives on, the first edit of a session took 6.0 s until `Start` (the worker's start 5.8 s), the next three 0.5 s each (the worker's write 0.30 s); with a 15 s limit the worker and its DataStore stopped by themselves, and none was left once the session stopped. A copy of the sample ran 200 frames with every proof line again after the former loader was removed.

Verified on 2026-10-20 on the Addressables copy of the sample (`Temp/e2e/addr`): every proof line passed — a prefab by address and its instance with its material, the same prefab by reference, by label and by `InstantiateAsync`, the host's material, the content scene loaded additively with its roots and the active scene unchanged, unloaded and loaded again.

Verified on 2026-10-20 on a copy of Dungeon (`Temp/bench/nav`, `scripts/nscene.sh`): TestRoom, which refers to a `.bt` asset of a project importer, ran 600 frames in the host Player without an error or exception; its bootstrap entered the hub and made the local player. The cold preparation took 868 s: the worker's run of the scene 258 s (1,957 imports), the shaders of the pipeline 548 s (kept for later preparations). A preparation with the project's library took 83 s, of which the worker took 29 s and kept 1,955 of the 1,957 imports. One warning remains: a behaviour of the instantiated player prefab reports a missing script although every script of the prefabs is in the host's assemblies.

Limits:

- The worker is the pinned Unity `6000.6.3f1`; a worker of another version is refused.
- Writing a scene needs an Editor compilation of the project, as [scripted importers](#scripted-importers) do: the worker loads the assemblies from there. A watched session does not compile them again, so the worker keeps writing script objects with the classes of the compilation the preparation made. What that means for a serialized field added or renamed during the session was not verified.
- The first write of a session, and one whose assets or their `.meta` files changed, starts a worker (about 6 s on the sample).
- Every edit that changes what the scene is made of loads the written scene again, so the scene's state starts over; names, active states and poses are not patched into the running scene.
- The first worker of a project imports everything its scene refers to (258 s for TestRoom); with the library, a later worker took 29 s for TestRoom. Every content scene (Addressables) is a worker run of its own.
- The pipeline's global settings are those of the host's build, not the project's current ones.
- The Player reads back only the objects the aliases name; one sprite of Dungeon's file was not read back by its index.
- `typeDB` (the Player compilation of the scripts) is not given to the write: whether a script's fields under `#if UNITY_EDITOR` are then left out as a build leaves them out is not checked.
- The agent's runtime tools find objects by hierarchy path or by a unique name. Source file IDs of runtime objects do not exist: the scene has no object records.
- A content scene built into the Addressables provider's scene keeps an empty scene of its own loaded beside it until it is unloaded.
- A session disposed before the Player quits leaves the scene's files loaded until it does.
- A session depends on prepared content: the Editor compilation the preparation made or found, and the host Player, whose built-in resources, pipeline global settings and shaders found only by name (`Shader.Find`) the scene gets. A project without an Editor compilation cannot be written.

## UI and fonts

UXML, USS, TSS and font assets are imported by the worker with UI Toolkit's own importers, like any other source of the scene's closure. The coordinator writes the register of the engine's scripts (`mem://Library/MonoManager.asset`) into every source database, because those importers add objects of the engine's script classes. See [UI_IMPORTS.md](UI_IMPORTS.md).

## Limits

- The wire protocol, LMDB schema and DataStore layout were derived for one Unity version and are not a stable public API. A worker version mismatch is refused, and another engine version needs its own verification.
- An import is the engine's. The worker's artifacts were compared with the Editor's only where this document says so: animation inside models, skinned meshes, complex model settings, texture formats of other platform families and custom `AssetPostprocessor`s were not compared or verified in a worker.
- Video other than VP8 `.webm` yields no clip on Linux, and sprite atlases of the first format (`.spriteatlas`) are not packed.
- `.obj`, `.dae`, `.blend`, scripted importers and every scene write need files of an Editor run or compilation of the project (the shader name table, the script assemblies); a project that has none is refused with that reason.
- A shader is delivered only in the cases listed under [Shaders](#shaders). In a project with a scriptable render pipeline a shader, shader graph or compute shader needs the script assemblies of an Editor run of the project, like the entries above, and is refused in the cases listed [there](#refused-in-a-pipeline-project); in a project without one, shader graphs and compute shaders are not verified.
- The store reader handles inline and concatenated storage, limits one file to 64 MiB and refuses other layouts. The LMDB reader targets the observed Linux x64 format.

## Code

The coordinator lives in `rust/src/native/`, with the wire format, LMDB reader and SourceAssetDB writer beside it:

| File | Responsibility |
| --- | --- |
| `rust/src/native/import.rs`, `context.rs` | `project-import`, owned contexts and what a worker run is given |
| `rust/src/native/importer.rs` | The native cache of a session: the written scene, the shaders it publishes, their index and the published resources |
| `rust/src/native/native_scene.rs`, `rust/src/project/native_scene.rs` | The scene in the Player format: its key, its write, its files and the manifest fields |
| `rust/src/native/scene_worker.rs` | The worker that lives on and the project's library of its imports |
| `rust/src/native/player_objects.rs`, `build_settings.rs`, `tools/WorkerScripts/PlayerObjectImporter.cs` | The scene's closure, what a build reads, the importer that calls the Editor's build code |
| `rust/src/native/built_store.rs` | Written scenes and shader builds kept for the project (`built-sources`) |
| `rust/src/native/probe.rs` | One AssetImportWorker run, and a worker kept alive |
| `rust/src/registry.rs` | The owned SourceAssetDB and native source hashes |
| `rust/src/protocol.rs` | Wire format, requests, responses and `ArtifactMetaInfo` |
| `rust/src/native/datastore.rs` | DataStore namespace, readiness and shutdown |
| `rust/src/lmdb.rs` | Reading a stopped LMDB without a native library; committing a revision to a live one |
| `rust/src/native/artifact.rs` | Extracting stored files and checking content keys |
| `rust/src/native/environment.rs` | The import environment fingerprint |
| `rust/src/native/scripted.rs` | What an import takes from an Editor run: compiled importers, the script register and the shader name table |
| `rust/src/native/baked.rs`, `rust/src/project/native.rs` | The shaders a session builds and publishes, and their validation |
| `rust/src/playerbake/shader.rs`, `shader/`, `rust/src/native/shader_compiler.rs`, `host_shaders.rs` | Shaders of a built-in pipeline project: variants, program tables, the client of `UnityShaderCompiler`, the Editor's built-in shader names |
| `rust/src/native/artifactdb.rs`, `rust/src/lmdb.rs` (`commit`) | The records of accepted imports in a live worker's ArtifactDB |
| `Packages/com.unitymicroeditor.runtime/Runtime/Raw/NativeSceneRuntime.cs`, `NativeContentRuntime.cs`, `NativePipelineRuntime.cs` | Loading the content files and the scene in the Player, the pipeline and the runtime initializers |

The ordinary tests check protocol and database bounds, corrupted content and wrong settings without starting Unity.
