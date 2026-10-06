# Verified Dungeon package profile

These snapshots record Dungeon's requested packages and resolved direct/transitive versions at the Unity Micro Editor extraction checkpoint. They include the local runtime package introduced by the extraction.

`manifest.snapshot.json` paths are relative to Dungeon's `Packages` directory. The snapshots are provenance records, not manifests to copy unchanged into another project. VContainer's resolved Git hash is recorded in `packages-lock.snapshot.json`.

The tested host used the same Unity `6000.6.3f1` installation, Linux Mono, URP `17.6.0`, Input System `1.20.0`, and Netcode for GameObjects `2.13.3`. The multiplayer proof also depends on Dungeon's own relay and game code; neither is a dependency of the minimal host.
