# Managed probe

This experiment loads installed Unity managed assemblies in a .NET 8 console
process. It checks `Vector3` addition, dot product, cross product, magnitude and
normalization against expected values. It also tries `new GameObject`, `Time.time`
and `PlayerLoop.GetCurrentPlayerLoop`.

Each sample runs in its own child process with a 15 second timeout. The parent
collects JSON, exceptions, process exit codes and bounded crash diagnostics.
Native failures are expected observations. A successful report means the
experiment completed; `nativeRuntimeAvailable` records whether all three sampled
native-backed APIs worked. No scene is loaded and `enginePlayModeStarted` is always
`false`.

The project has no NuGet dependencies and restores with package sources disabled.
`UnityManagedPath` must point to the directory containing
`UnityEngine.CoreModule.dll`. Use installed DLLs matching the project version.

```bash
UNITY_DATA=/path/to/Unity/Editor/Data
PROBE_OUT="$PWD/Temp/UnityPlay/ManagedProbe"
"$UNITY_DATA/DotNetSdk/dotnet" build \
  $UNITY_MICRO_EDITOR_ROOT/ManagedProbe/ManagedProbe.csproj -c Release \
  -p:UnityManagedPath="$UNITY_DATA/Managed/UnityEngine" \
  -p:BaseIntermediateOutputPath="$PROBE_OUT/obj/" \
  -o "$PROBE_OUT/bin" --nologo
"$UNITY_DATA/DotNetSdk/dotnet" "$PROBE_OUT/bin/ManagedProbe.dll"
```

For Mono Player managed assemblies, change `UnityManagedPath` to
`$UNITY_DATA/PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed`.
Use a separate output directory when comparing assembly sets.

Exit codes are `0` when the managed math sample succeeds, `1` when it fails,
`2` for invalid arguments, and `3` for failure of the parent experiment itself.
Read the individual probe statuses to assess native calls; their failure does
not make the parent exit with an error.

On the tested Linux Unity `6000.6.3f1` installation, both Editor and Mono Player
assemblies produced correct `Vector3` results under .NET `8.0.21`. All three
native-backed calls raised `System.Security.SecurityException` with
`ECall methods must be packaged into a system module.` This is a host refusal
to execute Unity's internal calls, rather than proof that the math library can
replace the Unity runtime.
