# UI and font imports

UXML, USS, TSS (themes) and TTF/OTF assets go through the scene like every
other asset: the scene worker imports them with UI Toolkit's own importers
(`UIElementsViewImporter`, `StyleSheetImporter`, `ThemeStyleSheetImporter`)
and the font importer, and the Editor's build code writes their objects into
the file beside the scene ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)).
The Player loads them with the scene. There is no separate UI route any more:
the warm extraction from the Editor's import cache, the projection of
artifacts for the Player and the runtime restoration of style sheet references
were parts of the former scene loader and are gone with it.

What the worker needs for these importers is given to every import context the
coordinator makes, for the scene worker and for `project-import` alike.

## The register of the engine's scripts

These importers add objects of the engine's script classes (`VisualTreeAsset`,
`StyleSheet`). `AddObjectToAsset` takes such an object only when its
`MonoScript` is in the register of scripts. The main Editor keeps that
register in the source database as `mem://Library/MonoManager.asset` under
`SerializedSingletons`, and a worker reads it from there. Without it, every
script-backed object is refused with "Failed to add object of type
`VisualTreeAsset`. Check that the definition is in a file of the same name",
the engine's own classes included.

The coordinator therefore writes the register into every source database it
makes (`rust/src/native/scripted.rs`, `script_register`). It lists the
`MonoScript` objects of the installed Editor's
`Data/Resources/unity default resources` (30 in 6000.6.3f1, for example
`VisualTreeAsset` 11995 and `StyleSheet` 11998) as references into
`Library/unity default resources`. The Editor's own register also lists the
project's scripts. A worker finds those through the source registry, so they
are left out. With the register:

- `RuntimeDebugWindow.uxml` of SRP Core imports with the same import result
  as with the register a real Editor wrote.
- An `.inputactions` import keeps the same result ID as without it.

## Recorded imports

UI Toolkit's importers load the style sheets a source imports, which the worker
finds only among the imports recorded before in the context's `ArtifactDB`. A
context that holds a `.uss`, `.tss` or `.uxml` source, and every context of a
build, records each import there ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#what-the-worker-needs-to-find-imported-assets)).
Without that record, a theme's `@import` of a USS stayed empty.

## Custom elements

A document's custom elements (`[UxmlElement]` classes, written with their
namespace's prefix such as `<d:SegmentBar>`) are created in a Player from the
serialized data the importer writes into the `VisualTreeAsset`. The importer
writes it only for a class it finds among the loaded assemblies; without the
class it logs "missing a UxmlElementAttribute", and the Player creates nothing
there. `project-import` therefore loads, from the Editor compilation, each
assembly whose metadata holds the name and the namespace of such an element,
and what that assembly refers to (`uxml_elements` in
`rust/src/native/scripted.rs`). The scene worker loads the assemblies of the
Editor compilation it selects for the scene ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)).

## Limits

- UI assets are imported and written as the engine does it; no comparison of a
  UI document or style sheet in a worker-written scene with an Editor build was
  made.
- A `PanelSettings` names UI Toolkit's shaders and ICU data in the engine's
  `unity_builtin_extra`. The worker-written file refers to the Player's own
  copy of that file, which holds what the host's build kept. No public API
  reaches a Player's ICU data.
- Whether the scene worker's choice of assemblies always includes the assembly
  of a custom element that only a `.uxml` names was not verified.
