# Tests of the hot reload applier and of the bridge's code reload

The harness runs the applier (`tools/HotReloadRuntime`) on Unity's Mono, because a redirect is specific to its JIT. Requests go the way they go in a Player: the harness contains the bridge's own `Runtime/CodeReload.cs`, which reads a real request file, loads the applier from bytes, binds `Entry.Handle` and writes the ack file. `Runtime/CliPlayArguments.cs` and `Runtime/Raw/RawJson.cs` are compiled in as well. No Unity process is started.

| Directory | Content |
| --- | --- |
| `Game/` | The "game": two assemblies the process loads normally, compiled with the flags of game assemblies |
| `Patches/` | Hand-written patch assemblies in the forms the patch builder emits; `FieldStoreProbe.cs` uses the field store as a patch does |
| `Stubs/` | A stand-in for `UnityEngine.CoreModule` (object query, log) |
| `Harness/` | The driver, a stand-in for the bridge's atomic file write, and an applier that fails on purpose |

`run.sh` adds generated sources to the game and one generated patch (`bin/gen/`): 4,400 methods of 16 bytes of compiled code, with which the harness moves the JIT's code area forward; 600 methods that are never called before they are patched; and small groups of bodies that start with `push rbp`.

Hand-written C# cannot name non-public members of another assembly. `run.sh` therefore compiles `Game/Game.cs` a second time with every access keyword replaced by `public` and compiles the patches against that reference. At run time the patches meet the real assembly, where the members are private, and reach them through the `SkipVerification` declaration that `-unsafe+` gives them.

## Run

```bash
tests/HotReloadRuntimeTests/run.sh                                                   # 74 processes, about 30 s
tests/HotReloadRuntimeTests/run.sh --host Temp/hotreload/mono-probe/out/host        # 144 processes, about 40 s
```

The script exits 0 when every run printed `RESULT ok`. It uses `UNITY_EDITOR_DIR` (default `$HOME/Unity/Hub/Editor/6000.6.3f1/Editor`) and writes to `HOTRELOAD_TEST_OUT` (default `bin/` beside the script, ignored by Git). It needs bash, sed and the Unity installation. `--cycles N` sets the length of every stress loop (default 200), `--stress N` repeats the three stress runs N times per runtime, `--no-sweep` leaves the 64 processes of the sweep out.

| Run | Runtime and options | Expects |
| --- | --- | --- |
| suite | `mono -O=-float32,-inline` | everything listed below |
| `undecided` | the same | a check of the JIT that cannot run is no answer and is not kept |
| `unwind`, `relay`, `fallback` | the same | the three ways of a first redirect, each under three threads that call the method |
| sweep: `chunk` × 64 | the same | the first request of a process is applied at every fill level of the JIT's code area |
| suite | `mono` | the canary finds inlining; a patch is refused with stage `inlining`, nothing is written |
| suite | `mono`, `MONO_INLINELIMIT=0` | the same: forced inlining remains |
| `faulty entry`, `faulty throw` | `mono` | the bridge answers for an applier without entry point, and for one that throws or returns no ack |
| the first four again | Player runtime, `MONO_ARGUMENTS=-O=-float32,-inline` | the same |
| suite | Player runtime, the Player's default options | the canary finds inlining |

The standalone `mono` (`Data/MonoBleedingEdge/bin-linux64/mono`) is an sgen build of the Mono line the Player embeds. The Player runtime is the Player's own `Data/PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono/Data/MonoBleedingEdge/x86_64/libmonobdwgc-2.0.so` (Boehm), started by the embedding host of the research scratch, which repeats the Player's JIT initialisation and passes `HOST_MONO_ARGUMENTS` where the Player passes `MONO_ARGUMENTS`. The host is not part of the repository; build it once:

```bash
P="$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Data/PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono/Data/MonoBleedingEdge/x86_64"
cd Temp/hotreload/mono-probe && mkdir -p out
ln -sf "$P/libmonobdwgc-2.0.so" out/libmonoboehm-2.0.so.1
gcc -O1 -g -o out/host host/host.c -include unistd.h -L"$P" -l:libmonobdwgc-2.0.so -Wl,-rpath,'$ORIGIN' -ldl -rdynamic
```

## The commands the script runs

`D` is `<Unity>/Data`, `B` is `$D/MonoBleedingEdge/lib/mono/unityaot-linux`, `O` the output directory, and

```bash
csc() { "$D/DotNetSdk/dotnet" "$D"/DotNetSdk/sdk/8.0.318/Roslyn/bincore/csc.dll -nologo -nostdlib+ -langversion:9 \
    -reference:$B/mscorlib.dll -reference:$B/System.dll -reference:$B/System.Core.dll "$@"; }
SESSION="-target:library -warnaserror+ -nowarn:0649,0618 -unsafe+"        # rust/src/compiler/host.rs
GAME="-target:library -deterministic+ -debug:portable -optimize-"         # rust/src/compiler/compile.rs
ENGINE="-reference:$O/UnityEngine.CoreModule.dll -reference:$O/UnityMicroEditor.dll"
R=Packages/com.unitymicroeditor.runtime/Runtime
```

```bash
# stand-ins: the engine, and the bridge assembly made of the real attribute file
csc $SESSION -out:$O/UnityEngine.CoreModule.dll Stubs/UnityEngine.cs
csc $SESSION -reference:$O/UnityEngine.CoreModule.dll -out:$O/UnityMicroEditor.dll $R/HotReloadAttributes.cs
# the applier, off the search path
csc $SESSION $ENGINE -out:$O/applier/UnityMicroEditor.HotReload.dll tools/HotReloadRuntime/*.cs
# the game with its generated part, and its all-public reference
csc $GAME -unsafe- $ENGINE -out:$O/Game.dll Game/Game.cs $O/gen/Bulk.cs
csc $GAME -unsafe- $ENGINE -out:$O/Game.Core.dll Game/Core.cs
sed -E 's/\b(private|protected|internal)\b/public/g' Game/Game.cs > $O/ref/Game.cs
csc $GAME -unsafe- -define:REFERENCE $ENGINE -out:$O/ref/Game.dll $O/ref/Game.cs $O/gen/Bulk.cs
# patches; Denied is the one without the SkipVerification declaration
for p in 1:Patch1 2:Patch2 Stale:PatchStale Broken:PatchBroken Shapes:PatchShapes Islands:PatchIslands Lazy:PatchLazy Eager:PatchEager Local:PatchLocal; do
  csc $GAME -unsafe+ $ENGINE -reference:$O/ref/Game.dll -reference:$O/Game.Core.dll -out:$O/patches/Game.HotReload.${p%%:*}.dll Patches/${p##*:}.cs
done
csc $GAME -unsafe+ $ENGINE -reference:$O/ref/Game.dll -reference:$O/Game.Core.dll -out:$O/patches/Game.HotReload.Bulk.dll $O/gen/PatchBulk.cs
csc $GAME -unsafe+ $ENGINE -reference:$O/ref/Game.dll -reference:$O/Game.Core.dll -out:$O/patches/Game.Core.HotReload.1.dll Patches/PatchCore.cs
csc $GAME -unsafe- $ENGINE -reference:$O/ref/Game.dll -reference:$O/Game.Core.dll -out:$O/patches/Game.HotReload.Denied.dll Patches/PatchDenied.cs
csc -target:library -reference:$O/applier/UnityMicroEditor.HotReload.dll -reference:$O/Game.dll -out:$O/probe/FieldStore.Probe.dll Patches/FieldStoreProbe.cs
csc -target:library -out:$O/faulty/UnityMicroEditor.HotReload.dll Harness/FaultyApplier.cs
# the harness, with the bridge's request handling compiled in
csc -target:exe $ENGINE -reference:$O/Game.dll -reference:$O/Game.Core.dll -out:$O/Harness.exe \
    Harness/Program.cs Harness/Bridge.cs $R/CodeReload.cs $R/CliPlayArguments.cs $R/Raw/RawJson.cs tools/HotReloadRuntime/Json.cs
```

```bash
M="$D/MonoBleedingEdge"; J="$M/lib/mono/unityjit-linux"
# metadata tokens of the replacements, in a process of their own
MONO_PATH=$J $M/bin-linux64/mono $O/Harness.exe tokens $O/patches/*.dll
# standalone
MONO_PATH=$J $M/bin-linux64/mono -O=-float32,-inline $O/Harness.exe run $O off 200
MONO_PATH=$J $M/bin-linux64/mono -O=-float32,-inline $O/Harness.exe undecided $O
MONO_PATH=$J $M/bin-linux64/mono -O=-float32,-inline $O/Harness.exe unwind $O 200        # also: relay, fallback
MONO_PATH=$J $M/bin-linux64/mono -O=-float32,-inline $O/Harness.exe chunk $O 1216       # 0, 64, ... 4032
MONO_PATH=$J $M/bin-linux64/mono $O/Harness.exe run $O on 200
MONO_PATH=$J MONO_INLINELIMIT=0 $M/bin-linux64/mono $O/Harness.exe run $O on 200
MONO_PATH=$J $M/bin-linux64/mono $O/Harness.exe faulty $O entry
MONO_PATH=$J $M/bin-linux64/mono $O/Harness.exe faulty $O throw
# Player runtime: host <mono lib dir> <mono etc dir> <assembly path> <program> ...
E="$D/PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono/Data/MonoBleedingEdge/etc"
HOST_MONO_ARGUMENTS=-O=-float32,-inline $HOST $M/lib $E $J:$O $O/Harness.exe run $O off 200
$HOST $M/lib $E $J:$O $O/Harness.exe run $O on 200
```

The last argument of `run` says whether this JIT inlines (`on`) and so what the canary must report.

## What the suite checks

- **The bridge**: the launch argument `--unity-cli-code-reload` in the bridge's parser; no request, an unreadable one, one above 64 KiB, another schema version, a relative or missing applier, a hash that is not the applier's, a request id taken once, `CodeReload.Generation` after every ack. The bridge keeps no clock of its own: two polls in a row both look at the request file (the bootstrap polls it and the scene request on one clock, code first). A request that names another applier than the loaded one is answered with `restartRequired` and one failure of stage `applier`.
- **Refusals before anything is resolved**: descriptor missing or of another hash, schema or generation; a stale generation; patch hash, MVID, assembly not loaded.
- **Apply**: 19 redirects in two game assemblies. Class receivers (non-public field and method, 32-byte result), struct receivers (12 and 32-byte results, a method that changes the struct, an empty and a layout-identical stand-in), constructor, property, static method, a removed message; reached by direct, virtual, base, interface, explicit-interface, boxed and constrained calls, by delegates (closed, open, static) and reflection bound before the patch, by objects created before it and methods first called after it. Every redirected entry is followed through its jump, island or relay to the compiled replacement.
- **Atomicity**: one bad token of either side, a token of another table, a method listed twice, an assembly without the `SkipVerification` declaration, a replacement the JIT refuses next to a good one, a method that is no replacement and does not compile, seven signature mismatches, generic and bodiless originals. After each of them the first eight bytes of all watched methods are what they were, the generation is unchanged and no callback ran.
- **Generations**: a second generation that retargets, adds a first redirect and drops twelve methods; an empty descriptor (the bytes of every method are the original ones again); the first patch assembly applied again after a restore.
- **Callbacks**: static and per live object, attributes of another namespace, the patch list with original, previous and new method, an unchanged body left out, local callbacks only for changed bodies, a throwing callback and two that cannot be called.
- **First redirects**, each way and its bookkeeping:
  - the first byte and an island, for bodies that start with `push rbp`: only that byte changes; a retarget stores the slot and leaves the body alone; a restore puts the byte back; the next first redirect finds its island;
  - an island across the end of a page, two more in its second page, and a later one in a page that is already mapped (the harness compiles filler methods until the JIT's next body lies where its island crosses a page end);
  - an island address the harness holds with a mapping of its own: refused with stage `write` and nothing written, for a `push rbp` body and for a `sub rsp, imm8` body that does not load a constant; applied once the address is free;
  - five bodies of ten bytes compiled 16 bytes apart: the island of one overlaps the pointer slot of the one before it (refused), the slot goes before the jump when the next island lies after it, and two of them in one request both find room;
  - the second instruction and a relay, for a body that first loads a constant: its first instruction is untouched, retarget, restore of all five bytes, the relay found again;
  - a whole jump behind the gate, for a body that starts with `sub rsp, imm32` and whose island address the harness holds; through an island once the address is free;
  - a body whose first bytes something else changed since the applier read them: refused.
- **The process map**: one request with 590 methods that were never compiled, whose code fills more than two 64 KiB chunks of the JIT's code area.
- **Static initializers**: `initializersRun` false for a request refused before it compiled, true for one refused after; a failing initializer of the patch itself only refuses the patch; a failing initializer of the game is answered with `restartRequired`, and the type is then unusable in the process.
- **Stress**: three threads call one method while it is redirected for the first time, retargeted and restored, 200 times each; every call returns the result of one of the three bodies.
- **Field store**: defaults, values per object, change through the reference, a changed field type, ids built at run time, static fields, a null owner, four threads on the same objects, 2000 dead owners collected with values that refer back to them, the cost of an access.

## The runs of their own

- `undecided`: the first request needs no check of the JIT and reports it as not checked. The harness then compiles the three probe methods of the applier between two runs of 300 filler methods, so that their pages hold nothing the JIT writes to again, and takes write access from those pages for one request. That request is refused with stage `write` and `The inlining check could not run`, the ack still reports the JIT as not checked, and the next request checks again and is applied.
- `unwind`, `relay`, `fallback`: the stress loop on `Hot.Guarded` (`push rbp`: island), `Hot.Flat` (loads a constant: relay) and `Hot.Large` (`sub rsp, imm32`, island page held by the harness: gate), each with a check that every first redirect went the expected way.
- `chunk N`: compiles N filler methods of 16 bytes and sends the first request of the process. The JIT maps a new chunk whenever one fills, also while the applier compiles its probes and the methods of the request; the sweep puts that boundary at 64 places. With the applier of stage 1, which read the process map before it compiled, 3 of the 64 levels fail on either runtime (standalone 1152 to 1280 methods, Player runtime 640 to 768).

Measured numbers of the stress runs are in `tools/HotReloadRuntime/README.md`.

## Not covered

- A real Player: engine messages, coroutines, engine threads, the `AgentTools` catalog with a patch loaded, `AgentControl.Status()`, and the bootstrap's poll (`CliPlayBootstrap.LoadRaw`), which needs the engine. Its order, one clock and code first, was read and compiled, not run.
- Patch assemblies produced by the patch builder. The patches here are hand-written in its forms, with types in a namespace named after the generation as the builder does.

`check-compile.sh <host>/<Name>_Data/Managed` compiles the whole bridge with the session command and without `-unsafe+`, and the applier against the same references plus that bridge.
