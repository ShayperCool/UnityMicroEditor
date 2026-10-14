// The Player side of hot reload. The runner compiles this directory into UnityMicroEditor.HotReload.dll
// for a session; the Player bridge loads it on the first code reload request and calls Entry.Handle
// on the main thread, between two frames. It is not part of the bridge, so it changes without a
// host rebuild.
//
// A request names a descriptor: per game assembly a patch assembly and the methods it replaces,
// by metadata token. A descriptor is cumulative. It lists everything that differs from the
// assemblies the Player loaded at its start and replaces the generation before it entirely: methods
// it no longer lists get their original code back, and an empty one restores everything.
//
// One request is all or nothing. Everything is resolved, checked and compiled first, and the way
// every method will be written is decided; only when nothing failed are the redirects written,
// then the dropped ones restored, then the callbacks run.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Security.Cryptography;
using System.Text;

namespace UnityMicroEditor.HotReload
{
    /// <summary>Bound by the Player bridge by name.</summary>
    public static class Entry
    {
        private static readonly Applier applier = new Applier();

        /// <summary>Applies the code reload request in the file and returns the text of its ack. Never throws.</summary>
        public static string Handle(string requestPath)
        {
            lock (applier) return applier.Handle(requestPath);
        }
    }

    internal sealed class Applier
    {
        private const int RequestLimit = 65536, DescriptorLimit = 16 << 20, AssemblyLimit = 64 << 20, FailureLimit = 32, MessageLimit = 1024;
        private const BindingFlags Declared = BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static | BindingFlags.Instance | BindingFlags.DeclaredOnly;
        private const string InliningAdvice = "The JIT of this Player inlines small methods, so a redirect would not reach every caller. Start the Player with MONO_ARGUMENTS=-O=-float32,-inline.";

        private sealed class Redirect
        {
            internal MethodBase Original;
            internal MethodBase Replacement;   // null while the original code runs again
            internal IntPtr From, To;
            internal ulong Saved;              // first word of the original body, read before the first write
            internal byte Rest;                // and its ninth byte: the end of a second instruction of five bytes
            internal Island Island, Relay;     // kept once mapped: a later first redirect uses them again
            internal int Way;                  // how the body is redirected now, or reaches its kept island
        }

        // The ways into a replacement. Each replaces bytes of one instruction of the body only.
        // ByGatedIsland rewrites the whole first instruction (sub rsp, imm8) behind the gate to
        // reach an island in a 16 MiB window, when the fixed island address is taken.
        private const int ByFirstByte = 0, BySecondInstruction = 1, ByWholeJump = 2, ByGatedIsland = 3;

        private sealed class Step
        {
            internal string Assembly, Member;
            internal bool Removed;
            internal int OriginalToken, ReplacementToken;
            internal MethodBase Original, Replacement;
            internal IntPtr From, To;
            internal Redirect Redirect;
            internal bool First;               // decided before anything is written: a first redirect,
            internal int Way;                  // and the way it takes
        }

        private sealed class Target
        {
            internal string Name, Path, Sha256, Pdb;
            internal Guid Mvid;
            internal byte[] Raw, Symbols;
            internal Assembly Loaded, Patch;
            internal readonly List<Step> Steps = new List<Step>();
        }

        private sealed class Result
        {
            internal string RequestId = string.Empty;
            internal long Generation;
            internal bool RestartRequired, InitializersRun;
            internal int Failed, Applied, Restored, IslandWrites, GatedWrites, Invoked, CallbackErrors;
            internal readonly List<object> Failures = new List<object>();
            internal string Error = string.Empty;

            internal void Fail(string assembly, string member, string stage, string message)
            {
                if (message.Length > MessageLimit) message = message.Substring(0, MessageLimit);
                if (Failed++ == 0) Error = message;
                if (Failures.Count < FailureLimit)
                    Failures.Add(new Dictionary<string, object> { ["assembly"] = assembly, ["member"] = member, ["stage"] = stage, ["message"] = message });
            }
        }

        // Keyed by the entry of the original method.
        private readonly Dictionary<long, Redirect> redirects = new Dictionary<long, Redirect>();
        // Patch assemblies by SHA-256. Nothing is ever unloaded.
        private readonly Dictionary<string, Assembly> patches = new Dictionary<string, Assembly>(StringComparer.Ordinal);
        private readonly HashSet<Assembly> compiled = new HashSet<Assembly>();
        // Methods whose entry was asked for before: their types are initialized.
        private readonly HashSet<IntPtr> asked = new HashSet<IntPtr>();
        private Host host;
        private Callbacks callbacks;
        private long applied;
        private bool writing;
        private int inlining;   // 0 not decided, 1 disabled, 2 the JIT inlines
        private string undecided;
        private string lastRequest, lastAck;

        internal string Handle(string requestPath)
        {
            Stopwatch watch = Stopwatch.StartNew();
            var result = new Result();
            try
            {
                if (!Apply(requestPath, result)) return lastAck;
            }
            catch (Exception error)
            {
                // Before the first store the Player is untouched. Between the first
                // and the last one, methods may be left half redirected.
                result.Fail(string.Empty, string.Empty, writing ? "write" : "resolve", Explain(error));
                result.RestartRequired |= writing;
            }
            writing = false;
            if (result.Failed > 1) result.Error = result.Failed + " failures; the first: " + result.Error;
            var ack = new Dictionary<string, object>
            {
                ["schemaVersion"] = 1L, ["requestId"] = result.RequestId, ["pid"] = (long)Process.GetCurrentProcess().Id,
                ["success"] = result.Failed == 0, ["generation"] = result.Generation, ["appliedGeneration"] = applied,
                ["restartRequired"] = result.RestartRequired, ["error"] = result.Error, ["failures"] = result.Failures,
                ["detoursApplied"] = (long)result.Applied, ["detoursRestored"] = (long)result.Restored,
                ["islandWrites"] = (long)result.IslandWrites, ["gatedWrites"] = (long)result.GatedWrites,
                ["callbacksInvoked"] = (long)result.Invoked, ["callbackErrors"] = (long)result.CallbackErrors,
                ["inliningDisabled"] = inlining == 1, ["inliningChecked"] = inlining != 0, ["initializersRun"] = result.InitializersRun,
                ["elapsedSeconds"] = watch.Elapsed.TotalSeconds,
                ["completedUtcUnixMilliseconds"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
            };
            lastRequest = result.RequestId; lastAck = Json.Write(ack) + "\n";
            return lastAck;
        }

        /// <summary>False when the request was answered before; its ack is repeated and nothing runs twice.</summary>
        private bool Apply(string requestPath, Result result)
        {
            Dictionary<string, object> request = ReadObject(requestPath, RequestLimit, null, "The code reload request");
            result.RequestId = Json.Text(request, "requestId");
            result.Generation = Json.Integer(request, "generation");
            if (result.RequestId.Length != 0 && result.RequestId == lastRequest) return false;
            if (Json.Integer(request, "schemaVersion") != 1 || result.RequestId.Length == 0 || result.RequestId.Length > 128)
                throw new InvalidDataException("The code reload request has an invalid identity or schema.");
            Dictionary<string, object> descriptor = ReadObject(Json.Text(request, "descriptor"), DescriptorLimit, Json.Text(request, "descriptorSha256"), "The code patch descriptor");
            if (Json.Integer(descriptor, "schemaVersion") != 1 || Json.Integer(descriptor, "generation") != result.Generation)
                throw new InvalidDataException("The code patch descriptor differs from its request in schema or generation.");
            if (result.Generation <= applied)
                throw new InvalidDataException("Code generation " + result.Generation + " is not newer than the applied generation " + applied + ".");

            List<Target> targets = Read(descriptor, result);
            int steps = 0;
            foreach (Target target in targets) steps += target.Steps.Count;
            if (steps != 0)
            {
                int state = Inlining();
                if (state == 2) result.Fail(string.Empty, string.Empty, "inlining", InliningAdvice);
                // Not a JIT that inlines: nothing is known about it, and the next request checks again.
                else if (state == 0) result.Fail(string.Empty, string.Empty, "write", "The inlining check could not run, so no redirect is written. " + undecided);
            }
            if (result.Failed != 0) return true;

            var loaded = new Dictionary<string, List<Assembly>>(StringComparer.Ordinal);
            foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies())
            {
                if (assembly.IsDynamic) continue;
                string name = assembly.GetName().Name;
                if (!loaded.TryGetValue(name, out List<Assembly> named)) loaded.Add(name, named = new List<Assembly>());
                named.Add(assembly);
            }
            foreach (Target target in targets) Locate(target, loaded, result);
            if (result.Failed != 0) return true;
            // A patch assembly is loaded only for originals that all exist: it can never be unloaded.
            foreach (Target target in targets) Load(target, loaded, result);
            if (result.Failed != 0) return true;
            Compile(targets, result);
            if (result.Failed != 0) return true;

            List<Change> changes = Write(targets, result);
            applied = result.Generation;
            if (result.Applied + result.Restored != 0) Notify(descriptor, targets, loaded, changes, result);
            return true;
        }

        private static List<Target> Read(Dictionary<string, object> descriptor, Result result)
        {
            var targets = new List<Target>();
            var names = new HashSet<string>(StringComparer.Ordinal);
            foreach (object entry in Json.Array(descriptor, "assemblies"))
            {
                var map = entry as Dictionary<string, object> ?? throw new InvalidDataException("The code patch descriptor lists an assembly that is not an object.");
                var target = new Target { Name = Json.Text(map, "name"), Path = Json.Text(map, "patch"), Sha256 = Json.Text(map, "patchSha256"), Pdb = Json.Text(map, "pdb") };
                if (target.Name.Length == 0 || !names.Add(target.Name) || !Guid.TryParse(Json.Text(map, "loadedMvid"), out target.Mvid))
                    throw new InvalidDataException("The code patch descriptor names an assembly twice or without a valid MVID: " + target.Name);
                foreach (object detour in Json.Array(map, "detours"))
                {
                    var fields = detour as Dictionary<string, object> ?? throw new InvalidDataException("The code patch descriptor lists a detour that is not an object.");
                    var step = new Step { Assembly = target.Name, Member = Json.Text(fields, "member"), Removed = Json.Text(fields, "kind") == "removed" };
                    long original = Json.Integer(fields, "originalToken"), replacement = Json.Integer(fields, "replacementToken");
                    // Only method definitions: a reference or a specification could name a generic instantiation.
                    if (original >> 24 != 0x06 || replacement >> 24 != 0x06)
                    { result.Fail(target.Name, step.Member, "resolve", "A detour needs two method definition tokens."); continue; }
                    step.OriginalToken = (int)original; step.ReplacementToken = (int)replacement;
                    target.Steps.Add(step);
                }
                try
                {
                    target.Raw = ReadFile(target.Path, AssemblyLimit, target.Sha256, "The patch assembly");
                    if (target.Pdb.Length != 0) target.Symbols = ReadFile(target.Pdb, AssemblyLimit, null, "The patch symbol file");
                }
                catch (Exception error) { result.Fail(target.Name, string.Empty, "resolve", Explain(error)); }
                targets.Add(target);
            }
            return targets;
        }

        private static void Locate(Target target, Dictionary<string, List<Assembly>> loaded, Result result)
        {
            if (loaded.TryGetValue(target.Name, out List<Assembly> named))
                foreach (Assembly assembly in named)
                    if (assembly.ManifestModule.ModuleVersionId == target.Mvid) { target.Loaded = assembly; break; }
            if (target.Loaded == null)
            {
                // Tokens are only valid for the exact module they were read from.
                result.Fail(target.Name, string.Empty, "resolve", named == null ? "The assembly is not loaded in this Player."
                    : "The loaded assembly has MVID " + named[0].ManifestModule.ModuleVersionId + ", the patch was built against " + target.Mvid + ".");
                return;
            }
            foreach (Step step in target.Steps)
            {
                try { step.Original = target.Loaded.ManifestModule.ResolveMethod(step.OriginalToken); }
                catch (Exception error)
                {
                    result.Fail(target.Name, step.Member, "resolve", "Original token 0x" + step.OriginalToken.ToString("x8", CultureInfo.InvariantCulture) + " does not resolve. " + Explain(error));
                    continue;
                }
                string problem = step.Original.IsConstructor && step.Original.IsStatic ? "is a static constructor, which has already run" : Unpatchable(step.Original);
                if (problem != null) result.Fail(target.Name, step.Member, "resolve", "The original method " + problem + ".");
            }
        }

        private void Load(Target target, Dictionary<string, List<Assembly>> loaded, Result result)
        {
            try
            {
                if (!patches.TryGetValue(target.Sha256, out target.Patch))
                {
                    // Without the declaration the JIT refuses the patch's access to non-public fields, and a
                    // call of a non-public method compiles and throws only when it runs.
                    if (!DeclaresSkipVerification(target.Raw))
                        throw new InvalidDataException("The assembly does not declare SecurityPermission(RequestMinimum, SkipVerification = true).");
                    // A reference by name binds to the first assembly loaded under that name.
                    string name = AssemblyName.GetAssemblyName(target.Path).Name;
                    if (loaded.ContainsKey(name))
                        throw new InvalidDataException("An assembly named " + name + " is already loaded; every patch assembly needs a name of its own.");
                    target.Patch = target.Symbols == null ? Assembly.Load(target.Raw) : Assembly.Load(target.Raw, target.Symbols);
                    patches.Add(target.Sha256, target.Patch);
                    loaded.Add(name, new List<Assembly> { target.Patch });
                }
            }
            catch (Exception error)
            {
                result.Fail(target.Name, string.Empty, "resolve", "The patch assembly does not load. " + Explain(error));
                return;
            }
            foreach (Step step in target.Steps)
            {
                try
                {
                    step.Replacement = target.Patch.ManifestModule.ResolveMethod(step.ReplacementToken);
                    string problem = Unpatchable(step.Replacement);
                    problem = problem == null ? Mismatch(step.Original, step.Replacement) : "The replacement method " + problem + ".";
                    if (problem != null) result.Fail(target.Name, step.Member, "resolve", problem);
                }
                catch (Exception error)
                {
                    result.Fail(target.Name, step.Member, "resolve", "Replacement token 0x" + step.ReplacementToken.ToString("x8", CultureInfo.InvariantCulture) + " does not resolve. " + Explain(error));
                }
            }
        }

        private void Compile(List<Target> targets, Result result)
        {
            foreach (Target target in targets)
                foreach (Step step in target.Steps)
                {
                    try { step.To = EntryOf(step.Replacement, result); }
                    catch (Exception error) { Uncompiled(targets, step.Assembly, step.Member, "The replacement does not compile. ", error, result); continue; }
                    try { step.From = EntryOf(step.Original, result); }
                    catch (Exception error) { Uncompiled(targets, step.Assembly, step.Member, "The original does not compile. ", error, result); }
                }
            // A request that is refused anyway compiles no more than it has: compiling runs initializers of the game.
            if (result.Failed != 0) return;
            Plan(targets, result);
            if (result.Failed != 0) return;
            // The rest of the patch (added methods, accessors, closures) is called from the
            // replacements. Compiling it now turns a later exception in game code into a refusal.
            foreach (Target target in targets)
            {
                if (compiled.Contains(target.Patch)) continue;
                int before = result.Failed;
                Type[] types;
                try { types = target.Patch.GetTypes(); }
                catch (ReflectionTypeLoadException error)
                {
                    result.Fail(target.Name, string.Empty, "compile", "A type of the patch assembly does not load. " + Explain(error.LoaderExceptions.Length == 0 ? error : error.LoaderExceptions[0]));
                    continue;
                }
                foreach (Type type in types)
                {
                    var methods = new List<MethodBase>(type.GetMethods(Declared));
                    methods.AddRange(type.GetConstructors(Declared & ~BindingFlags.Static));
                    foreach (MethodBase method in methods)
                    {
                        if (Unpatchable(method) != null) continue;
                        try { EntryOf(method, result); }
                        catch (Exception error) { Uncompiled(targets, target.Name, type.FullName + "::" + method.Name, "A method of the patch assembly does not compile. ", error, result); }
                    }
                }
                if (result.Failed == before) compiled.Add(target.Patch);
            }
        }

        // The runtime runs the static initializers that are still pending for the types a method
        // involves when it compiles the method: its own type, and types whose static fields it reads.
        private IntPtr EntryOf(MethodBase method, Result result)
        {
            if (asked.Add(method.MethodHandle.Value)) result.InitializersRun = true;
            return Detour.Entry(method);
        }

        private static void Uncompiled(List<Target> targets, string assembly, string member, string what, Exception error, Result result)
        {
            for (Exception inner = error; inner != null; inner = inner.InnerException)
            {
                if (!(inner is TypeInitializationException failed)) continue;
                bool local = false;
                foreach (Target target in targets) local |= target.Patch != null && target.Patch.GetType(failed.TypeName, false) != null;
                if (local) break;
                // A failed static initializer is final: the runtime throws for every later use of
                // the type. The game has lost it through this request, whatever state the game
                // would have been in when it first used the type itself.
                result.RestartRequired = true;
                result.Fail(assembly, member, "compile", "The static initializer of " + failed.TypeName + " failed when the patch was compiled, and the type is unusable until the Player restarts. " + Explain(error));
                return;
            }
            result.Fail(assembly, member, "compile", what + Explain(error));
        }

        // Decides how every listed method is written, before anything is: over what already
        // redirects it, or for the first time by its first byte and an island, by its second
        // instruction and a relay, or by a whole jump behind the gate. Everything listed is
        // compiled by now, so the process map is current for every entry.
        private void Plan(List<Target> targets, Result result)
        {
            bool mapRead = false;
            var sources = new HashSet<long>();
            var unmapped = new List<Step>();
            foreach (Target target in targets)
                foreach (Step step in target.Steps)
                {
                    long key = step.From.ToInt64();
                    string refusal = Detour.Refusal(step.From, step.To, ref mapRead);
                    if (refusal == null && !sources.Add(key)) refusal = "The method is listed twice.";
                    if (refusal != null) { result.Fail(step.Assembly, step.Member, "write", refusal); continue; }
                    if (!redirects.TryGetValue(key, out step.Redirect))
                        redirects.Add(key, step.Redirect = new Redirect { Original = step.Original, From = step.From, Saved = Detour.Read(step.From), Rest = Detour.Rest(step.From) });
                    Redirect redirect = step.Redirect;
                    if (redirect.Replacement != null)
                    {
                        if (redirect.Way == ByWholeJump && !Detour.Reaches(step.From, step.To))
                            result.Fail(step.Assembly, step.Member, "write", "The replacement is out of reach of a 32-bit jump.");
                        continue;
                    }
                    step.First = true;
                    // What a redirect replaces or reads as the distance of its jump: the first five
                    // bytes, and the whole second instruction where that is the one replaced.
                    ulong start = Detour.Read(step.From);
                    if (Detour.LoadsConstant(redirect.Saved) ? start != redirect.Saved || Detour.Rest(step.From) != redirect.Rest : ((start ^ redirect.Saved) & Detour.JumpBytes) != 0)
                        result.Fail(step.Assembly, step.Member, "write", "The start of the compiled body is not what it was when the applier first read it; something else rewrites this method.");
                    // An island already mapped is used again, the way it was mapped for. Both are kept
                    // together when the island is mapped, even by a request refused after its plan.
                    else if (redirect.Island != null) step.Way = redirect.Way;
                    else unmapped.Add(step);
                }
            // Highest entry first: of two bodies 16 bytes apart the lower one then takes the slot before its jump.
            unmapped.Sort((left, right) => right.From.ToInt64().CompareTo(left.From.ToInt64()));
            foreach (Step step in unmapped)
            {
                Redirect redirect = step.Redirect;
                // A mapped island is never unmapped, so its way is kept with it now, not when the
                // request is written: a request refused after this plan leaves the island for the
                // next first redirect, which must reach it the same way. A first byte alone leads
                // to the fixed address only; a window island needs the jump written behind the gate.
                redirect.Island = Island.Map(step.From, out string problem);
                if (redirect.Island != null) { redirect.Way = ByFirstByte; continue; }
                if (Detour.LoadsConstant(redirect.Saved))
                {
                    string other = null;
                    if (redirect.Relay == null) redirect.Relay = Island.Relay(step.From, (byte)(redirect.Saved >> 24), out other);
                    if (redirect.Relay != null) step.Way = BySecondInstruction;
                    else result.Fail(step.Assembly, step.Member, "write", "The method cannot be redirected. Its island cannot be mapped: " + problem + ". The relay of its second instruction cannot either: " + other + ".");
                }
                else if (Detour.SubImm8(redirect.Saved))
                {
                    // Its first instruction is four bytes, so bytes 1 to 3 can be rewritten behind
                    // the gate to reach an island anywhere in the 16 MiB window of its fifth byte.
                    redirect.Island = Island.MapInWindow(step.From, out string window);
                    if (redirect.Island != null) step.Way = redirect.Way = ByGatedIsland;
                    else result.Fail(step.Assembly, step.Member, "write", "The method cannot be redirected while other threads may run it. Its fixed island address is taken (" + problem
                        + ") and " + window + ".");
                }
                else if (!Detour.Gateable(redirect.Saved))
                    result.Fail(step.Assembly, step.Member, "write", "The method cannot be redirected while other threads may run it. Its island cannot be mapped: " + problem
                        + ". A whole jump would reach past the first instruction of the compiled body, where another thread may be about to continue.");
                else if (!Detour.Reaches(step.From, step.To))
                    result.Fail(step.Assembly, step.Member, "write", "The island of the method cannot be mapped: " + problem + ". The replacement is out of reach of a 32-bit jump.");
                else step.Way = ByWholeJump;
            }
        }

        private List<Change> Write(List<Target> targets, Result result)
        {
            var changes = new List<Change>();
            var kept = new HashSet<long>();
            var gated = new List<Step>();
            writing = true;
            foreach (Target target in targets)
                foreach (Step step in target.Steps)
                {
                    kept.Add(step.From.ToInt64());
                    if (!step.First) continue;
                    Redirect redirect = step.Redirect;
                    redirect.Way = step.Way;
                    // A gated write (a whole jump, or a sub rsp, imm8 body reaching an island in its
                    // window) closes the gate now and completes after the one drain. The island slot
                    // of the gated-island form is aimed first, as the island's own slot always is.
                    if (step.Way == ByWholeJump || step.Way == ByGatedIsland)
                    {
                        if (step.Way == ByGatedIsland) redirect.Island.Aim(step.To);
                        Detour.Close(step.From);
                        gated.Add(step);
                        continue;
                    }
                    // The slot first: what opens the way to it is written last.
                    Via(redirect).Aim(step.To);
                    if (step.Way == BySecondInstruction) Detour.EnterSecond(step.From, redirect.Relay.Code);
                    else Detour.Enter(step.From);
                    result.IslandWrites++;
                }
            // One drain for the whole batch.
            if (gated.Count != 0) Detour.Drain();
            foreach (Step step in gated)
            {
                if (step.Way == ByGatedIsland) Detour.OpenIsland(step.From, step.Redirect.Island.Code);
                else Detour.Open(step.From, step.To);
                result.GatedWrites++;
            }
            foreach (Target target in targets)
                foreach (Step step in target.Steps)
                {
                    Redirect redirect = step.Redirect;
                    if (!step.First && redirect.To != step.To)
                    {
                        if (redirect.Way == ByWholeJump) Detour.Retarget(step.From, step.To);
                        else Via(redirect).Aim(step.To);
                    }
                    if (redirect.Replacement != step.Replacement)
                        changes.Add(new Change { Original = step.Original, Previous = redirect.Replacement ?? step.Original, Current = step.Replacement, Removed = step.Removed });
                    redirect.Replacement = step.Replacement; redirect.To = step.To;
                    result.Applied++;
                }
            foreach (Redirect redirect in redirects.Values)
            {
                if (redirect.Replacement == null || kept.Contains(redirect.From.ToInt64())) continue;
                if (redirect.Way == ByWholeJump) Detour.Restore(redirect.From, redirect.Saved);
                else if (redirect.Way == ByGatedIsland) Detour.RestoreIsland(redirect.From, redirect.Saved);
                else if (redirect.Way == BySecondInstruction) Detour.LeaveSecond(redirect.From, redirect.Saved, redirect.Rest);
                else Detour.Leave(redirect.From, redirect.Saved);
                changes.Add(new Change { Original = redirect.Original, Previous = redirect.Replacement, Current = redirect.Original });
                redirect.Replacement = null;
                result.Restored++;
            }
            writing = false;
            return changes;
        }

        private static Island Via(Redirect redirect)
        {
            return redirect.Way == BySecondInstruction ? redirect.Relay : redirect.Island;
        }

        private void Notify(Dictionary<string, object> descriptor, List<Target> targets, Dictionary<string, List<Assembly>> loaded, List<Change> changes, Result result)
        {
            // The patch is written; whatever fails from here on is counted, not returned as a failure.
            if (callbacks == null) callbacks = new Callbacks(host = Host.Create());
            try
            {
                // A method that an earlier generation already replaced with this very body did not change now.
                var pass = new Bodies.Pass();
                changes.RemoveAll(change => change.Previous != change.Original && change.Current != change.Original && Bodies.Same(change.Previous, change.Current, pass));
                // A generation that changes nothing the Player runs (a comment, a blank line) is
                // no event for the game: its callbacks are for code that changed.
                if (changes.Count == 0) { result.Invoked = 0; result.CallbackErrors = 0; return; }
                var game = new List<Assembly>();
                foreach (object name in Json.Array(descriptor, "gameAssemblies"))
                {
                    Assembly assembly = null;
                    foreach (Target target in targets) if (target.Name == name as string) assembly = target.Loaded;
                    if (assembly == null && name is string text && loaded.TryGetValue(text, out List<Assembly> named)) assembly = named[0];
                    if (assembly != null && !game.Contains(assembly)) game.Add(assembly);
                }
                foreach (Target target in targets) if (!game.Contains(target.Loaded)) game.Add(target.Loaded);
                try { callbacks.Run(game, changes); }
                finally { result.Invoked = callbacks.Invoked; result.CallbackErrors = callbacks.Errors; }
            }
            catch (Exception error)
            {
                result.CallbackErrors++;
                try { host.Report(error, null); }
                catch (Exception) { }
            }
        }

        /// <summary>Why the method cannot take part in a redirect, or null. Never ask for the entry of a method this refuses.</summary>
        private static string Unpatchable(MethodBase method)
        {
            Type type = method.DeclaringType;
            if (type == null) return "has no declaring type";
            if (type.IsGenericType || type.ContainsGenericParameters) return "belongs to a generic type";
            if (method.IsGenericMethod || method.ContainsGenericParameters) return "is generic";
            if ((method.CallingConvention & CallingConventions.VarArgs) != 0) return "takes a variable argument list";
            if (method.IsAbstract || method.GetMethodBody() == null) return "has no IL body";
            return null;
        }

        // The jump keeps the registers of the caller, so both methods must share one native signature.
        private static string Mismatch(MethodBase original, MethodBase replacement)
        {
            if (replacement.IsConstructor && replacement.IsStatic) return "The replacement is a static constructor.";
            ParameterInfo[] parameters = original.GetParameters(), replaced = replacement.GetParameters();
            int self = 0;
            if (original.IsStatic)
            {
                if (!replacement.IsStatic) return "A static method needs a static replacement.";
            }
            else if (original.DeclaringType.IsValueType)
            {
                // A static method that takes the struct by reference receives it in the register of the hidden return buffer.
                if (replacement.IsStatic || !replacement.DeclaringType.IsValueType)
                    return "A struct method needs an instance method of a stand-in struct as its replacement.";
            }
            else if (replacement.IsStatic)
            {
                Type receiver = replaced.Length == 0 ? null : replaced[0].ParameterType;
                if (receiver == null || receiver.IsValueType || receiver.IsByRef || receiver.IsPointer || !receiver.IsAssignableFrom(original.DeclaringType))
                    return "A class method needs a static replacement whose first parameter is the instance.";
                self = 1;
            }
            else if (replacement.DeclaringType.IsValueType) return "A class method cannot be replaced by a struct method.";
            if (replaced.Length - self != parameters.Length) return "The replacement takes another number of parameters.";
            for (int index = 0; index < parameters.Length; index++)
                if (parameters[index].ParameterType != replaced[index + self].ParameterType)
                    return "Parameter " + parameters[index].Name + " has another type in the replacement.";
            if (Returned(original) != Returned(replacement)) return "The replacement returns another type.";
            return null;
        }

        private static Type Returned(MethodBase method)
        {
            return method is MethodInfo info ? info.ReturnType : typeof(void);
        }

        /// <summary>
        /// 1 when redirects reach every caller, 2 when the JIT inlines, 0 when that could not be
        /// found out. Only an answer is kept: what stood in the way of the check may be gone the
        /// next time, and a Player that was never checked must not be reported as one that inlines.
        /// </summary>
        private int Inlining()
        {
            if (inlining != 0) return inlining;
            undecided = Detour.Unsupported();
            if (undecided == null)
            {
                try { inlining = Canary.InliningDisabled() ? 1 : 2; }
                catch (Exception error) { undecided = Explain(error); }
            }
            return inlining;
        }

        private static Dictionary<string, object> ReadObject(string path, int limit, string sha256, string what)
        {
            byte[] bytes = ReadFile(path, limit, sha256, what);
            return Json.Parse(new UTF8Encoding(false, true).GetString(bytes)) as Dictionary<string, object>
                ?? throw new InvalidDataException(what + " is not a JSON object: " + path);
        }

        private static byte[] ReadFile(string path, int limit, string sha256, string what)
        {
            if (string.IsNullOrEmpty(path) || !System.IO.Path.IsPathRooted(path)) throw new InvalidDataException(what + " needs an absolute path.");
            var file = new FileInfo(path);
            if (!file.Exists) throw new FileNotFoundException(what + " does not exist: " + path);
            if (file.Length > limit) throw new InvalidDataException(what + " exceeds the size limit: " + path);
            byte[] bytes = File.ReadAllBytes(path);
            if (sha256 == null) return bytes;
            var text = new StringBuilder(64);
            using (SHA256 hash = SHA256.Create())
                foreach (byte value in hash.ComputeHash(bytes)) text.Append(value.ToString("x2", CultureInfo.InvariantCulture));
            if (text.ToString() != sha256) throw new InvalidDataException(what + " differs from its SHA-256: " + path);
            return bytes;
        }

        // The named argument of the permission set blob (ECMA-335 II.23.3): property, boolean, "SkipVerification", true.
        private static bool DeclaresSkipVerification(byte[] assembly)
        {
            byte[] argument = Encoding.ASCII.GetBytes("\u0054\u0002\u0010SkipVerification\u0001");
            for (int at = Array.IndexOf(assembly, argument[0]); at >= 0 && at + argument.Length <= assembly.Length; at = Array.IndexOf(assembly, argument[0], at + 1))
            {
                int index = 1;
                while (index < argument.Length && assembly[at + index] == argument[index]) index++;
                if (index == argument.Length) return true;
            }
            return false;
        }

        private static string Explain(Exception error)
        {
            while ((error is TargetInvocationException || error is TypeInitializationException) && error.InnerException != null) error = error.InnerException;
            return error.GetType().Name + ": " + error.Message;
        }
    }
}
