SDK tests of the hot reload patch builder (`tools/HotReloadPatcher`). They need the Unity installation only: its .NET SDK, Roslyn and standalone Mono. No Editor or Player is started.

```bash
"$UNITY_DATA/DotNetSdk/dotnet" run --disable-build-servers \
  --project tests/HotReloadPatcherTests/HotReloadPatcherTests.csproj
```

`--disable-build-servers` keeps the run from leaving a compiler server behind; the build of the patcher inside the run has the same switch.

`UNITY_DATA` defaults to `$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Data`. The run takes about 50 seconds, prints one summary line and exits non-zero on any failure. Everything it writes is below `bin/`.

What a run does:

- It builds the patcher the way the runner does: a clean copy of its sources, a `NuGet.Config` without sources, `dotnet build <csproj> --nologo -v:q -o payload -p:RoslynPath=... -p:NuGetAudit=false --disable-build-servers -nodeReuse:false`.
- It compiles the sources under `Fixtures/` with the SDK's `csc` and the pipeline's switches (`-langversion:9.0 -deterministic+ -debug:portable -optimize- -nostdlib+`) against Unity's reference class library. The fixtures are text; no binary is committed.
- Classification, through the real CLI. `Fixtures/Classify/Edits.cs` holds one `CASE_*` symbol per kind of edit; the loaded assembly is the file without a symbol. Every case states the exact redirects, added members, reasons and notes it expects, so an edit that leaks into an unrelated member fails. `Fixtures/Quiet` covers a comment-only edit and a new file that sorts first, `Fixtures/Cross` an addition used across two assemblies and a base class in an assembly outside the request.
- Netcode for GameObjects. `Fixtures/Classify/Net.cs` is an assembly as the Netcode post-processor leaves it, written by hand against the stand-in runtime `Fixtures/Stubs/Netcode.cs`, with one `CASE_NET_*` symbol per kind of edit: what is patched, what is no change (registrations in another order), and what is refused with each `netcode_*` code. `NetRaw.cs` is an assembly that uses Netcode and is not in that form, one `RAW_*` symbol per way of not being in it; `NetFirst.cs` an assembly that starts to use Netcode in a method body, with and without something to register. The stand-in itself, edited, is the Netcode runtime as a request assembly. The package's own post-processor is not run here: `hot_reload_netcode_sdk` of `rust/src/hotreload/tests.rs` does that, on a host built for a project with the package.
- Behaviour, on Unity's Mono. `Fixtures/Behaviour` is a two-assembly game in two versions (`Game.v*.cs` and `More.v*.cs` are one assembly) and a driver compiled against the loaded one. `Fixtures/Host/Host.cs` loads the loaded assemblies, applies the patch by overwriting the start of the compiled originals with a jump, and runs the driver; the output must equal the run on the new compilation. Lines about state from before the patch (objects, delegates and iterators that already existed) are expected to differ, and the test names both values.
- Every body of the new compilation is forced into a patch and compiled by Mono's JIT.
- Two patches in a row. `Fixtures/Generations` is one assembly in three versions. The second patch is built with the result of the first as `previousResult`; the test checks `patchMembers`, the notes about what the first patch leaves behind, and on Mono the facts those notes are about (a delegate and an iterator the first patch made, a handler that `-=` no longer finds) next to what must keep working (a reverted method, the value of an added field).
- The interface: refused requests, the handling of a defect of the tool (the diagnostic request member `fault`), and the code table. Every code of the table must be produced by some test. `fault: "owner"` hides the owner of a lambda of generic code, which is how the tests reach the refusal `unowned_generated_code`; the `CASE_DEAD_*` symbols are the edits that must not reach it (generated code that nothing names).
- A type reference scoped to its own module, which no compiler writes: `Fixtures/Classify/Scoped.cs` names a type of a second assembly and the test rewrites the resolution scope of that reference in the compiled image.

The host carries its own minimal redirect and `FieldStore`; the applier is tested with the Player.
