# Repository guidance

- The active CLI and importer coordinator are Go. Keep Python out of their execution path. C# implements the Unity runtime bridge and SDK compiler helpers.
- Resolve tool sources from `UNITY_MICRO_EDITOR_ROOT` or the installation directory. Target game projects must be supplied explicitly. Do not infer a game project from this repository's parents.
- Unity `6000.6.3f1` is the verified profile. Engine upgrades require matching host, package locks, protocol checks, and runtime comparisons.
- Authored scenes, prefabs, materials, and `.meta` GUIDs are the source of truth. Do not silently overwrite them. Keep scene loading in our implementation and use the native importer for supported heavy content.
- Use public Unity/package APIs for runtime access. Do not reach into foreign private members. Validate imported identities and dependencies before publishing content.
- Keep caches, builds, reports, and SDK binaries out of Git. Run the relevant Go and SDK checks from `CONTRIBUTING.md` before committing.
- Document limitations explicitly when a path depends on prepared game content or retained shader output.
