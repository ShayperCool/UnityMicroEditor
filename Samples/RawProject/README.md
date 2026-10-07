# Raw sample project

A small authored project used as a fixture. `Assets` holds a text scene, a prefab, a material, a ScriptableObject, a PNG and two C# classes. It has no `Library`, imported assets, Player or bundles.

The scene shows a rotating cube. `RawIterationProbe` checks private references and serialized bool/enum/array values before `Awake`, the original parent, and the active flags of linked roots. In `Start` it logs `[unity-raw-proof]` with data from the loaded assets and the code revision.

`Integrations/EditorParity/start-reference.sh` copies this project to create the isolated Editor reference. To exercise iteration by hand, change `CodeRevision` in the script, `revision` in `Tuning.asset`, the colour of `Checker.mat`, or a prefab override in `RawSample.unity`.

This is a separate check scene. It does not stand in for Dungeon's SampleScene and proves nothing about URP, FBX or animation support.
