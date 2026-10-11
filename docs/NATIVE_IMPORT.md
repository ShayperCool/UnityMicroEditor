# Native import

Heavy content (models, audio, compressed textures) is imported by Unity's own AssetImportWorker, driven by this tool's coordinator. The main Editor process, its GUI and a pre-imported `Library` are not needed for a worker run. Scenes and authored prefabs do not go through this path; the runtime loader reads them from source.

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

Print the current import environment fingerprint without starting any Unity process:

```bash
unity-micro-editor native-environment --project /absolute/project
```

## During a run

Before expanding a scene, the runner walks the GUID references of the selected scene, its runtime prefabs and their authored dependencies. New or changed FBX and WAV files, and PNG files that need native resize or compression, are imported in batches of 32 assets per worker. An unchanged baseline or cache entry is skipped, as is a PNG in the supported raw profile.

Each cache entry records an `environmentSha256`. The fingerprint covers the pinned protocol revision, the Unity path, the exact SHA-256 of `ProjectSettings.asset`, the parsed settings and the wire environment. An index with no fingerprint, or a different one, is skipped and its assets are imported again. Changing this environment while a runner is live requires restarting the runner.

## How an import works

1. The coordinator creates an isolated context with its own `Assets`, `ProjectSettings`, `Packages`, `Library` and `Logs`, and copies the exact source and `.meta` bytes into it.
2. The coordinator writes a minimal `SourceAssetDB` directly: GUIDs, paths, importer settings and XXH3-128 file hashes taken from the sources. Nothing is copied from an Editor.
3. It starts a DataStore with a unique namespace, accepts the worker's local TCP connection, and sends `Initialize`, `Prepare` and `ImportRequest`.
4. Requests use the primary importer ID `-1`, so the native pipeline picks the importer and reads its settings from `.meta`. Explicit importer IDs run an extra importer with default settings and are rejected by content validation.
5. The worker is stopped through the protocol, then the DataStore. It reads only the stopped store, following `ImportResponse.ArtifactBlobs[1]` → `ArtifactMetaInfo.Files[].Hash` → the stored file.

Worker parameters come from an exact copy of `ProjectSettings.asset`: color space, shader precision and the Linux graphics API override. The profile is limited to the Linux standalone target, with OpenGLCore and Vulkan as the verified automatic API set. Unsupported or ambiguous settings are refused. The applied values and the settings hash are written to `environment` in the report.

For each linked file the coordinator checks length, storage bounds and the native XXH3-128 content key, and records SHA-256. Serialized files are inspected with Unity's standalone `binary2text`, which does not start an Editor. Checked files are saved in `<output>/artifacts`, wire payloads in `<output>/frame-*.bin` and process logs in `<output>/logs`.

## Textures

TextureImporter receives the project's default texture compression format and an explicit record that no user overrides exist. DXTC is the verified default.

Cold import is limited to ordinary uncompressed textures. A compressed PNG uses an exact warm artifact from an existing Unity import; a changed source or `.meta` without such an artifact is refused. For BC7, the compressor setting recorded in the artifact database is checked against `EditorSettings.asset`, and its hash is part of the cache key.

## Mesh colliders

An Editor artifact for a mesh with `m_IsReadable=0` can hold vertex and index data but no cooked collision data for a Player. For a MeshCollider, the coordinator writes a separate immutable runtime file with `m_IsReadable`, `m_KeepVertices` and `m_KeepIndices` enabled on the originally unreadable mesh. The FBX, its `.meta`, the scene and the collider geometry are not changed, and the projection is proven byte-identical to the import apart from those three flags.

The loader calls the public `Physics.BakeMesh` with the authored `convex` and `cookingOptions` values, then `UploadMeshData(true)` restores the original unreadable state before user callbacks run. Bounding-box substitutes are not used.

## Animation clips

Unity has no importer output for a `.anim` file: the worker returns only the importer object, and the Editor derives the Player's clip data from the YAML during a build. The tool performs that derivation itself (`rust/src/playerbake/`), so a new or changed clip needs neither an Editor nor a worker. It is baked in about a millisecond and published as native content like an imported asset; a watched session applies it to the running Player.

A legacy clip keeps its authored curves. Any other clip gets the muscle clip Mecanim evaluates: streamed cubic segments, the constant clip, value deltas and bindings. The result is compared byte for byte with objects built by Unity `6000.6.3f1`: the fixtures under `rust/tests/testdata/playerbake`, and any project's clips through `UNITY_PLAYERBAKE_CLIP_CORPUS`.

A float curve on a field of a script and an animation event that passes a script are supported: the baked file carries a `MonoScript` object naming the class, which the Player ties to the loaded class. The session takes the class and its assembly from the compiler graph of its start; a script added later is unknown until the next session.

Humanoid clips are covered (`rust/src/playerbake/humanoid.rs`). An Animator float curve named exactly like one of the 200 curves of the humanoid rig (`RootT.x`, `Spine Front-Back`, `LeftHand.Thumb.Spread`, `SpineTDOF.x`, ...) is bound to the rig; any other name remains a parameter. From those curves the bake derives what a build derives: the root transform at the start and stop of the clip, the left and right foot at the start, the pose difference between start and stop, and the average speed and turning speed of the root over twenty steps, from the motion curves when the clip has any. The root motion flags are derived from the curves as well, and the authored values are ignored: a clip has motion float curves when a float drives `MotionT` or `MotionQ`, and a generic root transform when a position or rotation curve sits on the root object. Generic root motion is only that flag. The 28 fixtures under `rust/tests/testdata/playerbake/humanoid` hold one clip per rule. The turning speed takes an arc tangent from the system C library, as the Editor does, so a library that rounds differently can move it by one unit in the last place.

An additive reference pose is derived when the caller supplies the source of the reference clip: one value per curve, taken from the reference clip at the reference time where that clip animates the same binding. The flag without a clip, or a clip without the flag, changes nothing. A session supplies it and bakes the clip again when its reference clip changes.

The baker refuses what it does not reproduce exactly, and the session reports the clip instead of publishing an approximation:

- a clip with `m_UseHighQualityCurve` off in which a changing curve qualifies for the dense clip: no stepped key, a key on nearly every frame (four bytes a frame take no more than 28 bytes a key), and no two keys closer than a frame less half a millisecond. The dense samples are not reproduced. For float curves this is the engine's rule; whether Transform curves follow it has not been established, so a Transform curve that would qualify is refused too;
- human curves whose root, motion or goal rotation has zero length at a sampled time, which makes the derived values not a number;
- float curves on a built-in component other than an Animator parameter or `GameObject.m_IsActive`, and float curves marked as discrete;
- object reference curves and an event that passes an object other than a script;
- an additive reference pose in a session, in a clip imported from a model, on a clip with script curves, from a reference that is legacy, has no curves or is stored in a model, and between an Euler and a quaternion rotation curve of one object;
- a legacy clip or a clip without curves whose source sets a root motion flag or an effective additive reference pose;
- weighted tangents and negative infinite tangents;
- a source larger than 256 MiB. A source of tens of megabytes is read whole and takes a few seconds and up to a few gigabytes of memory.

## Animator controllers

A `.controller` file holds Editor objects (state machines, states, transitions, blend trees) that do not exist in a Player. A build flattens them into one constant per controller; the tool does the same (`rust/src/playerbake/controller.rs`). A new or changed controller is baked and published like a clip, and so is every clip it plays, because a baked controller refers to baked clips. The Player then runs a controller that no Editor has seen. When a clip changes, the baked controllers that play it are baked again, since a controller holds facts of its clips.

The comparison with Unity `6000.6.3f1` covers every field of the built object, with two deliberate differences. The hash-to-name table has no fixed order in the engine and is compared as a set. `m_ControllerSize`, the memory the Player reserves before reading the constant, is an upper bound rather than the engine's figure: it stays above it for every reference controller.

Covered: layers with blending, weights, avatar masks and synced layers with their override motions; nested state machines with entry, exit and any-state transitions; conditions, mute and solo; parameters; every kind of blend tree, nested. For a freeform 2D tree the tables of the engine's precompute are reproduced bit for bit against 202 blend spaces built by the Editor; its arc cosine comes from the system C library, as in the Editor, so a library that rounds differently can move an angle by one unit in the last place.

An `.overrideController` is baked as well: its Player form holds the same references as the source, so it is published with the baked controller and clips it names, and baked again when one of them changes.

An avatar mask has a Player form of its own, which holds what the `.mask` source holds: the body part switches and the transform paths with their weights. `playerbake::bake_mask` (`rust/src/playerbake/mask.rs`) writes it; 24 masks built by the Editor, among them masks of body parts alone and masks of some seventy transform paths, are equal byte for byte. A layer may also name a mask stored inside the controller's own file. The layer's constant takes the mask's body parts and transform paths, and the controller object refers to no mask, so the baked file holds none: a build writes such a mask only as an object of its own, when it is a visible object of a bundled asset.

A controller or override controller may play clips stored inside a model. The worker imports the model, which takes a few seconds, and leaves its clips in the Editor's form; they are baked like authored clips and published as content derived from the model, under a GUID no source owns. A model that only controllers refer to is not imported as a whole, so a skinned or otherwise unsupported model can still supply clips. When such a model changes, the controllers that refer to it are baked again.

State machine behaviours stay objects in a Player. The baked file carries each behaviour under its authored file ID, published beside the controller as an object of its class, a `MonoScript` object for every class and the class's type tree, which the engine otherwise derives from the loaded class; the controller lists the behaviours and records which of them every state and state machine runs (`rust/src/playerbake/managed.rs`). The fields a class serializes are read from the compiled assemblies by the SDK helper (`--behaviour-layouts` of `tools/LifecycleCompiler`), which follows the engine's rules: public or `[SerializeField]` fields that are not static, constant, read-only or `[NonSerialized]`, base class fields first, `[Serializable]` classes and structs inline, arrays and lists, enums, fixed buffers, object references, the engine's own structs such as `Vector3`, `AnimationCurve` and `Gradient`, and a `[SerializeField]` dictionary. A behaviour on a state machine that overrides `OnStateMachineEnter` or `OnStateMachineExit` turns off multithreaded evaluation of the controller, as in a build.

The comparison set is 47 controllers built by the Editor with 29 behaviour classes the reader lays out. 40 controllers are baked and match field by field; the others hold something from the list below. Apart from the controllers, every behaviour document is encoded on its own with its object references resolved as in the reference build: 175 objects equal the engine's byte for byte, as does the type table entry of every class. `rust/tests/testdata/playerbake/behaviours` holds a part of that set; `UNITY_PLAYERBAKE_BEHAVIOUR_CORPUS` compares a directory of the same form. The three hashes of a script type entry (script identity, layout, type tree) are written as a build writes them, although a Player loading a ContentFile was seen to check none of them.

A behaviour field may refer to any object. For an object of another asset the baker asks its caller (`ControllerSources::reference`, with the target's GUID, file ID and type, the class the field declares and the field's path) and takes one of three answers: the object is in another published file, and the field becomes a pointer through an external reference; the reference leads nowhere, and the field is null; or the object is bound later, in which case the field is written as null, a list keeps its length, and the baked controller reports the behaviour's file ID, the field's path (`field`, `nested.field`, `list[3]`, `list[3].field`) and the target. A session answers the first for clips, which are baked and published with the controller, and has no answer yet for other assets, so such a controller stays refused there. In a build a field holds its object only if the object exists and is of the field's class or derives from it; a caller that answers "published" has to check that, and the reader reports the classes a script class derives from for it (`bases`). The baker itself resolves a reference into the controller's own file: to the controller, from a field that may hold one, or to a behaviour the controller runs; a reference to an Editor object such as a state, or to nothing, is null as in a build. Three controllers that refer to a prefab and its components, materials, a texture, a sound, a mesh, a text asset, scriptable objects and built-in resources, from plain fields, lists and nested classes, are compared with a build that holds those assets in another bundle.

A behaviour document need not hold every field of its class: the class may have gained fields since the asset was saved. The engine reads a document over a newly constructed object, so such a field keeps what the constructor left in it. The helper follows the IL of the parameterless constructor, base constructors and field initializers included, and reports per field the constant stored in it (`initial`), or that something is stored which it does not follow (`initialUnknown`): a call, a branch, an array with elements, an object made with arguments or changed after it is made. A field without either is as the engine makes it: zero, an empty string or list, a null reference, a curve without keys, a gradient from white to white, and for a `[Serializable]` class an object the engine constructs itself, through that class's own constructor if it has a public one without parameters and without running code otherwise. The baker fills a missing field from that and refuses the object when a missing field is one whose value is not known. A missing `m_Enabled` is 1. Keys the class does not have are ignored. Text of another kind than the field's type is read as the engine reads it: an integer as far as the text is one (`1.5` is 1, `abc` is 0, wrapping at 64 bits and then cut to the field's width), a float by its leading decimal number and as not a number without one, and a structure where a number or a class is expected leaves the field as constructed. Twelve documents that lack some or all of their fields, in nested classes and list elements as well, equal the engine's objects byte for byte, as do 50 one-field edits of that kind.

The same reader lays out any class derived from `ScriptableObject`, not only behaviours, and nine objects of scriptable object assets with objects stored inside them, encoded with the layouts it reports, equal the engine's byte for byte. Nothing bakes such an asset yet.

Two findings of that comparison changed the controller baker for every controller. Paths of states start at the name of the layer's root state machine, written with its dots, not at the layer's name; the Editor keeps the two equal, a hand-edited file may not. The exit of a nested state machine that no transition leaves leads to the default state of the machine around it.

Refused, never approximated:

- a behaviour whose document lacks a field that its class's constructor sets in a way the helper does not follow, or sets through a constructor that is not public; a list authored as a mapping, a reference or an engine struct authored as a plain value, and an integer array authored as a hex dump of another width, of which the engine makes something the baker does not reproduce;
- in a session, a behaviour field that refers to an asset other than an animation clip, such as a ScriptableObject, a prefab or a material; a behaviour field that refers to a behaviour of the same controller that no state or state machine runs, which a build was not seen to hold or drop;
- a class with a `[SerializeReference]` field; a class whose serialized types nest more than ten deep, where the engine cuts them off, which includes every type that holds a list of itself; an engine type outside the compared set, a dictionary below the top level of the class, a fixed buffer of a type other than `int`, `byte`, `float`, `bool`, `char`, `short`, `long` or `double`, and a method that hides `OnStateMachineEnter` or `OnStateMachineExit` without overriding it;
- a field authored in an older serialized version of an engine struct; an engine struct that has had several forms and lacks a part, other than a rectangle or a curve;
- a behaviour whose script the session does not know, is not a source file of the project, or is a nested class;
- two states or state machines of one layer that share a path and both have behaviours; a nested state machine without exit transitions inside one without a default state;
- any clip the clip baker refuses;
- a behaviour field that refers to an avatar mask stored inside the controller, which the baked file does not hold.

A controller text the reader rejects fails outright rather than being refused: the Editor writes the closing quote of a string that ends in a line break at the start of a line, and a hand-edited file may repeat a key, of which the engine reads the first.

Limits of the path as a whole:

- A controller built into the native catalog keeps pointing at the clips it was built with. A changed clip reaches an Animator only when its controller can be baked; otherwise the session reports `native-bake-refused` and the Animator keeps the catalog's clip. The same holds for an override controller built into the catalog.
- A changed avatar mask does not bake the controllers that use it again; edit or touch the controller.
- A controller is baked with the behaviour classes of the script assemblies current at that moment. When a compile changes the fields of a behaviour class, the session bakes the controllers it published with that class again, and a controller with behaviours baked by an earlier session is baked anew instead of being replayed. A controller held by the native catalog or baked into a baseline is baked as well once a compile during the session changes such a class; until then the Player reads its behaviours as it reads any content built before a script changed, by field name.
- `auto-prepare` uses the same path when it publishes its baseline: a scene that refers to a clip or controller the native catalog lacks, or holds in an older form, is prepared without an Editor, and the baked files stay with the baseline. A source the baker refuses still needs a refreshed catalog.

## UI and fonts

Unchanged UXML, USS and font assets are extracted from an existing Unity import cache rather than imported cold. See [UI_IMPORTS.md](UI_IMPORTS.md).

## Limits

- The wire protocol, LMDB schema and DataStore layout were derived for one Unity version and are not a stable public API. A worker version mismatch is refused, and another engine version needs its own verification.
- There is no proven general support for animation inside models, skinned meshes, complex model settings, arbitrary texture settings, other audio codecs, platform overrides, custom AssetPostprocessors or every dependency of an arbitrary project.
- A shader can be imported, but the result has no platform GPU programs and cannot be delivered to a Player without a separate bake.
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
| `rust/src/native/baked.rs`, `rust/src/playerbake/` | Assets baked from authored YAML without a worker |

`rust/src/runtimeartifact/` adapts imported serialized files for the Player.

The ordinary tests check protocol and database bounds, corrupted content and wrong settings without starting Unity.
