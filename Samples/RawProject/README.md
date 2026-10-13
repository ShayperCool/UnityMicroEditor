# Raw sample project

A small authored project used as a fixture. `Assets` holds a text scene, a prefab, a material, a ScriptableObject, a PNG and two C# classes. It has no `Library`, imported assets, Player or bundles.

The scene shows a rotating cube. `RawIterationProbe` checks private references and serialized bool/enum/array values before `Awake`, the original parent, and the active flags of linked roots. In `Start` it logs `[unity-raw-proof]` with data from the loaded assets and the code revision.

`Integrations/EditorParity/start-reference.sh` copies this project to create the isolated Editor reference. To exercise iteration by hand, change `CodeRevision` in the script, `revision` in `Tuning.asset`, the colour of `Checker.mat`, or a prefab override in `RawSample.unity`. A watched session patches a new `CodeRevision` into `Start` of the running Player, which has run by then: the proof line shows it after the next scene reload or restart. An edit of `Update` shows at once.

The sample proves nothing about URP, FBX or animation support.
