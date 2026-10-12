# Native import

Heavy content (models, audio, video, textures, sprite atlases, text assets, input actions) is imported by Unity's own AssetImportWorker, driven by this tool's coordinator. The main Editor process, its GUI and a pre-imported `Library` are not needed for a worker run. Scenes and authored prefabs do not go through this path; the runtime loader reads them from source.

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

This imports the chosen files. It does not bootstrap a project, and it does not create the baseline or catalog that `run` needs.

Accept a successful import into a session's runtime cache without running the worker again:

```bash
unity-micro-editor native-adopt \
  --project /absolute/project \
  --baseline /absolute/.../current.json \
  --output /absolute/project/Library/UnityMicroEditor/Sessions/dev \
  --report /absolute/project/Library/UnityMicroEditor/imports/manual-01/result.json
```

`native-adopt` checks source, metadata and GUID again, along with every copied dependency, the current import environment and the native XXH3-128 and SHA-256 of each artifact. A report without a complete environment record is refused.

Run the inspectors of material variants in a standalone worker and write what the engine leaves of each ([below](#variants-of-a-shader-with-an-inspector)):

```bash
unity-micro-editor material-inspect \
  --project /absolute/project \
  --output /absolute/project/Library/UnityMicroEditor/inspections/manual-01 \
  --material Assets/Materials/Glass_Variant.mat
```

`--material` can be repeated, and `--list` names a file with one variant a line. The two documents of each variant are `<output>/inspected/<guid>.a.mat` and `.b.mat`; `<output>/result.json` names the inspector that ran, or the reason a variant has no result.

Print the current import environment fingerprint without starting any Unity process:

```bash
unity-micro-editor native-environment --project /absolute/project
```

## During a run

Before expanding a scene, the runner walks the GUID references of the selected scene, its runtime prefabs and their authored dependencies. New or changed models (FBX, OBJ, COLLADA, Blender), audio files, VP8 video, textures, importer-based sprite atlases, text assets and `.inputactions` files are imported in batches of up to 32 assets per worker; the sections below list the source formats and the importer settings each covers. An unchanged baseline, catalog or cache entry is skipped, as is a PNG or JPEG in the supported raw profile.

Each cache entry records an `environmentSha256`. The fingerprint covers the pinned protocol revision, the Unity path, the exact SHA-256 of `ProjectSettings.asset`, the parsed settings and the wire environment. An index with no fingerprint, or a different one, is skipped and its assets are imported again. Changing this environment while a runner is live requires restarting the runner.

## How an import works

1. The coordinator creates an isolated context with its own `Assets`, `ProjectSettings`, `Packages`, `Library` and `Logs`, and copies the exact source and `.meta` bytes into it.
2. The coordinator writes a minimal `SourceAssetDB` directly: GUIDs, paths, importer settings and XXH3-128 file hashes taken from the sources. Nothing is copied from an Editor, except for the two kinds of import that need a piece of one: [models that make their own material](#obj-and-collada-models) and [scripted importers](#scripted-importers). Those run in batches of their own, so what an asset is imported with does not depend on what changed along with it.
3. It starts a DataStore with a unique namespace, accepts the worker's local TCP connection, and sends `Initialize`, `Prepare` and `ImportRequest`.
4. Requests use the primary importer ID `-1`, so the native pipeline picks the importer and reads its settings from `.meta`. Explicit importer IDs run an extra importer with default settings and are rejected by content validation.
5. The worker is stopped through the protocol, then the DataStore. It reads only the stopped store, following `ImportResponse.ArtifactBlobs[1]` → `ArtifactMetaInfo.Files[].Hash` → the stored file.

Worker parameters come from an exact copy of `ProjectSettings.asset`: color space, shader precision and the Linux graphics API override. The profile is limited to the Linux standalone target, with OpenGLCore and Vulkan as the verified automatic API set. Unsupported or ambiguous settings are refused. The applied values and the settings hash are written to `environment` in the report.

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

## Textures

TextureImporter receives the project's default texture compression format and an explicit record that no user overrides exist. DXTC is the verified default.

Every source format TextureImporter reads goes through one route: `.png`, `.jpg`, `.jpeg`, `.tga`, `.tif`, `.tiff`, `.bmp`, `.psd`, `.gif`, `.exr` and `.hdr`. `.dds` and other container formats are not covered.

A standalone worker imports a texture with the importer's resize and mip chain under the settings below. For each of them the worker's artifact was compared with the artifact the Editor writes for the same source and `.meta`, and is byte-identical (54 fixtures, written through the Editor's own importer API):

- The automatic format (`textureFormat: -1`) at every compression quality, crunched or not: uncompressed, normal and low quality (DXT1, DXT5, BC6H for the HDR formats) and high quality, which selects BC7.
- An explicit format: Alpha8, ARGB16, RGB24, RGBA32, RGB16, R16, RG16, R8, RGBA64, RGBAHalf, RGBAFloat, RGB9E5, DXT1, DXT5, their crunched forms, BC4, BC5, BC6H and BC7. A format of another platform family (ETC, ASTC, PVRTC) was not compared.
- A `Standalone` platform override of size, format or quality. An override for another platform is ignored, as the engine ignores it.
- The texture types Default, NormalMap (from a normal map and from a height map), GUI, Cursor, Cookie, Lightmap, DirectionalLightmap, Shadowmask, SingleChannel and Sprite.
- The shapes 2D, Cube (automatic and cylindrical mapping, with and without specular or diffuse convolution, LDR and HDR), 2D array and 3D.

A sprite texture yields its Texture2D and one Sprite per sprite, single or multiple, under the local IDs the Editor gives them (`21300000`, or the IDs of the `.meta`'s `nameFileIdTable`); a cube, an array or a volume yields a Cubemap (`8900000`), a Texture2DArray (`18700000`) or a Texture3D (`11700000`) in place of the Texture2D. The adapter keeps these objects as imported and drops the importer.

Loaded in a Player as new assets, with pixels read back: BC7 from the automatic format, from an explicit format and from a platform override, RGBA64, BC6H, a resized DXT5 override, normal maps in DXT5 and BC7, a lightmap, a single channel texture, a cubemap in DXT1 and one in BC6H (a face copied out), a texture array (a slice copied out) and a volume. Sprites, single, with a custom pivot and border, and one of a multiple sheet, were each drawn by a `SpriteRenderer` through a camera.

A setting outside this list keeps to an exact warm artifact from an existing Unity import; a changed source or `.meta` without such an artifact is refused. For a warm BC7 artifact, the compressor setting recorded in the artifact database is checked against `EditorSettings.asset`, and its hash is part of the cache key.

A `.meta` that holds a GUID but no `TextureImporter` block is refused. The engine reads it as a version 1 importer and makes an 8x8 cubemap of the file, in the Editor as in a worker, so there is no Texture2D to deliver; the Editor writes the block when it first sees the file.

## Audio

AudioImporter sources share one route: `.wav`, `.ogg`, `.mp3`, `.aif`, `.aiff` and `.flac`. The check is on the imported AudioClip, not on the source format. Accepted: load type DecompressOnLoad, CompressedInMemory or Streaming with PCM, Vorbis or ADPCM data, preloaded or not, loaded on the main thread or in the background, 16 bits, one sub-sound, not ambisonic and not a tracker module. Each source format was loaded in a Player as DecompressOnLoad and as CompressedInMemory Vorbis, and WAV with all six combinations: a decompressed clip was read back with `AudioClip.GetData`, a compressed one was played and heard at the mixer output.

The clip's data stays in the artifact's resource file, which the runner mounts under `VirtualArtifacts/Primary/<guid>.resource` in the run directory and next to the ContentFile; the clip names that path whatever its load type. A Streaming clip reads from the mounted file while it plays, a clip that is not preloaded when the game loads or plays it, and a background-loaded clip from a loading thread. The Player's loader therefore accepts a clip in the state its settings give it: only a preloaded, synchronous DecompressOnLoad or CompressedInMemory clip has to be loaded when it is published, and a clip whose load failed is refused. Played and heard in a Player as new Vorbis assets: Streaming (preloaded or not, with and without background loading), DecompressOnLoad not preloaded with and without background loading (the `.meta` the pinned Editor writes for a new audio file has `preloadAudioData: 0`), and preloaded background loading for DecompressOnLoad and CompressedInMemory.

A resource is replaced by renaming a new file over the mounted path, so a clip that is streaming keeps reading the revision it opened.

## OBJ and COLLADA models

`.obj` and `.dae` go the way of FBX: the same static profile (GameObject, Transform, MeshFilter, MeshRenderer, Mesh, Material), the same graph checks.

A model without a material of its own gets one from the importer, which looks up the `Standard` shader by name. A worker fills its table of shaders by name with the built-in shaders when it starts, then replaces it with the `mem://Library/ScriptMapper` record of its source database; with no such record the table ends up empty, the lookup fails, and the importer copies the built-in default material instead, under another name and local ID (`Default-Material`, `3297912226980038121`) than the Editor's (`defaultMat`, `-3033667219593020291`; the ID is XXH64 of `Type:Material->` + name + `0`). The coordinator therefore writes that record for these two formats, with the bytes of `Library/ScriptMapper` from an Editor run of the project: the staging clone a preparation with a host build leaves in `Library/UnityMicroEditor/stage/project`, or else the project's own `Library`. Without either the import is refused. With the record the worker's artifacts for `.obj` and `.dae` are byte-identical to the Editor's. FBX imports are left as they were.

Both were loaded in a Player as new assets: hierarchy, mesh and, for `.obj`, the generated material under the Editor's local ID. That material refers to the built-in `Standard` shader; it renders only in a host that holds that shader, and shows the error shader otherwise.

### Blender files

The model importer does not read a `.blend`. `FBXImporter::ConvertToFBXFile` starts Blender, and imports the FBX it exports:

```
blender -b <project>/Assets/.../file.blend -P <Editor>/Data/Tools/Unity-BlenderToFBX.py -- <project>/Temp/FBXExport/<guid>.fbx
```

The program is found through `PATH` (`execvp`), in the Editor as in a worker; the script is the Editor's own and runs inside Blender. A worker started by this tool inherits the tool's `PATH`, so nothing else is needed on a machine that has Blender, and the artifact is byte-identical to the Editor's with the shader name table above (compared with Blender 5.2.2). Without a `blender` on `PATH` the importer leaves an asset with an import log and no model, so the tool refuses the import before starting a worker and names the missing program. A sandbox that hides the program from the worker has the same effect: a snap-packaged Blender does not start inside `bwrap`, for one.

Loaded in a Player as a new asset: a mesh with its material. A `.blend` with a camera or a light yields those components, which the static model profile refuses, as it does for an FBX.

## Video

On Linux the engine reads VP8 video in a WebM container and nothing else, in the Editor, in a worker and in a Player: an H.264 `.mp4`, `.mov` or `.m4v` and a VP9 `.webm` import as a `DefaultAsset` with the log `Can't create LinuxVideoMedia, format 0 not supported` or `Unsupported video codec 'VP9'`, and a Theora `.ogv` as a clip without frames. No system library is involved; the codecs are compiled into the engine. So `.webm` is the one video extension that is imported, and an artifact without a VP8 `VideoClip` with frames is refused.

A worker's artifact for a VP8 `.webm` is byte-identical to the Editor's, with and without transcoding. Transcoded, the clip's frames lie in the artifact's resource file, which is mounted like an audio resource. Not transcoded (the default), the clip names no resource: its frames are the source file, which a build copies. The adapter then names the mounted resource in the clip and publishes a copy of the imported source revision as that resource. Both forms, with and without a Vorbis track, were loaded in a Player as new assets, prepared by a `VideoPlayer` and played to a texture.

## Scripted importers

A `.inputactions` file is imported by the Input System package's own `InputActionImporter`, run in the standalone worker. The worker's artifact is byte-identical to the Editor's, so every object has the local ID the Editor gives it.

The worker has no compiler; what it needs comes from an Editor compilation of the project, the staging clone of a preparation with a host build (`Library/UnityMicroEditor/stage/project`) or else the project's own `Library`:

- `Library/Bee/*E-inputdata.json` is the Editor's record of that compilation. It lists every assembly with its sources, the assemblies it refers to and the directory of every package.
- The importer's assembly is the one compiled from the script the asset's `.meta` names, and it is loaded together with the assemblies it refers to, from `Library/ScriptAssemblies`. Only immutable packages (`Library/PackageCache/<name>@<hash>`) qualify. An assembly that uses a precompiled assembly of a package or the project is refused, because the worker finds those only through an asset database; the check is whether the compiled code names that assembly.
- The scripts and assembly definitions those assemblies were compiled from are copied into the context where the Editor compiled them, and registered as script compilation assets. An assembly records each script by its path relative to the project; the worker gives an imported object its `MonoScript` only when it can map that path to a registered asset, and it reads where a package lies from the database's root folders only when the database states a refresh version.

The adapter then turns the artifact into a ContentFile. An object of a script class points at its script asset by GUID, which a Player does not have; the file gets one `MonoScript` object per class, named after the class identifier the Editor wrote into each object, and the objects point at it. Their data, local IDs and type trees stay as imported, and the importer object is dropped. An object that refers to another asset is refused.

Proven in a Player as a new asset: the `InputActionAsset` (map and actions found by name, enabled) and an `InputActionReference` sub-object resolving its action. Other scripted importers are not enabled; the route is general but each needs its own verification.

The cache key of such an import covers the compilation record, so another compilation imports again.

## Sprite atlases

A `.spriteatlasv2` is packed by an importer, `SpriteAtlasImporter`, when the project's sprite packer mode is Sprite Atlas V2 (`m_SpritePackerMode: 5` in `EditorSettings.asset`); in any other mode the importer leaves an atlas without a packed result. A standalone worker runs that importer, and its artifact is byte-identical to the Editor's (compared for an atlas of five sprites from three textures, one of them a sheet, and for an atlas of one sprite).

### Importing with dependencies

The importer packs the sprites of other assets, so it needs their artifacts. It asks the worker's database for them; a worker that does not find them answers the import with error 6 and the addresses it lacks, and the Editor imports those and asks again. The coordinator does the same with two workers of one run:

1. The first worker imports the textures the atlas packs.
2. The coordinator writes what that worker returned into the `Library/ArtifactDB` the worker left in the context, as the Editor commits an import: the result becomes the current revision of its address (`CurrentRevisions`, `ImportAddressToImportResultIDs`), and the three blobs of the response become the records of the result (`ImportResultIDToArtifactDependencies`, `ImportResultIDToArtifactMetaInfo`, `ImportResultIDToImportStats`). The key of an address is the XXH3-128 of its GUID, importer ID and importer hash (`ArtifactDBWriteTxn::SetCurrentRevision`). The database is written only between workers, when no process has it open.
3. The second worker imports the atlas. It finds the textures' revisions in the database and reads their files from the DataStore, which stays up between the two.

`project-import` does this for a `.spriteatlasv2` given together with its textures.

### The Player form

The importer leaves the packed result in an Editor-only object: the `SpriteAtlas` of the artifact has an empty render data map, and a `CachedSpriteAtlasRuntimeData` beside it holds one frame per packed sprite. A build writes it otherwise, and so does the adapter (`rust/src/runtimeartifact/atlas.rs`):

- The atlas gets the cached frames as its `m_RenderDataMap`. The file holds the atlas and its packed textures; the importer, the source asset and the cached result are dropped.
- A packed sprite, in the file of its texture, gets its key into that map (`m_RenderDataKey`: the texture's GUID and the sprite's local ID), the atlas tag (`m_AtlasTags`), a pointer to the atlas (`m_SpriteAtlas`) and no texture of its own (`m_RD.texture`). The atlas becomes the one file the texture's file refers to, so the Player loads it first.

Both were compared with AssetBundles built by the pinned Editor from the same sources, field by field through the type trees: the map of a built atlas equals the cached frames, entry for entry and in the same order, and a built packed sprite differs from the imported one in those four fields and nothing else. `rust/tests/testdata/native` holds the worker's artifacts of a one-sprite atlas and the serialized file of its build. One deliberate difference from a build remains: the texture's own `Texture2D` stays in its file, for a material or a script that refers to the texture directly; a build drops it when nothing but the packed sprites uses it.

### In a session

No scene refers to an atlas. A sprite belongs to one because the atlas lists its texture, one of its sprites, or a folder above it. So each walk reads the project's `.spriteatlasv2` sources (`rust/src/native/atlas.rs`), and an atlas that packs a texture the scene uses, or one a session already published, is imported in one batch with all of its textures. The batch is imported again when the atlas, its `.meta`, one of the textures or a texture's `.meta` changes, or when the atlas gains or loses a texture; a texture that an atlas no longer packs is imported again on its own. The atlas records the textures it was packed from, and a bound texture the revision of the atlas, so a cache entry made from other revisions is not replayed.

In a Player, as new assets (`Temp/e2e/kinds3`): a `SpriteRenderer` drew a sprite of a single-sprite texture and a sprite of a sheet through a camera, each reporting `packed=True` and the atlas texture (`sactx-0-256x512-DXT5|BC3-atlas2-...`, 256x512) as its texture, with the pixels of the sprite; a script field of type `SpriteAtlas` held the atlas with its five sprites.

Refused, with the reason named:

- a variant atlas, an atlas with a scriptable packer (project code), and an atlas that is not included in the build (`bindAsDefault: 0`), whose sprites a build leaves for the game to bind;
- a texture two atlases pack; an atlas of more than 31 textures, which one import batch does not hold; an atlas that packs a texture whose importer settings are outside the [verified list](#textures);
- a packed frame with an alpha or secondary texture, and an atlas texture with streamed data;
- an artifact without a packed result, which is what the importer leaves in a project whose packer mode is not Sprite Atlas V2.

### Atlases packed by a build

A `.spriteatlas` (the first format) is not packed by its import: the worker, like the Editor, runs `NativeFormatImporter` over it and yields the authored asset. The Editor packs it when it builds a Player or an AssetBundle, in Sprite Atlas V2 mode as well: a bundle built from a sprite texture that such an atlas lists holds the packed atlas and the sprite bound to it. Nothing outside the Editor's build does that packing, so the tool does not deliver such an atlas, and its sprites are drawn from their own textures. To deliver one, the coordinator would have to give the worker a `.spriteatlasv2` made from the authored asset (the same packables, with the texture, packing and platform settings moved into the importer's `.meta`, under the atlas's GUID), and the result would have to be compared with a build of the first format; neither was done.

## Text assets

TextScriptImporter sources are imported by the worker and delivered as the one TextAsset they yield: `.txt`, `.json`, `.bytes`, `.xml`, `.csv`, `.html`, `.htm`, `.yaml` and `.fnt`. A file of one of these extensions that another importer claims does not yield a TextAsset and is refused.

## Mesh colliders

An Editor artifact for a mesh with `m_IsReadable=0` can hold vertex and index data but no cooked collision data for a Player. For a MeshCollider, the coordinator writes a separate immutable runtime file with `m_IsReadable`, `m_KeepVertices` and `m_KeepIndices` enabled on the originally unreadable mesh. The FBX, its `.meta`, the scene and the collider geometry are not changed, and the projection is proven byte-identical to the import apart from those three flags.

The loader calls the public `Physics.BakeMesh` with the authored `convex` and `cookingOptions` values, then `UploadMeshData(true)` restores the original unreadable state before user callbacks run. Bounding-box substitutes are not used.

## Animation clips

Unity has no importer output for a `.anim` file: the worker returns only the importer object, and the Editor derives the Player's clip data from the YAML during a build. The tool performs that derivation itself (`rust/src/playerbake/`), so a new or changed clip needs neither an Editor nor a worker. It is baked in about a millisecond and published as native content like an imported asset; a watched session applies it to the running Player.

A legacy clip keeps its authored curves. Any other clip gets the muscle clip Mecanim evaluates: streamed cubic segments, the dense clip, the constant clip, value deltas and bindings. The result is compared byte for byte with objects built by Unity `6000.6.3f1`: the fixtures under `rust/tests/testdata/playerbake`, and any project's clips through `UNITY_PLAYERBAKE_CLIP_CORPUS`. The rules below were read out of the Editor binary and then confirmed against those builds.

A clip with `m_UseHighQualityCurve` off has the curves keyed on nearly every frame sampled into the dense clip. A changing binding qualifies when none of its keys is stepped, the samples of all frames take no more bytes than its streamed keys would (4 bytes a frame against 28 a key for a float, 12 against 60 for a position, scale or Euler rotation, 16 against 76 for a quaternion) and no two keys are closer than a frame less half a millisecond; a single key qualifies. The dense clip holds one row per frame of the clip's range, sampled from the authored curve, and the value array is ordered streamed, dense, constant. The range of a clip is that of all its curves, its events and the Editor's copies of its float curves; an object reference curve reaches one frame past its last key.

A tangent of negative infinity holds a segment at its second key, as one of positive infinity holds it at its first; a quaternion is held as a whole when one of its components is. Weighted tangents do not reach a Player: a build samples every weighted segment once per frame of the clip's sample rate and joins the samples with straight tangents (`rust/src/playerbake/weighted.rs`). The samples come from the Editor's Bézier evaluation, whose logarithm, exponential, arc tangent and cosine are taken from the system C library, as the Editor does, so a library that rounds differently can move a sample by one unit in the last place. A curve whose weighted keys weight no segment is left out of the muscle clip, as in a build.

A float curve on any component is bound as a build binds it, from the class and the attribute alone (`rust/src/playerbake/binding.rs`): a material property (`material._Color.r`, `material._Cutoff`), a blend shape, `m_ReceiveShadows` and `m_SortingOrder` of a renderer, the animated properties of a `Light`, a `Camera`, a `RectTransform`, a `LineRenderer` and a `TrailRenderer`, `m_Enabled` of a script, and any other serialized property by the hash of its path. A float curve on a `Transform` and a curve marked as discrete on a property that is not are left out, as in a build; a clip all of whose curves are left out has no muscle clip unless it has events, and then an empty one over the range of those curves. A clip marked `m_Compressed` holds its rotations in the compressed curves alone: a build ignores its plain rotation curves, in the muscle clip and in a legacy clip alike, and so does the baker.

A float curve on a field of a script and an animation event that passes a script are supported: the baked file carries a `MonoScript` object naming the class, which the Player ties to the loaded class. The session takes the class and its assembly from the compiler graph of its start; a script added later is unknown until the next session. A script field is animated as a curve of whole numbers when its class makes it discrete: an enum, or an `int` marked `[DiscreteEvaluation]`. That is a fact of the class, not of the clip, so the baker asks its caller (`ClipSources::discrete`). A caller that does not know the class leaves the question open: a curve the clip marks as discrete is then refused, and an unmarked one is baked as a float, which differs from a build only when the field was made discrete after the clip was authored.

Object reference curves are supported: a sprite animation, a material or mesh swap, an object field of a script. Each key becomes an entry of the clip's object list and the curve a stepped curve of positions in it. An object is identified by the GUID of its asset, its file ID and the `type` of the authored reference; `ClipSources::object` decides where it is found. By default that is the ContentFile of its asset, under the authored file ID, and the asset becomes an external reference of the baked clip (`Baked::externals`), which the caller has to publish and load in that order. An event may pass such an object as well.

Humanoid clips are covered (`rust/src/playerbake/humanoid.rs`). An Animator float curve named exactly like one of the 200 curves of the humanoid rig (`RootT.x`, `Spine Front-Back`, `LeftHand.Thumb.Spread`, `SpineTDOF.x`, ...) is bound to the rig; any other name remains a parameter. From those curves the bake derives what a build derives: the root transform at the start and stop of the clip, the left and right foot at the start, the pose difference between start and stop, and the average speed and turning speed of the root over twenty steps, from the motion curves when the clip has any. The root motion flags are derived from the curves as well, and the authored values are ignored: a clip has motion float curves when a float drives `MotionT` or `MotionQ`, and a generic root transform when a position or rotation curve sits on the root object. Generic root motion is only that flag. The 28 fixtures under `rust/tests/testdata/playerbake/humanoid` hold one clip per rule. The turning speed takes an arc tangent from the system C library, as the Editor does, so a library that rounds differently can move it by one unit in the last place.

An additive reference pose is derived when the caller supplies the source of the reference clip: one value per curve, taken from the reference clip at the reference time where that clip animates the same binding. The flag without a clip, or a clip without the flag, changes nothing. A session supplies it and bakes the clip again when its reference clip changes.

The baker refuses what it does not reproduce exactly, and the session reports the clip instead of publishing an approximation:

- human curves whose root, motion or goal rotation has zero length at a sampled time, which makes the derived values not a number;
- a curve on a particle system, a particle force field, a constraint, a visual effect or a UI Toolkit renderer, whose bindings are encoded in ways that are not reproduced; a script curve without a script or a script on a curve of another class; a curve into a managed reference; and a curve marked as discrete on a script whose class the caller does not know;
- an object reference to something outside the project's assets (an object of the clip's own file or one built into the engine), a material curve whose index is not a plain number, and, in a session, a reference to an asset the session does not publish in baked form;
- an additive reference pose in a session, in a clip imported from a model, on a clip with script curves, on or from a clip with dense, discrete or object reference curves, from a reference that is legacy, has no curves or is stored in a model, and between an Euler and a quaternion rotation curve of one object;
- a legacy clip or a clip without curves whose source sets a root motion flag or an effective additive reference pose;
- weighted tangents with a weight that is not finite, that sample to a value that is not finite, or on keys the sample rate does not tell apart; and a key that keeps its weight behind a sampled segment in a clip with `m_UseHighQualityCurve` off, where the dense clip could evaluate it;
- compressed rotation curves in the source (`m_CompressedRotationCurves`), whose packed form is not read, and a clip marked `m_Compressed` whose plain rotation curves reach beyond the time range of its other curves and events;
- a source larger than 256 MiB. A source of tens of megabytes is read whole and takes a few seconds and up to a few gigabytes of memory.

## Animator controllers

A `.controller` file holds Editor objects (state machines, states, transitions, blend trees) that do not exist in a Player. A build flattens them into one constant per controller; the tool does the same (`rust/src/playerbake/controller.rs`). A new or changed controller is baked and published like a clip, and so is every clip it plays, because a baked controller refers to baked clips. The Player then runs a controller that no Editor has seen. When a clip changes, the baked controllers that play it are baked again, since a controller holds facts of its clips.

The comparison with Unity `6000.6.3f1` covers every field of the built object, with one deliberate difference: the hash-to-name table has no fixed order in the engine and is compared as a set. `m_ControllerSize` is the size of the constant as the engine lays it out in memory, one block that the Player reserves before reading. The baker computes it from the layout: C++ structure layout, a 32-bit count and a sixteen-byte pointer where the serialized form has an array, and everything a pointer leads to appended at its own alignment, depth first in field order; a layer mask without transforms and blend data of another kind than its node take no room. The figure equals the engine's for all 84 distinct reference controllers that have layers.

Covered: layers with blending, weights, avatar masks and synced layers with their override motions; nested state machines with entry, exit and any-state transitions; conditions, mute and solo; parameters; every kind of blend tree, nested. For a freeform 2D tree the tables of the engine's precompute are reproduced bit for bit against 202 blend spaces built by the Editor; its arc cosine comes from the system C library, as in the Editor, so a library that rounds differently can move an angle by one unit in the last place.

An `.overrideController` is baked as well: its Player form holds the same references as the source, so it is published with the baked controller and clips it names, and baked again when one of them changes.

An avatar mask has a Player form of its own, which holds what the `.mask` source holds: the body part switches and the transform paths with their weights. `playerbake::bake_mask` (`rust/src/playerbake/mask.rs`) writes it; 24 masks built by the Editor, among them masks of body parts alone and masks of some seventy transform paths, are equal byte for byte. A layer may also name a mask stored inside the controller's own file. The layer's constant takes the mask's body parts and transform paths, and the controller object refers to no mask, so the baked file holds none: a build writes such a mask only as an object of its own, when it is a visible object of a bundled asset. A session publishes a new or changed `.mask` this way.

A controller or override controller may play clips stored inside a model. The worker imports the model, which takes a few seconds, and leaves its clips in the Editor's form; they are baked like authored clips and published as content derived from the model, under a GUID no source owns. A model that only controllers refer to is not imported as a whole, so a skinned or otherwise unsupported model can still supply clips. When such a model changes, the controllers that refer to it are baked again.

State machine behaviours stay objects in a Player. The baked file carries each behaviour under its authored file ID, published beside the controller as an object of its class, a `MonoScript` object for every class and the class's type tree, which the engine otherwise derives from the loaded class; the controller lists the behaviours and records which of them every state and state machine runs (`rust/src/playerbake/managed.rs`). The `MonoScript` objects lead the file: the Player reads the objects in the order the file lists them and looks up the class of a behaviour as it reads it, so a behaviour listed before its script loads without a class and the Player reports its script as missing. This was measured with behaviours whose IDs sort below the ID of their script, with the class in an asmdef assembly. A controller baked by a preparation reaches the session with the preparation's baseline, and the pointers its file leaves null are bound from the baseline as well. A ScriptableObject the native catalog holds and whose source has changed since is read from its authored text, in a session started from a preparation made after the change too. A catalog controller whose behaviour points at such an object is baked: the Animator runs copies of the behaviours the controller itself holds, and a pointer set on the behaviour object the catalog publishes was measured not to reach them. The fields a class serializes are read from the compiled assemblies by the SDK helper (`--behaviour-layouts` of `tools/LifecycleCompiler`), which follows the engine's rules: public or `[SerializeField]` fields that are not static, constant, read-only or `[NonSerialized]`, base class fields first, `[Serializable]` classes and structs inline, arrays and lists, enums, fixed buffers, object references, the engine's own structs such as `Vector3`, `AnimationCurve` and `Gradient`, and a `[SerializeField]` dictionary. A type of the engine's own assemblies is stored in one of two ways. Native code stores some in a form their C# fields do not show: the vectors, `Quaternion`, `Color`, `Color32`, `Rect`, `RectInt`, `Bounds`, `BoundsInt`, `Matrix4x4`, `LayerMask`, `RenderingLayerMask`, `AnimationCurve`, `Gradient`, `RectOffset`, `Hash128`, `SphericalHarmonicsL2`, `LazyLoadReference<T>`, and also `GUID` (four words, written in text as 32 hex digits), `PropertyName` (a build keeps the CRC-32 of the name as a signed number in decimal digits) and `GUIStyle`. Every other type is stored by its C# fields under the same rules as a type of the project. This was established against a build of a behaviour field and an array of every public, non-generic type the engine's assemblies export to scripts in the reference project, 1375 types of which the engine serializes 269: with the helper laying out every engine type by its fields, the type table entries and the objects, as created and with every field set, equal the engine's for all of them but the native ones above, the two `Unity.Loading` IDs, which stay refused, and one type whose serialization callback changes its data.

A build runs `ISerializationCallbackReceiver` callbacks: it calls `OnAfterDeserialize` after reading the text and `OnBeforeSerialize` before writing, the latter several times for a nested class, so the object holds what the callbacks leave, not what the text holds. A class with such callbacks, as a behaviour or as the type of a field, is therefore refused; the engine's own `UnityEvent` types have them and store what they read.

A `[SerializeReference]` field holds the ID of an object that the behaviour stores in a registry at its end, with the object's class by name and its data. The type tree marks such a field, and a list of them, with type flags of their own and ends in the registry; the layout hash covers those flags. In the bytes of an object the registry lists the objects as the text does, the entry of the null reference first and the others by ID, each with its data laid out by the tree of its class. Those trees travel in a table of the file after its external references, and the type of the behaviour names the ones its objects use by position. Which classes an object holds is in the text, not in the behaviour's class, so the helper lays out every serializable, non-generic class of the project that can be assigned to one of the declared types and reports them with the behaviour (`managedTypes`); a change to one of them is a change to the behaviour's layout. Seven behaviour objects of two classes, with references that are shared, null, in a cycle, held from a class stored inline and from other referenced objects, in fields and lists, and with a clip reference inside a referenced object, equal the engine's objects byte for byte in two builds, as do their type table entries and the table entries of eight referenced classes. Whether a Player loads such a ContentFile was not run; only the bytes were compared.

The engine stops at a type that ten serialized types enclose: of such a type it keeps the fields that are numbers or strings and drops everything else, enums, arrays, references, engine structs and further classes alike. The helper cuts the same way, so a class that holds a list of itself is laid out down to that level; four such classes have the engine's type table entries and objects. A fifth, which nests itself through two fields a level, has some 7000 fields and a type tree of 18000 nodes; the cut reproduces that one as well, but the helper refuses a class of more than 4096 fields rather than let a baked file carry such a tree. A behaviour on a state machine that overrides `OnStateMachineEnter` or `OnStateMachineExit` turns off multithreaded evaluation of the controller, as in a build.

The comparison set is 68 controllers built by the Editor. 53 are baked and match field by field; the others hold something from the list below, or text the reader rejects. Apart from the controllers, every behaviour document is encoded on its own with its object references resolved as in the reference build: 320 objects equal the engine's byte for byte, as does the type table entry of every class laid out. `rust/tests/testdata/playerbake/behaviours` holds a part of that set; `UNITY_PLAYERBAKE_BEHAVIOUR_CORPUS` compares a directory of the same form. The three hashes of a script type entry (script identity, layout, type tree) are written as a build writes them, although a Player loading a ContentFile was seen to check none of them.

A behaviour field may refer to any object. For an object of another asset the baker asks its caller (`ControllerSources::reference`, with the target's GUID, file ID and type, the class the field declares and the field's path) and takes one of three answers: the object is in another published file, and the field becomes a pointer through an external reference; the reference leads nowhere, and the field is null; or the object is bound later, in which case the field is written as null, a list keeps its length, and the baked controller reports the behaviour's file ID, the field's path (`field`, `nested.field`, `list[3]`, `list[3].field`) and the target. A session answers the first for clips, which are baked and published with the controller, and the third for every other asset of the project: a ScriptableObject, a prefab or one of its objects, a material, a texture, a sound. It publishes each such reference as a deferred binding of the behaviour, brings the target into the scene's asset closure like any other reference, and the Player sets the field once the scene's assets are loaded, before an Animator runs. A clip in a field that declares another class is null, as in a build. In a build a field holds its object only if the object exists and is of the field's class or derives from it; a caller that answers "published" has to check that, and the reader reports the classes a script class derives from for it (`bases`). The baker itself resolves a reference into the controller's own file: to the controller, from a field that may hold one, or to a behaviour the controller runs; a reference to an Editor object such as a state, or to nothing, is null as in a build. Three controllers that refer to a prefab and its components, materials, a texture, a sound, a mesh, a text asset, scriptable objects and built-in resources, from plain fields, lists and nested classes, are compared with a build that holds those assets in another bundle.

A behaviour document need not hold every field of its class: the class may have gained fields since the asset was saved. The engine reads a document over a newly constructed object, so such a field keeps what the constructor left in it. The helper follows the IL of the parameterless constructor, base constructors and field initializers included, and reports per field the constant stored in it (`initial`), or that something is stored which it does not follow (`initialUnknown`): a call, a branch, an array with elements, an object made with arguments or changed after it is made. A field without either is as the engine makes it: zero, an empty string or list, a null reference, a curve without keys, a gradient from white to white, and for a `[Serializable]` class an object the engine constructs itself, through that class's own constructor if it has a public one without parameters and without running code otherwise. The baker fills a missing field from that and refuses the object when a missing field is one whose value is not known. A missing `m_Enabled` is 1. Keys the class does not have are ignored. Text of another kind than the field's type is read as the engine reads it: an integer as far as the text is one (`1.5` is 1, `abc` is 0, wrapping at 64 bits and then cut to the field's width), a float by its leading decimal number and as not a number without one, and a structure where a number or a class is expected leaves the field as constructed. Twelve documents that lack some or all of their fields, in nested classes and list elements as well, equal the engine's objects byte for byte, as do 50 one-field edits of that kind.

The same reader lays out any class derived from `ScriptableObject`, not only behaviours, and nine objects of scriptable object assets with objects stored inside them, encoded with the layouts it reports, equal the engine's byte for byte. Nothing bakes such an asset yet.

Two findings of that comparison changed the controller baker for every controller. Paths of states start at the name of the layer's root state machine, written with its dots, not at the layer's name; the Editor keeps the two equal, a hand-edited file may not. The exit of a nested state machine that no transition leaves leads to the default state of the machine around it.

Refused, never approximated:

- a behaviour whose document lacks a field that its class's constructor sets in a way the helper does not follow, or sets through a constructor that is not public; a list authored as a mapping, a reference or an engine struct authored as a plain value, and an integer array authored as a hex dump of another width, of which the engine makes something the baker does not reproduce;
- in a session, a behaviour field that refers to a resource built into the engine or to an asset that is not a source of the project, and a field of an object held by managed reference that refers to anything but a clip; a behaviour field that refers to a behaviour of the same controller that no state or state machine runs, which a build was not seen to hold or drop;
- a managed reference declared as `object`, of a generic or engine type, or more than ten types deep; an object held by managed reference whose class is generic, is missing, or is one of more than 256 candidates; a registry of managed references that is not in the Editor's order or does not hold exactly the objects referred to; a managed reference field the document lacks; a class that serializes more than 4096 fields once its nested types are written out, as one that nests itself through several fields does; a fixed buffer more than ten types deep; a class that implements `ISerializationCallbackReceiver`, itself or in a field's type; a generic engine type other than `UnityEvent<T>` and `LazyLoadReference<T>`, `Unity.Loading.LoadableObjectId` and `LoadableSceneId`, a missing `GUIStyle` field or one that lacks a part, a dictionary below the top level of the class, a fixed buffer of a type other than `int`, `byte`, `float`, `bool`, `char`, `short`, `long` or `double`, and a method that hides `OnStateMachineEnter` or `OnStateMachineExit` without overriding it;
- a field authored in an older serialized version of an engine struct; an engine struct that has had several forms and lacks a part, other than a rectangle or a curve;
- a behaviour whose script the session does not know, is not a source file of the project, or is a nested class;
- two states or state machines of one layer that share a path and both have behaviours; a nested state machine without exit transitions inside one without a default state;
- any clip the clip baker refuses;
- a behaviour field that refers to an avatar mask stored inside the controller, which the baked file does not hold.

A controller text the reader rejects fails outright rather than being refused: the Editor writes the closing quote of a string that ends in a line break at the start of a line, and a hand-edited file may repeat a key, of which the engine reads the first.

Limits of the path as a whole:

- A controller built into the native catalog keeps pointing at the clips it was built with. A changed clip reaches an Animator only when its controller can be baked; otherwise the session reports `native-bake-refused` and the Animator keeps the catalog's clip. The same holds for an override controller built into the catalog.
- A deferred binding is set by the Player without the build's class check: a field that refers to an object of a class it cannot hold is null in a build and reads as a mismatched reference here.
- A changed avatar mask bakes the controllers whose layers use it again. A baked controller records the masks it was made from; a controller of the native catalog is baked when a mask differs from the one the catalog was built with. The mask itself is published in its Player form as well, for a script field that refers to it.
- A controller is baked with the behaviour classes of the script assemblies current at that moment. When a compile changes the fields of a behaviour class, the session bakes the controllers it published with that class again, and a controller with behaviours baked by an earlier session is baked anew instead of being replayed. A controller held by the native catalog or baked into a baseline is baked as well once a compile during the session changes such a class; until then the Player reads its behaviours as it reads any content built before a script changed, by field name.
- `auto-prepare` uses the same path when it publishes its baseline: a scene that refers to a clip or controller the native catalog lacks, or holds in an older form, is prepared without an Editor, and the baked files stay with the baseline. A source the baker refuses still needs a refreshed catalog.

## Other native-format assets

Some assets reach a Player as the fields their YAML holds. `rust/src/playerbake/native.rs` bakes them with one transcoder: it walks the Player type tree of the object's class, pinned from the reference build (`engine/type-<class>.bin`), and takes every field from the source by name. A source field the Player lacks is Editor-only and dropped, as the engine drops it. Object references become references to the same file or to another source, which `bake_native` returns in order of first use. Every object is published under its authored file ID.

Nothing is guessed. A Player field the source lacks would take a default only the engine's constructor knows; a structure written in another serialized version than the pinned engine writes would be converted on load; a number spelled otherwise than the Editor spells one (`inf`, `.5`, `-1` for an unsigned field, a value beyond single precision) may be read differently. Each of these is refused.

The Player form is not always the authored fields. The Editor corrects an object after reading it (`CheckConsistency`, and conversions inside the YAML reader), and a build writes some fields from project settings. `rust/src/playerbake/settle.rs` restates these per class from the pinned Editor:

- `PhysicsMaterial`: frictions limited to zero and one step below the largest single, bounciness to 0..1.
- `PhysicsMaterial2D`: friction and bounciness limited to 0..100000; an unknown combine mode falls back to the default of its kind.
- `RenderTexture`, `CustomRenderTexture`: sizes of at least one, 1..32 samples, filter and wrap modes limited, a cube as high as wide, no mipmaps for a depth-only texture, no depth for a volume, no anisotropy with a depth buffer, a zero mip count read as unset, a negative update period as zero. The color format is read as its sRGB or linear counterpart: sRGB only when the texture is marked sRGB **and the project works in linear color space**.
- `LightingSettings`: the Player keeps seven fields. Bounce scale, albedo boost and indirect scale are limited; the shadowmask flag follows the mixed bake mode; baked and realtime lightmaps stay enabled only **when the project's render pipeline lights with them** (`SupportedRenderingFeatures.lightmapBakeTypes`), and realtime environment lighting follows from both.

The two project settings come from the caller (`NativeSources::linear_color_space`, `NativeSources::lightmaps`). When the caller does not know one, an asset whose Player form depends on it is refused; an asset it cannot affect (a render texture not marked sRGB, lighting settings without lightmaps) is still baked.

| Class | Source | Main file ID | Compared with the engine build |
| --- | --- | --- | --- |
| `AvatarMask` (319) | `.mask` | 31900000 | written by the mask baker above (`mask.rs`), which `bake_native` calls for this class; six more masks byte for byte, among them a shortened and an empty body part list |
| `PhysicsMaterial` (134) | `.physicMaterial` | 13400000 | byte for byte, including out-of-range values |
| `PhysicsMaterial2D` (62) | `.physicsMaterial2D` | 6200000 | byte for byte, including out-of-range values |
| `RenderTexture` (84) | `.renderTexture` | 8400000 | byte for byte in gamma and linear projects; every color format, marked sRGB and not |
| `CustomRenderTexture` (86) | `.asset` | 8600000 | byte for byte in gamma and linear projects, with update zones and an initialization texture |
| `Flare` (121) | `.flare` | 12100000 | byte for byte, with elements and a texture |
| `TerrainLayer` (1953259897) | `.terrainlayer` | 8574412962073106934 | byte for byte, with textures |
| `LightingSettings` (850595691) | `.lighting` | 4890085278179872738 | byte for byte under pipelines lighting with both kinds of lightmap, with only baked, only realtime, and neither |
| `Material` (21) | `.mat` | 2100000 | byte for byte in gamma and linear projects: hand-written sources in every state (stale, missing, misfiled, unsorted and repeated properties and keywords), the materials of a game built with the universal pipeline, and variants: chains up to seven deep with overrides of every kind, locks, other shaders than the parent's and keyword drawers, and the 78 variants of that game with their 11 parents |

Beyond the committed fixtures, randomized sources of each class (several hundred, with out-of-range values) were compared with builds of a gamma and a linear project.

Refused:

- A custom render texture whose `m_UpdateZoneSpace` differs from `m_CurrentUpdateZoneSpace`: the engine converts its update zones between the two spaces.
- A source holding an object of any other class.

A session publishes this way a new or changed `.mask`, `.physicMaterial`, `.physicsMaterial2D` and `.renderTexture`, each as one object without references to other files; a scene that refers to one the native catalog lacks, or holds in an older form, is prepared without an Editor. It reads the color space a render texture needs from `ProjectSettings/ProjectSettings.asset`. The other classes are baked and compared but not published by a session yet: what they refer to (a flare's or terrain layer's textures, a material's shader and textures) lives in the catalog's bundles, which a published file cannot point into, so a material keeps its raw path and a custom render texture, flare, terrain layer or lighting settings asset still needs a refreshed catalog.

### Materials

A build derives a material from its shader (`rust/src/playerbake/material.rs`), so the caller supplies what the shader declares: `NativeSources::shader` returns `ShaderFacts` (name, keywords, properties with defaults, tags). `read_shader_facts` reads them from a `Shader` object in the Player form, as an earlier build of the project wrote it.

- Keywords: the two authored lists are joined, sorted, and split by whether the shader declares the keyword. Repeats stay.
- Saved properties: a texture is kept only for a texture property the shader has; integers, floats and colors are kept whether the shader has them or not. A property the shader has and the source lacks is added with the shader's default. Each list is written sorted by name; of a repeated name the first entry counts.
- An added default of a color (not `[HDR]`) or `[Gamma]` property goes through the engine's two color space conversions in a linear project and can come back one step off. The baker repeats them with the C library's power function; that was compared with the Editor on Linux with glibc, and is refused elsewhere and when the color space is not known. A default the conversions leave alone (0, 1) needs neither.
- Override tags and disabled passes: the engine finds a shader tag without regard to case and writes it the way it first met it. A tag the engine registers at start is written its way (`QUEUE`, `IGNOREPROJECTOR`, `SHADOWCASTER`, `MOTIONVECTORS`, `true`, ...). Any other is written as `NativeSources::tag_spelling` gives it, which a session answers from the tags of the materials a build of the project wrote. Without that answer the authored spelling is kept, which holds when no other asset of the project spells the tag otherwise; a tag that the shader or a late engine registration spells in another case is refused. Override tags are written sorted by name, and of two that differ only in case the later in byte order counts.
- The Editor-only script objects a render pipeline keeps next to a material (hidden `MonoBehaviour` documents of its Editor assemblies) are left out: only the material is published.

#### Material variants

A variant (`m_Parent`) stores its overrides; a build writes it as a whole material, without a parent (`Material::MergeHierarchy`, then the drawers of the material inspector). The caller supplies the source of a parent by GUID (`NativeSources::material`), and the chain is followed up to the material without a parent:

- The shader is that of the material at the top of the chain, whatever the variant's own `m_Shader` says. Its properties decide which textures are kept and which defaults are added, and its keywords which of the variant's are valid.
- Saved properties: the parent's merged properties, then the variant's own entries over them. An entry whose name a parent above it locks (`m_LockedProperties`, names separated by spaces) is left out. A material's own locks bind its variants, not itself.
- `m_LightmapFlags`, `m_EnableInstancingVariants`, `m_DoubleSidedGI` and `m_CustomRenderQueue` are the variant's only where `m_ModifiedSerializedProperties` has their bit (2, 4, 8, 16) and no parent locks them (`:m_LightmapFlags`, ... in `m_LockedProperties`); otherwise the parent's.
- Keywords, override tags and disabled passes are the variant's own, not the parent's. Then the drawers of the shader's properties set keywords from the merged values, as `MaterialEditor.ApplyMaterialPropertyDrawers` does when the engine builds the properties of a variant: `[Toggle]` and `[ToggleOff]` turn `<NAME>_ON`, `<NAME>_OFF` or the keyword they name on or off (on above 0.001 in magnitude, an integer property when not zero), `[KeywordEnum(A,B)]` turns on `<NAME>_<A>` for the truncated value and the others off. A plain material gets none of this: its keywords stay as authored.

Refused:

- A variant whose parent the caller does not know, whose parent's source holds no material under the named file ID, or whose chain returns to itself (the Editor rewrites such sources) or is longer than 32.
- A variant of a shader that names an inspector (`CustomEditor`, as every Shader Graph shader does), baked from its source alone: the inspector's `ValidateMaterial` runs on the variant as the project's code. With the documents an inspection leaves it is baked ([below](#variants-of-a-shader-with-an-inspector)).
- A variant of a shader with a property attribute that is not one of the engine's (`Toggle`, `ToggleOff`, `KeywordEnum`, `Enum`, `ToggleUI`, `Header`, `Space`, `PowerSlider`, `IntRange`), since a project can define drawers; with several keyword drawers on one property; or with drawer arguments that are not plain text.
- A variant that inherits a texture held inside a parent's own source.

##### Variants of a shader with an inspector

Building the properties of a variant also runs the `ValidateMaterial` of the shader's inspector, which is C# of a package or the project. URP's inspectors set the surface and blend keywords, the render queue, the `RenderType` tag, the disabled passes (`DepthOnly`, `SHADOWCASTER`, `MOTIONVECTORS`), `_SrcBlend`, `_DstBlend`, `_ZWrite`, `_AlphaToMask`, the copies `_MainTex` and `_Color`, the emission flag and the keywords of the maps a material sets. A plain material gets none of it in a build: one whose source is stale is written stale. This was compared on 104 plain materials of URP's shaders and Shader Graph shaders in both color spaces.

None of that code is restated. `native::inspect_material_variants` (`rust/src/native/inspector.rs`, command `material-inspect`) has the engine run the project's own compiled inspector in a standalone worker:

1. A first worker run imports the shaders the chains name, with the engine's shader importer.
2. A second run gets a context that holds the materials of each chain, each shader as the first run wrote it, a texture of one pixel for every texture a chain names, the render pipeline asset the project settings name, the assemblies of the inspectors from an Editor compilation of the project (as for [scripted importers](#scripted-importers), with the precompiled assemblies they name next to them) and an importer of this tool (`tools/MaterialInspector`, compiled with the compiler of the Unity installation). That importer sets the project's color space and render pipeline, reads each variant from its source, queries a property, which makes the engine build the properties and run the drawers and the inspector, and saves the material as text.

A standalone worker has no artifact database. It finds an asset only where a source file holds the object itself, so the shaders and textures lie in the context as native assets under their own GUIDs and the references in the context's copies of the materials are typed to match; the documents that come back carry the authored types and file ID again. An asset the project lacks stays missing. The worker starts in gamma space whatever the project settings say, and the engine converts the colors an inspector reads and sets by that space, so the importer sets it.

`playerbake::bake_inspected_variant` bakes a variant from those documents: it merges them as above, without running the drawers again, since the engine has. A build builds the properties of a material when it first needs them, and an inspector decides by the values the parents have at that moment. For a variant of a variant a build may get there before or after the parent's own inspector has run, and it may merge the two at yet another moment; Editor builds of the same sources were measured to do each. The inspection therefore saves every variant twice, over parents as they were read and after the variants among its parents were built, and the baker bakes three states: both of those, and the first document over parents each built alone. A variant they disagree on is refused. That happens only where a parent's source is not as its inspector leaves it and the variant turns back what the parent's inspector would derive; a source the Editor saved is already as its inspector leaves it.

Compared with Editor builds, object for object, in gamma and linear space, with URP 17.6.0: 603 variants of `Lit`, `Complex Lit`, `Unlit`, `Simple Lit`, `Baked Lit`, `Particles/Lit` and `Particles/Unlit` (hand-written cases of surface type, blend mode, alpha clipping, emission, normal and other maps, shadows, culling, queue offset and render queue, locked properties, stale keywords, tags and passes, plus random ones, 88 of them variants of variants). 591 are equal and 12 are refused for the order; none differs. The repository's fixtures use a shader of the reference project that names URP's `LitShader`.

Refused, each with its reason:

- A variant of a Shader Graph shader. URP's inspectors decide the motion vector pass by the graph's metadata, which they read through the asset database (`AssetDatabase.GetAssetPath`, `AssetImporter.GetAtPath`, `LoadAllAssetsAtPath`); a standalone worker answers none of these, and the inspector then silently leaves `MOTIONVECTORS` enabled. In a trial with the graphs imported by the worker, 288 of the 295 Shader Graph variants of the comparison differed from the Editor's, 283 of them in the disabled passes alone. The worker does import a graph with the package's own importer when the packages lie whole in its context; making the result loadable needs the artifact database the main Editor writes.
- An inspector other than `BakedLitShader`, `LitShader`, `ParticlesLitShader`, `ParticlesUnlitShader`, `SimpleLitShader` and `UnlitShader` of `UnityEditor.Rendering.Universal.ShaderGUI`, or those of another package version than `com.unity.render-pipelines.universal@0ffb3d4f1461` (17.6.0): an inspector that asks the asset database gets no answer and no error, so only inspectors compared with the Editor are accepted.
- A chain with a material outside `Assets`, a reference to an asset other than a shader source or a `Texture2D` (file ID 2800000), or a project whose quality levels name different render pipeline assets.
- A project without an Editor compilation, or whose inspector is not compiled from an immutable package.

Limits: a one-pixel texture stands for every texture, which is right for an inspector that asks only whether a texture is set, as the accepted ones do. Both worker runs are made on every call; nothing is cached. A session does not publish materials yet (above), so nothing calls the inspection during a run; `NativeSources::inspected_material` is what a publishing step answers for the parents.

Without its parent the engine writes a variant from its own overrides and its own defaults for the four settings; that is not reproduced, since whether the parent exists is the project's to say.

A session answers `NativeSources::material` from the project's sources, so a bake reads the parent as it stands. Materials are not published by a session yet (above), so nothing re-bakes a variant when its parent changes; the publishing step has to add that, as it does for a controller's masks.

Also refused:

- A material whose shader the caller does not know, or without a shader: it is written without properties, and whether the shader exists is the project's to say.
- A material of another serialized version than 8, or with texture stacks (virtual texturing).
- A material of `Particles/Standard Unlit` or `Particles/Standard Surface`: a build rewrites their disabled passes.

Not covered: a dedicated server build writes a material without its shader reference. A YAML mapping with a repeated key (`stringTagMap`) is read by this tool's parser before the baker sees it.

### Audio mixers

A `.mixer` is baked by `rust/src/playerbake/mixer.rs`, which `bake_native` calls for a source holding an `AudioMixerController`. Its Player form is a derivation, not its fields. Of the four classes of the source, three reach a Player, under their authored file IDs and with the class IDs of the source; the effects (`AudioMixerEffectController`, 244) do not:

- `AudioMixerGroupController` (243) keeps its name, its mixer, its GUID as four integers and its children.
- `AudioMixerSnapshotController` (245) keeps its name, its mixer and its GUID. Its values move into the mixer.
- `AudioMixerController` (241) keeps the output group, master group, snapshots, start snapshot, suspend threshold, suspend switch and update mode, and gains an `AudioMixerConstant`.

The constant was derived from engine builds of mixers authored through the Editor's own mixer API and of hand-written ones (`BuildAudioMixerConstant` in the Editor binary writes it; no tool outside the Editor does):

- Groups are listed depth first from the master group, children in authored order, each with the index of its parent. A group or snapshot of the source that nothing lists is still an object of the Player, but takes no part in the constant.
- Every value a snapshot can hold has an index. The volume and the pitch of each group come first, two per group in group order. Then, per effect in effect order: its level, when it has one, followed by its parameters in the order the engine defines them, whichever order the source lists them in.
- Effects are listed group by group in authored order, each naming the effect before it in the whole list. An `Attenuation` that is the last effect of its group is left out, as is a `Send` without a target; an attenuation anywhere else stays. A send names its target by index, or none when the target was left out. A send always has a level; another effect has one only with its wet mix enabled.
- An effect type is a negative number for the four kinds the mixer implements itself (attenuation −2, send −3, receive −4, duck volume −5) and the FMOD type of the others (lowpass 3, highpass 5, echo 6, flange 7, distortion 8, normalize 9, parametric EQ 10, pitch shifter 11, chorus 12, compressor 16, SFX reverb 17, simple lowpass 18, simple highpass 22).
- The side-chain buffer count is the number of `Receive` and `Duck Volume` effects.
- A snapshot holds the CRC-32 of its name and one value per index: the stored one, as written and without limiting it to the parameter's range, or the default. Defaults are 0 dB for a volume, 1 for a pitch, −80 dB for a level and the engine's value per parameter. These are the values an engine build writes, which differ from the list the Editor shows in one place: the release time of the ducker is 0.3, not 0.1. A value stored for a GUID without an index is dropped.
- Transition overrides are listed in the order of their GUIDs as the engine compares them (four 32-bit integers, each written in the source with its hexadecimal digits reversed), as a type and a value index each; one for a GUID without an index is dropped.
- Exposed parameters are two lists: the index of every exposed GUID that has one, and beside them as many name hashes taken from the head of the authored list. When a parameter is exposed whose GUID has no index (the level of an effect without wet mix), the names therefore shift against the indices. The baker reproduces this.
- The name buffers hold the group and snapshot names in order, each ended by a zero byte, with one more zero at the end.

Compared with the engine build byte for byte, every object of every mixer: 54 mixers authored through the Editor API (groups several deep, each built-in effect, sends, receives and ducking, up to five snapshots, transition overrides, exposed parameters, mute, solo and bypass switches, suspend and update mode, a mixer playing into a group of another, and the Editor's own randomized test mixers), 13 hand-written edge cases (values outside their ranges, parameters in another order, stray values and overrides, groups without or with two attenuations, sends to effects that take none, unlisted groups and snapshots, names with quotes and non-ASCII letters), 80 randomized hand-written mixers and the mixer of a game: 148 mixers, 1218 objects. In 18 of them 44 values are the one a build leaves undefined (below); those floats alone are passed over, every other byte matches. A selection is committed under `rust/tests/testdata/playerbake/mixers`; `UNITY_PLAYERBAKE_MIXER_CORPUS` names further directories of `<name>.mixer` / `<name>.mixer.bin` pairs.

#### The value a build leaves undefined

A build has no value for the `Feedback` of a `Chorus` in a snapshot that does not store it, which is every chorus whose feedback was never moved. The baker writes 0, the default the effect declares.

Where the build's value comes from, in the Editor binary:

- `BuildAudioMixerConstant` takes the default of a parameter from the effect's definition (`GetAudioEffectDefinition`). For a built-in effect, `AudioEffectInternalDescription::AudioEffectInternalDescription(FMOD_DSP_TYPE, bool*)` fills that definition once per Editor run: it creates the effect (`FMOD::System::createDSPByType`), asks how many parameters it has, and for each reads the range with `FMOD::DSP::getParameterInfo` and the default with `FMOD::DSP::getParameter`, the current value of the new effect.
- FMOD's chorus declares eight parameters (`FMOD::DSPChorus::getDescriptionEx` sets the count to 8; the eighth entry of its table is `Feedback`, range −1..1, default 0). Its `setParameterCallback` and `getParameterCallback` handle seven: both leave for an index above 6 without doing anything and report success. The effect has no member for a feedback.
- `FMOD::DSPI::getParameter` hands the callback a variable on its own stack, which it never set, and on success copies it to the caller. For index 7 that is whatever the stack held.

So the value is uninitialised stack memory read once per Editor run. Four Editor runs gave the bit patterns `00005fb3`, `00005e63`, `00005954` and `00005a14` (denormals near 3·10⁻⁴¹, which look like the upper half of an address); within one run every chorus got the same one (44 values of 18 mixers in the last). The Editor's own list of effect definitions (`MixerEffectDefinitions`) holds the same kind of number as this parameter's default.

Whatever is written has no effect on the sound: the Player's chorus drops the parameter the same way (`cmp esi,6` / `ja` in both callbacks of `UnityPlayer.so`). It can be read back, since the mixer keeps the value itself: in a Player, with a baked mixer holding a default chorus and `Feedback` exposed, `GetFloat` returned 0 with the bit pattern `00000000`, the neighbouring `Depth` 0.03, and after `SetFloat(0.25)` it returned 0.25. `playerbake::mixer_undefined_values` names these values of a source, as snapshot and value index; the comparison with engine builds uses it to pass over exactly them.

Every other parameter of every built-in effect was checked: the defaults each FMOD effect declares in the Editor binary against the values of four builds, and, where the callbacks check a range (echo, flange, chorus, compressor, SFX reverb, pitch shifter), the count the effect declares against the indices they handle. None is undefined: all were the same in every build. One more differs from its declaration and is a rule, not chance: the `Decay` of an `Echo` is declared as 0.5 and builds as 0 in every run, because setting it and reading it use two members of the effect (`setParameterCallback` writes one, `getParameterInternal` reads the next), and the one read is still zero in a new effect. The baker writes 0, as the builds do. The release time of the ducker, 0.3 in a build against 0.1 in the Editor's list, is likewise the same in every run.


Refused:

- An effect that is not built into the engine (a native audio plugin), an effect whose parameters are not exactly the ones the engine defines, and an effect other than a send with a send target.
- A transition override of a type outside the seven the engine names; the engine reads it as another.
- A mixer without snapshots, a value that is not a finite plain number, two values sharing a GUID or a value without one, a group with two parents, an effect listed twice, a send to an effect no group lists, a reference to an object the source lacks or of another class, a mixer that plays into a group of its own, a group or snapshot that belongs to another mixer, and a source holding an object of any other class.

A session publishes a new or changed `.mixer` this way, and a scene that refers to one the native catalog lacks, or holds in an older form, is prepared without an Editor. The groups and snapshots a mixer publishes follow its source, so one built into the catalog may gain and lose them. A mixer that plays into a group of another mixer refers to that mixer's baked file, which is published with it; when the other mixer is refused, so is this one. In a Player (`Temp/e2e/mixer`), with the mixers baked: an `AudioSource` routed to a group reported that group, script fields held the mixer, a group and a snapshot, `FindMatchingGroups` and `FindSnapshot` found them by name and path, `GetFloat`, `SetFloat` and `ClearFloat` worked on exposed parameters, `TransitionTo` reached the values of the other snapshot, and a second mixer reported the first mixer's group as its output.

Not covered: what the mixers sound like was not measured, only that the Player holds the objects an engine build holds; a mixer that changes while a Player has it loaded was not tried.

## Shaders

The worker parses a `.shader` and leaves a `Shader` object with the parsed passes and the source of every program, but no compiled program. A build compiles each program with `UnityShaderCompiler`, a separate program the Editor ships in `Data/Tools`, and stores the results in compressed tables. The tool does the same (`rust/src/playerbake/shader.rs` and `shader/`, `rust/src/native/shader_compiler.rs`): the worker imports the shader, the tool starts the compiler, takes the Editor's side of its local socket protocol, asks for every variant a build compiles and writes the Player's object through the Player's type tree. The main Editor does not run. A new or changed `.shader` is published like a baked clip, and a watched session applies it to the running Player. An unlit shader of two passes takes about three seconds, most of it the worker.

What is reproduced is an AssetBundle build for Linux standalone, started from the command line, of the shader and the materials of the project that use it. The result is compared byte for byte with objects built by Unity `6000.6.3f1`: 83 objects of 48 shaders, built with default settings, without materials, with changed settings (linear colour space, another lightmap encoding, the unified precision model, deferred reflections off, hardware tiers that differ, custom lightmap and fog stripping, instancing variants kept, a shader chunk size of 1 MB) and for OpenGL Core alone, Vulkan alone and both listed in either order. 69 are equal and 14 are refused for a reason of the list below; none differs. `rust/tests/testdata/playerbake/shaders` holds 28 of them with the compiler's replies, each stored under the hash of its request, so the test also pins what the compiler is asked. `UNITY_PLAYERBAKE_SHADER_CORPUS` with `UNITY_PLAYERBAKE_EDITOR` compares directories of the same form with the real compiler. The rules below were read out of the Editor binary and then confirmed against those builds.

Covered: `CGPROGRAM` and `HLSLPROGRAM` passes with `HLSLINCLUDE` blocks, vertex, fragment, geometry and tessellation stages, surface shaders (the worker generates their passes), fixed-function passes (the worker generates their programs), grab passes, several subshaders, every property type and attribute, render state given as values or as properties, includes of files under `Assets` and `#include_with_pragmas`, `multi_compile` and `shader_feature` in their global, local and per-stage forms, `dynamic_branch`, `skip_variants`, `only_renderers` and `exclude_renderers`, `multi_compile_instancing`, `multi_compile_fog`, `multi_compile_fwdbase`, `multi_compile_fwdadd_fullshadows`, `multi_compile_shadowcaster`, `hardware_tier_variants`, structures, arrays, integer and matrix constants, constant buffers, every texture dimension, separate and inline samplers, buffers and random-access targets.

Which variants are compiled:

- A `multi_compile` group is compiled in every combination. A `shader_feature` group is compiled in the combinations the materials enable: a material's keywords are reduced to those of the pass, and a material that enables two keywords of one group, or a `dynamic_branch` keyword, adds nothing. Without materials only the first combination remains.
- The built-in keyword sets (`multi_compile_fwdbase` and the like) are stripped as a build strips them: stereo variants always; instancing variants unless a material enables instancing or the project keeps them; lightmap, shadowmask and fog variants by the project's stripping settings. Under automatic stripping a build reads the scene open in the Editor, and a build from the command line has none, so fog and lightmap variants are left out. This is the one point where the result is narrower than an interactive build can be: a shader that needs fog or lightmap variants needs custom stripping in the graphics settings.
- A pass is enumerated by material usage until one graphics API has no more than one combination; every later API of that pass then gets all combinations, unstripped, as in the Editor.
- A pass whose `LightMode` is `Meta` is kept only when the project keeps dynamic lightmaps; `Picking` and `SceneSelectionPass` passes are dropped.
- Hardware tier variants are compiled when the three tiers of the standalone target differ or the pass asks for them.

A shader is compiled for the graphics APIs of the project's Linux standalone target: OpenGL Core and Vulkan, or those of the two the project lists itself; a build visits them by their number, whatever the listed order. The settings read are `ProjectSettings.asset` (graphics APIs, colour space, precision model, virtual texturing, lightmap encoding, shader chunk size), `GraphicsSettings.asset` (deferred reflections, tier settings, stripping), `QualitySettings.asset` (level-of-detail cross-fade) and `Packages/manifest.json`. A file the project lacks stands for the engine's defaults.

The parameters all programs of a pass share move into the pass, as in a build, and each graphics API gets a table of entries cut at the project's chunk size and compressed with LZ4 at high compression level 9. The reference LZ4 library is linked (`lz4-sys`, MIT), since the compressed bytes depend on its match search.

`Fallback` and `UsePass` name another shader. When that shader is a source of the project, it is baked and published first, and the object lists it and takes over its keywords as a build does; the reference to it leads to the other published file, which is the one deliberate difference from a bundle, whose references depend on its layout. Chains of fallbacks are followed.

A baked shader is kept with the hashes of everything it was made from: the files it includes, the three settings files, the materials that use it and the shaders it names. A session bakes it again when one of them changes or a material starts to use it. The programs of a shader are compiled one after another by one compiler process, so a shader of thousands of variants takes minutes; a pass of more than a million keyword combinations is refused.

Refused, never approximated:

- a project with a scriptable render pipeline asset: the pipeline's Editor scripts decide which variants remain, and its pass kinds and keyword filters are not reproduced;
- a `Fallback` or `UsePass` that names a shader built into the engine (`VertexLit`, `Diffuse`), a shader the project has no source for or one that several sources declare, a used pass its shader lacks, and a shader whose keywords more than one other shader extends, for which the order was not established;
- a program that does not compile, with the compiler's message, and a program the compiler warns about. A build leaves a failed variant out and goes on;
- a shader with an import error; an include that is not a literal path; an include of a package file, which the worker's project does not hold;
- a material variant that uses the shader, whose keywords come from its parent;
- a Linux graphics API other than OpenGL Core and Vulkan, non-empty shader build settings, 360 stereo capture, a pass that selects a compiler toolchain, a ray tracing pass, a pass without a vertex program;
- a shader in `Packages`.

Not covered:

- What a build takes from outside the project's files: scriptable stripping (`IPreprocessShaders`), shader variant collections, XR, native shader plugins. A project that has one of these gets the variants described above, not those of its own build.
- A material-less shader used only from code (`Shader.Find`, `new Material`) gets no `shader_feature` variant beyond the first.
- Compute shaders: the worker imports a `.compute`, but its kernels are compiled by another request of the compiler and laid out otherwise. A `.shadergraph` is imported by a script, which a standalone worker does not run.
- A Player loaded shaders with one and two passes, keyword variants, an include, a fallback and a used pass, on OpenGL Core. The Vulkan programs are compared with the engine's but were not run.
- Rules read from the Editor binary that no reference build exercises: virtual texturing, entities graphics, a `stats:` or warning line in a compiler reply.

## UI and fonts

Unchanged UXML, USS and font assets are extracted from an existing Unity import cache rather than imported cold. See [UI_IMPORTS.md](UI_IMPORTS.md).

## Limits

- The wire protocol, LMDB schema and DataStore layout were derived for one Unity version and are not a stable public API. A worker version mismatch is refused, and another engine version needs its own verification.
- There is no proven general support for animation inside models, skinned meshes, complex model settings, texture formats of other platform families, custom AssetPostprocessors or every dependency of an arbitrary project. A project with an `AssetPostprocessor` for textures imports them in the Editor through that script; a standalone worker has no script assemblies and does not run it.
- Video other than VP8 `.webm`, ScriptedImporter assets other than `.inputactions`, sprite atlases of the first format (`.spriteatlas`) and compute shaders are not imported; a scene that refers to a new one needs a catalog built by the Editor.
- `.obj`, `.dae`, `.blend` and `.inputactions` need files of an Editor run of the project (the shader name table, the script assemblies); a project that was only ever prepared with a reused host and never opened in an Editor has none, and such an import is refused with that reason.
- A model's generated material uses the built-in `Standard` shader and depends on the host holding it.
- A shader is delivered only in the cases listed under [Shaders](#shaders); a shader of a project with a scriptable render pipeline and a compute shader still need a catalog built by the Editor.
- The store reader handles inline and concatenated storage, limits one file to 64 MiB and refuses other layouts. The LMDB reader targets the observed Linux x64 format.
- A worker is started per import batch. A persistent native service is not used.

## Code

The coordinator lives in `rust/src/native/`, with the wire format, LMDB reader and SourceAssetDB writer beside it:

| File | Responsibility |
| --- | --- |
| `rust/src/native/import.rs`, `adopt.rs`, `batch.rs`, `importer.rs`, `warm.rs` | Project import, adoption, cold batches and the runtime cache |
| `rust/src/native/probe.rs` | One AssetImportWorker run |
| `rust/src/registry.rs` | The owned SourceAssetDB and native source hashes |
| `rust/src/protocol.rs` | Wire format, requests, responses and `ArtifactMetaInfo` |
| `rust/src/native/datastore.rs` | DataStore namespace, readiness and shutdown |
| `rust/src/lmdb.rs` | Reading a stopped LMDB without a native library |
| `rust/src/native/artifact.rs` | Extracting stored files and checking content keys |
| `rust/src/native/environment.rs` | The import environment fingerprint |
| `rust/src/native/scripted.rs` | What an import takes from an Editor run: compiled importers and the shader name table |
| `rust/src/native/atlas.rs` | The project's sprite atlases, the textures they pack and the batches they are imported in |
| `rust/src/runtimeartifact/scripted.rs`, `video.rs`, `atlas.rs` | The Player form of a scripted importer's objects, of a video clip, and of a sprite atlas and its sprites |
| `rust/src/native/baked.rs`, `rust/src/playerbake/` | Assets baked from authored YAML without a worker |
| `rust/src/playerbake/shader.rs`, `shader/`, `rust/src/native/shader_compiler.rs` | Shaders: variants, program tables, the client of `UnityShaderCompiler` |

`rust/src/runtimeartifact/` adapts imported serialized files for the Player.

The ordinary tests check protocol and database bounds, corrupted content and wrong settings without starting Unity.
