# Unity Micro Editor runtime

The shared C# package implements source scene loading, lifecycle barriers, native content delivery, scene patches, and runtime reports. It has no compile-time dependency on Dungeon gameplay assemblies.

Both Dungeon and the minimal host reference this directory as a local Unity package. It requires SRP Core `17.6.0` and Collections `6.6.0`. Script `.meta` files, assembly names, and namespaces are preserved so existing component references continue to resolve. The assembly remains `Dungeon.CLIPlay` for compatibility.

Unity `6000.6.3f1` is the verified engine version. Host engine modules and game-specific package dependencies belong to the host profile. `Editor/` contains legacy build and benchmark utilities; `Tests/` contains optional EditMode tests.
