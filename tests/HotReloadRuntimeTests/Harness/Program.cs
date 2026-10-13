// Drives the applier as a Player does: through the bridge's own CodeReload.cs, which loads the
// assembly from bytes and binds Entry.Handle by name, and through real request, descriptor and ack
// files. Runs on Unity's Mono only; the redirect is specific to its JIT.
//
//   Harness.exe tokens <patch.dll>...          writes <patch.dll>.tokens (Type::Method=token) for each patch
//   Harness.exe run <output> off|on [cycles]   the suite; the second argument says whether the JIT inlines
//   Harness.exe faulty <output> entry|throw    the bridge with an assembly that has no entry point, or whose entry
//                                              throws or returns no ack
//   Harness.exe unwind <output> [cycles]       first redirects of a method that starts with the one-byte push rbp,
//                                              under threads calling it: through the island of its first byte
//   Harness.exe relay <output> [cycles]        the same for a method that first loads a constant: through its second
//                                              instruction
//   Harness.exe fallback <output> [cycles]     the same for a method with a first instruction of seven bytes whose
//                                              island cannot be mapped: the gated write of a whole jump
//   Harness.exe chunk <output> <methods>       the first request of a process whose JIT compiled that many methods
//                                              of 16 bytes before it
//   Harness.exe undecided <output>             a check of the JIT that cannot run is no answer and is not remembered
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Threading;
using Game;
using UnityMicroEditor;
using UnityMicroEditor.HotReload;   // Json.cs of the applier, compiled into the harness as well

internal static class Program
{
    private const BindingFlags Declared = BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static | BindingFlags.Instance | BindingFlags.DeclaredOnly;

    private sealed class Patch
    {
        internal string File;
        internal readonly Dictionary<string, long> Tokens = new Dictionary<string, long>(StringComparer.Ordinal);
    }

    // What the harness asks of libc to stand in for other code of the process: a foreign mapping
    // where an island would go, and a page of JIT code that is not writable.
    private const int CanRead = 1, CanWrite = 2, CanExecute = 4, PrivateAnonymousNoReplace = 0x02 | 0x20 | 0x100000;

    [DllImport("libc", SetLastError = true)]
    private static extern IntPtr mmap(IntPtr address, UIntPtr length, int protection, int flags, int file, IntPtr offset);

    [DllImport("libc", SetLastError = true)]
    private static extern int munmap(IntPtr address, UIntPtr length);

    [DllImport("libc", SetLastError = true)]
    private static extern int mprotect(IntPtr address, UIntPtr length, int protection);

    private static string output, run, applierPath, applierHash, requestPath;
    private static CodeReload bridge;
    private static int passed, failed, requests, fillers;
    private static MethodInfo[] fill;
    private static long generation, applied;
    private static Patch patch1, patch2, patchCore, patchDenied, patchStale, patchBroken, patchShapes, patchIslands, patchLazy, patchEager, patchLocal, patchBulk, patchMenu1, patchMenu2, patchHelpers;
    // Methods whose first eight bytes every check of "nothing was written" compares, and those bytes as the JIT compiled them.
    private static readonly List<MethodBase> watched = new List<MethodBase>();
    private static readonly List<long> compiled = new List<long>();
    private static long[] expected = new long[0];

    public static int Main(string[] arguments)
    {
        string mode = arguments.Length >= 2 ? arguments[0] : string.Empty;
        if (mode == "tokens") return Tokens(arguments);
        bool detailed = mode == "run" || mode == "faulty" || mode == "chunk";
        if (Array.IndexOf(new[] { "run", "faulty", "unwind", "relay", "fallback", "chunk", "undecided" }, mode) < 0 || (detailed && arguments.Length < 3))
        {
            Console.Error.WriteLine("usage: Harness.exe tokens <patch.dll>... | run <output> off|on [cycles] | faulty <output> entry|throw | unwind <output> [cycles]"
                + " | relay <output> [cycles] | fallback <output> [cycles] | chunk <output> <methods> | undecided <output>");
            return 2;
        }
        output = Path.GetFullPath(arguments[1]);
        string detail = detailed ? arguments[2] : string.Empty;
        int counted = mode == "run" ? 3 : 2;
        int cycles = mode != "faulty" && mode != "chunk" && arguments.Length > counted ? int.Parse(arguments[counted], CultureInfo.InvariantCulture) : 200;
        run = Path.Combine(output, "run-" + (mode == "run" ? "inlining" : mode) + (detailed ? "-" + detail : string.Empty) + "-" + Process.GetCurrentProcess().Id);
        Directory.CreateDirectory(run);
        Console.WriteLine("runtime " + Type.GetType("Mono.Runtime").GetMethod("GetDisplayName", BindingFlags.NonPublic | BindingFlags.Static).Invoke(null, null)
            + ", gc generations " + (GC.MaxGeneration + 1) + ", files in " + run);

        patch1 = Load("Game.HotReload.1"); patch2 = Load("Game.HotReload.2"); patchCore = Load("Game.Core.HotReload.1");
        patchDenied = Load("Game.HotReload.Denied"); patchStale = Load("Game.HotReload.Stale"); patchBroken = Load("Game.HotReload.Broken"); patchShapes = Load("Game.HotReload.Shapes");
        patchIslands = Load("Game.HotReload.Islands"); patchLazy = Load("Game.HotReload.Lazy"); patchEager = Load("Game.HotReload.Eager"); patchLocal = Load("Game.HotReload.Local");
        patchBulk = Load("Game.HotReload.Bulk"); patchMenu1 = Load("Game.HotReload.Menu1"); patchMenu2 = Load("Game.HotReload.Menu2");
        patchHelpers = Load("Game.HotReload.Helpers");
        // The bridge reads the applier from bytes on the first valid request. No file of its name is on the search path.
        applierPath = Path.Combine(output, "applier", "UnityMicroEditor.HotReload.dll");
        applierHash = Hash(applierPath);
        requestPath = Path.Combine(run, "code-reload-request.json");
        bridge = new CodeReload(requestPath);

        if (mode == "faulty") Faulty(detail);
        else if (mode == "unwind") Unwind(cycles);
        else if (mode == "relay") Relay(cycles);
        else if (mode == "fallback") Fallback(cycles);
        else if (mode == "chunk") Chunk(int.Parse(detail, CultureInfo.InvariantCulture));
        else if (mode == "undecided") Undecided();
        else
        {
            Suite(detail == "on", cycles);
            FieldStore();
        }
        Console.WriteLine("RESULT " + (failed == 0 ? "ok" : "FAILED") + " passed=" + passed + " failed=" + failed);
        return failed == 0 ? 0 : 1;
    }


    // ------------------------------------------------------------------ the suite

    private static void Suite(bool inlining, int cycles)
    {
        MethodBase health = Method(typeof(Player), "Health"), level = Method(typeof(Player), "get_Level"), describe = Method(typeof(Player), "Describe"),
            twice = Method(typeof(Player), "Twice"), construct = typeof(Player).GetConstructor(Type.EmptyTypes), stats = Method(typeof(Player), "Stats"),
            scaled = Method(typeof(Vec3), "Scaled"), spread = Method(typeof(Vec3), "Spread"), shift = Method(typeof(Vec3), "Shift"), sum = Method(typeof(Vec3), "Sum"),
            terse = Method(typeof(Terse), "Greet"), animal = Method(typeof(Animal), "Speak"), dog = Method(typeof(Dog), "Speak"),
            greeter = Method(typeof(Greeter), "Greet"), formal = Method(typeof(Formal), "Game.IGreeter.Greet"), work = Method(typeof(Hot), "Work"),
            update = Method(typeof(Ticker), "Update"), tick = Method(typeof(Ticker), "Tick"), localText = Method(typeof(Reloads), "LocalText"),
            limit = Method(typeof(Game.Core.Rules), "Limit"), puppy = Method(typeof(Puppy), "Speak"), secret = Method(typeof(Player), "Secret");
        MethodBase[] game = { health, level, describe, twice, construct, stats, scaled, spread, shift, sum, terse, animal, dog, greeter, formal, work, update, tick, localText, limit, puppy, secret };

        // Live state from before any patch: objects, and delegates bound to the original methods.
        var player = new Player();
        Func<int> closed = player.Health;
        Func<int, int> bound = Player.Twice;
        var open = (Func<Player, int>)Delegate.CreateDelegate(typeof(Func<Player, int>), (MethodInfo)health);
        Animal pet = new Dog(), young = new Puppy();
        IGreeter polite = new Formal(), friendly = new Greeter();
        Ticker first = new Ticker(), second = new Ticker(), gone = new Ticker();
        UnityEngine.Object.DestroyImmediate(gone);
        int created = Player.Created;

        // Some originals run before they are patched, others (Describe, Stats, Spread) are first called afterwards.
        Console.WriteLine("originals");
        Equal(player.Health(), 10, "Player.Health");
        Equal(closed() + bound(4) + open(player), 28, "delegates bound to the originals");
        Equal(pet.Speak() + "/" + young.Speak() + "/" + pet.Twice(), "dog/puppy+dog/dogdog", "virtual calls");
        Equal(friendly.Greet("x") + "/" + polite.Greet("x") + "/" + Greet(new Terse { Count = 3 }, "x"), "hello x/good day x/hi x3", "interface calls");
        Equal(Hot.Work(7), 22L, "Hot.Work");
        Equal(Game.Core.Rules.Limit(), 5, "Rules.Limit");
        Watch(game);
        long[] pristine = Words();

        var target = new List<object> { Target("Game", patch1, Detour(health, patch1, "Patch.G1.Players::Health")) };
        Arguments();
        Bridge(target);

        Console.WriteLine("requests that the applier refuses before anything is resolved");
        Refused(Send(1, target, request => request["descriptorSha256"] = new string('0', 64)), "resolve", "differs from its SHA-256", "descriptor hash");
        Check(Applier() != null && Applier().Location.Length == 0, "the bridge loaded the applier from bytes for the first valid request");
        Refused(Send(1, target, request => request["descriptor"] = Path.Combine(run, "missing.json")), "resolve", "does not exist", "missing descriptor");
        Refused(Send(1, target, null, descriptor => descriptor["schemaVersion"] = 2L), "resolve", "differs from its request", "descriptor schema");
        Refused(Send(1, target, null, descriptor => descriptor["generation"] = 9L), "resolve", "differs from its request", "descriptor generation");
        Refused(Send(0, target), "resolve", "is not newer than the applied generation", "generation zero");
        Refused(Send(1, new List<object> { With(Target("Game", patch1, Detour(health, patch1, "Patch.G1.Players::Health")), "patchSha256", new string('a', 64)) }),
            "resolve", "differs from its SHA-256", "patch hash");
        if (!inlining)
        {
            // Under an inlining JIT the request is refused for that reason before a module is looked up.
            Refused(Send(1, new List<object> { With(Target("Game", patch1, Detour(health, patch1, "Patch.G1.Players::Health")), "loadedMvid", Guid.NewGuid().ToString()) }),
                "resolve", "the patch was built against", "MVID of another build");
            Refused(Send(1, new List<object> { With(Target("Game", patch1, Detour(health, patch1, "Patch.G1.Players::Health")), "name", "Game.Absent") }),
                "resolve", "is not loaded in this Player", "assembly that is not loaded");
        }
        Equal(Patches(".HotReload."), 0, "no patch assembly was loaded for a refused request");

        Console.WriteLine("generation 1");
        var game1 = Target("Game", patch1,
            Detour(health, patch1, "Patch.G1.Players::Health"), Detour(level, patch1, "Patch.G1.Players::Level"), Detour(describe, patch1, "Patch.G1.Players::Describe"),
            Detour(twice, patch1, "Patch.G1.Players::Twice"), Detour(construct, patch1, "Patch.G1.Players::Construct"), Detour(stats, patch1, "Patch.G1.Players::Stats"),
            Detour(scaled, patch1, "Patch.G1.Vec3::Scaled"), Detour(spread, patch1, "Patch.G1.Vec3::Spread"), Detour(shift, patch1, "Patch.G1.Vec3::Shift"),
            Detour(terse, patch1, "Patch.G1.Terse::Greet"), Detour(animal, patch1, "Patch.G1.Voices::Animal"), Detour(dog, patch1, "Patch.G1.Voices::Dog"),
            Detour(greeter, patch1, "Patch.G1.Voices::Greeter"), Detour(formal, patch1, "Patch.G1.Voices::Formal"), Detour(work, patch1, "Patch.G1.Others::Work"),
            Detour(update, patch1, "Patch.G1.Others::Update", "removed"), Detour(tick, patch1, "Patch.G1.Others::Tick"), Detour(localText, patch1, "Patch.G1.Others::LocalText"));
        var core1 = Target("Game.Core", patchCore, Detour(limit, patchCore, "PatchCore.Rules::Limit"));
        Dictionary<string, object> ack = Send(1, new List<object> { game1, core1 });
        Equal(Flag(ack, "inliningDisabled"), !inlining, "the canary reports inliningDisabled=" + !inlining);
        if (inlining)
        {
            // A JIT that inlines must be refused as a whole: a redirect would not reach the copies in the callers.
            Refused(ack, "inlining", "MONO_ARGUMENTS=-O=-float32,-inline", "a patch under an inlining JIT");
            Equal(player.Health() + player.Level + Player.Twice(4), 19, "the originals still run");
            Applied(Send(1, new List<object>()), 1, 0, 0, "an empty descriptor needs no redirect and is accepted");
            return;
        }
        Applied(ack, 1, 19, 0, "generation 1 is applied");
        // Ticker.Refresh on two live objects, five static callbacks, Rules.OnReload; then the patched Tick twice and LocalText.
        Equal(Json.Integer(ack, "callbacksInvoked"), 11L, "callbacks invoked");
        // OnThrowing threw, OnWrong takes an unsupported parameter, Plain.Refresh has no enumerable instances.
        Equal(Json.Integer(ack, "callbackErrors"), 3L, "callback errors are counted and do not fail the patch");
        long[] first1 = expected = Words();
        Check(Differs(first1, pristine, 19), "19 method entries start with a jump, the other watched ones are untouched");

        Console.WriteLine("generation 1: how the redirects were written");
        // Each way replaces bytes of one instruction: the first byte of the body, its second
        // instruction (sub rsp, imm8; mov r32, imm32), or a first instruction of seven bytes.
        int islands = 0, relays = 0, gates = 0, torn = 0;
        for (int index = 0; index < first1.Length; index++)
        {
            long was = pristine[index], now = first1[index];
            if (now == was) continue;
            if (ByFirstByte(now, was)) islands++;
            else if (BySecondInstruction(now, was)) { relays++; if (!LoadsConstant(was)) torn++; }
            else { gates++; if ((was & 0xFFFFFF) != 0xEC8148) torn++; }
            // An island below the lowest address a process can map does not exist.
            if (ByFirstByte(now, was) && Entry(watched[index]) + 5 + (int)(was >> 8) < 65536) torn++;
        }
        Equal(Json.Integer(ack, "islandWrites") + "/" + Json.Integer(ack, "gatedWrites"), islands + relays + "/" + gates, "the ack counts the first redirects through an island and those behind the gate");
        Check(islands > 0 && relays > 0 && torn == 0, "every first redirect replaced bytes of one instruction only (" + islands + " by the first byte, " + relays + " through the second instruction, "
            + gates + " whole jumps)");
        var pairs1 = new[]
        {
            Pair(health, patch1, "Patch.G1.Players::Health"), Pair(level, patch1, "Patch.G1.Players::Level"), Pair(describe, patch1, "Patch.G1.Players::Describe"),
            Pair(twice, patch1, "Patch.G1.Players::Twice"), Pair(construct, patch1, "Patch.G1.Players::Construct"), Pair(stats, patch1, "Patch.G1.Players::Stats"),
            Pair(scaled, patch1, "Patch.G1.Vec3::Scaled"), Pair(spread, patch1, "Patch.G1.Vec3::Spread"), Pair(shift, patch1, "Patch.G1.Vec3::Shift"),
            Pair(terse, patch1, "Patch.G1.Terse::Greet"), Pair(animal, patch1, "Patch.G1.Voices::Animal"), Pair(dog, patch1, "Patch.G1.Voices::Dog"),
            Pair(greeter, patch1, "Patch.G1.Voices::Greeter"), Pair(formal, patch1, "Patch.G1.Voices::Formal"), Pair(work, patch1, "Patch.G1.Others::Work"),
            Pair(update, patch1, "Patch.G1.Others::Update"), Pair(tick, patch1, "Patch.G1.Others::Tick"), Pair(localText, patch1, "Patch.G1.Others::LocalText"),
            Pair(limit, patchCore, "PatchCore.Rules::Limit"),
        };
        Check(Lead(pairs1), "every redirected entry leads to the compiled replacement, directly or through its island");
        Check(Flag(ack, "initializersRun") && Flag(ack, "inliningChecked"), "the ack says that the request compiled methods, which runs pending static initializers, and that the JIT was checked");

        Console.WriteLine("generation 1: every kind of caller reaches the new bodies");
        Equal(player.Health(), 110, "class receiver reading a non-public field, object created before the patch");
        Equal(closed() + "/" + open(player) + "/" + health.Invoke(player, null), "110/110/110", "closed delegate, open delegate and reflection bound before the patch");
        Equal(player.Level, 2, "property getter");
        Equal(player.Describe("a", 3), "p1:a3:11", "method first called after the patch, calling a non-public method");
        Equal(Player.Twice(4) + bound(4) + (int)twice.Invoke(null, new object[] { 4 }), 3024, "static method: direct, delegate, reflection");
        var later = new Player();
        Equal(later.Name + "/" + later.Health() + "/" + (Player.Created - created), "ctor1/111/100", "constructor");
        Equal(((Player)Activator.CreateInstance(typeof(Player))).Name, "ctor1", "constructor through Activator");
        Wide wide = player.Stats(5);
        Equal(wide.A + "," + wide.B + "," + wide.C + "," + wide.D, "6,10,30,40", "class receiver returning 32 bytes through the hidden buffer");
        var vector = new Vec3(1, 2, 3);
        Equal(vector.Scaled(2).ToString(), "3,5,7", "struct receiver returning 12 bytes");
        wide = vector.Spread(5);
        Equal(wide.A + "," + wide.B + "," + wide.C + "," + wide.D, "6,1,2,4", "struct receiver returning 32 bytes");
        vector.Shift(1);
        Equal(vector.Sum(), 16f, "struct receiver changing the struct in place");
        var brief = new Terse { Count = 3 };
        Equal(brief.Greet("x") + "/" + ((IGreeter)brief).Greet("x") + "/" + Greet(brief, "x"), "p1 hi x3/p1 hi x3/p1 hi x3", "struct interface method: direct, boxed, constrained");
        Equal(new Animal().Speak() + "/" + pet.Speak() + "/" + pet.Twice() + "/" + young.Speak(), "animal1/dog1/dog1dog1/puppy+dog1", "virtual, override and base calls");
        Equal(friendly.Greet("x") + "/" + polite.Greet("x"), "p1 hello x/p1 good day x", "interface and explicit interface implementations");
        Equal(Hot.Work(7), 23L, "Hot.Work");
        Equal(Game.Core.Rules.Limit(), 50, "second game assembly");
        first.Update();
        Equal(first.Updates, 0, "a removed message runs an empty method");

        Console.WriteLine("generation 1: callbacks");
        Equal(Reloads.Simple + "/" + Reloads.Listed + "/" + Reloads.Foreign + "/" + Reloads.Named + "/" + Reloads.Thrown + "/" + Game.Core.Rules.Reloaded, "1/1/1/1/1/1",
            "static callbacks ran once each, also those with attributes of another namespace");
        Equal(first.Refreshed + "/" + second.Refreshed + "/" + gone.Refreshed, "1/1/0", "instance callback on every live object, not on the destroyed one");
        Equal(Reloads.Unreachable, 0, "callbacks that cannot be called were skipped");
        Equal(Reloads.Local + "/" + first.Ticks + "/" + second.Ticks + "/" + gone.Ticks, "100/10/10/0", "InvokeOnHotReloadLocal ran the new bodies, static and per live object");
        Equal(Reloads.Last.Count, 18, "the patch list holds every replaced method but the removed message");
        MethodPatch entry = Reloads.Last.Find(patch => patch.originalMethod == health);
        Check(entry != null && entry.previousMethod == health && From(entry.newMethod) == "Game.HotReload.1", "a first patch: previous is the original, new is the replacement");
        Check(UnityEngine.Debug.Exceptions.Count == 1 && UnityEngine.Debug.Exceptions[0].Message == "callback failure", "the callback exception went to the engine log, unwrapped");
        Equal(UnityEngine.Debug.Warnings.FindAll(warning => warning.Contains("callback-skipped")).Count, 2, "two skipped callbacks were reported");

        Console.WriteLine("a repeated request and a stale generation");
        string answered = File.ReadAllText(requestPath + ".ack.json");
        File.WriteAllText(requestPath, File.ReadAllText(requestPath) + "\n");
        Poll();
        Check(File.ReadAllText(requestPath + ".ack.json") == answered && Reloads.Simple == 1, "the bridge takes a request id once: a rewritten request file changes nothing");
        // The applier keeps its own guard, for a bridge that asks twice.
        var direct = (Func<string, string>)Delegate.CreateDelegate(typeof(Func<string, string>), Applier().GetType("UnityMicroEditor.HotReload.Entry", true).GetMethod("Handle"));
        Check(direct(requestPath) == answered && Reloads.Simple == 1, "the applier answers a repeated request id with the same ack and runs nothing again");
        Refused(Send(1, new List<object> { game1 }), "resolve", "is not newer than the applied generation", "the applied generation again");
        Restart(Send(2, new List<object> { game1 }, request => request["applierSha256"] = new string('1', 64)), new string('1', 64), "a second applier in one Player");

        Console.WriteLine("failures leave generation 1 untouched");
        var good = Detour(twice, patch2, "Patch.G2.Players::Twice");
        Refused(Send(2, new List<object> { Target("Game", patch2, good, With(Detour(health, patch2, "Patch.G2.Players::Health"), "originalToken", 0x06FFFFFFL)) }),
            "resolve", "does not resolve", "one original token that does not exist");
        Refused(Send(2, new List<object> { Target("Game", patch2, good, With(Detour(health, patch2, "Patch.G2.Players::Health"), "replacementToken", 0x06FFFFFFL)) }),
            "resolve", "Replacement token", "one replacement token that does not exist");
        Refused(Send(2, new List<object> { Target("Game", patch2, good, With(Detour(health, patch2, "Patch.G2.Players::Health"), "originalToken", 0x02000002L)) }),
            "resolve", "two method definition tokens", "a token of another table");
        Refused(Send(2, new List<object> { Target("Game", patch2, good, Detour(twice, patch2, "Patch.G2.Players::Twice")) }), "write", "listed twice", "one method listed twice");
        Refused(Send(2, new List<object> { Target("Game", patchDenied, Detour(twice, patchDenied, "PatchDenied.Players::Twice"), Detour(health, patchDenied, "PatchDenied.Players::Health")) }),
            "resolve", "SkipVerification", "a patch assembly without the SkipVerification declaration");
        Equal(Patches("Game.HotReload.Denied"), 0, "that assembly was not loaded");
        Refused(Send(2, new List<object> { Target("Game", patchStale, Detour(twice, patchStale, "PatchStale.Players::Twice"), Detour(health, patchStale, "PatchStale.Players::Health")) }),
            "compile", "The replacement does not compile", "a replacement the JIT refuses, next to a good one");
        Refused(Send(2, new List<object> { Target("Game", patchBroken, Detour(twice, patchBroken, "PatchBroken.Players::Twice")) }),
            "compile", "PatchBroken.Players::Helper", "a method of the patch that is no replacement and does not compile");
        ack = Send(2, new List<object> { Target("Game", patchShapes,
            Detour(Method(typeof(Box<>), "Get"), patchShapes, "PatchShapes.Wrong::Good"), Detour(Method(typeof(Generic), "Echo"), patchShapes, "PatchShapes.Wrong::Good"),
            Detour(Method(typeof(IGreeter), "Greet"), patchShapes, "PatchShapes.Wrong::Good"), Detour(twice, patchShapes, "PatchShapes.Wrong::Good")) });
        Refused(ack, "resolve", "belongs to a generic type", "originals that are generic or have no body");
        Equal(((List<object>)ack["failures"]).Count + "/" + Patches("Game.HotReload.Shapes"), "3/0", "the three bad originals are reported and their patch assembly is not loaded");
        ack = Send(2, new List<object> { Target("Game", patchShapes,
            Detour(scaled, patchShapes, "PatchShapes.Wrong::Scaled"), Detour(health, patchShapes, "PatchShapes.Wrong::HealthMore"), Detour(health, patchShapes, "PatchShapes.Wrong::HealthText"),
            Detour(health, patchShapes, "PatchShapes.Wrong::HealthOther"), Detour(twice, patchShapes, "PatchShapes.Wrong::TwiceLong"), Detour(twice, patchShapes, "PatchShapes.Wrong::TwiceInstance"),
            Detour(twice, patchShapes, "PatchShapes.Wrong::Echo"), Detour(twice, patchShapes, "PatchShapes.Wrong::Good")) });
        Refused(ack, "resolve", "stand-in struct", "replacements of another native signature");
        Equal(((List<object>)ack["failures"]).Count, 7, "each of the seven bad pairs is reported, the good one is not");
        Equal(player.Health() + "/" + Player.Twice(4) + "/" + Hot.Work(7) + "/" + pet.Speak(), "110/1008/23/dog1", "generation 1 still runs");
        Equal(Reloads.Simple, 1, "no callback ran for a refused request");

        Console.WriteLine("generation 2 replaces generation 1");
        var game2 = Target("Game", patch2,
            Detour(health, patch2, "Patch.G2.Players::Health"), Detour(twice, patch2, "Patch.G2.Players::Twice"), Detour(dog, patch2, "Patch.G2.Voices::Dog"),
            Detour(work, patch2, "Patch.G2.Others::Work"), Detour(tick, patch2, "Patch.G2.Others::Tick"), Detour(localText, patch2, "Patch.G2.Others::LocalText"),
            Detour(animal, patch2, "Patch.G2.Voices::Animal"), Detour(sum, patch2, "Patch.G2.Vec3::Sum"));
        ack = Send(2, new List<object> { game2 });
        Applied(ack, 2, 8, 12, "generation 2 is applied: eight redirects, twelve originals restored");
        long[] second2 = Words();
        Check(Differs(second2, pristine, 8), "the dropped methods have their original bytes back");
        expected = second2;
        Check(Lead(Pair(health, patch2, "Patch.G2.Players::Health"), Pair(twice, patch2, "Patch.G2.Players::Twice"), Pair(dog, patch2, "Patch.G2.Voices::Dog"), Pair(work, patch2, "Patch.G2.Others::Work"),
            Pair(tick, patch2, "Patch.G2.Others::Tick"), Pair(localText, patch2, "Patch.G2.Others::LocalText"), Pair(animal, patch2, "Patch.G2.Voices::Animal"), Pair(sum, patch2, "Patch.G2.Vec3::Sum")),
            "retargeted entries lead to the replacements of generation 2, through the slot of their island or a rewritten jump");
        Equal(Json.Integer(ack, "islandWrites") + Json.Integer(ack, "gatedWrites"), 1L, "one first redirect among them; a retarget and a restore are neither");
        Equal(closed() + "/" + player.Level + "/" + player.Describe("a", 3) + "/" + bound(4), "210/1/orig:a3:10/2008", "retargeted and restored methods");
        later = new Player();
        wide = player.Stats(5);
        Equal(later.Name + "/" + later.Health() + "/" + wide.A + "," + wide.C, "ctor0/210/5,3", "restored constructor and struct return");
        vector = new Vec3(1, 2, 3);
        Equal(vector.Scaled(2) + "/" + vector.Sum(), "2,4,6/2001", "a method patched for the first time next to retargeted ones");
        Equal(new Animal().Speak() + "/" + pet.Speak() + "/" + young.Speak() + "/" + friendly.Greet("x") + "/" + polite.Greet("x") + "/" + Greet(brief, "x"),
            "animal2/dog2/puppy+dog2/hello x/good day x/hi x3", "virtual and interface callers");
        first.Update();
        Equal(Hot.Work(7) + "/" + Game.Core.Rules.Limit() + "/" + first.Updates, "24/5/1", "Hot.Work, the assembly the descriptor no longer lists, the removed message");
        // Eight global calls as before; the patched Tick on two objects. LocalText has the body it had in generation 1.
        Equal(Json.Integer(ack, "callbacksInvoked") + "/" + Json.Integer(ack, "callbackErrors"), "10/3", "callbacks of generation 2");
        Equal(Reloads.Local + "/" + first.Ticks + "/" + Reloads.Simple, "100/1010/2", "a local callback runs only when its own body changed");
        Equal(Reloads.Last.Count, 19, "the patch list: seven changed or newly patched bodies and twelve restored methods");
        entry = Reloads.Last.Find(patch => patch.originalMethod == health);
        Check(entry != null && From(entry.previousMethod) == "Game.HotReload.1" && From(entry.newMethod) == "Game.HotReload.2", "a second patch: previous is the older replacement");
        entry = Reloads.Last.Find(patch => patch.originalMethod == describe);
        Check(entry != null && From(entry.previousMethod) == "Game.HotReload.1" && entry.newMethod == describe, "a restored method: new is the original again");
        Check(Reloads.Last.Find(patch => patch.originalMethod == localText) == null, "a body that is the same as in the generation before is not listed");
        // Animal.Speak has the same text in both patches (a lambda, a generic list, a try block), and the helper its lambda calls changed.
        entry = Reloads.Last.Find(patch => patch.originalMethod == animal);
        Check(entry != null && From(entry.previousMethod) == "Game.HotReload.1" && From(entry.newMethod) == "Game.HotReload.2", "a body whose helper in the patch changed is listed as changed");
        Equal(UnityEngine.Debug.Warnings.FindAll(warning => warning.Contains("callback-skipped")).Count, 2, "skipped callbacks are reported once");

        Console.WriteLine("an empty descriptor restores everything");
        ack = Send(3, new List<object>());
        Applied(ack, 3, 0, 8, "generation 3 is applied");
        expected = pristine;
        Check(Same(Words(), pristine), "every watched method has exactly its original bytes");
        Equal(player.Health() + "/" + closed() + "/" + Player.Twice(4) + "/" + pet.Speak() + "/" + Hot.Work(7) + "/" + new Vec3(1, 2, 3).Sum(), "10/10/8/dog/22/6", "the originals run again");
        Equal(Json.Integer(ack, "callbacksInvoked") + "/" + Reloads.Local + "/" + first.Ticks, "11/101/1011", "callbacks of a restore; the local ones ran their original bodies");

        Console.WriteLine("patching again after a restore");
        ack = Send(4, new List<object> { game1, core1 });
        Applied(ack, 4, 19, 0, "generation 4 reuses the patch assembly of generation 1");
        Check(Same(expected = Words(), first1) && Lead(pairs1), "the entries lead to the same compiled replacements as in generation 1");
        Equal(Json.Integer(ack, "islandWrites") + "/" + Json.Integer(ack, "gatedWrites"), islands + relays + "/" + gates, "first redirects again, through the islands mapped for generation 1");
        Equal(player.Health() + "/" + Player.Twice(4) + "/" + pet.Speak() + "/" + Hot.Work(7) + "/" + new Vec3(1, 2, 3).Scaled(2), "110/1008/dog1/23/3,5,7", "generation 1 bodies run again");
        Applied(Send(5, new List<object>()), 5, 0, 19, "generation 5 restores everything");
        Check(Same(Words(), expected = pristine), "original bytes after the second restore");

        Generations();

        Islands(twice);
        Large();
        Initializers(twice);
        ack = Send(++generation, new List<object>());
        Applied(ack, generation, 0, Json.Integer(ack, "detoursRestored"), "an empty descriptor after the three parts above");
        Check(Same(Words(), expected = Compiled()), "every method they redirected has exactly its original bytes");
        Stress(work, "Work", new long[] { 22, 23, 24 }, cycles, null);
        Check(Same(Words(), Compiled()), "original bytes after the stress loop");
    }

    // ------------------------------------------------------------------ what only a patch defines

    // Whether a method changed with a generation is read off the two replacements. What a
    // replacement names of its own patch is part of it: a lambda, a helper, the state machine of an
    // iterator. Two generations with the same members in the same order differ in exactly those.
    private static void Generations()
    {
        Console.WriteLine("callbacks follow a change in what only the patch defines");
        MethodBase build = Method(typeof(Menu), "Build"), rebuild = Method(typeof(Menu), "Rebuild"), keep = Method(typeof(Menu), "Keep"),
            count = Method(typeof(Menu), "Count"), enable = Method(typeof(Menu), "__UnityPlay_OnEnable"), steady = Method(typeof(Menu), "Steady"),
            walk = Method(typeof(Menu), "Walk"), stroll = Method(typeof(Menu), "Stroll"), wait = Method(typeof(Menu), "Wait");
        Menu shown = new Menu(), closed = new Menu(), fading = new Menu();
        Func<Patch, string, List<object>> menus = (patch, space) => new List<object> { Target("Game", patch,
            Detour(build, patch, space + ".Menus::Build"), Detour(rebuild, patch, space + ".Menus::Rebuild"), Detour(keep, patch, space + ".Menus::Keep"),
            Detour(count, patch, space + ".Menus::Count"), Detour(enable, patch, space + ".Menus::Enable"), Detour(steady, patch, space + ".Menus::Steady"),
            Detour(walk, patch, space + ".Menus::Walk"), Detour(stroll, patch, space + ".Menus::Stroll"), Detour(wait, patch, space + ".Menus::Wait")) };

        int simple = Reloads.Simple;
        Dictionary<string, object> ack = Send(++generation, menus(patchMenu1, "Patch.M1"));
        Applied(ack, generation, 9, 0, "the first of two generations with the same members");
        Equal(shown.Shown + "/" + Menu.Rebuilt + "/" + Menu.Kept + "/" + Menu.Counted, "click1/10/1/1", "a first redirect runs every local callback with its new body");
        Equal(Menu.Steadied + "/" + Menu.Walked + "/" + Menu.Strolled + "/" + Menu.Waited, "7/3/1/10", "also those whose helpers call themselves, call each other or are async");
        Equal(shown.Enabled + "/" + closed.Enabled, "enabled1/enabled1", "a local attribute on a lifecycle entry runs the helper that holds its body, on every live object");
        Equal(shown.Refreshed + "/" + closed.Refreshed, "1/1", "the instance callback ran on both objects");

        // The same generation again, as a comment in the source makes it: nothing the Player runs
        // changed, so no callback of the game runs, neither a local one nor one for every patch.
        ack = Send(++generation, menus(patchMenu1, "Patch.M1"));
        Applied(ack, generation, 9, 0, "a generation with the bodies of the one before");
        Equal(Json.Integer(ack, "callbacksInvoked") + "/" + (Reloads.Simple - simple) + "/" + shown.Refreshed + "/" + Menu.Kept, "0/1/1/1", "a generation that changes no body runs no callback");

        // The first object's callback destroys the second one at once while the pass still lists it,
        // and the third one for the end of the frame, until which it exists.
        shown.Closes = closed;
        shown.Fades = fading;
        shown.Enabled = string.Empty;
        ack = Send(++generation, menus(patchMenu2, "Patch.M2"));
        Applied(ack, generation, 9, 0, "the next generation retargets the nine methods");
        Equal(shown.Shown + "/" + Menu.Rebuilt + "/" + Menu.Counted, "click2/110/6", "a changed lambda, a changed helper and a changed iterator are changes of the method that names them");
        Equal(Menu.Walked + "/" + Menu.Strolled + "/" + Menu.Waited, "33/11/110", "so are a changed method that calls itself, for each of the two that name it, and a changed async method");
        Equal(Menu.Kept + "/" + Menu.Steadied + "/" + shown.Enabled, "1/7/", "a body whose lambda and helpers are the same code did not change, also when they call themselves and each other");
        var listed = new List<string>();
        foreach (MethodPatch patch in Reloads.Last) listed.Add(patch.originalMethod.Name);
        listed.Sort(StringComparer.Ordinal);
        Equal(string.Join(",", listed), "Build,Count,Rebuild,Stroll,Wait,Walk", "the patch list names exactly the six changed methods");
        Equal(shown.Refreshed + "/" + closed.Refreshed + "/" + fading.Refreshed, "2/1/2", "an object that a callback of the pass destroyed at once is not called by the rest of it; one that lives to the end of the frame is");
        UnityEngine.Object.EndOfFrame();
        shown.Closes = shown.Fades = null;

        ack = Send(++generation, new List<object>());
        Applied(ack, generation, 0, 9, "an empty descriptor restores the nine methods");
        Equal(shown.Shown + "/" + Menu.Rebuilt + "/" + Menu.Kept + "/" + Menu.Counted + "/" + shown.Enabled, "click0/111/2/7/enabled0", "the local callbacks of a restore ran the original bodies");
        Equal(Menu.Steadied + "/" + Menu.Walked + "/" + Menu.Strolled + "/" + Menu.Waited, "8/34/12/111", "all of them");
        UnityEngine.Object.DestroyImmediate(shown);

        Console.WriteLine("which method is the helper of a lifecycle callback");
        var crowd = new Crowded();
        Panel panel = new Panel(), quiet = new QuietPanel();
        MethodBase helper = Method(typeof(Crowded), "__UnityPlay_OnEnable_"), own = Method(typeof(Crowded), "__UnityPlay_OnEnable"),
            later = Method(typeof(Crowded), "__UnityPlay_OnEnableLater"), fooHelper = Method(typeof(Crowded), "__UnityPlay_Foo"),
            panelHelper = Method(typeof(Panel), "__UnityPlay_OnEnable");
        ack = Send(++generation, new List<object> { Target("Game", patchHelpers,
            Detour(helper, patchHelpers, "Patch.Helpers.Crowds::Helper"), Detour(panelHelper, patchHelpers, "Patch.Helpers.Crowds::PanelEnable")) });
        Applied(ack, generation, 2, 0, "the helpers of two lifecycle callbacks");
        Equal(crowd.Enabled + "/" + crowd.Own + "/" + crowd.Later, "crowded1/0/0", "the helper is the name the rewrite made free with underscores, not a shorter or a longer one the game declares");
        Equal(panel.Seen + "/" + quiet.Seen, "panel1;/quiet;", "a virtual callback runs as the engine calls it: an object whose class overrides it runs its own body");

        crowd.Enabled = string.Empty;
        ack = Send(++generation, new List<object> { Target("Game", patchHelpers,
            Detour(helper, patchHelpers, "Patch.Helpers.Crowds::Helper"), Detour(panelHelper, patchHelpers, "Patch.Helpers.Crowds::PanelEnable"),
            Detour(own, patchHelpers, "Patch.Helpers.Crowds::Own"), Detour(later, patchHelpers, "Patch.Helpers.Crowds::Later"),
            Detour(fooHelper, patchHelpers, "Patch.Helpers.Crowds::FooHelper")) });
        Applied(ack, generation, 5, 0, "three methods that only look like helpers");
        Equal(crowd.Enabled + "/" + crowd.Own + "/" + crowd.Later + "/" + crowd.Fooed, "/0/0/0", "a method named like a helper is no callback, and a method that is no lifecycle callback has no helper");
        Applied(Send(++generation, new List<object>()), generation, 0, 5, "restored");
        Equal(crowd.Enabled + "/" + crowd.Fooed, "crowded0/0", "the restore of a helper runs its callback with the original body");
        UnityEngine.Object.DestroyImmediate(crowd);
        UnityEngine.Object.DestroyImmediate(panel);
        UnityEngine.Object.DestroyImmediate(quiet);
    }

    // ------------------------------------------------------------------ first redirects

    // How a method is redirected for the first time. Only its first byte changes: the four bytes
    // after it then say where the jump lands, and the applier maps an island exactly there. These
    // checks cover what the islands must keep track of: one page with several islands, an island
    // across two pages, the island found again after a restore, the slot that retargets it, and the
    // refusals when the address belongs to something else.
    private static void Islands(MethodBase twice)
    {
        Console.WriteLine("first redirects: bodies that start with a one-byte instruction");
        MethodBase seven = Method(typeof(Framed), "Seven"), calls = Method(typeof(Framed), "CallsSeven"), guarded = Method(typeof(Hot), "Guarded");
        Equal(Framed.CallsSeven() + "/" + Hot.Guarded(2), "8/10", "the originals");
        Watch(seven, calls, guarded);
        Check(First(seven) == 0x55 && First(calls) == 0x55 && First(guarded) == 0x55, "an argument on the stack and a try block make the compiled body start with push rbp");
        long[] clean = Words();
        var framed = Target("Game", patchIslands, Detour(seven, patchIslands, "PatchIslands.Framed::Seven"), Detour(calls, patchIslands, "PatchIslands.Framed::CallsSeven"));
        Dictionary<string, object> ack = Send(++generation, new List<object> { framed });
        Applied(ack, generation, 2, 0, "two of them are redirected");
        Equal(Json.Integer(ack, "islandWrites") + "/" + Json.Integer(ack, "gatedWrites"), "2/0", "both through an island");
        long[] entered = expected = Words();
        Check(OnlyFirstByte(entered, clean, seven, calls), "only the first byte of each body changed");
        Check(Lead(Pair(seven, patchIslands, "PatchIslands.Framed::Seven"), Pair(calls, patchIslands, "PatchIslands.Framed::CallsSeven")), "each island leads to the replacement");
        Equal(Framed.CallsSeven() + "/" + Framed.Seven(1, 2, 3, 4, 5, 6, 7), "1/108", "the new bodies run");

        ack = Send(++generation, new List<object> { Target("Game", patch1, Detour(guarded, patch1, "Patch.G1.Others::Guarded")) });
        Applied(ack, generation, 1, 2, "the next generation redirects another one and drops the two");
        expected = Words();
        Check(Same(Words(seven, calls), Pick(clean, seven, calls)), "a restore puts the first byte back: the dropped bodies are the original bytes");
        long patched = Marshal.ReadInt64((IntPtr)Entry(guarded)), landed = Lands(guarded);
        Equal(Framed.CallsSeven() + "/" + Hot.Guarded(2), "8/11", "the originals and the new body run");
        ack = Send(++generation, new List<object> { Target("Game", patch2, Detour(guarded, patch2, "Patch.G2.Others::Guarded")) });
        Applied(ack, generation, 1, 0, "a retarget");
        Equal(Json.Integer(ack, "islandWrites") + "/" + Json.Integer(ack, "gatedWrites"), "0/0", "which is no first redirect");
        Check(Marshal.ReadInt64((IntPtr)Entry(guarded)) == patched && Lands(guarded) != landed && Lead(Pair(guarded, patch2, "Patch.G2.Others::Guarded")),
            "the retarget stored the slot of the island and left the body alone");
        Equal(Hot.Guarded(2), 12L, "the body of the second patch runs");

        ack = Send(++generation, new List<object> { framed });
        Applied(ack, generation, 2, 1, "the first two again, after their restore");
        Equal(Json.Integer(ack, "islandWrites"), 2L, "first redirects again: each found the island it had");
        Check(Same(expected = Words(), entered) && Lead(Pair(seven, patchIslands, "PatchIslands.Framed::Seven")), "the same byte, the same island, the same replacement");
        Equal(Framed.CallsSeven() + "/" + Hot.Guarded(2), "1/10", "new and restored bodies");

        // Something else rewrote a method between two patches: its bytes are not what the applier saved.
        Applied(Send(++generation, new List<object>()), generation, 0, 2, "everything restored");
        expected = Words();
        Marshal.WriteByte((IntPtr)(Entry(seven) + 2), (byte)(Marshal.ReadByte((IntPtr)(Entry(seven) + 2)) ^ 0x40));
        ack = Send(++generation, new List<object> { framed });
        Marshal.WriteByte((IntPtr)(Entry(seven) + 2), (byte)(Marshal.ReadByte((IntPtr)(Entry(seven) + 2)) ^ 0x40));
        Refused(ack, "write", "something else rewrites this method", "a body whose first bytes changed since the applier read them");

        Console.WriteLine("first redirects: where the islands lie");
        // All bodies of Edge have one prologue, so their islands lie at one distance from them.
        MethodBase sample = Method(typeof(Edge), "E23");
        long distance = Island(sample) - Entry(sample);
        MethodBase atEnd = null, beside = null, third = null;
        for (int index = 0; index < 18 && atEnd == null; index += 3)
        {
            // An island of six bytes that starts three bytes before the end of a page.
            MethodBase candidate = Method(typeof(Edge), "E" + index);
            beside = Method(typeof(Edge), "E" + (index + 1)); third = Method(typeof(Edge), "E" + (index + 2));
            if (Place(candidate, (int)((4093 - distance) & 4095))) atEnd = candidate;
        }
        if (atEnd == null) { Check(false, "a body could be compiled where its island crosses the end of a page"); return; }
        // The next two bodies follow within the same kilobyte: their islands lie in the second page of the first one.
        Entry(beside); Entry(third);
        Watch(atEnd, beside, third);
        long first = Island(atEnd) & ~4095L;
        Check((Island(atEnd) & 4095) == 4093 && (Island(beside) & ~4095L) == first + 4096 && (Island(third) & ~4095L) == first + 4096,
            "three bodies are compiled so that one island crosses the end of a page and two more lie in the page after it");
        string atEndName = "Patch.Bulk.Edge::" + atEnd.Name, besideName = "Patch.Bulk.Edge::" + beside.Name, thirdName = "Patch.Bulk.Edge::" + third.Name;
        ack = Send(++generation, new List<object> { Target("Game", patchBulk, Detour(atEnd, patchBulk, atEndName), Detour(beside, patchBulk, besideName)) });
        Applied(ack, generation, 2, 0, "the island across two pages and one more in its second page");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + "/" + Call(atEnd, 2) + "/" + Call(beside, 2) + "/" + Call(third, 2), "2/11/11/10", "both redirects work, the third body is the original");
        Check(Mapped(first, first + 8192, "rwx") && Lead(Pair(atEnd, patchBulk, atEndName), Pair(beside, patchBulk, besideName)), "both pages are mapped and the islands lead to the replacements");
        ack = Send(++generation, new List<object> { Target("Game", patchBulk, Detour(atEnd, patchBulk, atEndName), Detour(beside, patchBulk, besideName), Detour(third, patchBulk, thirdName)) });
        Applied(ack, generation, 3, 0, "a later first redirect whose island lies in a page that is already mapped");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + "/" + Call(atEnd, 2) + "/" + Call(beside, 2) + "/" + Call(third, 2), "1/11/11/11", "one more island in that page");

        Console.WriteLine("first redirects: an island address that belongs to something else");
        // Far enough from the islands above that its page is not one of theirs.
        for (int index = 0; index < 600; index++) Fill();
        MethodBase blocked = Method(typeof(Edge), "E20");
        Entry(blocked);
        Watch(blocked);
        string blockedName = "Patch.Bulk.Edge::" + blocked.Name;
        List<long> taken = Occupy(blocked);
        var edges = Target("Game", patchBulk, Detour(atEnd, patchBulk, atEndName), Detour(beside, patchBulk, besideName), Detour(third, patchBulk, thirdName), Detour(blocked, patchBulk, blockedName));
        ack = Send(++generation, new List<object> { edges });
        Check(taken.Count != 0, "the harness holds the page where the island of a fourth body would be mapped");
        Refused(ack, "write", "would reach past the first instruction", "a body that starts with push rbp and whose island cannot be mapped: no whole jump is written over several instructions");
        Check(Json.Text(ack, "error").Contains("occupied by another mapping"), "the refusal names the reason");
        Equal(Call(blocked, 2) + "/" + Call(third, 2), "10/11", "the original runs, and the generation before stays applied");
        Vacate(taken);
        edges = Target("Game", patchBulk, Detour(atEnd, patchBulk, atEndName), Detour(beside, patchBulk, besideName), Detour(third, patchBulk, thirdName), Detour(blocked, patchBulk, blockedName));
        ack = Send(++generation, new List<object> { edges });
        Applied(ack, generation, 4, 0, "the same request once the address is free");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + "/" + Call(blocked, 2), "1/11", "nothing of the refusal was kept");

        // The same for a body that starts with sub rsp, imm8 and goes on with something else than
        // a constant: a whole jump would end in its second instruction.
        MethodBase split = Method(typeof(Many), "M599");
        Entry(split);
        Watch(split);
        taken = Occupy(split);
        Check(taken.Count != 0 && (Words(split)[0] & 0xFFFFFF) == 0xEC8348 && !LoadsConstant(Words(split)[0]), "the harness holds the island page of a body that starts with sub rsp, imm8 and a mov to the stack");
        ack = Send(++generation, new List<object> { Target("Game", patchBulk, Detour(split, patchBulk, "Patch.Bulk.Many::M599")) });
        Refused(ack, "write", "would reach past the first instruction", "that body as well");
        Vacate(taken);
        ack = Send(++generation, new List<object> { Target("Game", patchBulk, Detour(split, patchBulk, "Patch.Bulk.Many::M599")) });
        Applied(ack, generation, 1, 4, "redirected once the address is free");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + "/" + Many.M599(1, "s"), "1/s1y602", "through its island");

        Console.WriteLine("first redirects: through the second instruction");
        // A body that first loads a constant: its island would lie below address zero.
        MethodBase flat = Method(typeof(Hot), "Flat");
        Equal(Hot.Flat(7), 22L, "the original");
        Watch(flat);
        long[] unflat = Words();
        byte ninth = Marshal.ReadByte((IntPtr)(Entry(flat) + 8));
        Check(LoadsConstant(Words(flat)[0]) && Island(flat) < 65536, "its compiled body starts with sub rsp, imm8; mov eax, imm32, and no island can exist for its first byte");
        ack = Send(++generation, new List<object> { Target("Game", patch1, Detour(flat, patch1, "Patch.G1.Others::Flat")) });
        Applied(ack, generation, 1, 1, "it is redirected");
        expected = Words();
        long relayed = Words(flat)[0];
        Check(Json.Integer(ack, "islandWrites") == 1 && Json.Integer(ack, "gatedWrites") == 0 && BySecondInstruction(relayed, Pick(unflat, flat)[0]) && Lead(Pair(flat, patch1, "Patch.G1.Others::Flat")),
            "its first instruction is untouched, its second is a jump to a relay that leads to the replacement");
        Equal(Hot.Flat(7), 23L, "the new body runs, on the stack of the caller");
        ack = Send(++generation, new List<object> { Target("Game", patch2, Detour(flat, patch2, "Patch.G2.Others::Flat")) });
        Applied(ack, generation, 1, 0, "a retarget");
        Check(Words(flat)[0] == relayed && Lead(Pair(flat, patch2, "Patch.G2.Others::Flat")) && Hot.Flat(7) == 24, "which stored the slot of the relay");
        Applied(Send(++generation, new List<object>()), generation, 0, 1, "restored");
        Check(Same(Words(flat), Pick(unflat, flat)) && Marshal.ReadByte((IntPtr)(Entry(flat) + 8)) == ninth && Hot.Flat(7) == 22, "all five bytes of the second instruction are back");
        expected = Words();
        ack = Send(++generation, new List<object> { Target("Game", patch1, Detour(flat, patch1, "Patch.G1.Others::Flat")) });
        Applied(ack, generation, 1, 0, "redirected again");
        expected = Words();
        Check(Words(flat)[0] == relayed && Hot.Flat(7) == 23, "through the relay it had");

        Console.WriteLine("first redirects: a whole jump");
        // A first instruction of seven bytes contains the jump: the one case for the gated write.
        MethodBase large = Method(typeof(Hot), "Large");
        Equal(Hot.Large(7), 22L, "the original");
        Watch(large);
        long[] unjumped = Words();
        taken = Occupy(large);
        Check(taken.Count != 0 && (Words(large)[0] & 0xFFFFFF) == 0xEC8148, "the harness holds the island page of a body that starts with sub rsp, imm32");
        ack = Send(++generation, new List<object> { Target("Game", patch1, Detour(large, patch1, "Patch.G1.Others::Large")) });
        Applied(ack, generation, 1, 1, "it is redirected all the same");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + "/" + Json.Integer(ack, "gatedWrites") + "/" + Hot.Large(7), "0/1/23", "behind the gate");
        Check(!ByFirstByte(Words(large)[0], Pick(unjumped, large)[0]) && Lead(Pair(large, patch1, "Patch.G1.Others::Large")), "its first five bytes are a jump to the replacement");
        ack = Send(++generation, new List<object> { Target("Game", patch2, Detour(large, patch2, "Patch.G2.Others::Large")) });
        Applied(ack, generation, 1, 0, "a retarget");
        Check(Lead(Pair(large, patch2, "Patch.G2.Others::Large")) && Hot.Large(7) == 24, "which rewrote the jump");
        Applied(Send(++generation, new List<object>()), generation, 0, 1, "restored");
        Check(Same(Words(large), Pick(unjumped, large)) && Hot.Large(7) == 22, "the saved instruction is back");
        expected = Words();
        Vacate(taken);
        ack = Send(++generation, new List<object> { Target("Game", patch1, Detour(large, patch1, "Patch.G1.Others::Large")) });
        Applied(ack, generation, 1, 0, "redirected again once the address is free");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + "/" + Json.Integer(ack, "gatedWrites") + "/" + Hot.Large(7), "1/0/23", "through an island now: the way is decided per redirect");

        Console.WriteLine("first redirects: islands 16 bytes apart");
        // Ten bytes of code each, compiled in a row: the smallest distance two islands can have.
        var tiny = new MethodBase[5];
        for (int index = 0; index < tiny.Length; index++) tiny[index] = Method(typeof(Tiny), "T" + index);
        foreach (MethodBase method in tiny) Entry(method);
        bool row = true;
        for (int index = 1; index < tiny.Length; index++) row &= Entry(tiny[index]) - Entry(tiny[index - 1]) == 16 && First(tiny[index]) == 0x55;
        if (!row) { Check(false, "five bodies that start with push rbp are compiled 16 bytes apart"); return; }
        Watch(tiny);
        Func<int[], Dictionary<string, object>> tinies = list =>
        {
            var detours = new Dictionary<string, object>[list.Length];
            for (int index = 0; index < list.Length; index++) detours[index] = Detour(tiny[list[index]], patchBulk, "Patch.Bulk.Tiny::T" + list[index]);
            return Target("Game", patchBulk, detours);
        };
        ack = Send(++generation, new List<object> { tinies(new[] { 1 }) });
        Applied(ack, generation, 1, 1, "the second of the row");
        expected = Words();
        // Its pointer slot is the aligned word after its jump, which is where the island of the third body would start.
        ack = Send(++generation, new List<object> { tinies(new[] { 1, 2 }) });
        Refused(ack, "write", "overlaps the island of another method", "the third: its island would overlap the slot of the second");
        ack = Send(++generation, new List<object> { tinies(new[] { 1, 0 }) });
        Applied(ack, generation, 2, 0, "the first: its slot goes before its jump, because the island of the second lies after it");
        expected = Words();
        Check(Json.Integer(ack, "islandWrites") == 1 && SlotBefore(tiny[0]) && !SlotBefore(tiny[1]), "one island more, with the slot on the other side");
        // Two in one request: the applier maps the higher one first, so both find room.
        ack = Send(++generation, new List<object> { tinies(new[] { 1, 0, 3, 4 }) });
        Applied(ack, generation, 4, 0, "the fourth and the fifth in one request");
        expected = Words();
        Check(Json.Integer(ack, "islandWrites") == 2 && SlotBefore(tiny[3]) && !SlotBefore(tiny[4]), "two islands 16 bytes apart");
        Tiny.Hits = 0;
        Tiny.T0(1, 2, 3, 4, 5, 6, 7); Tiny.T1(1, 2, 3, 4, 5, 6, 7); Tiny.T2(1, 2, 3, 4, 5, 6, 7); Tiny.T3(1, 2, 3, 4, 5, 6, 7); Tiny.T4(1, 2, 3, 4, 5, 6, 7);
        Equal(Tiny.Hits, 0x1B, "the four redirected bodies run their replacements, the refused one its original");
    }

    // One request that makes the JIT compile more code than one of its 64 KiB chunks holds. The
    // applier looks every entry up in the process map, which it must read after the compiling.
    private static void Large()
    {
        Console.WriteLine("a patch whose methods fill several chunks of JIT code");
        const int count = 590;
        var detours = new Dictionary<string, object>[count];
        for (int index = 0; index < count; index++) detours[index] = Detour(Method(typeof(Many), "M" + index), patchBulk, "Patch.Bulk.Many::M" + index);
        Dictionary<string, object> ack = Send(++generation, new List<object> { Target("Game", patchBulk, detours) });
        Applied(ack, generation, count, Json.Integer(ack, "detoursRestored"), count + " bodies that were never compiled before are redirected in one request");
        expected = Words();
        Equal(Json.Integer(ack, "islandWrites") + Json.Integer(ack, "gatedWrites") + "/" + Many.M0(1, "s") + "/" + Many.M589(2, "s"), count + "/s1y3/s2y595", "all of them for the first time, and they run");
        Console.WriteLine("  OBS " + Json.Integer(ack, "islandWrites") + " island writes and " + Json.Integer(ack, "gatedWrites") + " gated writes in " + Milliseconds((double)ack["elapsedSeconds"], 1) + ", compiling included");
    }

    // Compiling a method runs the static initializers that are pending for its types. The ack must
    // say so when the request is refused afterwards, and a failed initializer costs the game the type.
    private static void Initializers(MethodBase twice)
    {
        Console.WriteLine("static initializers that run when a patch is compiled");
        MethodBase eager = Method(typeof(Eager), "Value"), lazy = Method(typeof(Lazy), "Value");
        Dictionary<string, object> ack = Send(++generation, new List<object> { Target("Game", patchEager, With(Detour(eager, patchEager, "PatchEager.Eagers::Value"), "originalToken", 0x06FFFFFFL)) });
        Refused(ack, "resolve", "does not resolve", "a request refused before anything is compiled");
        Equal(Flag(ack, "initializersRun") + "/" + Ready.Eager, "False/0", "no initializer ran, and the ack says so");
        ack = Send(++generation, new List<object> { Target("Game", patchEager, Detour(eager, patchEager, "PatchEager.Eagers::Value"), Detour(eager, patchEager, "PatchEager.Eagers::Value")) });
        Refused(ack, "write", "listed twice", "a request refused after its methods were compiled");
        Equal(Flag(ack, "initializersRun") + "/" + Ready.Eager, "True/1", "the initializer of the edited type ran, and the ack says so");

        ack = Send(++generation, new List<object> { Target("Game", patchLocal, Detour(twice, patchLocal, "PatchLocal.Players::Twice")) });
        Refused(ack, "compile", "The replacement does not compile", "a static initializer of the patch itself that fails");

        ack = Send(++generation, new List<object> { Target("Game", patchLazy, Detour(twice, patchLazy, "PatchLazy.Players::Twice"), Detour(lazy, patchLazy, "PatchLazy.Lazies::Value")) });
        bool named = false;
        foreach (Dictionary<string, object> failure in (List<object>)ack["failures"])
            named |= Json.Text(failure, "stage") == "compile" && Json.Text(failure, "message").Contains("The static initializer of Game.Lazy failed");
        Check(named && Answers(ack) && !Flag(ack, "success") && Flag(ack, "restartRequired") && Flag(ack, "initializersRun") && Json.Integer(ack, "appliedGeneration") == applied
            && Json.Integer(ack, "detoursApplied") == 0 && Same(Words(), expected), "a static initializer of the game that fails: nothing is written and the ack asks for a restart");
        Ready.Now = true;
        bool lost = false;
        try { Lazy.Value(); } catch (TypeInitializationException) { lost = true; }
        Equal(lost + "/" + Ready.Lazy + "/" + Player.Twice(4), "True/1/8", "the game has lost the type for the life of the process, which is why");
    }

    // ------------------------------------------------------------------ stress

    // A method called from three other threads while it is redirected for the first time, redirected
    // again (retarget) and restored, over and over. Every call must return one of the three bodies'
    // results. `gated` says how the first redirects must have been written; null leaves it open.
    private static bool Stress(MethodBase method, string name, long[] results, int cycles, bool? gated)
    {
        Console.WriteLine("stress: " + name + " under three calling threads, " + cycles + " cycles of patch, patch again, restore");
        Console.WriteLine("  OBS first bytes of Hot." + name + ": " + BitConverter.ToString(BitConverter.GetBytes(Marshal.ReadInt64(method.MethodHandle.GetFunctionPointer()))));
        var call = (Func<long, long>)Delegate.CreateDelegate(typeof(Func<long, long>), (MethodInfo)method);
        int which = Array.IndexOf(new[] { "Work", "Guarded", "Flat", "Large" }, name);
        long seed = which == 1 ? 2 : 7, calls = 0, wrong = 0, islands = 0, gates = 0;
        var seen = new int[3];
        bool stop = false;
        var threads = new List<Thread>();
        for (int index = 0; index < 3; index++)
        {
            var thread = new Thread(() =>
            {
                long count = 0;
                while (!Volatile.Read(ref stop))
                {
                    long value = which == 0 ? Hot.Work(seed) : which == 1 ? Hot.Guarded(seed) : which == 2 ? Hot.Flat(seed) : Hot.Large(seed);
                    int body = Array.IndexOf(results, value);
                    if (body < 0) Interlocked.Increment(ref wrong); else seen[body] = 1;
                    count++;
                }
                Interlocked.Add(ref calls, count);
            });
            thread.IsBackground = true;
            thread.Start();
            threads.Add(thread);
        }
        Stopwatch watch = Stopwatch.StartNew();
        bool accepted = true;
        var spent = new double[3];
        for (int cycle = 0; cycle < cycles && accepted; cycle++)
        {
            var steps = new[]
            {
                new List<object> { Target("Game", patch1, Detour(method, patch1, "Patch.G1.Others::" + name)) },
                new List<object> { Target("Game", patch2, Detour(method, patch2, "Patch.G2.Others::" + name)) },
                new List<object>(),
            };
            for (int step = 0; step < 3; step++)
            {
                Dictionary<string, object> ack = Send(++generation, steps[step]);
                accepted &= Flag(ack, "success") && call(seed) == results[(step + 1) % 3];
                spent[step] += (double)ack["elapsedSeconds"];
                islands += Json.Integer(ack, "islandWrites"); gates += Json.Integer(ack, "gatedWrites");
            }
        }
        Volatile.Write(ref stop, true);
        foreach (Thread thread in threads) thread.Join();
        applied = generation;
        Check(accepted, "every request of the stress loop was applied and visible to the next call");
        Check(wrong == 0 && seen[0] + seen[1] + seen[2] == 3, "the threads saw the three bodies and nothing else (" + calls + " calls, " + cycles * 3 + " requests, "
            + (watch.Elapsed.TotalMilliseconds / (cycles * 3)).ToString("0.00", CultureInfo.InvariantCulture) + " ms per request)");
        Check(islands + gates == cycles && (gated == null || (gated.Value ? islands : gates) == 0), "every first redirect was written " + (gates == 0 ? "through an island" : islands == 0 ? "behind the gate" : "one way or the other")
            + " (" + islands + " island writes, " + gates + " gated writes)");
        Console.WriteLine("  OBS mean time inside the applier: first redirect " + Milliseconds(spent[0], cycles) + ", retarget " + Milliseconds(spent[1], cycles) + ", restore " + Milliseconds(spent[2], cycles));
        return accepted && wrong == 0;
    }

    // A try block makes the body start with push rbp, one byte. A thread that has executed it when
    // the start of the body is rewritten must find every other byte unchanged, which only the
    // redirect through an island guarantees. Three threads that call nothing else are the worst case.
    private static void Unwind(int cycles)
    {
        MethodBase guarded = Method(typeof(Hot), "Guarded");
        Hot.Guarded(2);
        Watch(guarded);
        Check(First(guarded) == 0x55, "the compiled body of Hot.Guarded starts with the one-byte push rbp");
        Stress(guarded, "Guarded", new long[] { 10, 11, 12 }, cycles, false);
        Check(Same(Words(), Compiled()), "original bytes after the stress loop");
    }

    // The same loop for a body that first loads a constant. The island of its first byte would lie
    // below address zero, and a whole jump would end in the instruction a thread may be about to
    // execute; the jump replaces exactly that instruction, and its relay undoes the one before it.
    private static void Relay(int cycles)
    {
        MethodBase flat = Method(typeof(Hot), "Flat");
        Hot.Flat(7);
        Watch(flat);
        byte ninth = Marshal.ReadByte((IntPtr)(Entry(flat) + 8));
        Check(LoadsConstant(Words(flat)[0]) && Island(flat) < 65536, "the compiled body of Hot.Flat starts with sub rsp, imm8; mov eax, imm32, and no island can exist for its first byte");
        Stress(flat, "Flat", new long[] { 22, 23, 24 }, cycles, false);
        Check(Same(Words(), Compiled()) && Marshal.ReadByte((IntPtr)(Entry(flat) + 8)) == ninth, "original bytes after the stress loop");
    }

    // And for a body with a first instruction of seven bytes whose island address belongs to
    // something else: every first redirect is a whole jump behind the gate, which that one
    // instruction contains.
    private static void Fallback(int cycles)
    {
        MethodBase large = Method(typeof(Hot), "Large");
        Hot.Large(7);
        Watch(large);
        List<long> taken = Occupy(large);
        Check(taken.Count != 0 && (Words(large)[0] & 0xFFFFFF) == 0xEC8148, "the harness holds the island page of Hot.Large, whose compiled body starts with sub rsp, imm32");
        Stress(large, "Large", new long[] { 22, 23, 24 }, cycles, true);
        Check(Same(Words(), Compiled()), "original bytes after the stress loop");
    }

    // ------------------------------------------------------------------ the process map

    // The JIT maps a new chunk of code whenever one fills, also while the applier compiles its own
    // probes and the methods of a request. Whatever the fill level, the first request of a process
    // must find every entry in the process map. run.sh starts this for 64 levels of one chunk.
    private static void Chunk(int methods)
    {
        for (int index = 0; index < methods; index++) Fill();
        MethodBase twice = Method(typeof(Player), "Twice");
        Dictionary<string, object> ack = Send(1, new List<object> { Target("Game", patch1, Detour(twice, patch1, "Patch.G1.Players::Twice")) });
        bool ok = Flag(ack, "success") && Flag(ack, "inliningDisabled") && Flag(ack, "inliningChecked") && Player.Twice(4) == 1008;
        Check(ok, "the first request after " + methods + " more methods of 16 bytes is applied" + (ok ? string.Empty : ": " + Json.Write(ack)));
    }

    // The check of the JIT redirects probes of the applier. When it cannot (here: their code is not
    // writable for one request), that is no answer: not "the JIT inlines", and not kept.
    private static void Undecided()
    {
        MethodBase twice = Method(typeof(Player), "Twice");
        Player.Twice(1);
        Watch(twice);
        var target = new List<object> { Target("Game", patch1, Detour(twice, patch1, "Patch.G1.Players::Twice")) };
        // Refused before any redirect is planned: the applier is loaded, its check has not run.
        Dictionary<string, object> ack = Send(1, target, request => request["descriptorSha256"] = new string('0', 64));
        Refused(ack, "resolve", "differs from its SHA-256", "a request that needs no check of the JIT");
        Check(!Flag(ack, "inliningChecked") && !Flag(ack, "inliningDisabled"), "the ack reports the JIT as not checked");

        // The three probes, with enough small methods before and after them that their pages hold
        // nothing the JIT writes to again. Those pages lose write access for one request.
        Type probe = Applier().GetType("UnityMicroEditor.HotReload.Canary+Probe", true);
        for (int index = 0; index < 300; index++) Fill();
        long low = long.MaxValue, high = 0;
        foreach (string name in new[] { "Small", "get_Value", "Forced" })
        {
            long entry = Entry(probe.GetMethod(name, Declared));
            low = Math.Min(low, entry); high = Math.Max(high, entry + 8);
        }
        for (int index = 0; index < 300; index++) Fill();
        long start = low & ~4095L, length = ((high + 4095) & ~4095L) - start;
        bool locked = mprotect((IntPtr)start, (UIntPtr)(ulong)length, CanRead | CanExecute) == 0;
        ack = locked ? Send(1, target) : ack;
        bool unlocked = mprotect((IntPtr)start, (UIntPtr)(ulong)length, CanRead | CanWrite | CanExecute) == 0;
        Check(locked && unlocked, "the harness took write access from the pages of the probes and gave it back");
        Refused(ack, "write", "The inlining check could not run", "a request with a redirect while the probes cannot be written");
        Check(Json.Text(ack, "error").Contains("not in a writable and executable mapping") && !Flag(ack, "inliningChecked") && !Flag(ack, "inliningDisabled"),
            "the ack names the reason and still reports the JIT as not checked: a JIT that inlines is another failure");
        ack = Send(2, target);
        Applied(ack, 2, 1, 0, "the next request checks again and is applied");
        Check(Flag(ack, "inliningChecked") && Flag(ack, "inliningDisabled") && Player.Twice(4) == 1008, "the JIT is reported as checked, and the new body runs");
    }

    // What the bridge decides alone: it bounds and validates the request, loads the applier once and
    // by hash, and answers in its place when it cannot run.
    private static void Bridge(List<object> target)
    {
        Console.WriteLine("the bridge");
        Check(CodeReload.Generation == 0 && Poll() == null, "without a request file nothing happens; the Player is at code generation zero");
        File.WriteAllText(requestPath, "{ not json");
        Rejected(Poll(), false, string.Empty, "an unreadable request");
        File.WriteAllText(requestPath, "{\"requestId\":\"large\",\"padding\":\"" + new string('x', 70000) + "\"}");
        Rejected(Poll(), false, "exceeds the size limit", "a request above 64 KiB");
        Rejected(Send(1, target, request => request["schemaVersion"] = 2L), true, "schema or applier is invalid", "another schema version");
        Rejected(Send(1, target, request => request["applier"] = "applier/UnityMicroEditor.HotReload.dll"), true, "schema or applier is invalid", "a relative applier path");
        Rejected(Send(1, target, request => request["applierSha256"] = new string('0', 64)), true, "differs from its request hash", "an applier that is not the one the request names");
        Rejected(Send(1, target, request => request["applier"] = Path.Combine(run, "absent.dll")), true, "missing or exceeds the size limit", "a missing applier");
        Check(Applier() == null, "no applier is loaded for a rejected request");

        // The bootstrap polls this request and the scene reload request on one clock, code first.
        // A clock of its own here would let a frame poll the scene request alone.
        File.WriteAllText(requestPath, "{\"schemaVersion\":1,\"requestId\":\"first\",\"applier\":\"relative\"}");
        bool taken = Json.Text(Poll(), "requestId") == "first";
        File.WriteAllText(requestPath, "{\"schemaVersion\":1,\"requestId\":\"second\",\"applier\":\"relative\"}");
        Check(taken && Json.Text(Poll(), "requestId") == "second", "the bridge keeps no clock of its own: every poll of its caller looks at the request file");
    }

    // The launch argument that names the request file, in the bridge's own parser.
    private static void Arguments()
    {
        Console.WriteLine("the launch argument");
        string[] launch = { "game", "--unity-cli-report", "/run/report.json", "--unity-cli-frames", "1", "--unity-cli-raw-manifest", "/revision/scene.json", "--unity-cli-interactive" };
        Equal(Parsed(launch, "--unity-cli-code-reload", "/run/code-reload-request.json"), "/run/code-reload-request.json", "--unity-cli-code-reload names the request file of an interactive raw run");
        Equal(Parsed(launch, "--unity-cli-raw-reload", "/run/reload-request.json", "--unity-cli-code-reload", "/run/code-reload-request.json", "--unity-cli-control", "/run/control"),
            "/run/code-reload-request.json", "beside the scene reload request and the control directory");
        Check(Parsed(launch) == null, "absent by default");
        bool refused = Unparsed(new[] { "game", "--unity-cli-report", "/run/report.json", "--unity-cli-frames", "1", "--unity-cli-raw-manifest", "/revision/scene.json", "--unity-cli-code-reload", "/run/request.json" },
                "requires an interactive raw manifest run")
            && Unparsed(new[] { "game", "--unity-cli-report", "/run/report.json", "--unity-cli-frames", "1", "--unity-cli-interactive", "--unity-cli-code-reload", "/run/request.json" }, "requires an interactive raw manifest run")
            && Unparsed(With(launch, "--unity-cli-code-reload", "/run/a.json", "--unity-cli-code-reload", "/run/b.json"), "Duplicate --unity-cli-code-reload")
            && Unparsed(With(launch, "--unity-cli-code-reload", "request.json"), "requires an absolute file path");
        Check(refused, "refused without an interactive raw run, twice, or with a relative path");
        refused = Unparsed(With(launch, "--unity-cli-raw-reload", "/run/reload.json", "--unity-cli-code-reload", "/run/reload.json"), "distinct paths")
            && Unparsed(With(launch, "--unity-cli-raw-reload", "/run/reload.json", "--unity-cli-code-reload", "/run/reload.json.ack.json"), "distinct paths")
            && Unparsed(With(launch, "--unity-cli-code-reload", "/run/report.json"), "distinct paths")
            && Unparsed(With(launch, "--unity-cli-code-reload", "/run/control", "--unity-cli-control", "/run/control"), "distinct paths");
        Check(refused, "refused when the request or its ack is another file of the run");
    }

    private static string[] With(string[] head, params string[] tail)
    {
        var all = new string[head.Length + tail.Length];
        head.CopyTo(all, 0); tail.CopyTo(all, head.Length);
        return all;
    }

    private static string Parsed(string[] head, params string[] tail)
    {
        CliPlayArguments.TryParse(With(head, tail), out CliPlayArguments options);
        return options.CodeReloadRequestPath;
    }

    private static bool Unparsed(string[] arguments, string fragment)
    {
        try { CliPlayArguments.TryParse(arguments, out _); return false; }
        catch (FormatException error) { return error.Message.Contains(fragment); }
    }

    // The ways an applier can fail the bridge, each in a process of its own: a Player loads one applier only.
    private static void Faulty(string kind)
    {
        Console.WriteLine("the bridge with a faulty applier: " + kind);
        string real = applierPath, realHash = applierHash;
        var none = new List<object>();
        if (kind == "entry")
        {
            // Any assembly without UnityMicroEditor.HotReload.Entry.
            applierPath = Path.Combine(output, "Game.Core.dll"); applierHash = Hash(applierPath);
            Rejected(Send(1, none), true, "no usable entry point", "an assembly without the entry point");
            Rejected(Send(1, none), true, "no usable entry point", "the same assembly again");
            Equal(Patches("Game.Core"), 1, "it was loaded once");
            applierPath = real; applierHash = realHash;
            Restart(Send(1, none), realHash, "the real applier after it");
            return;
        }
        applierPath = Path.Combine(output, "faulty", "UnityMicroEditor.HotReload.dll"); applierHash = Hash(applierPath);
        Dictionary<string, object> ack = Send(1, none);
        // An exception that escapes the applier may have left game code half redirected: the bridge asks for a restart.
        Check(Answers(ack) && !Flag(ack, "success") && Flag(ack, "restartRequired") && Json.Text(ack, "error") == "The faulty applier threw." && CodeReload.Generation == 0,
            "an entry point that throws: the bridge answers with restartRequired");
        ack = Send(2, none);
        Check(Answers(ack) && !Flag(ack, "success") && Flag(ack, "restartRequired") && Json.Text(ack, "error").Contains("returned no ack") && CodeReload.Generation == 0,
            "an entry point that returns no ack: the same");
        Send(3, none);
        Check(File.ReadAllText(requestPath + ".ack.json") == "{\"appliedGeneration\":7,\"published\":\"as it is\"}\n" && CodeReload.Generation == 7,
            "what an entry point returns is published unchanged, and its appliedGeneration is the Player's code generation");
    }

    // ------------------------------------------------------------------ the field store

    private static void FieldStore()
    {
        Console.WriteLine("field store");
        // Loaded from bytes like a patch assembly: its reference binds by name to the applier loaded above.
        Assembly probe = Assembly.Load(File.ReadAllBytes(Path.Combine(output, "probe", "FieldStore.Probe.dll")));
        Type type = probe.GetType("Probe", true);
        var bonus = Bind<Func<object, int>>(type, "Bonus"); var setBonus = Bind<Action<object, int>>(type, "SetBonus"); var addBonus = Bind<Func<object, int, int>>(type, "AddBonus");
        var bonusText = Bind<Func<object, string>>(type, "BonusText"); var setBonusText = Bind<Action<object, string>>(type, "SetBonusText");
        var title = Bind<Func<object, string>>(type, "Title"); var setTitle = Bind<Action<object, string>>(type, "SetTitle");
        var wide = Bind<Func<object, Wide>>(type, "Wide"); var setWideC = Bind<Action<object, long>>(type, "SetWideC");
        var any = Bind<Func<object, string, int>>(type, "Any"); var setAny = Bind<Action<object, string, int>>(type, "SetAny"); var count = Bind<Func<object, string, int>>(type, "Count");
        var total = Bind<Func<int>>(type, "Total"); var addTotal = Bind<Func<int, int>>(type, "AddTotal"); var totalAsLong = Bind<Func<long>>(type, "TotalAsLong");
        var label = Bind<Func<string>>(type, "Label"); var setLabel = Bind<Action<string>>(type, "SetLabel");
        var track = Bind<Action<object>>(type, "Track"); var spin = Bind<Func<object, int, long>>(type, "Spin");

        object one = new object(), two = new object();
        Equal(bonus(one) + "/" + (title(one) ?? "null") + "/" + wide(one).C, "0/null/0", "an existing object starts with the default value");
        setBonus(one, 5); setTitle(one, "first"); setWideC(one, 9);
        Equal(addBonus(one, 3) + "/" + bonus(one) + "/" + title(one) + "/" + wide(one).C + "/" + bonus(two), "8/8/first/9/0", "values are per object and are changed in place through the reference");
        setBonusText(one, "text");
        Equal(bonus(one) + "/" + bonusText(one), "8/text", "a field that changed its type has the same id and another slot");
        Equal(any(one, new string("Game|Game.Player|bonus".ToCharArray())), 8, "an id built at run time names the same slot as the literal");
        setAny(one, "Game|Game.Player|other", 4);
        Equal(any(one, "Game|Game.Player|other") + "/" + bonus(one), "4/8", "ids do not collide");
        Equal(total() + "/" + addTotal(5) + "/" + addTotal(2) + "/" + total() + "/" + totalAsLong() + "/" + (label() ?? "null"), "0/5/7/7/0/null", "static fields");
        setLabel("label");
        Equal(label(), "label", "a static field of a reference type");
        bool thrown = false;
        try { bonus(null); } catch (NullReferenceException) { thrown = true; }
        Check(thrown, "a null owner throws NullReferenceException, as a field access does");

        // Four threads create the slots of the same objects and count in them at the same time.
        var shared = new object[500];
        for (int index = 0; index < shared.Length; index++) shared[index] = new object();
        var workers = new List<Thread>();
        for (int worker = 0; worker < 4; worker++)
        {
            var thread = new Thread(() =>
            {
                for (int round = 0; round < 20; round++)
                    foreach (object owner in shared)
                        for (int field = 0; field < 6; field++) count(owner, "Game|Game.Player|counter" + field);
            });
            thread.Start(); workers.Add(thread);
        }
        foreach (Thread thread in workers) thread.Join();
        bool exact = true;
        foreach (object owner in shared)
            for (int field = 0; field < 6; field++) exact &= any(owner, "Game|Game.Player|counter" + field) == 80;
        Check(exact, "no update is lost when threads add and use fields of the same objects");

        // Owners die with their fields, also when a field value refers back to its owner.
        var filler = new Thread(() => { for (int index = 0; index < 2000; index++) track(new object()); });
        filler.Start(); filler.Join();
        for (int attempt = 0; attempt < 5; attempt++) { GC.Collect(); GC.WaitForPendingFinalizers(); }
        int finalized = (int)probe.GetType("Tracked", true).GetField("Finalized").GetValue(null);
        Check(finalized >= 1900, "dead owners are collected with their values (" + finalized + " of 2000 finalized)");
        Equal(bonus(one) + "/" + title(one), "8/first", "a live owner keeps its values across collections");

        spin(one, 1000);
        Stopwatch watch = Stopwatch.StartNew();
        spin(one, 2000000);
        double nanoseconds = watch.Elapsed.TotalMilliseconds * 1e6 / 2000000;
        Check(nanoseconds < 1000, "an instance field access costs " + nanoseconds.ToString("0", CultureInfo.InvariantCulture) + " ns");
        GC.KeepAlive(two);
    }

    // ------------------------------------------------------------------ requests and acks

    private static Dictionary<string, object> Send(long number, List<object> targets, Action<Dictionary<string, object>> editRequest = null, Action<Dictionary<string, object>> editDescriptor = null)
    {
        generation = Math.Max(generation, number);
        string directory = Path.Combine(run, "hot-reload", (++requests).ToString(CultureInfo.InvariantCulture));
        Directory.CreateDirectory(directory);
        var descriptor = new Dictionary<string, object>
        {
            ["schemaVersion"] = 1L, ["generation"] = number, ["gameAssemblies"] = new List<object> { "Game", "Game.Core" }, ["assemblies"] = targets,
        };
        if (editDescriptor != null) editDescriptor(descriptor);
        string descriptorPath = Path.Combine(directory, "apply.json");
        File.WriteAllText(descriptorPath, Json.Write(descriptor));
        var request = new Dictionary<string, object>
        {
            ["schemaVersion"] = 1L, ["requestId"] = "request-" + requests, ["generation"] = number, ["applier"] = applierPath, ["applierSha256"] = applierHash,
            ["descriptor"] = descriptorPath, ["descriptorSha256"] = Hash(descriptorPath),
            ["requestedUtcUnixMilliseconds"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), ["sourceChangedUtcUnixMilliseconds"] = 0L,
        };
        if (editRequest != null) editRequest(request);
        File.WriteAllText(requestPath, Json.Write(request));
        return Poll();
    }

    /// <summary>Lets the bridge poll, as the bootstrap does on its clock, and returns the ack file as it is then.</summary>
    private static Dictionary<string, object> Poll()
    {
        bridge.Poll();
        return File.Exists(requestPath + ".ack.json") ? (Dictionary<string, object>)Json.Parse(File.ReadAllText(requestPath + ".ack.json")) : null;
    }

    /// <summary>Whether the ack answers the request sent last, completely.</summary>
    private static bool Answers(Dictionary<string, object> ack, bool identified = true)
    {
        foreach (string member in new[] { "schemaVersion", "requestId", "pid", "success", "generation", "appliedGeneration", "restartRequired", "error", "failures", "detoursApplied",
            "detoursRestored", "callbacksInvoked", "callbackErrors", "inliningDisabled", "elapsedSeconds", "completedUtcUnixMilliseconds" })
            if (!ack.ContainsKey(member)) return false;
        return Json.Text(ack, "requestId") == (identified ? "request-" + requests : string.Empty) && Json.Integer(ack, "schemaVersion") == 1
            && Json.Integer(ack, "pid") == Process.GetCurrentProcess().Id && Json.Integer(ack, "completedUtcUnixMilliseconds") > 0 && CodeReload.Generation == applied;
    }

    private static Dictionary<string, object> Target(string game, Patch patch, params Dictionary<string, object>[] detours)
    {
        Assembly loaded = null;
        foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies()) if (assembly.GetName().Name == game) loaded = assembly;
        string symbols = Path.ChangeExtension(patch.File, ".pdb");
        return new Dictionary<string, object>
        {
            ["name"] = game, ["loadedMvid"] = loaded.ManifestModule.ModuleVersionId.ToString(), ["patch"] = patch.File, ["patchSha256"] = Hash(patch.File),
            ["pdb"] = File.Exists(symbols) ? symbols : string.Empty, ["detours"] = new List<object>(detours),
        };
    }

    private static Dictionary<string, object> Detour(MethodBase original, Patch patch, string replacement, string kind = "changed")
    {
        return new Dictionary<string, object>
        {
            ["member"] = original.DeclaringType.FullName + "::" + original.Name, ["kind"] = kind,
            ["originalToken"] = (long)original.MetadataToken, ["replacementToken"] = patch.Tokens[replacement],
        };
    }

    private static Dictionary<string, object> With(Dictionary<string, object> map, string key, object value) { map[key] = value; return map; }
    private static bool Flag(Dictionary<string, object> ack, string key) { return (bool)ack[key]; }

    /// <summary>Whether the ack has what only the applier reports, beyond what the bridge answers with in its place.</summary>
    private static bool Complete(Dictionary<string, object> ack)
    {
        foreach (string member in new[] { "islandWrites", "gatedWrites", "inliningChecked", "initializersRun" }) if (!ack.ContainsKey(member)) return false;
        return true;
    }

    private static void Applied(Dictionary<string, object> ack, long number, long redirects, long restored, string what)
    {
        applied = number;
        bool ok = Answers(ack) && Complete(ack) && Flag(ack, "success") && !Flag(ack, "restartRequired") && Json.Integer(ack, "generation") == number && Json.Integer(ack, "appliedGeneration") == number
            && Json.Text(ack, "error").Length == 0 && ((List<object>)ack["failures"]).Count == 0 && Json.Integer(ack, "detoursApplied") == redirects
            && Json.Integer(ack, "detoursRestored") == restored;
        Check(ok, what + (ok ? string.Empty : ": " + Json.Write(ack)));
    }

    // Refused by the applier: nothing written, the applied generation stays, and the reason is named with its stage.
    private static void Refused(Dictionary<string, object> ack, string stage, string fragment, string what)
    {
        bool named = false;
        foreach (Dictionary<string, object> failure in (List<object>)ack["failures"])
            named |= Json.Text(failure, "stage") == stage && (Json.Text(failure, "member") + " " + Json.Text(failure, "message")).Contains(fragment);
        bool ok = named && Complete(ack) && Untouched(ack, true) && Json.Integer(ack, "islandWrites") == 0 && Json.Integer(ack, "gatedWrites") == 0;
        Check(ok, "refused, nothing written: " + what + (ok ? string.Empty : ": " + Json.Write(ack)));
    }

    // A request that names another applier than the one this process loaded. The bridge answers it
    // so that the runner can act on the kind: a failure of stage "applier" and restartRequired.
    private static void Restart(Dictionary<string, object> ack, string named, string what)
    {
        var failures = (List<object>)ack["failures"];
        bool ok = Answers(ack) && !Flag(ack, "success") && Flag(ack, "restartRequired") && Json.Integer(ack, "appliedGeneration") == applied && failures.Count == 1
            && Json.Text((Dictionary<string, object>)failures[0], "stage") == "applier" && Json.Text((Dictionary<string, object>)failures[0], "message") == Json.Text(ack, "error")
            && Json.Text(ack, "error").Contains("cannot take applier " + named) && Json.Text(ack, "error").Contains("Restart the Player")
            && Json.Integer(ack, "detoursApplied") == 0 && Same(Words(), expected);
        Check(ok, "answered by the bridge with a restart: " + what + (ok ? string.Empty : ": " + Json.Write(ack)));
    }

    // Rejected by the bridge itself, before the applier ran.
    private static void Rejected(Dictionary<string, object> ack, bool identified, string fragment, string what)
    {
        bool ok = ack != null && Json.Text(ack, "error").Contains(fragment) && ((List<object>)ack["failures"]).Count == 0 && Untouched(ack, identified);
        Check(ok, "rejected by the bridge: " + what + (ok ? string.Empty : ": " + (ack == null ? "no ack" : Json.Write(ack))));
    }

    private static bool Untouched(Dictionary<string, object> ack, bool identified)
    {
        return Answers(ack, identified) && !Flag(ack, "success") && !Flag(ack, "restartRequired") && Json.Integer(ack, "appliedGeneration") == applied && Json.Text(ack, "error").Length != 0
            && Json.Integer(ack, "detoursApplied") == 0 && Json.Integer(ack, "detoursRestored") == 0 && Json.Integer(ack, "callbacksInvoked") == 0 && Same(Words(), expected);
    }

    // ------------------------------------------------------------------ compiled code

    private static long Entry(MethodBase method) { return method.MethodHandle.GetFunctionPointer().ToInt64(); }

    private static byte First(MethodBase method) { return Marshal.ReadByte((IntPtr)Entry(method)); }

    /// <summary>
    /// Where a jump made of E9 and bytes 1 to 4 of the body lands. While those bytes are the ones
    /// the JIT compiled, that is where the island of the method is, or would be.
    /// </summary>
    private static long Island(MethodBase method)
    {
        long entry = Entry(method);
        return entry + 5 + Marshal.ReadInt32((IntPtr)(entry + 1));
    }

    /// <summary>Where a call of the method arrives: in its own body, or where its jump leads, through the pointer slot if that is an island.</summary>
    private static long Lands(MethodBase method)
    {
        long entry = Entry(method), start = Marshal.ReadInt64((IntPtr)entry);
        if ((start & 0xFFFFFF) == 0xEC8348 && ((start >> 32) & 0xFF) == 0xE9)
        {
            // The jump is the second instruction, and its relay starts with add rsp, imm8.
            long relay = entry + 9 + Marshal.ReadInt32((IntPtr)(entry + 5));
            if ((Marshal.ReadInt32((IntPtr)relay) & 0xFFFFFF) != 0xC48348 || Marshal.ReadByte((IntPtr)(relay + 3)) != Marshal.ReadByte((IntPtr)(entry + 3))) return 0;
            return Marshal.ReadInt64((IntPtr)(relay + 10 + Marshal.ReadInt32((IntPtr)(relay + 6))));
        }
        if ((start & 0xFF) != 0xE9) return entry;
        long target = Island(method);
        // jmp [rip+disp]
        if (Marshal.ReadByte((IntPtr)target) != 0xFF || Marshal.ReadByte((IntPtr)(target + 1)) != 0x25) return target;
        return Marshal.ReadInt64((IntPtr)(target + 6 + Marshal.ReadInt32((IntPtr)(target + 2))));
    }

    /// <summary>sub rsp, imm8; mov r32, imm32: a body that first loads a constant.</summary>
    private static bool LoadsConstant(long start) { return (start & 0xFFFFFF) == 0xEC8348 && ((start >> 32) & 0xF8) == 0xB8; }

    /// <summary>Whether the body was redirected by its first byte alone.</summary>
    private static bool ByFirstByte(long now, long was) { return (now & 0xFF) == 0xE9 && ((now ^ was) & ~0xFFL) == 0; }

    /// <summary>Whether the body kept its first instruction and has a jump for its second.</summary>
    private static bool BySecondInstruction(long now, long was) { return ((now ^ was) & 0xFFFFFFFFL) == 0 && ((now >> 32) & 0xFF) == 0xE9; }

    /// <summary>Whether the pointer slot of the method's island lies before its jump.</summary>
    private static bool SlotBefore(MethodBase method) { return Marshal.ReadInt32((IntPtr)(Island(method) + 2)) < 0; }

    private static KeyValuePair<MethodBase, MethodBase> Pair(MethodBase original, Patch patch, string replacement)
    {
        string name = Path.GetFileNameWithoutExtension(patch.File);
        foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies())
            if (assembly.GetName().Name == name) return new KeyValuePair<MethodBase, MethodBase>(original, assembly.ManifestModule.ResolveMethod((int)patch.Tokens[replacement]));
        throw new InvalidOperationException("The patch assembly is not loaded: " + name);
    }

    /// <summary>Whether a call of each original arrives at the compiled body of its replacement.</summary>
    private static bool Lead(params KeyValuePair<MethodBase, MethodBase>[] pairs)
    {
        foreach (KeyValuePair<MethodBase, MethodBase> pair in pairs) if (Lands(pair.Key) != Entry(pair.Value)) return false;
        return true;
    }

    private static long Call(MethodBase method, long value) { return (long)method.Invoke(null, new object[] { value }); }

    /// <summary>Compiles one more method of 16 bytes: the JIT's next body starts 16 bytes after the returned address.</summary>
    private static long Fill()
    {
        // One reflection call for all of them: a lookup by name walks the whole type.
        if (fill == null) fill = typeof(Game.Fill).GetMethods(BindingFlags.Public | BindingFlags.Static | BindingFlags.DeclaredOnly);
        return Entry(fill[fillers++]);
    }

    /// <summary>Compiles the method at the given offset in its page, by filling the JIT's code area up to there. False when it landed elsewhere.</summary>
    private static bool Place(MethodBase method, int offset)
    {
        for (int attempt = 0; attempt < 600; attempt++)
        {
            if (((Fill() + 16) & 4095) != offset) continue;
            return (Entry(method) & 4095) == offset;
        }
        return false;
    }

    /// <summary>Takes the pages where the island of the method would start, as another mapping of the process does.</summary>
    private static List<long> Occupy(MethodBase method)
    {
        var taken = new List<long>();
        long island = Island(method);
        for (long page = island & ~4095L; page < island + 6; page += 4096)
        {
            long got = mmap((IntPtr)page, (UIntPtr)4096, CanRead, PrivateAnonymousNoReplace, -1, IntPtr.Zero).ToInt64();
            if (got == page) taken.Add(page);
            else if (got != -1) munmap((IntPtr)got, (UIntPtr)4096);
        }
        return taken;
    }

    private static void Vacate(List<long> pages)
    {
        foreach (long page in pages) munmap((IntPtr)page, (UIntPtr)4096);
    }

    /// <summary>Whether every byte of the range lies in mappings of the process whose permissions start as given.</summary>
    private static bool Mapped(long low, long high, string permissions)
    {
        long covered = low;
        foreach (string line in File.ReadAllLines("/proc/self/maps"))
        {
            int dash = line.IndexOf('-'), space = line.IndexOf(' ');
            ulong start = ulong.Parse(line.Substring(0, dash), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture), end = ulong.Parse(line.Substring(dash + 1, space - dash - 1), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture);
            if (end > long.MaxValue || (long)start > covered || (long)end <= covered) continue;
            if (!line.Substring(space + 1).StartsWith(permissions, StringComparison.Ordinal)) return false;
            covered = (long)end;
            if (covered >= high) return true;
        }
        return false;
    }

    // ------------------------------------------------------------------ helpers

    private static string Greet<T>(T greeter, string name) where T : IGreeter { return greeter.Greet(name); }

    private static string From(MethodBase method) { return method.DeclaringType.Assembly.GetName().Name; }

    private static Assembly Applier()
    {
        foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies()) if (assembly.GetName().Name == "UnityMicroEditor.HotReload") return assembly;
        return null;
    }

    private static string Milliseconds(double seconds, int count) { return (seconds * 1000 / count).ToString("0.000", CultureInfo.InvariantCulture) + " ms"; }

    private static MethodBase Method(Type type, string name)
    {
        MethodBase method = type.GetMethod(name, Declared);
        if (method == null) throw new MissingMethodException(type.FullName, name);
        return method;
    }

    private static T Bind<T>(Type type, string name) where T : Delegate
    {
        return (T)Delegate.CreateDelegate(typeof(T), type.GetMethod(name));
    }

    /// <summary>
    /// Adds methods to the watched ones. They are compiled by now at the latest, and nothing has
    /// redirected them: what they start with is what the JIT made.
    /// </summary>
    private static void Watch(params MethodBase[] methods)
    {
        foreach (MethodBase method in methods)
        {
            watched.Add(method);
            compiled.Add(Marshal.ReadInt64((IntPtr)Entry(method)));
        }
        expected = Words();
    }

    /// <summary>The first eight bytes of every watched method as the JIT compiled it.</summary>
    private static long[] Compiled() { return compiled.ToArray(); }

    /// <summary>The first eight bytes of every watched method's compiled body, or of the given ones.</summary>
    private static long[] Words(params MethodBase[] methods)
    {
        if (methods.Length == 0) methods = watched.ToArray();
        var words = new long[methods.Length];
        for (int index = 0; index < words.Length; index++) words[index] = Marshal.ReadInt64(methods[index].MethodHandle.GetFunctionPointer());
        return words;
    }

    /// <summary>The words of the given methods out of an array that has one for every watched method.</summary>
    private static long[] Pick(long[] words, params MethodBase[] methods)
    {
        var picked = new long[methods.Length];
        for (int index = 0; index < methods.Length; index++) picked[index] = words[watched.IndexOf(methods[index])];
        return picked;
    }

    /// <summary>Whether each of the given methods differs between the two arrays in nothing but a first byte of E9.</summary>
    private static bool OnlyFirstByte(long[] now, long[] before, params MethodBase[] methods)
    {
        foreach (MethodBase method in methods)
        {
            int index = watched.IndexOf(method);
            if ((now[index] & 0xFF) != 0xE9 || ((now[index] ^ before[index]) & ~0xFFL) != 0) return false;
        }
        return true;
    }

    private static bool Same(long[] left, long[] right)
    {
        for (int index = 0; index < left.Length; index++) if (left[index] != right[index]) return false;
        return true;
    }

    private static bool Differs(long[] left, long[] right, int count)
    {
        int different = 0;
        for (int index = 0; index < left.Length; index++)
        {
            if (left[index] == right[index]) continue;
            // A redirected entry starts with E9 and keeps its bytes 5 to 7, or keeps its first four bytes before an E9.
            if (((left[index] & 0xFF) != 0xE9 || (left[index] >> 40) != (right[index] >> 40)) && !BySecondInstruction(left[index], right[index])) return false;
            different++;
        }
        return different == count;
    }

    private static int Patches(string name)
    {
        int count = 0;
        foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies()) if (assembly.GetName().Name.Contains(name)) count++;
        return count;
    }

    private static Patch Load(string name)
    {
        var patch = new Patch { File = Path.Combine(output, "patches", name + ".dll") };
        foreach (string line in File.ReadAllLines(patch.File + ".tokens"))
        {
            int split = line.LastIndexOf('=');
            patch.Tokens[line.Substring(0, split)] = long.Parse(line.Substring(split + 1), CultureInfo.InvariantCulture);
        }
        return patch;
    }

    private static int Tokens(string[] arguments)
    {
        for (int index = 1; index < arguments.Length; index++)
        {
            var lines = new List<string>();
            foreach (Type type in Assembly.LoadFrom(arguments[index]).GetTypes())
            {
                var members = new List<MethodBase>(type.GetMethods(Declared));
                members.AddRange(type.GetConstructors(Declared));
                foreach (MethodBase member in members) lines.Add(type.FullName + "::" + member.Name + "=" + member.MetadataToken.ToString(CultureInfo.InvariantCulture));
            }
            File.WriteAllLines(arguments[index] + ".tokens", lines);
        }
        return 0;
    }

    private static string Hash(string path)
    {
        var text = new StringBuilder(64);
        using (SHA256 hash = SHA256.Create())
            foreach (byte value in hash.ComputeHash(File.ReadAllBytes(path))) text.Append(value.ToString("x2", CultureInfo.InvariantCulture));
        return text.ToString();
    }

    private static void Equal<T>(T actual, T wanted, string what)
    {
        bool ok = EqualityComparer<T>.Default.Equals(actual, wanted);
        Check(ok, what + (ok ? string.Empty : " (expected " + wanted + ", got " + actual + ")"));
    }

    private static void Check(bool ok, string what)
    {
        if (ok) passed++; else failed++;
        Console.WriteLine((ok ? "  PASS " : "  FAIL ") + what);
    }
}
