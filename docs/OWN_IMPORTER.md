# Toward imports without a Unity licence

Status, 2026-10-23: research. Branch `feat/own-importer`.

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
