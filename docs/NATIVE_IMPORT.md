# Native import

Heavy content (models, audio, textures, text assets) is imported by Unity's own AssetImportWorker, driven by this tool's coordinator. The main Editor process, its GUI and a pre-imported `Library` are not needed for a worker run. Scenes and authored prefabs do not go through this path; the runtime loader reads them from source.

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

Before expanding a scene, the runner walks the GUID references of the selected scene, its runtime prefabs and their authored dependencies. New or changed models (FBX), audio files, textures and text assets are imported in batches of 32 assets per worker; the sections below list the source formats and the importer settings each covers. An unchanged baseline, catalog or cache entry is skipped, as is a PNG or JPEG in the supported raw profile.

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

Every source format TextureImporter reads goes through one route: `.png`, `.jpg`, `.jpeg`, `.tga`, `.tif`, `.tiff`, `.bmp`, `.psd`, `.gif`, `.exr` and `.hdr`. `.dds` and other container formats are not covered.

A standalone worker imports an ordinary 2D texture (`textureType: 0`, `textureShape: 1`) whose selected platform uses the automatic format (`textureFormat: -1`) uncompressed or with normal or low quality compression, crunched or not, with the importer's resize and mip chain. For those settings the worker's artifact was compared with the artifact the Editor writes for the same source and `.meta` and is byte-identical: DXT1, DXT5, BC6H for the HDR formats, the crunched form, RGB24 and a readable texture without mips. Each of the eleven formats was loaded in a Player as a new asset.

High quality compression selects BC7, whose compressor the main Editor supplies as an import dependency; a standalone worker aborts on such an import. BC7, an explicit format, a platform override, and every other texture type or shape (sprites, normal maps, cubemaps) therefore use an exact warm artifact from an existing Unity import; a changed source or `.meta` without such an artifact is refused. For BC7, the compressor setting recorded in the artifact database is checked against `EditorSettings.asset`, and its hash is part of the cache key.

A `.meta` that holds a GUID but no `TextureImporter` block is refused. The engine reads it as a version 1 importer and makes an 8x8 cubemap of the file, in the Editor as in a worker, so there is no Texture2D to deliver; the Editor writes the block when it first sees the file.

## Audio

AudioImporter sources share one route: `.wav`, `.ogg`, `.mp3`, `.aif`, `.aiff` and `.flac`. The check is on the imported AudioClip, not on the source format. Accepted: load type DecompressOnLoad or CompressedInMemory with PCM, Vorbis or ADPCM data, `preloadAudioData: 1`, `loadInBackground: 0`, 16 bits, one sub-sound, not ambisonic and not a tracker module. Each source format was loaded in a Player as DecompressOnLoad and as CompressedInMemory Vorbis, and WAV with all six combinations: a decompressed clip was read back with `AudioClip.GetData`, a compressed one was played and heard at the mixer output.

Refused: Streaming, a clip that is not preloaded, and background loading. The Player's loader requires a loaded, preloaded clip of those two load types before it publishes the object, so these need a change of the runtime bridge, not of the importer path. Note that the `.meta` the pinned Editor writes for a new audio file has `preloadAudioData: 0`; such a file is refused until that setting is on.

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

State machine behaviours stay objects in a Player. The baked file carries each behaviour under its authored file ID, published beside the controller as an object of its class, a `MonoScript` object for every class and the class's type tree, which the engine otherwise derives from the loaded class; the controller lists the behaviours and records which of them every state and state machine runs (`rust/src/playerbake/managed.rs`). The `MonoScript` objects lead the file: the Player reads the objects in the order the file lists them and looks up the class of a behaviour as it reads it, so a behaviour listed before its script loads without a class and the Player reports its script as missing. This was measured with behaviours whose IDs sort below the ID of their script, with the class in an asmdef assembly. A controller baked by a preparation reaches the session with the preparation's baseline, and the pointers its file leaves null are bound from the baseline as well. The fields a class serializes are read from the compiled assemblies by the SDK helper (`--behaviour-layouts` of `tools/LifecycleCompiler`), which follows the engine's rules: public or `[SerializeField]` fields that are not static, constant, read-only or `[NonSerialized]`, base class fields first, `[Serializable]` classes and structs inline, arrays and lists, enums, fixed buffers, object references, the engine's own structs such as `Vector3`, `AnimationCurve` and `Gradient`, and a `[SerializeField]` dictionary. A type of the engine's own assemblies is stored in one of two ways. Native code stores some in a form their C# fields do not show: the vectors, `Quaternion`, `Color`, `Color32`, `Rect`, `RectInt`, `Bounds`, `BoundsInt`, `Matrix4x4`, `LayerMask`, `RenderingLayerMask`, `AnimationCurve`, `Gradient`, `RectOffset`, `Hash128`, `SphericalHarmonicsL2`, `LazyLoadReference<T>`, and also `GUID` (four words, written in text as 32 hex digits), `PropertyName` (a build keeps the CRC-32 of the name as a signed number in decimal digits) and `GUIStyle`. Every other type is stored by its C# fields under the same rules as a type of the project. This was established against a build of a behaviour field and an array of every public, non-generic type the engine's assemblies export to scripts in the reference project, 1375 types of which the engine serializes 269: with the helper laying out every engine type by its fields, the type table entries and the objects, as created and with every field set, equal the engine's for all of them but the native ones above, the two `Unity.Loading` IDs, which stay refused, and one type whose serialization callback changes its data.

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
| `Material` (21) | `.mat` | 2100000 | byte for byte in gamma and linear projects: hand-written sources in every state (stale, missing, misfiled, unsorted and repeated properties and keywords), and the materials of a game built with the universal pipeline |

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

Refused:

- A material variant (`m_Parent`): a build writes it merged with its parents.
- A material whose shader the caller does not know, or without a shader: it is written without properties, and whether the shader exists is the project's to say.
- A material of another serialized version than 8, or with texture stacks (virtual texturing).
- A material of `Particles/Standard Unlit` or `Particles/Standard Surface`: a build rewrites their disabled passes.

Not covered: a dedicated server build writes a material without its shader reference. A YAML mapping with a repeated key (`stringTagMap`) is read by this tool's parser before the baker sees it.

### Audio mixers

A `.mixer` is not baked; a source holding one is refused. Its Player form is a derivation, not its fields. Of the four classes of the source, three reach a Player and the effects (`AudioMixerEffectController`, 244) do not:

- `AudioMixerGroupController` (243) keeps its name, its mixer, its GUID as four integers and its children.
- `AudioMixerSnapshotController` (245) keeps its name, its mixer and its GUID. Its values move into the mixer.
- `AudioMixerController` (241) keeps the output group, master group, snapshots, start snapshot, suspend threshold and update mode, and gains an `AudioMixerConstant`: the groups in a flat list (parent index, the indices of volume and pitch among the snapshot values, mute, solo, bypass), the effects (type, group index, send target, wet mix index, previous effect, bypass, parameter indices), the count of side-chain buffers, each snapshot as a name hash with one value per index plus transition types and indices, the GUIDs of groups, effects and snapshots, three name buffers, and the exposed parameters as name hashes with value indices.

What is not derived: the order in which values get their indices, the effect types and their chaining (the attenuation effect of a default group leaves no effect entry), the defaults of values a snapshot does not store, the transition lists, and the two hashes. One engine-built mixer (a default one) is at hand, which fixes none of these.

## UI and fonts

Unchanged UXML, USS and font assets are extracted from an existing Unity import cache rather than imported cold. See [UI_IMPORTS.md](UI_IMPORTS.md).

## Limits

- The wire protocol, LMDB schema and DataStore layout were derived for one Unity version and are not a stable public API. A worker version mismatch is refused, and another engine version needs its own verification.
- There is no proven general support for animation inside models, skinned meshes, complex model settings, arbitrary texture settings, streamed audio, platform overrides, custom AssetPostprocessors or every dependency of an arbitrary project.
- Models other than FBX (`.obj`, `.dae`, `.blend`), video clips, `.inputactions` and other ScriptedImporter assets, sprite atlases and compute shaders are not imported; a scene that refers to a new one needs a catalog built by the Editor. A standalone worker has no script assemblies, so it runs no ScriptedImporter.
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
