#!/usr/bin/env bash
# run.sh [--host <embedding host>] [--cycles N] [--stress N] [--no-sweep]            (see README.md)
# Builds the applier, a "game" and hand-written patch assemblies with the SDK compiler of the Unity
# installation and runs the harness on Unity's Mono. Exit code 0 when every run passed.
#   default      the standalone mono of the installation: without inlining, with it, and with MONO_INLINELIMIT=0
#   --host P     also on the Player's own libmonobdwgc-2.0.so through the embedding host P
#                (Temp/hotreload/mono-probe/out/host, see README.md), with the Player's JIT options and with
#                the options the runner sets through MONO_ARGUMENTS
#   --cycles N   length of every stress loop (default 200 cycles of three requests)
#   --stress N   how often the three stress runs of their own are repeated per runtime (default 1)
#   --no-sweep   leave out the 64 processes of the code area sweep
# Output goes to $HOTRELOAD_TEST_OUT (default: bin/ beside this script).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${HOTRELOAD_TEST_OUT:-$here/bin}"
editor="${UNITY_EDITOR_DIR:-$HOME/Unity/Hub/Editor/6000.6.3f1/Editor}"
data="$editor/Data"
mbe="$data/MonoBleedingEdge"
aot="$mbe/lib/mono/unityaot-linux"      # what sessions compile against (rust/src/compiler/host.rs)
jit="$mbe/lib/mono/unityjit-linux"      # what a Mono Player runs on
player="$data/PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono"
mono="$mbe/bin-linux64/mono"
dotnet="$data/DotNetSdk/dotnet"
compiler="$(ls "$data"/DotNetSdk/sdk/*/Roslyn/bincore/csc.dll | sort | tail -1)"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
host=""; cycles=200; stress=1; sweep=1
while [ $# -gt 0 ]; do
    case "$1" in
        --host) host="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"; shift 2 ;;
        --cycles) cycles="$2"; shift 2 ;;
        --stress) stress="$2"; shift 2 ;;
        --no-sweep) sweep=0; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

csc() { "$dotnet" "$compiler" -nologo -nostdlib+ -langversion:9 "-reference:$aot/mscorlib.dll" "-reference:$aot/System.dll" "-reference:$aot/System.Core.dll" "$@"; }
session=(-target:library -warnaserror+ -nowarn:0649,0618 -unsafe+)     # the bridge's session flags
game=(-target:library -deterministic+ -debug:portable -optimize-)      # the flags of game assemblies (rust/src/compiler/compile.rs)

rm -rf "$out/applier" "$out/faulty" "$out/patches" "$out/probe" "$out/ref" "$out/gen" "$out"/run-*
mkdir -p "$out/applier" "$out/faulty" "$out/patches" "$out/probe" "$out/ref" "$out/gen"

# More methods than anyone writes by hand, for the game and for the patch of them:
#   Fill   16 bytes of compiled code each: the harness moves the JIT's code area forward with them
#   Many   never called before they are patched; their code and that of their replacements fills several 64 KiB chunks
#   Edge   bodies that start with the one-byte push rbp (a try block)
#   Tiny   ten bytes of code that start with push rbp (an argument on the stack); compiled in a row they lie 16 bytes apart
{
    echo "namespace Game {"
    echo "public static class Fill {"
    for ((i = 0; i < 4400; i++)); do echo "public static int F$i() { return $i; }"; done
    echo "}"
    echo "public static class Many {"
    for ((i = 0; i < 600; i++)); do echo "public static string M$i(int a, string b) { return b + a + \"x\" + (a * 3 + $i); }"; done
    echo "}"
    echo "public static class Edge {"
    for ((i = 0; i < 24; i++)); do echo "public static long E$i(long value) { try { return value * 5; } finally { Hot.Leave(); } }"; done
    echo "}"
    echo "public static class Tiny {"
    echo "public static int Hits;"
    for ((i = 0; i < 8; i++)); do echo "public static void T$i(int a, int b, int c, int d, int e, int f, int g) { }"; done
    echo "}"
    echo "}"
} > "$out/gen/Bulk.cs"
{
    echo "namespace Patch.Bulk {"
    echo "public static class Many {"
    for ((i = 0; i < 600; i++)); do echo "public static string M$i(int a, string b) { return b + a + \"y\" + (a * 3 + $i); }"; done
    echo "}"
    echo "public static class Edge {"
    for ((i = 0; i < 24; i++)); do echo "public static long E$i(long value) { try { return value * 5 + 1; } finally { Game.Hot.Leave(); } }"; done
    echo "}"
    echo "public static class Tiny {"
    for ((i = 0; i < 8; i++)); do echo "public static void T$i(int a, int b, int c, int d, int e, int f, int g) { Game.Tiny.Hits |= 1 << $i; }"; done
    echo "}"
    echo "}"
} > "$out/gen/PatchBulk.cs"

# Stand-ins the applier compiles against outside a Player: the engine, and the bridge made of the real attribute file.
csc "${session[@]}" "-out:$out/UnityEngine.CoreModule.dll" "$here/Stubs/UnityEngine.cs"
csc "${session[@]}" "-reference:$out/UnityEngine.CoreModule.dll" "-out:$out/UnityMicroEditor.dll" "$root/Packages/com.unitymicroeditor.runtime/Runtime/HotReloadAttributes.cs"
engine=("-reference:$out/UnityEngine.CoreModule.dll" "-reference:$out/UnityMicroEditor.dll")

# The applier, with the session flags. It is kept off the search path: the bridge code loads it from bytes.
csc "${session[@]}" "${engine[@]}" "-out:$out/applier/UnityMicroEditor.HotReload.dll" "$root"/tools/HotReloadRuntime/*.cs

# The loaded game, and the same source with every member public as the reference of the hand-written patches.
csc "${game[@]}" -unsafe- "${engine[@]}" "-out:$out/Game.dll" "$here/Game/Game.cs" "$out/gen/Bulk.cs"
csc "${game[@]}" -unsafe- "${engine[@]}" "-out:$out/Game.Core.dll" "$here/Game/Core.cs"
sed -E 's/\b(private|protected|internal)\b/public/g' "$here/Game/Game.cs" > "$out/ref/Game.cs"
csc "${game[@]}" -unsafe- -define:REFERENCE "${engine[@]}" "-out:$out/ref/Game.dll" "$out/ref/Game.cs" "$out/gen/Bulk.cs"

# Patches. -unsafe+ adds the SkipVerification declaration; the one without it must be refused.
patch() { local name="$1" unsafe="$2"; shift 2; csc "${game[@]}" "$unsafe" "${engine[@]}" "-reference:$out/ref/Game.dll" "-reference:$out/Game.Core.dll" "-out:$out/patches/$name.dll" "$@"; }
patch Game.HotReload.1 -unsafe+ "$here/Patches/Patch1.cs"
patch Game.HotReload.2 -unsafe+ "$here/Patches/Patch2.cs"
patch Game.Core.HotReload.1 -unsafe+ "$here/Patches/PatchCore.cs"
patch Game.HotReload.Shapes -unsafe+ "$here/Patches/PatchShapes.cs"
patch Game.HotReload.Stale -unsafe+ "$here/Patches/PatchStale.cs"
patch Game.HotReload.Broken -unsafe+ "$here/Patches/PatchBroken.cs"
patch Game.HotReload.Denied -unsafe- "$here/Patches/PatchDenied.cs"
patch Game.HotReload.Islands -unsafe+ "$here/Patches/PatchIslands.cs"
patch Game.HotReload.Lazy -unsafe+ "$here/Patches/PatchLazy.cs"
patch Game.HotReload.Eager -unsafe+ "$here/Patches/PatchEager.cs"
patch Game.HotReload.Local -unsafe+ "$here/Patches/PatchLocal.cs"
patch Game.HotReload.Menu1 -unsafe+ "$here/Patches/PatchMenu1.cs"
patch Game.HotReload.Menu2 -unsafe+ "$here/Patches/PatchMenu2.cs"
patch Game.HotReload.Bulk -unsafe+ "$out/gen/PatchBulk.cs"
csc -target:library "-reference:$out/applier/UnityMicroEditor.HotReload.dll" "-reference:$out/Game.dll" "-out:$out/probe/FieldStore.Probe.dll" "$here/Patches/FieldStoreProbe.cs"

csc -target:library "-out:$out/faulty/UnityMicroEditor.HotReload.dll" "$here/Harness/FaultyApplier.cs"

# The harness contains the bridge's own request handling: CodeReload.cs with the JSON reader it uses, and the argument parser.
bridge="$root/Packages/com.unitymicroeditor.runtime/Runtime"
csc -target:exe "${engine[@]}" "-reference:$out/Game.dll" "-reference:$out/Game.Core.dll" "-out:$out/Harness.exe" "$here/Harness/Program.cs" "$here/Harness/Bridge.cs" \
    "$bridge/CodeReload.cs" "$bridge/CliPlayArguments.cs" "$bridge/Raw/RawJson.cs" "$root/tools/HotReloadRuntime/Json.cs"
# Metadata tokens of the replacements, read in a process of their own: the test process must not load a patch itself.
MONO_PATH="$jit" "$mono" "$out/Harness.exe" tokens "$out"/patches/*.dll
echo "build ok: $out"
cd "$out"    # a crashing run leaves its mono_crash files here, not in the sources

failed=0
standalone=(env MONO_PATH="$jit" "$mono" -O=-float32,-inline)
embedded=("$host" "$mbe/lib" "$player/Data/MonoBleedingEdge/etc" "$jit:$out")
patchable=(env HOST_MONO_ARGUMENTS=-O=-float32,-inline "${embedded[@]}")
run() { # run <label> <command...>: one process of the harness
    local label="$1"; shift
    echo; echo "=== $label"
    if "$@"; then :; else echo "=== $label: FAILED (exit $?)"; failed=1; fi
}
# The first request of a process, after the "game" compiled more and more code: 64 processes that
# fill one 64 KiB chunk of the JIT's code area in steps of 1 KiB. Only failures are printed.
sweep() { # sweep <label> <command...>
    local label="$1" bad=0 fill; shift
    echo; echo "=== $label"
    for ((fill = 0; fill < 4096; fill += 64)); do
        if ! "$@" "$out/Harness.exe" chunk "$out" "$fill" > "$out/sweep.log" 2>&1; then bad=$((bad + 1)); echo "--- $fill methods of 16 bytes:"; grep -vE '^  PASS' "$out/sweep.log" || true; fi
    done
    if [ "$bad" = 0 ]; then echo "RESULT ok: the first request was applied at each of 64 fill levels"; else echo "=== $label: FAILED at $bad of 64 fill levels"; failed=1; fi
}
runs() { # runs <runtime label> <command...>: everything that needs a JIT without inlining
    local label="$1" round; shift
    run "$label: the suite" "$@" "$out/Harness.exe" run "$out" off "$cycles"
    run "$label: a check of the JIT that cannot run" "$@" "$out/Harness.exe" undecided "$out"
    for ((round = 1; round <= stress; round++)); do
        run "$label: first redirects of a push rbp body under three threads ($round of $stress)" "$@" "$out/Harness.exe" unwind "$out" "$cycles"
        run "$label: first redirects through the second instruction under three threads ($round of $stress)" "$@" "$out/Harness.exe" relay "$out" "$cycles"
        run "$label: the gated whole jump under three threads ($round of $stress)" "$@" "$out/Harness.exe" fallback "$out" "$cycles"
        run "$label: the gated island of a sub rsp, imm8 body under three threads ($round of $stress)" "$@" "$out/Harness.exe" window "$out" "$cycles"
    done
    if [ "$sweep" = 1 ]; then sweep "$label: the first request at every fill level of the code area" "$@"; fi
}

runs "standalone mono, -O=-float32,-inline" "${standalone[@]}"
run "standalone mono, default options (the JIT inlines)" env MONO_PATH="$jit" "$mono" "$out/Harness.exe" run "$out" on "$cycles"
run "standalone mono, MONO_INLINELIMIT=0 (forced inlining remains)" env MONO_PATH="$jit" MONO_INLINELIMIT=0 "$mono" "$out/Harness.exe" run "$out" on "$cycles"
for kind in entry throw; do
    run "standalone mono, the bridge with a faulty applier: $kind" env MONO_PATH="$jit" "$mono" "$out/Harness.exe" faulty "$out" "$kind"
done
if [ -n "$host" ]; then
    runs "Player runtime, MONO_ARGUMENTS=-O=-float32,-inline" "${patchable[@]}"
    run "Player runtime, the Player's default options (the JIT inlines)" "${embedded[@]}" "$out/Harness.exe" run "$out" on "$cycles"
fi
echo
if [ "$failed" = 0 ]; then echo "all runs passed"; else echo "FAILED"; fi
exit "$failed"
