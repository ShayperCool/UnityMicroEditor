# Linking Play mode code into a Player

A project prepared with `--play-mode` compiles its Player's code as the Editor compiles it for Play mode: with `UNITY_EDITOR` and the Editor's other symbols, against the Editor's builds of the engine modules and the Editor's own modules ([docs/UNITY_RELEASES.md](../../docs/UNITY_RELEASES.md#play-mode-code)). This small C# executable makes such code run in a Player. It uses the Cecil that ships with every Unity installation's IL2CPP (`Data/il2cpp/build/deploy`), is built with the .NET 8 SDK the compiler uses, and is kept in the project's compiler store by the hash of its sources, that Cecil and the SDK (`rust/crates/compiler/src/compiler/playmode.rs`). It starts neither the Editor nor a Player.

`link` takes one compiled assembly; the compiler runs it after Roslyn (and after the Netcode post-processor) for every game assembly, every package assembly it builds and every plugin the Player takes, and the link is part of the assembly's cache key:

- A member of the engine that the Player's engine modules (`PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed`) lack, and `Application.isEditor` and `Application.platform`, are called through a static stand-in of the same signature in `UnityMicroEditor.PlayModeEngine` (`UnityMicroEditor.PlayMode.Engine.<Type>`); an instance member takes the instance first. A field of that kind becomes a static field of the stand-in. A type the Player's engine lacks moves to that assembly under its flattened name (`UnityEngine.CanvasRenderer_OnRequestRebuild`).
- When the host has a build of the same assembly (kept in `<Player>_Data/UnityMicroEditorLayout`), the serialized fields of each type Unity serializes (an engine object's script or a `[Serializable]` type: public or `[SerializeField]`/`[SerializeReference]` instance fields that are neither constant nor read-only) are made those of the host's build: a field only this build declares is marked `[NonSerialized]`, a field only the host's declares is added at its place. The Player reads what the host build serialized without type trees, in that layout.

`stand-ins` takes the Player's managed directory and writes every assembly its code names that the Player lacks and the Editor has (`UnityEditor.CoreModule`, platform extensions), and `UnityMicroEditor.PlayModeEngine`, with exactly the types and members the code names. A type takes its shape from the Editor's definition (enums, delegates, value types and base types as the Editor has them); a type the Editor forwards from a facade (`UnityEditor`) is forwarded the same way. A method answers an empty array or text, or its type's default value, unless the runtime bridge has an answer: a public static method of the same name and parameters, the instance first, in `UnityMicroEditor.PlayModeEditor.<Namespace_Type>` of `UnityMicroEditor.dll` (`Runtime/PlayModeEditor.cs`). Every stand-in holds the type `UnityMicroEditor.PlayMode.StandIn`, by which a later run knows it and writes it again.

The report on standard output lists the stand-ins, the engine members and types redirected, the fields aligned, what could not be linked (a delegate to or `new` of an engine member only the Editor has), and the members no stand-in could give. The compiler prints the last two.

Request (`play-mode-link.json`, `play-mode-stand-ins.json` in the compile's folder):

```json
{"mode": "link", "engine": "<Player engine modules>", "managed": "<Player>_Data/Managed",
 "input": "<compiled dll>", "output": "<linked dll>", "layout": "<host's build or empty>",
 "editorDirectories": ["<Editor>/Data/Managed/UnityEngine", "..."]}
```
