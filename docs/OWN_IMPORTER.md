# Toward imports without a Unity licence

Status, 2026-10-23: a first writer, checked against the worker. Branch `feat/own-importer`.

The goal is a session that needs no activated Unity licence. Two parts of the
tool start Unity's own executable today, and only those check the licence:

- the import worker (`Unity -batchMode -adb2 -name AssetImportWorker…`), in
  every preparation and session ([NATIVE_IMPORT.md](NATIVE_IMPORT.md));
- the batch Editor that builds the host Player once per project
  ([HOSTLESS_PLAYER.md](HOSTLESS_PLAYER.md)).

The Player and the compilers start no licensing client: a `low` session that
compiles and patches code runs only the Player and the tool once its worker
stopped, and neither `UnityShaderCompiler` nor `Data/Tools/Compilation`
(`Unity.ILPP.Runner`) contains the licensing module's strings.

The licence is the user's seat on a machine, not a process's: every Unity
process of the user connects to one `Unity.Licensing.Client`
(`LicenseClient-<user>`). The worker logs of the Bouncemasters copy show 46
starts of the client and 82 connections to one that was running.

## What the worker does for the tool

Everything in [NATIVE_IMPORT.md](NATIVE_IMPORT.md): textures and sprites, audio,
OBJ/COLLADA/FBX/Blender models with generated materials, video, scripted
importers, sprite atlases, text assets, shaders with the build callbacks of a
render pipeline, scenes and their dependencies written in the Player format
(`CalculatePlayerDependenciesForScene`, `WriteSceneSerializedFile`), fonts. The
Play mode bridge also asks it for single assets during a session.

Bouncemasters (6000.0.71f1), files under `Assets`, `Packages` and the package
cache: 5798 PNG, 64 PSD, 14 JPG, 1 TIF; 686 `.asset`, 590 prefabs, 290
materials, 94 scenes, 17 sprite atlases; 140 WAV, 2 MP3, 1 MP4; 25 TTF/OTF;
10 FBX; 16 animator controllers, 7 clips; 77 shaders, 4 shader graphs, 1
compute shader; 7403 C# files.

## The engine binary

`Editor/Unity` of 6000.0.71f1 is 94 MB, 6000.6.3f1 120 MB. Both are stripped:
1071 static symbols, no DWARF, almost no RTTI class names. What remains are
strings, among them thousands of assertion expressions that name members
(`'defaultSettings.m_BuildTarget == TextureImporter::s_DefaultPlatformName'`).
A Ghidra project of the 6000.0.71f1 binary is kept outside the repository
(`~/tools/re/unity`).

Unity's licence forbids decompiling its software except where the law allows
it. Notes here describe formats and observable behaviour; no decompiled code is
copied into the tool. The C# side of the Editor is published as
`UnityCsReference` under a reference-only licence.

Libraries the importers link and their public equivalents:

| Work | In the Editor | Public |
|---|---|---|
| Image decoding | FreeImage 3.18 | FreeImage, Rust image crates |
| Texture compression | `ispc_texcomp`, `bc7e`, `astcenc`, `s3tcompress`, `etccompress` | the first three are open source |
| FBX | Autodesk FBX SDK | FBX SDK under Autodesk's licence, `ufbx` |
| Fonts | FreeType, HarfBuzz | the same |
| Shader programs | `UnityShaderCompiler`, `libdxcompiler` | separate programs, already driven by the tool |
| Audio to FSB5 | FMOD, linked statically | none; FSB5 with PCM is described publicly |
| Lightmaps, occlusion | Enlighten, RadeonRays, Umbra | none |

## What has no public equivalent

- The writer of Player serialized files from authored YAML: type trees of the
  release, `MonoBehaviour` fields by Unity's serialization rules, references
  between files. `playerbake` does part of it for 6000.6.3f1.
- Nested prefabs and variants with their overrides, merged for a build.
- Animator controllers and animation clips compiled into their runtime form.
- Sprite meshes, atlas packing, PSD flattening.
- Editor C# that runs during an import or a build: scripted importers of
  packages, Shader Graph generation, the pipeline's shader stripping,
  Addressables and Localization build steps. It calls `UnityEditor` bindings
  into the native Editor.
- The Player's built-in resources (`unity_builtin_extra`), which the
  Editor-free host prototype still had the worker write.

## The tool's own writer

`rust/crates/playerbake/src/playerbake/`:

- `authored.rs` converts a YAML document by the Player type tree of its
  class or script. A field the source lacks takes the value of an engine
  sample object of the class (without its references); what a build changes
  on the way is settled per class: tags become indices, a renderer's cached
  sorting layer is cleared, dangling components are dropped, unread particle
  curves and gradients are reset, a script field is read under its
  `[FormerlySerializedAs]` name.
- `prefab.rs` unpacks prefab instances, nested ones included: an object of
  an instance takes the file ID `(instance ^ source) & 2^63-1`, the
  modifications set fields by property path, removed objects are left out,
  added components and children are listed by their owners.
- `texture.rs` writes PNG sources kept uncompressed (RGBA32, RGB24, Alpha8)
  and their sprites, with the tight mesh and physics shape `spritemesh.rs`
  makes (below) or a full rect.
- `assemble.rs` writes the closure of root sources into one file with its
  streamed data, like the worker's `ume-deps`: text assets, textures held in
  `.asset` files, the objects of the Editor's `unity_builtin_extra` the
  sources name (but shaders), and a material's textures kept only where its
  shader declares them.
- `scripts.rs` lays out the type trees of script classes from what
  `tools/ScriptLayouts` (Cecil) reads of the compiled assemblies, and
  `TypeTree::of_nodes` encodes a tree's type table entry as the engine does
  (each name in the common strings by its offset, otherwise in the tree's
  own table at first use).
- `compare.rs` pairs the objects of two Player files by what they are
  (hierarchy path, component class and position, asset name), follows
  references by those keys and compares streamed data by its bytes.

```
unity-micro-editor player-file closure --library <worker file> --project P \
  --root Assets/Scenes/Main.unity --output F --unity-version 6000.0.71f1 \
  --builtin-extra <Editor>/Data/Resources/unity_builtin_extra
unity-micro-editor player-file compare F <worker's ume-deps>
```

Main of Bouncemasters, against the dependencies file the worker wrote: 1734
sources, 21185 objects paired (226 of the worker's unpaired), 20629 equal
field by field. The type trees and samples still come from a worker file
(`--library`); a release's trees and default objects are to be captured
once and shipped, so that a user needs no licence.

Transparent pixels of an RGBA texture with *Alpha Is Transparency* take the
colour of the nearest pixel whose alpha is not zero and stay transparent
(the Editor's `ImageAlphaDilate.cpp`): jump flooding over the image as the
engine stores it (rows from the bottom), the steps from half the larger
side, at most 16, halved down to 1, each pixel choosing from its own
nearest and those of its four neighbours at the step (left, right, below,
above) the one nearest in Manhattan distance, the first on a tie; a pixel
no step reaches, beyond 31, keeps its colour. An image with no pixel less
than opaque is `m_IsAlphaChannelOptional`, and stored RGB24 when its format
is left to the importer even if its source has an alpha channel. The usage
mode (`m_LightmapFormat`, set from what the importer computes) is 6, always
padded, for a sprite and 0 for a Default or Single Channel texture; other
texture types are left to the worker. Main: 469 of the 501 paired textures
equal (38 before these three rules). Of the other 32, 10 are scaled, 3 are
TextMesh Pro font atlases, and two Default textures of Spine (`chest_red`,
`tap_to_bump`, with the same `.meta` as a Default one the engine gives 0)
have usage mode 6 by a rule not known yet.

### Compressed and scaled textures

A texture whose format is left to the importer and whose compression is on
(normal or low quality, not crunched) is DXT5 with alpha and DXT1 without, if
its sides are multiples of four, else kept uncompressed. The blocks are
those of the Intel ISPC Texture Compressor the Editor ships and calls
(`CompressSingleImageTexture` → `CompressBlocksBC1`/`BC3` on 8-bit RGBA
rows from the bottom, DXT1 with alpha set opaque): `texcomp.rs` loads the
installation's `libispc_texcomp.so` (`player-file closure --editor`).

A texture larger than its maximum size is scaled down to it, the longer side
to the maximum and the other in proportion, rounded. The importer scales
with stb_image_resize v0.97 (`BlitScaleStbImageImpl`, vendored in
`playerbake/vendor/stb`): Mitchell along a shrinking side (a cubic B-spline
along another), edges clamped, alpha taken as premultiplied, in linear space
even for an sRGB texture, after the alpha dilation. It splits the rows into
`min(worker threads, rows / 16)` bands, each resized as a region of its own,
and the bands' sample positions round apart: the bytes depend on the count of
the engine's worker threads, which is the machine's threads less one (15 on
the machine whose worker wrote Bouncemasters' files), and the tool takes the
same count of its machine. A sprite of a scaled texture keeps its size in the
world: its rect scales along each side, its pixels per unit by the smaller
factor, and its mesh is made from the scaled pixels.

A readable texture keeps its pixels in its object (`image data`, an empty
`m_StreamData`), another's are streamed. `mipBias: -1`, as older settings
write it, is no bias.

Main without the worker's file: the 49 compressed and 38 scaled textures are
written too (refused sources 155 to 63); 584 of 588 textures equal, the other
four images of the same name paired the other way.

### Other images, power-of-two scaling and mipmaps

A PNG is read with `png`; the other formats the importer takes (JPEG,
Photoshop's composite image, TGA, BMP, GIF, and a `.png` that is not one)
with stb_image (vendored, `vendor/stb/stb_image.c`). A texture whose sides
are not powers of two is scaled before the maximum size applies
(`nPOTScale` 1 to the nearest, a tie going up; 2 to the next larger; 3 to
the next smaller; never a sprite) with the resizer above. Mipmaps go down
to one pixel, each level the mean of the up to four pixels above it, the
colour in linear space for an sRGB texture; a block-compressed level
smaller than a block is padded by repeating its edge. The Advanced (-1)
and GUI (2) types of older Editors are written as Default textures.
Bouncemasters' resources: 413 written (388 before), none refused for its
format, size or mipmaps. Not reproduced: the importer's own mipmap filters
(Kaiser, coverage preservation, fading), so mipmapped textures are not
byte-equal; high quality compression (BC7); lightmap textures.

### Fonts

`font.rs` makes a TrueType or OpenType font a dynamic `Font` as the
importer does with its data included and the face's ascent: the file's
bytes, the names and settings of its `TrueTypeFontImporter`, `m_Ascent` and
`m_Descent` as `hhea`'s ascender and descender times `size / unitsPerEm`
(in `f32`, the factor first), `m_LineSpacing` as (ascender − descender +
line gap) × size / unitsPerEm, and the two objects it holds, `Font
Material` (the built-in Font shader, white, the texture) and an empty `Font
Texture` the Player fills. They go through the authored conversion as the
documents a font asset would hold. Main: the 6 fonts equal but for which of
the identical materials and textures of the same name each pairs with.

### Animation

A build derives a Player's `AnimationClip`, `AnimatorController`,
`AnimatorOverrideController` and `AvatarMask` from their sources; the
Editor's state machines, states, transitions and blend trees have no Player
objects. The bakers of the raw scene path did this for 6000.6.3f1, verified
byte for byte against AssetBundles the engine built (the fixtures under
`rust/tests/testdata/playerbake`, `rust/tests/mecanim.rs`); they came back
from the history (`bba0f19^`) as `clip`, `controller`, `mask`,
`override_controller`, `humanoid`, `binding`, `weighted` and `freeform`,
now making values instead of files. `animation.rs` puts them into a
closure: a source's Player object stands among its documents as a document
of its class and file ID without fields, made beside it with pointers into
the source itself and into the sources it names (clips, scripts, objects of
object curves), which the closure resolves as it resolves a document's. A
controller's state machine behaviours stay documents, written as any
behaviour is; a mask stored in its file is published beside it. A value
made for 6000.6 is laid out by the release's tree: fields the release lacks
are left out (6000.6 added `m_EvaluateTransitionsOnStart` and the
`ValueArray`'s `m_EntityIdValues`), and a controller's `m_ControllerSize`,
the constant's size in memory, is taken again over the release's
structures.

Main of Bouncemasters for 6000.0: the three clips of `.anim` sources equal
to the worker's; the three controllers that play them equal but for the
order of the name table `m_TOS`, which the engine writes in the order of its
hash table (the Player reads it into a map). Controllers that play clips of
models are refused until models are imported. A behaviour's class overrides
`OnStateMachineEnter` or `OnStateMachineExit` when its source declares such
an override; one inherited from another class of the project is not seen.

An audio mixer is derived the same way (`mixer.rs`, from the same
history, 272 objects of 6000.6 fixtures equal): the mixer, its groups and
its snapshots, the effects and every snapshot value moved into the mixer's
`AudioMixerConstant`. Main's mixer, three groups and snapshot for 6000.0
equal the worker's, so the pack's sample constant no longer stands in.

### Models

`model/` reads an `.fbx` through `ufbx` (vendored C, MIT) and makes the
meshes the Editor's model importer makes (`player-file fbx <file> [--write
F <release>]` prints the file and what the tool imports of it):

- Unity's space: X mirrored (triangles turned the other way), lengths times
  `globalScale` and, with `useFileScale`, the file's unit in metres. A
  node's origin moves to its pivot and its geometry the other way (`ufbx`'s
  `AdjustToPivot`): Bouncemasters' background models keep their meshes
  centred on pivots 7.33 units off their parent's origin.
- The model's root is the scene's single top node if it has one, named
  after the file; names met again are told apart by a number in order of
  the hierarchy (`eye_l 1`). A mesh is named after its node; the root's
  after the file's node, not the file.
- Normals imported or calculated (smoothing angle, weighed by area and
  angle), tangents by MikkTSpace (`bevy_mikktspace`) over the first UV set,
  colours to bytes, up to eight UV sets; corners welded on all their
  attributes; one submesh per material slot; 16-bit indices up to 65535
  vertices.
- A skinned mesh: up to four weights per vertex, heaviest first; bones in
  the skin's order, each named by the CRC-32 of its path below the root;
  bind poses from the mesh's node into the bone's; a bone box per bone; the
  root bone the one above all others. Its positions stay in the first
  stream, colours and UVs in the second, the skin in the third (indices
  alone with one bone per vertex).
- File IDs from the `.meta`'s `fileIDToRecycleName` (before 2019.3) or
  `internalIDToNameTable` by class and name; in a `.meta` of the format
  before 2019.3 a new object takes its class's next free ID (`class *
  100000 + 2k`). Otherwise the importer hashes: `XXH64("Type:<class>-><key>
  <n>")` read as a signed integer, `n` counting IDs already taken. A game
  object's key is its path with the root renamed `root` below
  `//RootNode` (`//RootNode/root/a/b`), a component's that path plus `/`
  and its class; a mesh's the file's name of its node; a clip's the stored
  `internalID`, else the take's name; the avatar is `9000000`. Names are
  made unique among siblings (`nodeNameCollisionStrategy` 1: the repeats,
  from the last, take the first free `name 1`, `name 2`) or across the file
  (0, the format before: passes over the nodes sorted by name, depth and
  parent path), children sorted by name with numbers by value
  (`sortHierarchyByName`). The file's single top node is the root unless
  the hierarchy is kept, it is a bone or a humanoid's extra root is
  wanted. Worked out from the Editor's code and checked against 6000.0
  imports (268 objects) and every reference of Dungeon to its models:
  4737 of 5023 reproduced, the rest prefab overrides of nodes the models
  no longer have and clips the tool refuses.
- The hierarchy (`model/hierarchy.rs`): a game object and transform per
  node, a mesh filter and renderer or a skinned renderer (its bones, root
  bone and bounds), an animator on a rig's root with its avatar; the
  materials the `.meta`'s `externalObjects` map, else the engine's default
  material. A prefab instance of a model is unpacked from these documents;
  a closure keeps of a model only the objects referred to and what they
  refer to within it, as a build holds objects (Main: no object the
  worker's file lacks).

Main of Bouncemasters: the 33 meshes it uses have the worker's vertices and
triangles (positions within float rounding), the skinned ones their bind
poses and bone hashes. What still differs: the order of vertices and
triangles (the Editor's own vertex cache optimizer, in jobs), the UV
distribution metric by up to 1% (the rule is followed, see `uv_metric`),
the channels a build strips because no shader of the mesh's materials
reads them (`StripUnusedMeshComponents`: the worker's meshes keep positions
and colours only), and which meshes a build keeps readable for dynamic
batching (taken as up to 300 vertices). A take becomes the Editor's form of a clip
(`model/take.rs`): the local position, rotation and scale of each node the
take animates, sampled at every frame of the file's rate in Unity's space
over the clip's frames (`clipAnimations`, or the take's range), each key
sloped through its neighbours; the clip baker makes the Player's clip of
it, and a controller that plays it takes its name and loop from the
`.meta`. The importer reduces those keys (`animationCompression`), which
the tool does not: Main's two takes play as the worker's do within 0.003
(its reduction's tolerance) at every frame, with about twice the keys. A
generic rig's avatar (`model/avatar.rs`) holds every node as its skeleton
(by the CRC-32 of its path, the name IDs by that of its name), the nodes'
local transforms as its pose and an empty human part with the
description's settings and the engine's default bone masses; Main's two
equal the worker's but for `m_AvatarSize`, which the tool takes over the
release's tree with the controllers' rule and which comes out larger. The
Player only reserves that much before reading (`ChainedAllocator::Reserve`)
and takes more if it needs to. A humanoid avatar is refused. A model's own materials (`model/material.rs`), where the `.meta` maps
none to a project material, are made in a URP project as URP's material
description preprocessor makes them: URP Lit, opaque or premultiplied
transparent by the description's opacity (or one less its transparency),
the base colour from the diffuse texture and factor or the diffuse colour
(to gamma space in a linear project), the normal map with its scale, the
emission (its keyword and realtime emissive flag with a factor above
zero), the smoothness from the shininess; a texture is the source of its
file name beside the model or in a `Textures` folder on the way up to
`Assets`; the material's file ID hashes its name. A built-in pipeline
project's are not made yet (its renderers take the default material). Not
imported yet: clips with events or curves of their own, blend shapes,
humanoid avatars and clips, lights and cameras of a model.

### Baked lighting

A scene's `LightingData.asset` is a binary file of Editor objects: the
`LightingDataAsset` and the `LightProbes` it names (`lighting.rs`). The
closure writes the probes as an object of its own, laid out by the Player's
tree; the scene's `LightmapSettings` takes the probes, the lightmaps, their
mode and the Enlighten scene mapping from the asset, each renderer of
`m_LightmappedRendererDataIDs` its lightmap index and scale and offset (an
object of a prefab instance found by the instance's and the object's IDs
together, as unpacking numbers it), each light its baking output and each
reflection probe its baked cubemap. Main's `LightProbes` equals the
worker's. Not written yet: the Enlighten data of realtime global
illumination (`m_EnlightenData`) and the ambient probe of a bake.

### Audio

An `AudioImporter` source becomes an `AudioClip` whose samples the
Editor's own `FSBTool` (`Data/Tools/FSBTool/x64`, FMOD's bank tool, which
the Editor runs for each import over a socket) encodes, run here from its
command line: `-C fsb -c vorbis|pcm|adpcm -q <quality x 100>`, `-m` for
`forceToMono` and then `-n` for `normalize`, `-r` to optimize the sample
rate, `-s <rate>` to override it. The settings are the `.meta`'s override
for the standalone group (`1`), else its defaults. The FSB5 bank goes, at a
32-byte boundary, to the `.resource` file beside the written one (the
stream's name with that extension), and its sample header gives the clip's
channels, frequency and length (samples over frequency, in `f32`). 140
clips of Bouncemasters against the worker's: all 139 paired equal, banks
included, in 1.8 s. MP3 and the platforms' own formats are left to the
worker.

### Sprite meshes

`spritemesh.rs` makes a sprite's tight mesh and its fallback physics shape as
the 6000.0 Editor's `SpriteMeshGenerator` does (`Runtime/Geometry`), in
`f32` and in the order the engine computes, from the alpha of the pixels the
sprite's rect covers (floor of its corner to ceiling of its far corner,
rows from the bottom):

1. The pixels above the alpha tolerance (0 for the mesh, 200 for the physics
   shape), grown by the extrusion (`spriteExtrude`; 0 is 1, at most 32) by
   Manhattan distance, mark the corners of their pixels.
2. Outlines are traced over the corners, counter-clockwise from the first
   corner left in row order (Moore neighbours, starting one past the step
   back); after each, the corners between each row's crossings and the
   first vertex's column are toggled, which clears what the outline encloses
   and brings out its holes, then its own corners are cleared. An outline of
   less than 4 pixels is passed; a hole is one whose first corner was not
   marked. Holes are kept with hole detection (always, for both); with the
   detail left to the area, a hole of less than a quarter of the image goes.
3. An outline keeps the corners where a run of steps in at most two
   neighbouring directions ends, and between them the corners farther than
   half a pixel (along the line's longer axis) off the line, farthest first;
   every vertex then moves a pixel along its normal (the two edges' normals
   summed). Its bounds are those of the moved vertices within the image.
4. The detail is `1 - spriteTessellationDetail`, or, below zero, `1 -`
   (area of the outlines, holes negative) × 0.000406901 / (triangles: each
   outline's vertices less two, two per hole), within 0 and 1. Vertices go,
   the one costing least area (with the area its removal already moved
   onto it) first, down to the count `(1 - detail) × n` leaves (at least
   five, then one more): a convex vertex costs the triangle it and its
   neighbours make; a concave one moves its neighbours along their normals
   by the root of the quadratic that keeps the area and costs the area that
   adds. A removal that would cross the outline or leave its bounds is not
   made. An outer outline is then clipped to its bounds, left, right,
   bottom, top (Sutherland–Hodgman, crossings in `f64`), every vertex is
   rounded to a whole pixel, and vertices within 0.0001 of the segment
   between their neighbours are dropped.
5. libtess2 tessellates the outlines (non-zero winding); its vertices,
   deduplicated in the order its triangles use them and rounded again, are
   the mesh's, with the index buffer reversed. Where outlines touch, only
   libtess2 v1.0.2 (to its commits of 2018) gives Unity's triangles; it is
   vendored as C (`playerbake/vendor/libtess2`).

The sprite takes `textureRect` = rect corner + the outlines' bounds,
`textureRectOffset` = the bounds' corner, positions `(v - pivot) / ppu`
with the pivot `m_Offset + region / 2 - (rect - region corner)`,
`m_Offset = (pivot × size + corner) - (size / 2 + corner)`, the mesh type bit
64 in `settingsRaw`, and texture coordinates left zero in a second vertex
stream aligned to 16 bytes. A sprite less than 32 pixels across, or whose
outline is empty, keeps the full rect (whose corners are `(±size / 2 -
m_Offset) / ppu`). The physics shape (`spriteGenerateFallbackPhysicsShape`)
is the outlines at detail 0.25 and tolerance 200, before tessellation,
less the pivot and times `1 / ppu`.

A sprite's own outline (the Sprite Editor's `outline`, in pixels from the
rect's centre) is tessellated as it stands, moved to the region's corner,
whatever the sprite's size and mesh type; its own physics shape
(`physicsShape`) is taken less the pivot offset, times `1 / ppu`. A sheet's
sprite takes its 64-bit `internalID` exactly. A sprite of the built-in
resources keys its render data by them (`0000000000000000f000000000000000`,
its file ID) and keeps its vertex data.

Main: all 484 sprites equal paired by their render data key; by the
comparison's keys, sprites of the same name in other folders pair by order. `player-file sprite-mesh <png> [x y
w h]` prints the outlines and mesh of an image; `UME_DUMP_FULL_BYTES=1`
makes `player-file dump` print every byte string whole. The comparison
orders assets of the same class and name by a hash of their values without
references, so equal ones take the same key in both files.

### A release's engine classes

`player-file closure` without `--library` takes the engine's classes from the
release's pack (`playerbake/engine/<release>/library.bin`, a Player file of
one object per class, read like a worker's file), the script templates from
theirs and the scripts from `--layouts`: no file of the worker is read.
`player-file engine-pack <release> <output> <file>...` makes it from files
the engine wrote: each class's type tree and a sample that keeps its numbers
and short lists of them (up to 32, without strings or references; byte
strings up to 64), the defaults of the fields a source lacks
(`m_CallbackLayers`, `m_TrailVertexStreams`, ...), and loses strings,
references and longer lists. A `MonoScript` takes its execution order from
its `.cs.meta` (`MonoImporter.executionOrder`, by the file's name). A script
class of a plugin's assembly (a `.dll` with a `PluginImporter`) is named by
the file ID `MD4("s\0\0\0" + namespace + name)`'s first four bytes as a
little-endian integer, with the DLL's GUID: its `MonoScript` is written for
each file ID the sources name (Main: Gadsme's 24 behaviours, all equal).

6000.0.71f1's pack comes from the worker's files of Bouncemasters (with its
scene file, for the scene's settings managers) and the Editor's
`unity_builtin_extra`: the classes the game uses; a project with others
(`Light`, `Rigidbody`, `MeshCollider`, ...) needs them captured too. Main without the worker's file:
20574 objects equal, as with it; with the compressed and scaled textures
written too, 21320 paired and 20844 equal. A sample still carries the short lists of the one `AudioMixerController`
(`m_MixerConstant`), which the tool now derives from the mixer's source.

What differs, by count of objects:

| What | Objects | Needs |
|---|---|---|
| Shaders (project and built-in) | all materials | imported and baked by the tool for 6000.0 with `UME_OWN_IMPORTER` (below); otherwise the worker |
| Fields a newer engine added to an older source | few | per-class default objects instead of samples |
| Animator controllers, models, audio | few | not written yet |

### A scene's own file

`player-file closure ... --scene <scene>` writes, as a build does, the
scene's own objects to a file of their own (`<output>.scene`) and what they
refer to to the closure's file, which the scene names as its first external
file (`ume-deps`). The scene file holds the scene's objects and those of its
prefab instances, its `RenderSettings`, `LightmapSettings`,
`NavMeshSettings` and the `LightingSettings` it names, in order of class with
scripts last and local IDs from 1; its `MonoBehaviour`s take their scripts
from the closure's file. The Editor's `OcclusionCullingSettings` and
`SceneRoots` are not written. Unpacking an instance points a modification's
reference to an object of the source prefab at the instance's copy, and the
file's references to its stripped objects at the objects they stand for. A
reference to an asset the project lacks is written empty, as the worker
writes it. Only the fields the Player reads (those of the class's or
script's tree, under their names or former ones) bring their references into
the closure: a font asset's `m_SourceFontFile_EditorRef` or a renderer's
`m_LightmapParameters` do not.

`player-file compare <ours> <theirs> --external ume-deps <our closure>
<their closure>` pairs references into the closure's file by the objects
they lead to. Main against the worker's scene file: 169 objects paired, 163
equal; the other six are the five Odin managers' `serializationData` (see
below) and `LightmapSettings.m_LightProbes`, which comes from the scene's
`LightingData` the tool does not write yet. The closure's file: 21357
paired, 20919 equal, none only the tool's. The worker's own 54 are what
the tool does not import yet: the models' 33 meshes, 2 avatars and 2 clips,
3 shaders, the scene's `LightProbes`, a Spine `.skel.bytes`, the scripted
importers' 6 certificates, and 2 project settings objects
(`LocalizationSettings`, `UIParticleProjectSettings`), with their 4
`MonoScript`s.

### Objects held by managed reference

A `[SerializeReference]` field holds the ID of an object (`rid`, or `id` in
a source of format 1) in the registry `references` at the end of its
object. The tool writes the registry as the worker does, of format 2: the
objects by ID in increasing order (a source of format 1 names them by
hexadecimal keys), each with its class (`class`, `ns`, `asm`, a nested
class as `Outer/Inner`) and its fields laid out by the class's tree, and,
when a field holds no object (`-2`, which a field the source lacks holds
too), a first entry of ID -2 and no class. The file's table of such types
follows its external references (format 22: the class -1, the index, 32
zero bytes, the tree's node count, string size, nodes and strings, and the
three names); a type's tree ends in a registry of its own when it holds
such fields, which its objects leave out. `ScriptLayouts` lists every
`[Serializable]` class a field of that kind can hold (assignable to the
field's declared type, the classes those hold in turn) under `managed`.
Localization's settings and three locales against the worker's: equal.

A reference to a script of the `UnityEngine.UI.dll` uGUI was before it was
a package (`f70555f144d8491a825f0804e09c671c`, or Unity 5's
`f5f67c52d1564df4a8936ccd202a3bd8`, which SRDebugger's prefabs name) names the
class by its file ID; it is pointed at the package's script of that class
(`Image.cs`, ...), as the Editor finds it.

### In a session

`UME_OWN_IMPORTER=1` has a session write its native scenes with the tool's
writer and the worker only when that fails; `UME_OWN_IMPORTER=only` never
starts the worker for them. The engine's classes come from the release's
pack, the scripts' layouts from the Editor compilation the worker would
load (its assemblies and plugins over the session Player's `Managed`), the
texture compressor and `FSBTool` from the installation; the scene and its
closure are stored under a key of their own (`native-scene-own-v1` and
the hash of the running program, so a rebuilt tool writes them again).
Shaders stay external and are published by the session as for the worker's
files; references to other sources the tool does not write yet (models,
animator controllers, ...) are written empty and reported in
`native-scene-written`'s `refused`, and an addressed asset of which nothing
was written is given up with all the others in one pass. The assets Play
mode code asks for by path are written the same way, with the engine's
classes and layouts kept for the session.

Two things a Player needs of such files that a comparison of objects does
not show: the `MonoScript`s come first (local IDs from 1, as the worker
numbers them), else the Player loads behaviours before their scripts and
finds them missing; and an external reference to the engine's own files
(`Library/unity default resources`, `Resources/unity_builtin_extra`)
carries their GUID, by which the Player finds them in its global table.
A transform that an instance places under an object of another instance
(in a variant) is added to its parent's children.

Bouncemasters with `UME_OWN_IMPORTER=only` (2026-10-23): no worker
started for Loading and Main (3 s and 15 s to write them), the session
reaches `ready` and Main draws its screen. Limitations: a session still
needs the Editor installation (its texture compressor, `FSBTool`,
`unity_builtin_extra` and shader compiler) and the shaders a preparation
published; sources a `ScriptedImporter` imports (C# of the game's Editor
code: Bouncemasters' certificates, so its profile client throws), models,
animator controllers and the scene's lighting data are not written, and
what refers to them finds nothing.

### Script type trees

`ScriptLayouts <directory or assembly>...` prints, for every class derived from
`MonoBehaviour` or `ScriptableObject`, its serialized fields (the bases'
first) and those of the `[Serializable]` types they hold: instance fields that
are neither constant nor read-only nor `[NonSerialized]`, public or
`[SerializeField]`/`[SerializeReference]`, of a primitive, an enum (its
underlying type), a string, an engine object, an engine value type, a
serializable class or struct, or an array or `List<T>` of one.
`player-file script-trees <layouts.json> <file>` lays them out and compares
each with the tree the file has. The compiler builds the tool once into the
project's store, as it does the Play mode linker (`Compiler::built_tool`),
and `Compiler::script_layouts` runs it on a Player's managed assemblies,
the newest compile's in place of the Player's own, keeping the layouts in the
store by the assemblies' hashes (`player-file script-layouts <project>
<editor> <player> <output>`; Bouncemasters: 7 s built and read, 0.3 s
again). With a session Player's assemblies Main's trees are 245 equal: the
Player's Addressables has no `AssetReference.m_SubObjectGUID`, which the
worker lays out from the Editor's (`SoundConfig`), besides the 26 of Odin.

The rules the engine's trees show, besides the field order:

- `bool` is `UInt8` with the flags *align* and *integer as boolean*
  (`0x4100`), `char` is `UInt16` with *character* (`0x2000000`); 1- and 2-byte
  primitives align.
- An array is `vector` › `Array` (array flag) › `int size`, `data`. An array
  of a serializable type is named after that type (`PersistentCall m_Calls`),
  one of managed references after the referenced type with type flag 8 and
  `managedRefArrayItem` elements. Arrays of primitives, strings and engine
  values align after their elements; those of objects and serializable types
  do not. Elements narrower than four bytes do not align each, and their
  flags (`0x100` of `bool`) hold for `vector`, `Array` and `size` too.
- An engine object is `PPtr<$Class>` with `m_FileID`/`m_PathID` flagged
  *don't animate* and *hidden* (`0x800001`); `[HideInInspector]` sets *hidden*
  (`0x1`) on the whole field.
- A node with an aligned descendant is flagged `0x8000`. A structure has the
  size of its fields when all have one.
- An object with managed references ends in the `ManagedReferencesRegistry`
  `references`.
- The engine value types (`Vector3` as `Vector3f`, `Color32` as a 4-byte
  `ColorRGBA`, `LayerMask` as `BitField`, `AnimationCurve`, ...) and the
  header every script object starts with (`m_GameObject` to `m_Name`) are
  templates of the release (below).

A field an asset lacks (one the class gained after the asset was saved)
takes what the class's parameterless constructor assigns it, the bases'
first: `ScriptLayouts` reads it from the IL as far as it shows constants,
strings, `new List<T>` with the elements a collection initializer adds,
engine values made by their constructors or by `Color.white`,
`Vector3.one` and the like, and new serializable objects with their own
defaults. A curve the script leaves null is an empty curve with clamped ends
(`m_PreInfinity`/`m_PostInfinity` 2, `m_RotationOrder` 4), a gradient the
default one. Main takes such defaults for 162 fields of scripts, 3398 times
(`closure` prints them as `derived`), most of them TextMesh Pro's
(`m_characterHorizontalScale`, `m_EmojiFallbackSupport`,
`m_ActiveFontFeatures`).

In a type table, a script's hash is MD4 of its class name, namespace and
assembly name, concatenated (all 265 scripts of Main). Its type hash is the
`m_PropertiesHash` of its `MonoScript`: MD4 of the same three names followed
by each node of the tree in order, its type and name and then its byte size,
type flags, version and `meta & 0x4000` as four-byte little-endian integers
(read from the 6000.0.71f1 Editor; `player-file script-trees` checks it on a
file: 81808 script objects in the worker's 41 files of Bouncemasters, all
equal).

Main of Bouncemasters, against the 272 script trees of the worker's file
(the host Player's managed directory and the game's Player compile, with
the Editor's compile of Addressables): 246 equal. The other 26 derive from Odin's `SerializedMonoBehaviour`, whose
`serializationData` the worker leaves out although the Editor writes it into
the scenes and prefabs (`Main.unity`, the managers' prefabs): the worker's
files lose what Odin serialized there. The worker lays scripts out by the
Editor's compile, with fields under `UNITY_EDITOR`
(`AssetReference.m_SubObjectGUID`); the layout is that of the assemblies
given, which for a session are those its Player runs.

`player-file closure ... --layouts <layouts.json>` takes the trees, samples
and `MonoScript`s of all scripts from the layouts instead of the worker's
file (which still gives the classes of the engine). Main:
20574 objects equal against 20629 with the worker's script trees. All 57
fewer are Odin's: the 26 managers that hold `serializationData` and the
`MonoScript`s of 31 Odin classes, whose hashes follow the trees; two objects
whose fields the worker's samples gave wrongly are now equal.

The templates of a release ship with the tool
(`playerbake/engine/<release>/script-templates.bin`, a script type table
entry with the header, one field per value type named after it, and the
registry); `closure` and `script-trees ... --unity-version V` take them, and
`script-trees` reports any that differ from a file's.
`player-file script-templates <output> <file>...` makes them from files the
engine wrote: the header and the values scripts hold from script trees in
the Player's format, a value no script holds from the engine classes that
hold it (`Quaternion` from `Transform`, `Matrix4x4`, `Bounds`, `Gradient`,
`Hash128`, `SphericalHarmonicsL2`), without what such a field passes down
(hidden `0x1`, `0x10`, not animated `0x800000`, aligned after on the value
itself) and in the shape most of them give it. Where both give a value
(`Vector2` to `Vector4`, `Color`, `Color32`, `Rect`, `LayerMask`), the two
agree. 6000.0.71f1's come from the worker's 41 files of Bouncemasters, the
Editor's `unity_builtin_extra` and its `unity default resources`, which
holds the only `GUIStyle` (`GUISkin`), in the Editor's trees: that its
layout in a Player is the same is not checked. No file of the release has
`RectInt`, `BoundsInt`, `Vector3Int` or `PropertyName`, so a script holding
one is not laid out (Main: `VectorImage`).

## Order of work

Each step compares its output with what the worker wrote for the same input
(the imports in `native-store` are a corpus) and keeps the worker as the
fallback until the comparison holds.

1. Comparison harness: read both outputs object by object through the type
   tree.
2. Textures and sprites from PNG/JPG: Texture2D, Sprite, mipmaps, BC/DXT
   through `ispc_texcomp`/`bc7e`.
3. Authored native assets: materials, ScriptableObjects and `MonoBehaviour`s,
   with script type trees computed from the compiled assemblies.
4. Prefabs and scenes in the Player format, replacing
   `WriteSceneSerializedFile` for scenes whose assets steps 2–3 cover.
5. Audio as PCM in FSB5; fonts.
6. Models through `ufbx`; animation.
7. Shaders: ShaderLab parsing plus `UnityShaderCompiler`; Shader Graph and
   pipeline stripping stay open.
8. A host without the Editor ([editor-free host prototype](HOSTLESS_PLAYER.md)).

Steps 2–4 cover most of a 2D game like Bouncemasters. Full parity with the
worker, including the Editor C# that packages run, has no estimate.

### Shaders without the Editor

The Editor imports a `.shader` in two halves. `UnityShaderCompiler`, the
program beside it, preprocesses the source (`c:preprocess`): it cuts each
program out into a snippet (`snip:` with its ID, platform mask, hardware
tier mask, types mask, language, a flag, the hash of the included files as
four 32-bit words, the start line and two flags, then the code), lists its
keyword sets per kind and program type (`keywordsUserGlobal: <id> <type>
<count>` and the sets), ends them (`keywordsEnd:`, three texts, the base
requirements and each keyword with its requirements), names the included
files and the paths tried (`includes:`, `filepaths:`), its messages (`err:`,
-1 for none) and answers `shader: <ok> ...` with the ShaderLab whose
programs became `GpuProgramID <id>`. The request is the source, its
directory and file name, whether only surface shaders are wanted, the build
platform (24), the graphics API mask (0x48000, OpenGL Core and Vulkan) and
two keyword lists. The Editor parses the rest of the ShaderLab itself and
generates the programs of fixed-function passes (`SetTexture`, ...).

`player-file shader-preprocess --editor E --project P <asset>[=<file>]...`
makes the request. 6000.0's compiler reports 28 graphics APIs (6000.6: 29),
takes no environment in `initializeCompiler` and keeps its includes in
`Data/CGIncludes`. The 77 shaders of Bouncemasters against the replies a
6000.0 worker got (its conversation read off with `strace`): all equal.
`initializeCompiler` also takes the redirected folders, a count and then
each logical folder with its directory (`Packages/<name>` and where the
package lies, as `AssetDatabase::GetRedirectedAssetFolders` gives the
Editor); the tool sends the packages of the project, so a shader that
includes a file of its package compiles. An include is looked for beside
the including file and then beside the shader compiled; the tool's include
closures look in the same places. A source may start with a byte order
mark, which the tool's scans of shader names and links skip.

The Editor's half is the tool's (`playerbake::shaderlab`): the ShaderLab
is parsed into commands, each block knowing its keywords; a subshader's or
category's commands apply in order, a pass taking the state and name set
before it; tag names and values take the spelling of the names the engine
registers (`QUEUE`, `LIGHTMODE`, `SHADOWCASTER`, `RenderType`, `Vertex`,
`true`, ...), a pass's tags merge into its state's, in order of name. The
shader names the four stereo keywords and then each program's in order of
first use; a program lists the indices it reads, in order. `Blend` without
a target sets the first; `Int` properties are floats, `Integer` integers.
The compile information lays each snippet out as the compiler reported it
(the number before the language is the pragmas' compile flags). The import
compiles the shader once afterwards (`m_CompileSmokeTestAfterImport`)
unless it failed or takes passes of other shaders; the default textures of
the `.meta` that name a texture property are kept, in order of name.

A pass without a program (fixed function: `SetTexture`, `Lighting`,
`Material`, `ColorMaterial`, `Color`, `AlphaTest`, `Fog`, ...) gets the
program the Editor generates for it (`playerbake::shaderlab::fixed`, the
text read off the Editor's imports of 24 probes of each kind): its
combiners noted as `//! TexEnvN: <color> <alpha> [texture] [constant]`,
four bytes each (the scale, the first source, the operation and the second
source; sources previous 0, texture 1, constant 2, primary 3, +4 for alpha,
+8 for `one -`; replace 0, multiply 1, add 2, subtract 3, lerp 0x80 with
its source; `dot3`, which 6000.0 no longer supports, adds and warns). The
snippet's ID is the low 16 bits of XXH32 of its code with the seed
`0x8f37154b`, with the count of the shader's snippets before it above
them; its pass's fog and lighting state move into the program, and a lit
pass becomes a vertex-lit one (`LIGHTMODE` `Vertex`).

`player-file shader-import --editor E --project P --reference DIR
<asset>[=<file>]...` builds the Editor's `Shader` object and compares it
with `DIR/<guid>-0000.bin`. Bouncemasters' 77 shaders and the 24
fixed-function probes against the 6000.0 worker's artifacts: all equal to
the byte. Not covered by a probe yet: texture matrices (refused), alpha
scales, `SetTexture` with `Matrix`, `GrabPass`, `PackageRequirements` and
`Dependency`.

The Player half (`playerbake::shader`) bakes 6000.0's `Shader` with that
release's tree, compile request and reflection (`ShaderRelease`). What a
shader takes from others differs from 6000.6. Its dependencies are those of
its used passes and its fallback, and what each of those lists in turn. Its
keywords are its own, then those of the snippets it holds by then, in the
order of their IDs: the snippet of each used pass and every snippet its
fallback holds, the fallback's fallback's among them
(`ShaderCompilationInfo::AddSnippetsFromPass` and `AddSnippetsFromShader`);
a pass's `gpuProgramID` names its snippet in the Player object as well.
6000.0 lists no fallback it finds no shader of.

`player-file shader-bake --editor E --project P --source S --reference DIR
<asset>[=<file>]...` imports and bakes each source with the tool alone and
compares it with the worker's Player object (`DIR/<name>.objects`, built by
`player-objects`, the materials that use it in `DIR/<name>.users`).
Bouncemasters' 77 shaders: all equal to the byte. The comparison shows no
dependency's file (the worker's objects refer to themselves), so the order
of a shader's dependencies is the tool's: each found shader followed by
those it lists.

In a session, `UME_OWN_IMPORTER` (`1` or `only`) has the tool import a
6000.0 shader itself before baking it, so no worker starts for shaders; with
`only` a release whose Editor `Shader` tree the tool lacks is refused.
Shaders of a scriptable render pipeline, shader graphs and compute shaders
are still built by the Editor's code in a worker.
