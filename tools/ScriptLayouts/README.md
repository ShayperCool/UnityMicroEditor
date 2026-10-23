# Script layouts

Prints what Unity's serializer stores of the script classes of compiled assemblies, for `playerbake` to lay out as Player type trees without the Editor ([docs/OWN_IMPORTER.md](../../docs/OWN_IMPORTER.md#script-type-trees)). It uses the Cecil of the installation's IL2CPP (`Data/il2cpp/build/deploy`) and is built with the .NET SDK the compiler uses into the project's compiler store, like the Play mode linker (`Compiler::built_tool`); `Compiler::script_layouts` runs it on a Player's assemblies.

```
ScriptLayouts <directory or assembly>...
```

Every `.dll` of a directory is read, and the directories resolve the references. The output is JSON: `scripts` by full name (`Namespace.Class`, `Class` without a namespace) with their assembly, namespace, name and serialized fields, the bases' first; `types` the `[Serializable]` classes and structs those hold, by their full name. A field has its name, its kind (`primitive`, `string`, `object`, `engine`, `array`, `composite`, `reference`), `hidden` for `[HideInInspector]`, `formerly` for its `[FormerlySerializedAs]` names and `default` for what the parameterless constructor assigns it, as far as its IL shows (constants, collection initializers, engine values, new serializable objects).
