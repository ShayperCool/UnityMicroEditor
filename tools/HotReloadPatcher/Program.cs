// Hot reload patch builder: compares the game assemblies a Player loaded with their new
// compilation, classifies every difference, and emits one patch assembly per changed assembly.
//
//   dotnet HotReloadPatcher.dll <request.json>     one result document on stdout, exit 0
//   dotnet HotReloadPatcher.dll --codes            the reason and note codes with their meaning
//   dotnet HotReloadPatcher.dll --explain <request.json> <member id>
//                                                  what the comparison sees of one method, both sides
//   dotnet HotReloadPatcher.dll --serve            requests for the life of a watched session: one JSON
//                                                  line each way per request (see Serve)
//
// Request (camelCase, absolute paths):
//   {"schemaVersion":1,"generation":3,"output":"/dir/of/this/generation","runtimeAssembly":"UnityMicroEditor.HotReload",
//    "previousResult":"/dir/of/generation/2/result.json",
//    "assemblies":[{"name":"Assembly-CSharp","loaded":"/loaded/Assembly-CSharp.dll","current":"/new/Assembly-CSharp.dll"}]}
// Whether an assembly was rewritten by the Netcode post-processor is read from its image.
// `previousResult` is the result document of the patch the Player runs now, when it runs one: what
// that patch defined and this one changes or drops is reported, because nothing redirects it.
// Diagnostic members: `threads`, `forceAll` (every comparable body becomes a change), `fault`
// ("diff" or "emit": fail there, to exercise the handling of a defect; "owner": do not follow a
// reference into generic code to its definition, to exercise the check for generated code that no
// method owns).
//
// Result:
//   {"schemaVersion":1,"generation":3,"verdict":"patch|unchanged|unsupported",
//    "assemblies":[{"name":"Assembly-CSharp","loadedMvid":"<guid>","verdict":"patch|unchanged|unsupported",
//      "patch":{"assemblyName":"Assembly-CSharp.HotReload.3","path":"/dir/Assembly-CSharp.HotReload.3.dll","sha256":"<hex>","pdb":"/dir/Assembly-CSharp.HotReload.3.pdb"},
//      "detours":[{"member":"Game.Player::Update()","kind":"changed|removed|lambda","originalToken":100663301,"replacementToken":100663299}],
//      "addedMethods":[{"member":"Game.Player::Heal(System.Int32)","token":100663300}],
//      "addedFields":[{"member":"Game.Player::armor","id":"Assembly-CSharp|Game.Player|armor","static":false}],
//      "patchMembers":[{"member":"Game.Player::Wire() [lambda 0]","kind":"added|lambda|local|closure|state_machine","bodyHash":"<hex>"}],
//      "changes":[{"kind":"method_body","member":"Game.Player::Update()","action":"patch|none|note|unsupported","code":"","message":""}],
//      "unsupported":[{"code":"generic_method","member":"...","message":"..."}],
//      "notes":[{"code":"existing_instances_keep_default","member":"...","message":"...","revision":"<hex>"}]}],
//    "summary":{"patchedMethods":2,"addedMethods":1,"addedFields":1,"unsupported":0,"notes":1},
//    "timingsMs":{"read":1,"diff":2,"emit":3}}
// The lists of a patch are empty unless its file was written; `patchMembers` are the methods that
// only the patch defines. `revision` of a note is present when the note is about a method the
// patch carries: a short hash of what that method runs now (for `added_method_named_by_string`
// also of where the string stands), or `g<generation>` for a note about what the step from the
// previous patch changed. The same code, member and revision in the result before means that
// the note only still stands; anything else is about code that changed since.
//
// A request that cannot be read is a message on stderr and exit code 1; an edit that cannot be
// patched, and a failure of the tool on one assembly, is a normal result with the verdict "unsupported".
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

internal sealed record AssemblyRequest(string Name, string Loaded, string Current);

/// The result document of the patch that is applied now: what that patch alone defines, by assembly.
internal sealed record Previous(int Generation, Dictionary<string, List<PatchMember>> Members)
{
    public static Previous Read(string path)
    {
        try
        {
            static string Text(JsonElement parent, string name) => parent.GetProperty(name).GetString() ?? throw new FormatException($"'{name}' is null.");
            using var document = JsonDocument.Parse(File.ReadAllBytes(path));
            var root = document.RootElement;
            if (root.GetProperty("schemaVersion").GetInt32() != 1) throw new FormatException("schemaVersion is not 1.");
            var members = new Dictionary<string, List<PatchMember>>(StringComparer.Ordinal);
            foreach (var assembly in root.GetProperty("assemblies").EnumerateArray())
            {
                var list = new List<PatchMember>();
                // A result without a patch for the assembly lists nothing.
                if (assembly.TryGetProperty("patchMembers", out var items))
                    foreach (var item in items.EnumerateArray()) list.Add(new PatchMember(Text(item, "member"), Text(item, "kind"), Text(item, "bodyHash")));
                members[Text(assembly, "name")] = list;
            }
            return new Previous(root.GetProperty("generation").GetInt32(), members);
        }
        catch (Exception e) when (e is JsonException or KeyNotFoundException or InvalidOperationException or FormatException)
        {
            throw new InvalidDataException($"The previous result {path} is not a result document of this tool: {e.Message}");
        }
    }
}

internal sealed record Request(int Generation, string Output, string RuntimeAssembly, List<AssemblyRequest> Assemblies, int Threads, bool ForceAll, string? PreviousResult, string Fault)
{
    const long MaximumAssemblyBytes = 1L << 30;

    public static Request Read(string path)
    {
        using var document = JsonDocument.Parse(File.ReadAllBytes(path));
        var root = document.RootElement;
        if (root.ValueKind != JsonValueKind.Object) throw new InvalidDataException("The request must be a JSON object.");
        if (!root.TryGetProperty("schemaVersion", out var schema) || schema.ValueKind != JsonValueKind.Number || schema.GetInt32() != 1)
            throw new InvalidDataException("The request must have schemaVersion 1.");
        int generation = root.TryGetProperty("generation", out var g) && g.ValueKind == JsonValueKind.Number ? g.GetInt32() : -1;
        if (generation < 0) throw new InvalidDataException("The request needs a generation of 0 or more.");
        string output = Text(root, "output");
        if (!Path.IsPathRooted(output)) throw new InvalidDataException("The output directory must be an absolute path.");
        string runtime = root.TryGetProperty("runtimeAssembly", out var r) && r.ValueKind == JsonValueKind.String ? r.GetString()! : Emitter.RuntimeNamespace;
        if (runtime.Length == 0 || runtime.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0) throw new InvalidDataException("The runtime assembly name is invalid.");
        if (!root.TryGetProperty("assemblies", out var list) || list.ValueKind != JsonValueKind.Array || list.GetArrayLength() > 256)
            throw new InvalidDataException("The request needs an assemblies array of at most 256 entries.");
        var assemblies = new List<AssemblyRequest>();
        var names = new HashSet<string>(StringComparer.Ordinal);
        foreach (var item in list.EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.Object) throw new InvalidDataException("Every assembly of the request must be an object.");
            string name = Text(item, "name"), loaded = Text(item, "loaded"), current = Text(item, "current");
            if (name.Length == 0 || name.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0 || !names.Add(name))
                throw new InvalidDataException($"The assembly name '{name}' is empty, invalid or repeated.");
            foreach (string file in new[] { loaded, current })
            {
                if (!Path.IsPathRooted(file)) throw new InvalidDataException($"The assembly path {file} must be absolute.");
                var info = new FileInfo(file);
                if (!info.Exists) throw new InvalidDataException($"The assembly {file} does not exist.");
                if (info.Length > MaximumAssemblyBytes) throw new InvalidDataException($"The assembly {file} is larger than 1 GiB.");
            }
            assemblies.Add(new AssemblyRequest(name, loaded, current));
        }
        int threads = root.TryGetProperty("threads", out var t) && t.ValueKind == JsonValueKind.Number ? Math.Clamp(t.GetInt32(), 1, 64) : Math.Min(Environment.ProcessorCount, 8);
        bool forceAll = root.TryGetProperty("forceAll", out var f) && f.ValueKind == JsonValueKind.True;
        string? previous = root.TryGetProperty("previousResult", out var p) && p.ValueKind == JsonValueKind.String && p.GetString()!.Length > 0 ? p.GetString() : null;
        if (previous != null && (!Path.IsPathRooted(previous) || !File.Exists(previous))) throw new InvalidDataException($"The previous result {previous} must be an absolute path of an existing file.");
        string fault = root.TryGetProperty("fault", out var x) && x.ValueKind == JsonValueKind.String ? x.GetString()! : "";
        return new Request(generation, output, runtime, assemblies, threads, forceAll, previous, fault);
    }

    static string Text(JsonElement parent, string name) =>
        parent.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString()! : throw new InvalidDataException($"The request lacks the string '{name}'.");
}

internal static class Program
{
    const string Patch = "patch", Unchanged = "unchanged", Unsupported = "unsupported";

    sealed class Outcome
    {
        public Entry Entry = null!;
        public Emitter Emitter = null!;
        public List<Change> Changes = new();
        public string? NetcodeUnknown;
        public string Verdict = Unchanged;
        public Patch? Patch;
        public string Path = "", Sha256 = "", Pdb = "";
        public readonly List<(string Code, string Member, string Message)> Unsupported = new();
        /// `Holders`: of a note about an added field Unity serializes on an engine object, the classes
        /// whose saved documents hold its value (see `Differ.DocumentHolders`).
        public readonly List<(string Code, string Member, string Message, string Revision, IReadOnlyList<string>? Holders)> Notes = new();
    }

    public static int Main(string[] args)
    {
        if (args.Length == 1 && args[0] == "--serve") return Serve();
        try
        {
            if (args.Length == 1 && args[0] == "--codes") { Write(CodeTable()); return 0; }
            if (args.Length == 3 && args[0] == "--explain") { Explain(Request.Read(args[1]), args[2]); return 0; }
            if (args.Length != 1) throw new InvalidDataException("Expected one request JSON path.");
            Write(Run(Request.Read(args[0])));
            return 0;
        }
        catch (Exception error)
        {
            // A refused request is a sentence; anything else is a defect and keeps its stack.
            Console.Error.WriteLine(error is InvalidDataException or IOException or JsonException ? error.Message : error.ToString());
            return 1;
        }
    }

    /// <summary>
    /// The builder for the life of a watched session. Standard input carries one request per line,
    /// <c>{"id":3,"directory":"/abs","arguments":["/abs/request.json"]}</c>; standard output one answer
    /// per line, <c>{"id":3,"exit":0,"output":"&lt;result document&gt;\n","error":"","kind":""}</c>, with
    /// what the one-shot run of the arguments exits with and prints. The first line says
    /// <c>{"ready":true,"protocol":1}</c>. Only a request path is served (<c>--codes</c> and
    /// <c>--explain</c> are answered with exit 64). Nothing is kept from one request to the next;
    /// the end of standard input ends the process.
    /// </summary>
    static int Serve()
    {
        using var frames = Console.OpenStandardOutput();
        using var input = new StreamReader(Console.OpenStandardInput(), new UTF8Encoding(false));
        TextWriter console = Console.Out, consoleError = Console.Error;
        void Frame(object value)
        {
            frames.Write(JsonSerializer.SerializeToUtf8Bytes(value));
            frames.WriteByte((byte)'\n');
            frames.Flush();
        }
        Frame(new Dictionary<string, object> { ["ready"] = true, ["protocol"] = 1 });
        string? line;
        while ((line = input.ReadLine()) != null)
        {
            if (line.Length == 0) continue;
            long id = 0;
            string? directory = null;
            string[] arguments = Array.Empty<string>();
            try
            {
                using var document = JsonDocument.Parse(line);
                var request = document.RootElement;
                id = request.GetProperty("id").GetInt64();
                if (request.TryGetProperty("directory", out var where) && where.ValueKind == JsonValueKind.String) directory = where.GetString();
                arguments = request.GetProperty("arguments").EnumerateArray().Select(a => a.GetString() ?? "").ToArray();
            }
            catch (Exception e) when (e is JsonException or KeyNotFoundException or InvalidOperationException) { arguments = Array.Empty<string>(); }
            var output = new StringWriter();
            var error = new StringWriter();
            int exit;
            if (arguments.Length != 1 || arguments[0].StartsWith("--", StringComparison.Ordinal))
            {
                exit = 64;
                error.WriteLine("Not served: " + string.Join(" ", arguments));
            }
            else
            {
                // A stray console write goes into the answer, never between two frames.
                Console.SetOut(output);
                Console.SetError(error);
                try
                {
                    if (!string.IsNullOrEmpty(directory)) Directory.SetCurrentDirectory(directory);
                    Canon.StartRequest();
                    output.Write(Encoding.UTF8.GetString(Run(Request.Read(arguments[0]))));
                    output.Write('\n');
                    exit = 0;
                }
                catch (Exception e)
                {
                    error.WriteLine(e is InvalidDataException or IOException or JsonException ? e.Message : e.ToString());
                    exit = 1;
                }
                finally
                {
                    Console.SetOut(console);
                    Console.SetError(consoleError);
                }
            }
            Frame(new Dictionary<string, object>
            {
                ["id"] = id, ["exit"] = exit, ["output"] = output.ToString(), ["error"] = error.ToString(), ["kind"] = "",
            });
        }
        return 0;
    }

    static void Write(byte[] json)
    {
        using var stdout = Console.OpenStandardOutput();
        stdout.Write(json);
        stdout.WriteByte((byte)'\n');
    }

    static Workspace Open(Request request)
    {
        var workspace = new Workspace();
        try
        {
            var entries = new Entry?[request.Assemblies.Count];
            Parallel.For(0, entries.Length, new ParallelOptions { MaxDegreeOfParallelism = request.Threads }, i =>
            {
                var a = request.Assemblies[i];
                Asm? loaded = null;
                try
                {
                    loaded = new Asm(a.Loaded);
                    var current = new Asm(a.Current);
                    entries[i] = new Entry(a.Name, loaded, current);
                }
                catch { loaded?.Dispose(); throw; }
            });
            foreach (var e in entries) workspace.Entries.Add(e!);
            foreach (var e in workspace.Entries)
                if (e.Loaded.Name != e.Name || e.Current.Name != e.Name)
                    throw new InvalidDataException($"The request names assembly {e.Name}, but its files define {e.Loaded.Name} and {e.Current.Name}.");
            return workspace;
        }
        catch (AggregateException e) when (e.InnerExceptions.Count > 0)
        {
            workspace.Dispose();
            var inner = e.InnerExceptions[0];
            throw inner is BadImageFormatException ? new InvalidDataException("An assembly of the request is not a valid image: " + inner.Message) : inner;
        }
        catch { workspace.Dispose(); throw; }
    }

    /// A defect of the tool on one assembly must neither pass for an applied patch nor take the
    /// runner's fallback away: the assembly is refused, with the exception as the reason.
    static bool Guard(Outcome outcome, string stage, Action action)
    {
        try { action(); return true; }
        catch (Exception error) when (error is not global::Unsupported)
        {
            if (error is AggregateException { InnerExceptions.Count: > 0 } many) error = many.InnerExceptions[0];
            string where = (error.StackTrace ?? "").Split('\n')[0].Trim();
            outcome.Patch = null;
            outcome.Verdict = Unsupported;
            outcome.Unsupported.Add((Codes.InternalError, outcome.Entry.Name,
                $"The patch builder failed while {stage} {outcome.Entry.Name}: {error.GetType().Name}: {error.Message} {where}".TrimEnd()));
            return false;
        }
    }

    static byte[] Run(Request request)
    {
        var watch = Stopwatch.StartNew();
        using var workspace = Open(request);
        var previous = request.PreviousResult == null ? null : Previous.Read(request.PreviousResult);
        long readMs = watch.ElapsedMilliseconds;

        watch.Restart();
        var outcomes = new List<Outcome>();
        foreach (var entry in workspace.Entries)
        {
            var outcome = new Outcome { Entry = entry, Emitter = new Emitter(workspace, entry, request.Generation, request.RuntimeAssembly) };
            outcomes.Add(outcome);
            Guard(outcome, "comparing", () =>
            {
                if (request.Fault == "diff") throw new InvalidOperationException("The request asked for a fault.");
                var differ = new Differ(workspace, entry, blind: request.Fault == "owner") { ForceAll = request.ForceAll };
                differ.Run(request.Threads);
                outcome.Changes = differ.Changes;
                outcome.NetcodeUnknown = differ.NetcodeUnknown;
            });
        }
        long diffMs = watch.ElapsedMilliseconds;

        watch.Restart();
        foreach (var outcome in outcomes)
        {
            if (outcome.Verdict == Unsupported) continue;
            // What the Netcode post-processor registered at start-up is read from the images
            // (Netcode.cs). An assembly it cannot be read from gets no patch at all: it was
            // compared without those rules, only to see whether anything differs.
            if (outcome.NetcodeUnknown is { } form)
            {
                var first = outcome.Changes.FirstOrDefault(c => c.Action is Actions.Patch or Actions.Unsupported);
                if (first == null) continue;
                outcome.Verdict = Unsupported;
                outcome.Unsupported.Add((Codes.NetcodeAssembly, first.Member,
                    $"{outcome.Entry.Name} uses Netcode for GameObjects and is not in the form its post-processor is known to write ({form}), so nothing says what Netcode registered when the Player started; its edit of {first.Member} cannot be patched."));
                continue;
            }
            if (!Guard(outcome, "planning the patch of", () => outcome.Emitter.Prepare(outcome.Changes))) continue;
            foreach (var c in outcome.Changes)
                if (c.Action == Actions.Unsupported) outcome.Unsupported.Add((c.Code, c.Member, c.Message));
            outcome.Verdict = outcome.Unsupported.Count > 0 ? Unsupported : outcome.Changes.Any(c => c.Action == Actions.Patch) ? Patch : Unchanged;
        }

        // One application is all or nothing, across assemblies too: nothing is emitted for a
        // change that has an unsupported part.
        if (outcomes.All(x => x.Verdict != Unsupported))
        {
            foreach (var outcome in outcomes)
            {
                if (outcome.Verdict != Patch) continue;
                Guard(outcome, "emitting the patch of", () =>
                {
                    try
                    {
                        if (request.Fault == "emit") throw new InvalidOperationException("The request asked for a fault.");
                        outcome.Patch = outcome.Emitter.Build(outcome.Changes);
                        if (outcome.Patch == null) { outcome.Verdict = Unchanged; return; }
                        var problems = Verifier.Check(workspace, outcome.Entry, outcome.Patch);
                        if (problems.Count == 0) return;
                        outcome.Unsupported.Add((Codes.EmitVerificationFailed, outcome.Entry.Name,
                            $"The patch of {outcome.Entry.Name} failed verification ({problems.Count} finding(s)): {string.Join(" ", problems.Take(4))}"));
                    }
                    catch (Unsupported e)
                    {
                        outcome.Unsupported.Add((e.Code, outcome.Entry.Name, $"The patch of {outcome.Entry.Name} cannot be emitted: {e.Message}."));
                    }
                    outcome.Verdict = Unsupported;
                    outcome.Patch = null;
                });
            }
        }
        // What an earlier patch defined is compared with what the Player would run from now on.
        if (previous != null && outcomes.All(x => x.Verdict != Unsupported))
        {
            foreach (var outcome in outcomes)
            {
                if (!previous.Members.TryGetValue(outcome.Entry.Name, out var members) || members.Count == 0) continue;
                Guard(outcome, "comparing the earlier patch of", () =>
                {
                    foreach (var (code, member, message) in outcome.Emitter.EarlierPatch(members, previous.Generation, outcome.Patch))
                    {
                        // A method the earlier patch added and this one changed or dropped is a
                        // fact of this step. Two copies of an added method that code makes a
                        // delegate of are a condition that stands from patch to patch: it is told
                        // when it begins, not again with every patch of any file.
                        string revision = code == Codes.AddedMethodDelegateIdentity ? "" : "g" + request.Generation;
                        var change = new Change { Kind = "earlier_patch", Member = member, Action = Actions.Note, Revision = revision };
                        change.Notes.Add((code, message));
                        outcome.Changes.Add(change);
                    }
                });
            }
        }
        string verdict = outcomes.Any(x => x.Verdict == Unsupported) ? Unsupported : outcomes.Any(x => x.Verdict == Patch) ? Patch : Unchanged;
        if (verdict == Patch) Publish(request.Output, outcomes);
        // A note says what an APPLIED patch does differently; a refused change applies nothing.
        if (verdict != Unsupported)
            foreach (var outcome in outcomes)
                foreach (var c in outcome.Changes)
                    // A note names what the user wrote: Awake, not the lifecycle rewrite's helper.
                    foreach (var (code, message) in c.Notes)
                        outcome.Notes.Add((code, Differ.Authored(c.Member), message, c.Revision, code == Codes.SerializedFieldNotRestored ? c.Holders : null));
        long emitMs = watch.ElapsedMilliseconds;
        return Result(request, outcomes, verdict, readMs, diffMs, emitMs);
    }

    static void Publish(string directory, List<Outcome> outcomes)
    {
        const UnixFileMode ownerOnly = UnixFileMode.UserRead | UnixFileMode.UserWrite;
        if (OperatingSystem.IsWindows()) Directory.CreateDirectory(directory);
        else Directory.CreateDirectory(directory, ownerOnly | UnixFileMode.UserExecute);
        void Store(string path, byte[] data)
        {
            var options = new FileStreamOptions { Mode = FileMode.Create, Access = FileAccess.Write, Share = FileShare.None };
            if (!OperatingSystem.IsWindows()) options.UnixCreateMode = ownerOnly;
            using var stream = new FileStream(path, options);
            stream.Write(data);
        }
        foreach (var outcome in outcomes)
        {
            if (outcome.Patch is not { } patch) continue;
            outcome.Path = System.IO.Path.Combine(directory, patch.AssemblyName + ".dll");
            Store(outcome.Path, patch.Image);
            outcome.Sha256 = Convert.ToHexString(SHA256.HashData(patch.Image)).ToLowerInvariant();
            if (patch.Pdb == null) continue;
            outcome.Pdb = System.IO.Path.Combine(directory, patch.AssemblyName + ".pdb");
            Store(outcome.Pdb, patch.Pdb);
        }
    }

    static byte[] Result(Request request, List<Outcome> outcomes, string verdict, long readMs, long diffMs, long emitMs)
    {
        var buffer = new MemoryStream();
        using (var w = new Utf8JsonWriter(buffer, new JsonWriterOptions { Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping }))
        {
            void Reasons(string name, IEnumerable<(string Code, string Member, string Message, string Revision, IReadOnlyList<string>? Holders)> list)
            {
                w.WriteStartArray(name);
                foreach (var (code, member, message, revision, holders) in list)
                {
                    w.WriteStartObject();
                    w.WriteString("code", code);
                    w.WriteString("member", member);
                    w.WriteString("message", message);
                    if (revision.Length > 0) w.WriteString("revision", revision);
                    if (holders is { Count: > 0 })
                    {
                        w.WriteStartArray("holders");
                        foreach (var holder in holders) w.WriteStringValue(holder);
                        w.WriteEndArray();
                    }
                    w.WriteEndObject();
                }
                w.WriteEndArray();
            }

            w.WriteStartObject();
            w.WriteNumber("schemaVersion", 1);
            w.WriteNumber("generation", request.Generation);
            w.WriteString("verdict", verdict);
            w.WriteStartArray("assemblies");
            foreach (var outcome in outcomes)
            {
                var patch = outcome.Path.Length > 0 ? outcome.Patch : null;
                w.WriteStartObject();
                w.WriteString("name", outcome.Entry.Name);
                w.WriteString("loadedMvid", outcome.Entry.Loaded.Mvid.ToString());
                w.WriteString("verdict", outcome.Verdict);
                // Always present; the strings are empty unless a patch file was written.
                w.WriteStartObject("patch");
                w.WriteString("assemblyName", patch?.AssemblyName ?? "");
                w.WriteString("path", outcome.Path);
                w.WriteString("sha256", outcome.Sha256);
                w.WriteString("pdb", outcome.Pdb);
                w.WriteEndObject();
                w.WriteStartArray("detours");
                foreach (var d in patch?.Detours ?? new List<Detour>())
                {
                    w.WriteStartObject();
                    w.WriteString("member", d.Member);
                    w.WriteString("kind", d.Kind);
                    w.WriteNumber("originalToken", d.OriginalToken);
                    w.WriteNumber("replacementToken", d.ReplacementToken);
                    w.WriteEndObject();
                }
                w.WriteEndArray();
                w.WriteStartArray("addedMethods");
                foreach (var m in patch?.AddedMethods ?? new List<AddedMethod>())
                {
                    w.WriteStartObject();
                    w.WriteString("member", m.Member);
                    w.WriteNumber("token", m.Token);
                    w.WriteEndObject();
                }
                w.WriteEndArray();
                w.WriteStartArray("addedFields");
                foreach (var f in patch?.AddedFields ?? new List<AddedField>())
                {
                    w.WriteStartObject();
                    w.WriteString("member", f.Member);
                    w.WriteString("id", f.Id);
                    w.WriteBoolean("static", f.Static);
                    w.WriteEndObject();
                }
                w.WriteEndArray();
                w.WriteStartArray("patchMembers");
                foreach (var m in patch?.Members ?? new List<PatchMember>())
                {
                    w.WriteStartObject();
                    w.WriteString("member", m.Member);
                    w.WriteString("kind", m.Kind);
                    w.WriteString("bodyHash", m.BodyHash);
                    w.WriteEndObject();
                }
                w.WriteEndArray();
                w.WriteStartArray("changes");
                foreach (var c in outcome.Changes)
                {
                    bool noted = c.Action != Actions.Unsupported && c.Notes.Count > 0;
                    w.WriteStartObject();
                    w.WriteString("kind", c.Kind);
                    w.WriteString("member", c.Member);
                    w.WriteString("action", c.Action);
                    w.WriteString("code", noted ? c.Notes[0].Code : c.Code);
                    w.WriteString("message", noted && c.Message.Length == 0 ? c.Notes[0].Message : c.Message);
                    w.WriteEndObject();
                }
                w.WriteEndArray();
                Reasons("unsupported", outcome.Unsupported.Select(x => (x.Code, x.Member, x.Message, "", (IReadOnlyList<string>?)null)));
                Reasons("notes", outcome.Notes);
                w.WriteEndObject();
            }
            w.WriteEndArray();
            w.WriteStartObject("summary");
            w.WriteNumber("patchedMethods", outcomes.Sum(x => x.Path.Length > 0 ? x.Patch!.Detours.Count : 0));
            w.WriteNumber("addedMethods", outcomes.Sum(x => x.Path.Length > 0 ? x.Patch!.AddedMethods.Count : 0));
            w.WriteNumber("addedFields", outcomes.Sum(x => x.Path.Length > 0 ? x.Patch!.AddedFields.Count : 0));
            w.WriteNumber("unsupported", outcomes.Sum(x => x.Unsupported.Count));
            w.WriteNumber("notes", outcomes.Sum(x => x.Notes.Count));
            w.WriteEndObject();
            w.WriteStartObject("timingsMs");
            w.WriteNumber("read", readMs);
            w.WriteNumber("diff", diffMs);
            w.WriteNumber("emit", emitMs);
            w.WriteEndObject();
            w.WriteEndObject();
        }
        return buffer.ToArray();
    }

    static byte[] CodeTable()
    {
        var buffer = new MemoryStream();
        using (var w = new Utf8JsonWriter(buffer, new JsonWriterOptions { Indented = true, Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping }))
        {
            w.WriteStartObject();
            foreach (var (name, table) in new[] { ("unsupported", Codes.Unsupported), ("notes", Codes.Notes) })
            {
                w.WriteStartObject(name);
                foreach (var (code, meaning) in table) w.WriteString(code, meaning);
                w.WriteEndObject();
            }
            w.WriteEndObject();
        }
        return buffer.ToArray();
    }

    static void Explain(Request request, string member)
    {
        using var workspace = Open(request);
        var text = new StringBuilder();
        foreach (var entry in workspace.Entries)
        {
            var (loaded, current) = new Differ(workspace, entry).Explain(member);
            if (loaded == null && current == null) continue;
            text.Append("=== ").Append(entry.Name).Append(" loaded ===\n").Append(loaded ?? "(absent)\n");
            text.Append("=== ").Append(entry.Name).Append(" current ===\n").Append(current ?? "(absent)\n");
        }
        Console.Out.Write(text.Length > 0 ? text.ToString() : $"No method {member} in the assemblies of the request.\n");
    }
}
