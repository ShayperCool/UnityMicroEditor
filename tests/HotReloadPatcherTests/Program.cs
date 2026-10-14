// Tests of the hot reload patch builder (tools/HotReloadPatcher), run by hand:
//
//   "$UNITY_DATA/DotNetSdk/dotnet" run --project tests/HotReloadPatcherTests/HotReloadPatcherTests.csproj
//
// The program builds the patcher the way the runner does (a clean copy of its sources, package
// sources cleared), compiles the fixture sources under Fixtures/ with the SDK's csc and the
// pipeline's switches, and then checks
//   - the classification of every supported and every refused kind of edit, through the real CLI;
//   - the behaviour of a patch on Unity's standalone Mono: the loaded assemblies with the patch
//     applied through real redirects must act like the new compilation;
//   - the builder as a watched session runs it (--serve): every request goes to one process too,
//     whose answer must be the one-shot result (but for the timings) with the same patch bytes,
//     and at the end once more in the reverse order.
// It prints one summary line and exits non-zero on any failure. Everything it writes is below its
// own bin directory. UNITY_DATA defaults to the pinned Editor under $HOME.
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

internal sealed class Expectation
{
    public string Verdict = "unchanged";
    public string[] Unsupported = Array.Empty<string>(), Detours = Array.Empty<string>(), AddedMethods = Array.Empty<string>(),
        AddedFields = Array.Empty<string>(), Notes = Array.Empty<string>();
    /// The edit must not be visible at all: no entry in `changes`.
    public bool Silent;

    public Expectation Redirects(params string[] detours) { Detours = detours; return this; }
    public Expectation Methods(params string[] methods) { AddedMethods = methods; return this; }
    public Expectation Fields(params string[] fields) { AddedFields = fields; return this; }
    public Expectation Noted(params string[] notes) { Notes = notes; return this; }
}

internal sealed record AssemblyPair(string Name, string Loaded, string Current);

/// <summary>One <c>--serve</c> process of the builder for the whole run.</summary>
internal static class Served
{
    static readonly object gate = new();
    static Process? process;
    static long next;
    /// Every request answered so far, with its one-shot result and the hashes of its inputs.
    public static readonly List<(string Label, string Request, string Result, Dictionary<string, string> Inputs)> Seen = new();

    public static (int Exit, string Output, string Error, string Kind) Call(string dotnet, string patcher, string request)
    {
        lock (gate)
        {
            if (process == null)
            {
                var start = new ProcessStartInfo(dotnet) { RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true, UseShellExecute = false };
                start.ArgumentList.Add(patcher);
                start.ArgumentList.Add("--serve");
                start.Environment["DOTNET_CLI_TELEMETRY_OPTOUT"] = "1";
                start.Environment["DOTNET_NOLOGO"] = "1";
                process = Process.Start(start) ?? throw new InvalidOperationException("Cannot start the served builder");
                process.ErrorDataReceived += (_, _) => { };
                process.BeginErrorReadLine();
                string ready = process.StandardOutput.ReadLine() ?? "";
                if (ready != "{\"ready\":true,\"protocol\":1}") throw new InvalidOperationException("The served builder is not ready: " + ready);
            }
            long id = ++next;
            process.StandardInput.WriteLine(JsonSerializer.Serialize(new Dictionary<string, object>
            {
                ["id"] = id, ["directory"] = Path.GetDirectoryName(request)!, ["arguments"] = new[] { request },
            }));
            process.StandardInput.Flush();
            string line = process.StandardOutput.ReadLine() ?? throw new InvalidOperationException("The served builder ended");
            var answer = JsonDocument.Parse(line).RootElement;
            if (answer.GetProperty("id").GetInt64() != id) throw new InvalidOperationException("The served builder answered another request: " + line);
            return (answer.GetProperty("exit").GetInt32(), answer.GetProperty("output").GetString()!, answer.GetProperty("error").GetString()!, answer.GetProperty("kind").GetString()!);
        }
    }

    /// The end of standard input ends the process.
    public static bool Stop()
    {
        lock (gate)
        {
            if (process == null) return true;
            process.StandardInput.Close();
            bool ended = process.WaitForExit(10000);
            if (!ended) process.Kill(true);
            int code = ended ? process.ExitCode : -1;
            process.Dispose();
            process = null;
            return ended && code == 0;
        }
    }

    /// A result document without the timings, which are the one thing a run may change.
    public static string Comparable(string document)
    {
        var root = System.Text.Json.Nodes.JsonNode.Parse(document)!.AsObject();
        root.Remove("timingsMs");
        return root.ToJsonString();
    }

    public static Dictionary<string, string> Inputs(string request)
    {
        var inputs = new Dictionary<string, string>(StringComparer.Ordinal);
        void Add(string? path)
        {
            if (string.IsNullOrEmpty(path)) return;
            inputs[path] = File.Exists(path) ? Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))) : "absent";
        }
        Add(request);
        var root = JsonDocument.Parse(File.ReadAllBytes(request)).RootElement;
        if (root.TryGetProperty("previousResult", out var previous)) Add(previous.GetString());
        foreach (var a in root.GetProperty("assemblies").EnumerateArray())
        {
            string loaded = a.GetProperty("loaded").GetString() ?? "";
            Add(loaded);
            Add(a.GetProperty("current").GetString());
            // The builder also reads the assemblies next to a loaded one: for base types, and for
            // code that may name an added method by a string. One written there later is a change.
            string directory = Path.GetDirectoryName(loaded) ?? "";
            if (directory.Length > 0 && Directory.Exists(directory))
                foreach (string file in Directory.GetFiles(directory, "*.dll")) Add(file);
        }
        return inputs;
    }
}

internal static class Program
{
    static string unityData = "", dotnet = "", csc = "", mono = "", bclAot = "", bclJit = "", monoEtc = "";
    static string repository = "", fixtures = "", work = "", patcher = "";
    static int checks;
    static readonly ConcurrentQueue<string> failures = new();
    static readonly ConcurrentDictionary<string, bool> producedCodes = new();

    static void Check(bool condition, string message)
    {
        Interlocked.Increment(ref checks);
        if (!condition) failures.Enqueue(message);
    }

    static Expectation Patch() => new() { Verdict = "patch" };
    static Expectation Unchanged() => new() { Verdict = "unchanged" };
    static Expectation Silent() => new() { Verdict = "unchanged", Silent = true };
    static Expectation Refused(params string[] reasons) => new() { Verdict = "unsupported", Unsupported = reasons };

    // ---- processes -----------------------------------------------------------------------

    static (int Exit, string Output, string Error) Run(string file, IEnumerable<string> arguments, string? directory = null, Dictionary<string, string>? environment = null)
    {
        var start = new ProcessStartInfo(file) { RedirectStandardOutput = true, RedirectStandardError = true, UseShellExecute = false, WorkingDirectory = directory ?? work };
        foreach (string argument in arguments) start.ArgumentList.Add(argument);
        start.Environment["DOTNET_CLI_TELEMETRY_OPTOUT"] = "1";
        start.Environment["DOTNET_NOLOGO"] = "1";
        start.Environment["DOTNET_SKIP_FIRST_TIME_EXPERIENCE"] = "1";
        if (environment != null) foreach (var (key, value) in environment) start.Environment[key] = value;
        using var process = Process.Start(start) ?? throw new InvalidOperationException("Cannot start " + file);
        var error = process.StandardError.ReadToEndAsync();
        string output = process.StandardOutput.ReadToEnd();
        process.WaitForExit();
        return (process.ExitCode, output, error.Result);
    }

    static readonly string[] Switches = { "-nologo", "-nostdlib+", "-langversion:9.0", "-deterministic+", "-debug:portable", "-optimize-", "-unsafe-", "-nowarn:0169,0414,0649,0067,1998,0612,0618" };

    /// Compiles a game assembly with the switches of rust/src/compiler/compile.rs against Unity's
    /// reference class library.
    static string Compile(string output, IEnumerable<string> sources, IEnumerable<string>? references = null, string? define = null, string target = "library", string? library = null, bool unsafeCode = false)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(output)!);
        var lines = new List<string>(Switches.Where(s => !unsafeCode || s != "-unsafe-")) { "-target:" + target, "-out:" + output };
        if (unsafeCode) lines.Add("-unsafe+");
        if (define != null) lines.Add("-define:" + define);
        library ??= bclAot;
        foreach (string file in Directory.GetFiles(library, "*.dll").OrderBy(x => x, StringComparer.Ordinal)) lines.Add("-reference:" + file);
        if (Directory.Exists(Path.Combine(library, "Facades")) && target == "library")
            foreach (string file in Directory.GetFiles(Path.Combine(library, "Facades"), "*.dll").OrderBy(x => x, StringComparer.Ordinal)) lines.Add("-reference:" + file);
        foreach (string reference in references ?? Array.Empty<string>()) lines.Add("-reference:" + reference);
        lines.AddRange(sources);
        string response = output + ".rsp";
        File.WriteAllLines(response, lines.Select(line => "\"" + line + "\""));
        var (exit, text, error) = Run(dotnet, new[] { csc, "@" + response });
        if (exit != 0 || !File.Exists(output)) throw new InvalidOperationException($"csc failed for {output}:\n{text}{error}");
        return output;
    }

    static string Fixture(params string[] parts) => Path.Combine(new[] { fixtures }.Concat(parts).ToArray());

    // ---- the patcher ---------------------------------------------------------------------

    /// Builds the tool exactly as the runner will: sources copied to a staging directory, a
    /// NuGet.Config without sources, and the command line of Compiler::prepare_lifecycle_helper.
    static void BuildPatcher()
    {
        string source = Path.Combine(repository, "tools", "HotReloadPatcher");
        string staging = Path.Combine(work, "patcher-build");
        Directory.CreateDirectory(staging);
        foreach (string file in Directory.GetFiles(source).Where(f => f.EndsWith(".cs", StringComparison.Ordinal) || f.EndsWith(".csproj", StringComparison.Ordinal)))
            File.Copy(file, Path.Combine(staging, Path.GetFileName(file)));
        File.WriteAllText(Path.Combine(staging, "NuGet.Config"), "<configuration><packageSources><clear /></packageSources></configuration>\n");
        string payload = Path.Combine(staging, "payload");
        var (exit, output, error) = Run(dotnet, new[] { "build", Path.Combine(staging, "HotReloadPatcher.csproj"), "--nologo", "-v:q", "-o", payload,
            "-p:RoslynPath=" + Path.GetDirectoryName(csc), "-p:NuGetAudit=false", "--disable-build-servers", "-nodeReuse:false" }, staging);
        patcher = Path.Combine(payload, "HotReloadPatcher.dll");
        if (exit != 0 || !File.Exists(patcher)) throw new InvalidOperationException("The patcher does not build from a clean copy:\n" + output + error);
        Check(!output.Contains("warning", StringComparison.Ordinal), "The patcher builds with warnings:\n" + output);
    }

    /// The same request through the served builder: the same document but for the timings, the
    /// same patch bytes, and an empty standard error.
    static void CheckServed(string label, string request, string oneShotOutput, string document)
    {
        var inputs = Served.Inputs(request);
        var (exit, output, error, kind) = Served.Call(dotnet, patcher, request);
        Check(exit == 0 && error.Length == 0 && kind.Length == 0, $"{label}: served exit {exit} kind '{kind}': {error}");
        Check(output.EndsWith('\n') && output.Split('\n', StringSplitOptions.RemoveEmptyEntries).Length == 1, $"{label}: the served output is not one line as the one-shot output is.");
        if (exit != 0) return;
        string served = output.TrimEnd('\n');
        Check(Served.Comparable(served) == Served.Comparable(document), $"{label}: the served result differs from the one-shot result:\n{served}\n{document}");
        CheckPatchFiles(label + " (served)", served);
        lock (Served.Seen) Served.Seen.Add((label, request, document, inputs));
    }

    static void CheckPatchFiles(string label, string document)
    {
        foreach (var a in JsonDocument.Parse(document).RootElement.GetProperty("assemblies").EnumerateArray())
        {
            var patch = a.GetProperty("patch");
            string path = patch.GetProperty("path").GetString()!, hash = patch.GetProperty("sha256").GetString()!;
            if (path.Length > 0)
                Check(File.Exists(path) && Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant() == hash, $"{label}: the patch file {path} does not have the bytes of the result.");
        }
    }

    /// Every request once more, newest first, through the same served process: a builder that
    /// kept anything from one request to the next would answer one of them otherwise. A request
    /// whose inputs a later case changed is left out.
    static void ServedAgainInReverse()
    {
        int again = 0;
        List<(string Label, string Request, string Result, Dictionary<string, string> Inputs)> seen;
        lock (Served.Seen) seen = Served.Seen.ToList();
        for (int index = seen.Count - 1; index >= 0; index--)
        {
            var (label, request, result, inputs) = seen[index];
            if (!Served.Inputs(request).OrderBy(x => x.Key).SequenceEqual(inputs.OrderBy(x => x.Key))) continue;
            var (exit, output, error, _) = Served.Call(dotnet, patcher, request);
            Check(exit == 0 && Served.Comparable(output.TrimEnd('\n')) == Served.Comparable(result), $"{label}: served again in reverse order, the result differs (exit {exit}): {error}");
            if (exit == 0) CheckPatchFiles(label + " (served again)", output.TrimEnd('\n'));
            again++;
        }
        Check(again * 2 >= seen.Count && seen.Count > 100, $"Only {again} of {seen.Count} served requests could be sent again.");
        // What is not served is refused, and the end of the input ends the process.
        var (codesExit, _, codesError, codesKind) = Served.Call(dotnet, patcher, "--codes");
        Check(codesExit == 64 && codesError.Contains("Not served", StringComparison.Ordinal), $"--codes was served (exit {codesExit}, kind '{codesKind}').");
        Check(Served.Stop(), "The served builder did not end with its input.");
        Console.WriteLine($"served: {seen.Count} requests, {again} again in reverse order");
    }

    static string WriteRequest(string directory, int generation, IEnumerable<AssemblyPair> assemblies, bool forceAll = false, string? previous = null, string? fault = null, bool netcode = false)
    {
        Directory.CreateDirectory(directory);
        string path = Path.Combine(directory, "request.json");
        using (var stream = File.Create(path))
        using (var w = new Utf8JsonWriter(stream))
        {
            w.WriteStartObject();
            w.WriteNumber("schemaVersion", 1);
            w.WriteNumber("generation", generation);
            w.WriteString("output", Path.Combine(directory, "out"));
            w.WriteString("runtimeAssembly", "UnityMicroEditor.HotReload");
            if (forceAll) w.WriteBoolean("forceAll", true);
            if (previous != null) w.WriteString("previousResult", previous);
            if (fault != null) w.WriteString("fault", fault);
            w.WriteStartArray("assemblies");
            foreach (var a in assemblies)
            {
                w.WriteStartObject();
                w.WriteString("name", a.Name);
                w.WriteString("loaded", a.Loaded);
                w.WriteString("current", a.Current);
                if (netcode) w.WriteBoolean("netcode", true);
                w.WriteEndObject();
            }
            w.WriteEndArray();
            w.WriteEndObject();
        }
        return path;
    }

    /// Runs the CLI and checks what interface 5.1 promises for every result.
    static JsonElement? Patcher(string label, string directory, int generation, IEnumerable<AssemblyPair> assemblies, bool forceAll = false, string? previous = null, string? fault = null, bool netcode = false)
    {
        var list = assemblies.ToList();
        string request = WriteRequest(directory, generation, list, forceAll, previous, fault, netcode);
        var (exit, output, error) = Run(dotnet, new[] { patcher, request });
        Check(exit == 0, $"{label}: exit code {exit}: {error}");
        Check(error.Length == 0, $"{label}: stderr is not empty: {error}");
        string[] documents = output.Split('\n', StringSplitOptions.RemoveEmptyEntries);
        Check(documents.Length == 1, $"{label}: stdout has {documents.Length} lines instead of one JSON document.");
        if (exit != 0 || documents.Length != 1) return null;
        JsonElement root;
        try { root = JsonDocument.Parse(documents[0]).RootElement; }
        catch (JsonException e) { Check(false, $"{label}: stdout is not JSON: {e.Message}"); return null; }
        CheckServed(label, request, output, documents[0]);
        File.WriteAllText(Path.Combine(directory, "result.json"), documents[0]);

        string verdict = root.GetProperty("verdict").GetString()!;
        Check(root.GetProperty("schemaVersion").GetInt32() == 1 && root.GetProperty("generation").GetInt32() == generation, $"{label}: wrong schemaVersion or generation.");
        Check(verdict is "patch" or "unchanged" or "unsupported", $"{label}: unknown verdict {verdict}.");
        var timings = root.GetProperty("timingsMs");
        Check(timings.GetProperty("read").TryGetInt64(out _) && timings.GetProperty("diff").TryGetInt64(out _) && timings.GetProperty("emit").TryGetInt64(out _), $"{label}: timings are not whole milliseconds.");
        var results = root.GetProperty("assemblies").EnumerateArray().ToList();
        Check(results.Count == list.Count, $"{label}: {results.Count} assembly results for {list.Count} requested.");
        int detours = 0, addedMethods = 0, addedFields = 0, unsupported = 0, notes = 0;
        var verdicts = new List<string>();
        foreach (var a in results)
        {
            string name = a.GetProperty("name").GetString()!, own = a.GetProperty("verdict").GetString()!;
            verdicts.Add(own);
            Check(Guid.TryParse(a.GetProperty("loadedMvid").GetString(), out _), $"{label}: loadedMvid of {name} is not a GUID.");
            var patch = a.GetProperty("patch");
            string path = patch.GetProperty("path").GetString()!, hash = patch.GetProperty("sha256").GetString()!, pdb = patch.GetProperty("pdb").GetString()!;
            bool published = verdict == "patch" && own == "patch";
            Check(published == (path.Length > 0), $"{label}: {name} has verdict {own} in a {verdict} result, but patch.path is '{path}'.");
            if (path.Length > 0)
            {
                Check(patch.GetProperty("assemblyName").GetString() == $"{name}.HotReload.{generation}", $"{label}: patch assembly of {name} is named {patch.GetProperty("assemblyName").GetString()}.");
                Check(Path.GetFileName(path) == $"{name}.HotReload.{generation}.dll" && File.Exists(path), $"{label}: patch file {path} is missing or misnamed.");
                if (File.Exists(path)) Check(Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant() == hash, $"{label}: sha256 of {path} does not match.");
                Check(pdb.Length == 0 || File.Exists(pdb), $"{label}: pdb {pdb} is missing.");
            }
            else Check(patch.GetProperty("assemblyName").GetString() == "" && hash == "" && pdb == "", $"{label}: {name} has no patch file but non-empty patch strings.");
            if (!published)
                Check(a.GetProperty("detours").GetArrayLength() + a.GetProperty("addedMethods").GetArrayLength() + a.GetProperty("addedFields").GetArrayLength() + a.GetProperty("patchMembers").GetArrayLength() == 0, $"{label}: {name} lists redirects or patch members without a patch.");
            var redirects = a.GetProperty("detours").EnumerateArray().Where(d => d.GetProperty("kind").GetString() == "lambda").Select(d => d.GetProperty("member").GetString()).ToList();
            var members = a.GetProperty("patchMembers").EnumerateArray().ToList();
            foreach (var m in members)
            {
                string code = m.GetProperty("bodyHash").GetString()!;
                Check(m.GetProperty("kind").GetString() is "added" or "lambda" or "local" or "closure" or "state_machine", $"{label}: unknown patch member kind {m.GetProperty("kind")}.");
                Check(m.GetProperty("member").GetString()!.Length > 0 && code.Length == 64 && code.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f'), $"{label}: a patch member lacks its id or a SHA-256 body hash.");
            }
            // An added method and a redirected lambda are the same member wherever the result names them.
            foreach (var added in a.GetProperty("addedMethods").EnumerateArray())
                Check(members.Any(m => m.GetProperty("kind").GetString() == "added" && m.GetProperty("member").GetString() == added.GetProperty("member").GetString()), $"{label}: the added method {added.GetProperty("member")} is not a patch member.");
            foreach (string? redirect in redirects)
                Check(members.Any(m => m.GetProperty("kind").GetString() is "lambda" or "local" && m.GetProperty("member").GetString() == redirect), $"{label}: the redirected lambda {redirect} is not a patch member.");
            detours += a.GetProperty("detours").GetArrayLength();
            addedMethods += a.GetProperty("addedMethods").GetArrayLength();
            addedFields += a.GetProperty("addedFields").GetArrayLength();
            unsupported += a.GetProperty("unsupported").GetArrayLength();
            notes += a.GetProperty("notes").GetArrayLength();
            foreach (var c in a.GetProperty("changes").EnumerateArray())
            {
                string action = c.GetProperty("action").GetString()!;
                Check(action is "patch" or "none" or "note" or "unsupported", $"{label}: unknown action {action}.");
                Check(c.GetProperty("kind").GetString()!.Length > 0 && c.GetProperty("member").GetString()!.Length > 0, $"{label}: a change lacks kind or member.");
                if (action == "unsupported") Check(c.GetProperty("code").GetString()!.Length > 0 && c.GetProperty("message").GetString()!.Length > 0, $"{label}: an unsupported change lacks code or message.");
            }
            foreach (string group in new[] { "unsupported", "notes" })
                foreach (var entry in a.GetProperty(group).EnumerateArray())
                {
                    producedCodes[group + ":" + entry.GetProperty("code").GetString()] = true;
                    Check(entry.GetProperty("member").GetString()!.Length > 0 && entry.GetProperty("message").GetString()!.Length > 0, $"{label}: a {group} entry lacks member or message.");
                }
            if (own == "unsupported") Check(a.GetProperty("unsupported").GetArrayLength() > 0, $"{label}: {name} is unsupported without a reason.");
        }
        string expected = verdicts.Contains("unsupported") ? "unsupported" : verdicts.Contains("patch") ? "patch" : "unchanged";
        Check(verdict == expected, $"{label}: verdict {verdict} does not follow from the assembly verdicts [{string.Join(",", verdicts)}].");
        var summary = root.GetProperty("summary");
        Check(summary.GetProperty("patchedMethods").GetInt32() == detours && summary.GetProperty("addedMethods").GetInt32() == addedMethods &&
            summary.GetProperty("addedFields").GetInt32() == addedFields && summary.GetProperty("unsupported").GetInt32() == unsupported &&
            summary.GetProperty("notes").GetInt32() == notes, $"{label}: the summary does not match the lists.");
        if (verdict != "patch") Check(!Directory.Exists(Path.Combine(directory, "out")), $"{label}: files were written for a {verdict} result.");
        return root;
    }

    static List<string> Pairs(JsonElement assembly, string list, string first, string second) =>
        assembly.GetProperty(list).EnumerateArray().Select(e => e.GetProperty(first).GetString() + " " + e.GetProperty(second).GetString()).OrderBy(x => x, StringComparer.Ordinal).ToList();

    static List<string> Singles(JsonElement assembly, string list, string property) =>
        assembly.GetProperty(list).EnumerateArray().Select(e => e.GetProperty(property).GetString()!).OrderBy(x => x, StringComparer.Ordinal).ToList();

    static void Same(string label, string what, IEnumerable<string> expected, List<string> actual)
    {
        var wanted = expected.OrderBy(x => x, StringComparer.Ordinal).ToList();
        Check(wanted.SequenceEqual(actual), $"{label}: {what}\n      expected: [{string.Join(" | ", wanted)}]\n      actual:   [{string.Join(" | ", actual)}]");
    }

    static void Expect(string label, JsonElement? result, string assemblyName, Expectation e)
    {
        if (result is not { } root) return;
        var assembly = root.GetProperty("assemblies").EnumerateArray().FirstOrDefault(a => a.GetProperty("name").GetString() == assemblyName);
        if (assembly.ValueKind != JsonValueKind.Object) { Check(false, $"{label}: no result for {assemblyName}."); return; }
        Check(assembly.GetProperty("verdict").GetString() == e.Verdict, $"{label}: verdict of {assemblyName} is {assembly.GetProperty("verdict").GetString()}, expected {e.Verdict}.");
        Same(label, "unsupported", e.Unsupported, Pairs(assembly, "unsupported", "code", "member"));
        Same(label, "notes", e.Notes, Pairs(assembly, "notes", "code", "member"));
        Same(label, "detours", e.Detours, Pairs(assembly, "detours", "kind", "member"));
        Same(label, "addedMethods", e.AddedMethods, Singles(assembly, "addedMethods", "member"));
        Same(label, "addedFields", e.AddedFields, Singles(assembly, "addedFields", "member"));
        if (e.Silent) Check(assembly.GetProperty("changes").GetArrayLength() == 0, $"{label}: the edit must not be reported at all, but changes has {assembly.GetProperty("changes").GetArrayLength()} entries: {assembly.GetProperty("changes")}");
        foreach (var field in assembly.GetProperty("addedFields").EnumerateArray())
        {
            string member = field.GetProperty("member").GetString()!;
            Check(field.GetProperty("id").GetString() == assemblyName + "|" + member.Replace("::", "|"), $"{label}: field id {field.GetProperty("id")} does not follow <assembly>|<type>|<field>.");
        }
    }

    // ---- classification --------------------------------------------------------------------

    static string Lists(int count) => string.Join(",", Enumerable.Repeat("System.Collections.Generic.List`1<System.Action>", count));

    static Dictionary<string, Expectation> EditCases() => new()
    {
        // Body edits.
        ["BODY"] = Patch().Redirects("changed Edits.Bodies::Instance()"),
        ["BODY_STATIC"] = Patch().Redirects("changed Edits.Bodies::Static(System.Int32)"),
        ["BODY_ACCESSOR"] = Patch().Redirects("changed Edits.Bodies::get_Accessed()", "changed Edits.Bodies::set_Accessed(System.Int32)", "changed Edits.Bodies::get_Item(System.Int32)",
            "changed Edits.Bodies::add_Changed(System.Action)", "changed Edits.Bodies::remove_Changed(System.Action)"),
        ["BODY_OPERATOR"] = Patch().Redirects("changed Edits.Bodies::op_Addition(Edits.Bodies,System.Int32)", "changed Edits.Bodies::op_Implicit(Edits.Bodies):System.Int32"),
        ["BODY_EXPLICIT_INTERFACE"] = Patch().Redirects("changed Edits.Bodies::Edits.IWork.Work()", "changed Edits.Bodies::System.IDisposable.Dispose()"),
        ["BODY_FINALIZER"] = Patch().Redirects("changed Edits.Bodies::Finalize()"),
        ["BODY_CTOR"] = Patch().Redirects("changed Edits.Constructed::.ctor()", "changed Edits.Constructed::.ctor(System.Int32)")
            .Noted("constructor_applies_to_new_objects Edits.Constructed::.ctor()", "constructor_applies_to_new_objects Edits.Constructed::.ctor(System.Int32)"),
        ["BODY_VIRTUAL"] = Patch().Redirects("changed Edits.Animal::Speak()", "changed Edits.Dog::Speak()"),
        ["BODY_STRUCT"] = Patch().Redirects("changed Edits.Point::Len()", "changed Edits.Point::Flip()"),
        ["BODY_LAMBDA"] = Patch().Redirects("changed Edits.Lambdas::Capturing(System.Int32)", "changed Edits.Lambdas::Static()", "changed Edits.Lambdas::This()",
            "lambda Edits.Lambdas::Capturing(System.Int32) [lambda 0]", "lambda Edits.Lambdas::Static() [lambda 0]", "lambda Edits.Lambdas::This() [lambda 0]"),
        ["LAMBDA_NOT_REMAPPED"] = Patch().Redirects("changed Edits.Lambdas::Recaptured(System.Int32)").Noted("lambda_not_remapped Edits.Lambdas::Recaptured(System.Int32)"),
        // The outer closure of the lambda gained a field: an old closure object is too small for the new body.
        ["LAMBDA_OUTER_CAPTURE"] = Patch().Redirects("changed Edits.Lambdas::Nested(System.Int32)").Noted("lambda_not_remapped Edits.Lambdas::Nested(System.Int32)"),
        ["LAMBDA_NESTED"] = Patch().Redirects("changed Edits.Lambdas::Nested(System.Int32)", "lambda Edits.Lambdas::Nested(System.Int32) [lambda 0]"),
        ["BODY_LOCAL_FUNCTION"] = Patch().Redirects("changed Edits.Lambdas::Local(System.Int32)", "lambda Edits.Lambdas::Local(System.Int32) [lambda 0]"),
        // A local function that takes its closure by reference names a type the patch copies: no redirect.
        ["BODY_LOCAL_CAPTURE"] = Patch().Redirects("changed Edits.Lambdas::LocalCapture(System.Int32)").Noted("lambda_not_remapped Edits.Lambdas::LocalCapture(System.Int32)"),
        ["BODY_ITERATOR"] = Patch().Redirects("changed Edits.Lambdas::Iterate()").Noted("running_coroutines_keep_old_code Edits.Lambdas::Iterate()"),
        ["BODY_ASYNC"] = Patch().Redirects("changed Edits.Lambdas::Wait()").Noted("running_coroutines_keep_old_code Edits.Lambdas::Wait()"),
        // Lambdas are paired through the code that creates them, never by their position alone.
        ["LAMBDA_TWIN"] = Patch().Redirects("changed Edits.Wiring::Twins(" + Lists(2) + ")", "lambda Edits.Wiring::Twins(" + Lists(2) + ") [lambda 1]"),
        ["LAMBDA_BODIES_SWAPPED"] = Patch().Redirects("changed Edits.Wiring::Swapped(" + Lists(2) + ")", "lambda Edits.Wiring::Swapped(" + Lists(2) + ") [lambda 0]", "lambda Edits.Wiring::Swapped(" + Lists(2) + ") [lambda 1]"),
        ["LAMBDA_REPLACED"] = Patch().Redirects("changed Edits.Wiring::Replaced(" + Lists(3) + ")").Noted("lambda_not_remapped Edits.Wiring::Replaced(" + Lists(3) + ")"),
        // The lambda itself is as it was: nothing to redirect and nothing to tell.
        ["LAMBDA_AROUND"] = Patch().Redirects("changed Edits.Wiring::Around(System.Int32)"),
        ["LAMBDA_APPENDED"] = Patch().Redirects("changed Edits.Wiring::Appended(" + Lists(2) + ")"),
        // The old body exists in the new method, but not as the first lambda any more.
        ["LAMBDA_COPIED"] = Patch().Redirects("changed Edits.Wiring::Copied(" + Lists(2) + ")").Noted("lambda_not_remapped Edits.Wiring::Copied(" + Lists(2) + ")"),
        // The edit is in the state machine of the lambda; the stub that starts it is redirected.
        ["LAMBDA_ASYNC"] = Patch().Redirects("changed Edits.Wiring::Greeter(System.String)", "lambda Edits.Wiring::Greeter(System.String) [lambda 0]")
            .Noted("running_coroutines_keep_old_code Edits.Wiring::Greeter(System.String)"),
        ["LAMBDA_ITERATOR_LOCAL"] = Patch().Redirects("changed Edits.Wiring::Numbers(System.Int32)", "lambda Edits.Wiring::Numbers(System.Int32) [lambda 0]")
            .Noted("running_coroutines_keep_old_code Edits.Wiring::Numbers(System.Int32)"),
        ["LAMBDA_IN_ITERATOR"] = Patch().Redirects("changed Edits.Wiring::Makers()", "lambda Edits.Wiring::Makers() [lambda 0]")
            .Noted("running_coroutines_keep_old_code Edits.Wiring::Makers()"),
        ["RECORD_WITH"] = Patch().Redirects("changed Edits.Figures::Grow(Edits.Figure)"),
        // The note names the Awake the user wrote, not the helper the lifecycle rewrite moved its body into.
        ["LIFECYCLE_BODY"] = Patch().Redirects("changed Edits.Behaviour1::__UnityPlay_Awake()").Noted("started_objects_keep_state Edits.Behaviour1::Awake()"),
        // Start of an engine object ran for the objects that exist; a plain class has no such method.
        ["START_BODY"] = Patch().Redirects("changed Edits.Starting::Start()", "changed Edits.PlainStarting::Start()").Noted("started_objects_keep_state Edits.Starting::Start()"),
        ["CHANGE_CONST"] = Patch().Redirects("changed Edits.Consts::Use()"),
        ["ANONYMOUS_TYPE_EXISTING"] = Patch().Redirects("changed Edits.Anonymous::Reuse()"),
        // The parameter of the edited lambda is an anonymous type: the loaded one, on both sides of the redirect.
        ["LAMBDA_ANONYMOUS_PARAMETER"] = Patch().Redirects("changed Edits.Anonymous::Existing()", "lambda Edits.Anonymous::Existing() [lambda 1]"),

        // Added, removed and renamed members.
        ["INSERT_MEMBER"] = Patch().Methods("Edits.Lambdas::Inserted()"),
        ["ADD_METHOD"] = Patch().Methods("Edits.AddMethod::Helper(System.Int32)", "Edits.AddMethod::StaticHelper(System.Int32)").Redirects("changed Edits.AddMethod::Use()"),
        ["ADD_MEMBERS"] = Patch().Methods("Edits.Members::get_Prop()", "Edits.Members::set_Prop(System.Int32)", "Edits.Members::add_Evt(System.Action)", "Edits.Members::remove_Evt(System.Action)",
                "Edits.Members::get_Item(System.Int32)", "Edits.Members::op_UnaryNegation(Edits.Members)")
            // Neither field has an initializer: its default is what every object has anyway.
            .Fields("Edits.Members::<Prop>k__BackingField", "Edits.Members::Evt").Redirects("changed Edits.Members::Use()"),
        ["SIGNATURE_CHANGE"] = Patch().Methods("Edits.Signatures::Heal(System.Int32,System.Boolean)").Redirects("changed Edits.Signatures::Turn()"),
        ["RENAME_METHOD"] = Patch().Methods("Edits.Signatures::Mend(System.Int32)").Redirects("changed Edits.Signatures::Turn()"),
        ["STATIC_TOGGLE"] = Patch().Methods("Edits.Signatures::Heal(System.Int32)").Redirects("changed Edits.Signatures::Turn()"),
        ["REMOVE_METHOD"] = Unchanged(),
        // A string that only equals the name ("Launch" returned by Name()) calls nothing (builder6-9).
        ["ADD_ATTRIBUTED_METHOD"] = Patch().Methods("Edits.Tools::Launch()").Noted("added_member_attributes_ignored Edits.Tools::Launch()"),
        ["ADD_METHOD_STRING_ONLY"] = Patch().Methods("Edits.Hopper::Hop()").Noted("added_method_invisible_by_name Edits.Hopper::Hop()"),
        ["ADD_UPDATE_PLAIN"] = Patch().Methods("Edits.PlainTicker::Update()"),
        // The engine finds a method of its objects by name; a static one it does not look for.
        ["ADD_ENGINE_METHOD"] = Patch().Methods("Edits.Behaviour1::OnJump()", "Edits.Behaviour1::set_Level(System.Int32)", "Edits.Behaviour1::Helper()")
            .Noted("added_method_invisible_by_name Edits.Behaviour1::OnJump()", "added_method_invisible_by_name Edits.Behaviour1::set_Level(System.Int32)"),
        ["REMOVE_UNITY_MESSAGE"] = Patch().Redirects("removed Edits.Behaviour1::Update()"),
        ["PARAMETER_RENAME"] = Unchanged(),
        ["ACCESSIBILITY"] = Unchanged().Noted("accessibility_changed Edits.Access::field", "accessibility_changed Edits.Access::Method()"),
        // Unity serializes these as before: static, read-only, marked already.
        ["ACCESSIBILITY_NOT_SERIALIZED"] = Unchanged().Noted("accessibility_changed Edits.Exposure::shared", "accessibility_changed Edits.Exposure::frozen", "accessibility_changed Edits.Exposure::marked"),
        ["RUNTIME_INITIALIZER_ADD"] = Patch().Methods("Edits.BootA::Extra()").Noted("runtime_initializer_added Edits.BootA"),
        ["RUNTIME_INITIALIZER_REMOVE"] = Unchanged().Noted("runtime_initializer_removed Edits.BootB"),
        // Methods that ran once for what exists, and methods only they call.
        ["ONCE_INITIALIZER"] = Patch().Redirects("changed Edits.BootC::Init()", "changed Edits.BootC::Register()")
            .Noted("runtime_initializer_changed Edits.BootC::Init()", "runtime_initializer_changed Edits.BootC::Register()"),
        ["ONCE_ENABLE"] = Patch().Redirects("changed Edits.Listener::__UnityPlay_OnEnable()", "changed Edits.Listener::__UnityPlay_OnDisable()")
            .Noted("started_objects_keep_state Edits.Listener::OnEnable()"),
        ["ONCE_ASSET"] = Patch().Redirects("changed Edits.Settings::__UnityPlay_OnEnable()").Noted("started_objects_keep_state Edits.Settings::OnEnable()"),
        ["ONCE_DESERIALIZED"] = Patch().Redirects("changed Edits.Table::OnAfterDeserialize()").Noted("started_objects_keep_state Edits.Table::OnAfterDeserialize()"),
        // Bonus() is called by Update as well, and Rebuild() has an attribute a framework calls it
        // by: they run again, and get no note.
        ["ONCE_HOOK"] = Patch().Redirects("changed Edits.Enemy::OnSetup()", "changed Edits.Actor::Prepare()", "changed Edits.Actor::Bonus()", "changed Edits.Actor::Rebuild()")
            .Noted("started_objects_keep_state Edits.Enemy::OnSetup()", "started_objects_keep_state Edits.Actor::Prepare()"),
        // A static field initializer's helper, field initializers' helpers of another class (a
        // property among them), a public method only Awake calls, a method Start calls by the name
        // it writes (builder6-2, builder6-3, netcode6-1).
        ["ONCE_CCTOR_HELPER"] = Patch().Redirects("changed Edits.Tables::BuildDamage()").Noted("runtime_initializer_changed Edits.Tables::BuildDamage()"),
        ["ONCE_CTOR_HELPER"] = Patch().Redirects("changed Edits.Rules::get_Level()", "changed Edits.Rules::Weights()")
            .Noted("constructor_applies_to_new_objects Edits.Rules::get_Level()", "constructor_applies_to_new_objects Edits.Rules::Weights()"),
        ["ONCE_PUBLIC_INIT"] = Patch().Redirects("changed Edits.Brute::Init()").Noted("started_objects_keep_state Edits.Brute::Init()"),
        ["ONCE_INVOKE_NAMEOF"] = Patch().Redirects("changed Edits.Delayed::LateInit()").Noted("started_objects_keep_state Edits.Delayed::LateInit()"),
        // Only the lambda OnEnable subscribes changed, and it is redirected: live, no note (builder6-8).
        ["ONCE_LAMBDA_ONLY"] = Patch().Redirects("changed Edits.Jumper::OnEnable()", "lambda Edits.Jumper::OnEnable() [lambda 0]"),
        // The instances of a renamed coroutine that run keep the removed code.
        ["COROUTINE_REMOVED"] = Patch().Methods("Edits.Blinker::Pulse()").Redirects("changed Edits.Blinker::Update()")
            .Noted("running_coroutines_keep_old_code Edits.Blinker::Blink()", "added_method_invisible_by_name Edits.Blinker::Pulse()"),

        // Fields.
        ["ADD_INSTANCE_FIELD"] = Patch().Fields("Edits.AddField::extra", "Edits.AddField::primed").Redirects("changed Edits.AddField::.ctor()", "changed Edits.AddField::Use()")
            // `primed` has an initializer, which existing objects never ran; `extra` has none.
            .Noted("existing_instances_keep_default Edits.AddField::primed", "constructor_applies_to_new_objects Edits.AddField::.ctor()"),
        ["ADD_STATIC_FIELD"] = Patch().Fields("Edits.AddField::shared").Redirects("changed Edits.AddField::Use()"),
        ["ADD_SERIALIZED_FIELD"] = Patch().Fields("Edits.Serialized::exposed", "Edits.Serialized::hidden", "Edits.Serialized::plain").Redirects("changed Edits.Serialized::Sum()")
            .Noted("serialized_field_not_restored Edits.Serialized::exposed", "serialized_field_not_restored Edits.Serialized::hidden", "added_member_attributes_ignored Edits.Serialized::hidden"),
        ["FIELD_RENAME"] = Patch().Fields("Edits.Fields::renamed").Redirects("changed Edits.Fields::Use()").Noted("existing_instances_keep_default Edits.Fields::renamed"),
        ["FIELD_TYPE_CHANGE"] = Patch().Fields("Edits.Fields::speed").Redirects("changed Edits.Fields::.ctor()", "changed Edits.Fields::Use()")
            .Noted("field_value_reset Edits.Fields::speed", "constructor_applies_to_new_objects Edits.Fields::.ctor()"),
        ["REMOVE_FIELD"] = Unchanged(),
        ["READONLY_BENIGN"] = Patch().Redirects("changed Edits.Thawed::Raise()", "changed Edits.Thawed::Grow()", "changed Edits.Thawed::Dig()"),
        ["ADD_CONST"] = Unchanged(),

        // Quiet: nothing may be reported.
        ["REORDER"] = Silent(),
        ["INLINING_HINT"] = Silent(),

        // Refused.
        ["GENERIC_TYPE_BODY"] = Refused("generic_type Edits.Box`1::Get()"),
        ["GENERIC_TYPE_ADD"] = Refused("generic_type Edits.Box`1::counter", "generic_type Edits.Box`1::Count()", "generic_type Edits.BoxUser::Use(Edits.Box`1<System.Int32>)"),
        ["GENERIC_METHOD_BODY"] = Refused("generic_method Edits.Generics::Echo`1(!!0)"),
        ["GENERIC_METHOD_ADD"] = Refused("generic_method Edits.Generics::Make`1()"),
        ["GENERIC_LOCAL_FUNCTION"] = Refused("generic_generated_member Edits.LocalGeneric::Run(System.Int32)"),
        // An edit that lies only in a lambda or local function of generic code is an edit of the
        // method that owns it.
        ["GENERIC_LOCAL_LAMBDA"] = Refused("generic_generated_member Edits.LocalGeneric::Deep(System.Int32)"),
        ["GENERIC_TYPE_LAMBDA"] = Refused("generic_type Edits.GenericLambdas`1::Pooled()"),
        ["GENERIC_TYPE_THIS_LAMBDA"] = Refused("generic_type Edits.GenericLambdas`1::Bound()"),
        ["GENERIC_TYPE_LOCAL_FUNCTION"] = Refused("generic_type Edits.GenericLambdas`1::Local()"),
        ["GENERIC_NESTED_LAMBDA"] = Refused("generic_type Edits.GenericLambdas`1+Inner::Pooled()"),
        ["GENERIC_METHOD_LAMBDA"] = Refused("generic_method Edits.GenericMethodLambdas::Pool`1(!!0)", "generic_method Edits.GenericMethodLambdas::Query`1(!!0)"),
        ["NEW_ANONYMOUS_TYPE"] = Refused("generated_type_not_loaded Edits.Anonymous::Fresh()"),
        ["ADD_TYPES"] = Refused("type_added Edits.AddedClass", "type_added Edits.AddedStruct", "type_added Edits.AddedEnum", "type_added Edits.IAdded", "type_added Edits.AddedDelegate"),
        ["REMOVE_TYPE"] = Refused("type_removed Edits.Doomed"),
        ["RENAME_TYPE"] = Refused("type_removed Edits.Renamable", "type_added Edits.Renamed"),
        ["ENUM_MEMBER"] = Refused("enum_changed Edits.Mode"),
        ["ENUM_VALUE"] = Refused("enum_changed Edits.Level"),
        ["ENUM_ATTRIBUTE"] = Refused("custom_attributes_changed Edits.Mode::Off"),
        ["BASE_TYPE"] = Refused("base_type_changed Edits.Circle"),
        ["INTERFACE_LIST"] = Refused("interfaces_changed Edits.Marked"),
        ["TYPE_FLAGS"] = Refused("type_flags_changed Edits.Flagged"),
        ["STRUCT_FIELD_ADD"] = Refused("struct_layout_changed Edits.Layout::Added"),
        ["STRUCT_FIELD_REORDER"] = Refused("struct_layout_changed Edits.Ordered"),
        ["EXPLICIT_LAYOUT"] = Refused("type_layout_changed Edits.Overlay::B"),
        ["INTERFACE_MEMBER_ADD"] = Refused("interface_changed Edits.IGrowing::B()"),
        ["ADD_VIRTUAL"] = Refused("virtual_method_added Edits.Virtuals::Fresh()"),
        ["ADD_OVERRIDE"] = Refused("virtual_method_added Edits.Virtuals::Speak()"),
        ["ADD_ABSTRACT"] = Refused("virtual_method_added Edits.Contract::Also()"),
        ["REMOVE_OVERRIDE"] = Refused("virtual_method_removed Edits.Virtuals::ToString()"),
        ["METHOD_FLAGS"] = Refused("method_flags_changed Edits.Virtuals::Plain()"),
        ["ADD_INTERFACE_IMPLEMENTATION"] = Refused("virtual_method_added Edits.Implementer::Edits.IWork.Work()", "method_flags_changed Edits.Implementer::Work()", "method_overrides_changed Edits.Implementer"),
        ["STATIC_CTOR_CHANGE"] = Refused("static_constructor_changed Edits.Statics::.cctor()"),
        ["STATIC_CTOR_ADD"] = Refused("static_constructor_changed Edits.NoStatics::.cctor()"),
        ["ADD_CTOR"] = Refused("constructor_added Edits.Built::.ctor(System.Int32)"),
        ["ADD_SYNCHRONIZED"] = Refused("emit_not_supported Edits.Locks::Locked()"),
        ["VARARGS_BODY"] = Refused("emit_not_supported Edits.Variadic::Count()"),
        ["ARGLIST_CALLER"] = Refused("emit_not_supported Edits.Variadic::Use()", "emit_not_supported Edits.Variadic::UseNone(System.Int32)"),
        ["ADD_EXTERN"] = Refused("extern_method_added Edits.Native::getpid()", "extern_method_added Edits.Native::Pid()"),
        ["EXTERN_CHANGED"] = Refused("extern_method_changed Edits.Native::Id()"),
        ["MARSHAL"] = Refused("extern_method_changed Edits.Native::strlen(System.String)", "extern_method_changed Edits.Native::memset(System.Char[],System.Int32,System.IntPtr)",
            "extern_method_changed Edits.NativeCallback::Invoke(System.String)", "extern_method_changed Edits.NativeCallback::BeginInvoke(System.String,System.AsyncCallback,System.Object)"),
        ["ADD_SPECIAL_FIELD"] = Refused("special_field_added Edits.Special::perThread", "special_field_added Edits.Special::flag", "special_field_added Edits.Special::Use()"),
        ["FIELD_NONSERIALIZED"] = Refused("field_flags_changed Edits.Data::cache"),
        // A field that Unity starts or stops serializing: nothing of the edit is in the Player.
        ["ACCESSIBILITY_SERIALIZED"] = Refused("serialization_changed Edits.Exposure::tuned", "serialization_changed Edits.Exposure::shown", "serialization_changed Edits.ExposureData::value"),
        ["STATIC_READONLY_REMOVED"] = Refused("static_readonly_removed Edits.Frozen::limit"),
        ["ATTRIBUTE_TYPE"] = Refused("custom_attributes_changed Edits.AttributedType"),
        ["ATTRIBUTE_METHOD"] = Refused("custom_attributes_changed Edits.Attributed::M()"),
        ["ATTRIBUTE_FIELD"] = Refused("custom_attributes_changed Edits.Attributed::f"),
        ["ATTRIBUTE_PARAMETER"] = Refused("custom_attributes_changed Edits.Attributed::P(System.String) parameter 1"),
        ["ASSEMBLY_ATTRIBUTE"] = Refused("custom_attributes_changed Edits"),
        ["ASSEMBLY_VERSION"] = Refused("assembly_identity_changed Edits"),
        // The attributes the compiler writes on an async method or an iterator are not compared.
        ["ASYNC_ADDED"] = Patch().Redirects("changed Edits.Becoming::Run()"),
        ["ITERATOR_ADDED"] = Patch().Redirects("changed Edits.Becoming::List()"),
        ["ASYNC_REMOVED"] = Patch().Redirects("changed Edits.Becoming::Was()").Noted("running_coroutines_keep_old_code Edits.Becoming::Was()"),
        ["ITERATOR_REMOVED"] = Patch().Redirects("changed Edits.Becoming::WasList()").Noted("running_coroutines_keep_old_code Edits.Becoming::WasList()"),
        ["EXPRESSION_TREE"] = Refused("unsupported_instruction Edits.Trees::Build()"),
        ["ADD_UNITY_MESSAGE"] = Refused("unity_message_added Edits.Behaviour1::FixedUpdate()"),
        ["ADD_LIFECYCLE"] = Refused("unity_message_added Edits.Behaviour1::OnEnable()"),
        ["REMOVE_UNITY_MESSAGE_NONVOID"] = Refused("unity_message_removed Edits.Starter::Start()"),
        ["REMOVE_UNITY_MESSAGE_BASE"] = Refused("unity_message_removed Edits.DerivedBehaviour::LateUpdate()"),
        ["REMOVE_PRESENCE_MESSAGE"] = Refused("unity_message_removed Edits.Rooted::OnAnimatorMove()", "unity_message_removed Edits.Rooted::OnGUI()"),
        // Generated code that nothing names cannot run, in either compilation. An edit that
        // makes it unreachable, reachable again, adds it or changes it is the edit of the method
        // around it and no more; inside dead code it is no edit at all.
        ["DEAD_EARLY_RETURN"] = Patch().Redirects("changed Edits.Dead::Early()").Noted("lambda_not_remapped Edits.Dead::Early()"),
        ["DEAD_RETURN_REMOVED"] = Patch().Redirects("changed Edits.Dead::Late()"),
        ["DEAD_THROW"] = Patch().Redirects("changed Edits.Dead::Thrown(System.Int32)"),
        ["DEAD_FLAG_ON"] = Patch().Redirects("changed Edits.Dead::Flagged()"),
        ["DEAD_FLAG_OFF"] = Patch().Redirects("changed Edits.Dead::Flagged()").Noted("lambda_not_remapped Edits.Dead::Flagged()"),
        ["DEAD_LAMBDA_EDITED"] = Silent(),
        // The declaration of a local function is one more `nop` in the method around it.
        ["DEAD_LOCAL_ADDED"] = Patch().Redirects("changed Edits.Dead::Helper()"),
        ["DEAD_LOCAL_EDITED"] = Silent(),
        ["DEAD_CALL_REMOVED"] = Patch().Redirects("changed Edits.Dead::Called()").Noted("running_coroutines_keep_old_code Edits.Dead::Called()", "lambda_not_remapped Edits.Dead::Called()"),
        // The type of a field decides with its accessibility whether Unity serializes it.
        ["ACCESSIBILITY_TYPES_SERIALIZED"] = Refused(new[] { "title", "mode", "numbers", "weights", "data", "other", "all" }.Select(field => "serialization_changed Edits.ExposureTypes::" + field).ToArray()),
        ["ACCESSIBILITY_TYPES_NOT_SERIALIZED"] = Unchanged().Noted(new[] { "table", "onChanged", "onTick", "tag", "marker", "grid", "rows", "nested", "maybe", "pair", "loose", "unlisted", "when", "set", "task", "routine" }
            .Select(field => "accessibility_changed Edits.ExposureTypes::" + field).ToArray()),
        // `readonly` on a public or marked field starts or stops its serialization; on a private
        // field, and on one of a type Unity never serializes, it changes nothing.
        ["READONLY_SERIALIZED"] = Refused("serialization_changed Edits.Frozen2::max", "serialization_changed Edits.Frozen2::min", "serialization_changed Edits.Frozen2::marked"),
        // A Start or Awake with parameters and the Start of a ScriptableObject are methods of the
        // game: the engine calls none of them, and nothing "already ran". Awake of a ScriptableObject is called.
        ["START_NOT_A_MESSAGE"] = Patch().Redirects("changed Edits.StartOverload::Start(System.Int32)", "changed Edits.StartOverload::Awake(System.Int32)", "changed Edits.Quest::Start()", "changed Edits.Quest::Awake()")
            .Noted("started_objects_keep_state Edits.Quest::Awake()"),
        // `readonly` on a struct method, the names of tuple elements and `params` are read by compilers only.
        ["COMPILER_ATTRIBUTES"] = Unchanged(),
        // An added method of an engine object, and the same one once a string of the game names it.
        ["ADD_METHOD_UNNAMED"] = Patch().Methods("Edits.Caller::Blast()").Noted("added_method_invisible_by_name Edits.Caller::Blast()"),
        ["ADD_METHOD_NAMED"] = Patch().Methods("Edits.Caller::Blast()").Redirects("changed Edits.Caller::Tick()").Noted("added_method_named_by_string Edits.Caller::Blast()"),
        ["ADD_METHOD_NAMED_TWICE"] = Patch().Methods("Edits.Caller::Blast()").Redirects("changed Edits.Caller::Tick()", "changed Edits.Caller::Tock()").Noted("added_method_named_by_string Edits.Caller::Blast()"),
    };

    static void Classification(string unityStub, string netcodeStub)
    {
        string root = Path.Combine(work, "classify");
        string source = Fixture("Classify", "Edits.cs");
        string loaded = Compile(Path.Combine(root, "loaded", "Edits.dll"), new[] { source }, new[] { unityStub });
        var cases = EditCases();
        Parallel.ForEach(cases, new ParallelOptions { MaxDegreeOfParallelism = 8 }, pair =>
        {
            string label = "CASE_" + pair.Key, directory = Path.Combine(root, pair.Key);
            try
            {
                string current = Compile(Path.Combine(directory, "Edits.dll"), new[] { source }, new[] { unityStub }, define: label);
                Expect(label, Patcher(label, directory, 1, new[] { new AssemblyPair("Edits", loaded, current) }), "Edits", pair.Value);
            }
            catch (Exception e) { Check(false, $"{label}: {e.Message}"); }
        });

        // What the note of a public method says (stage 6).
        JsonElement? Result(string name)
        {
            string path = Path.Combine(root, name, "result.json");
            return File.Exists(path) ? JsonDocument.Parse(File.ReadAllText(path)).RootElement : null;
        }
        string exposed = Message(Result("ONCE_PUBLIC_INIT"), "started_objects_keep_state", "Edits.Brute::Init()");
        Check(exposed.Contains("in the game's code only from Edits.Brute::Awake()", StringComparison.Ordinal) && exposed.Contains("UnityEvent", StringComparison.Ordinal),
            "ONCE_PUBLIC_INIT: the note of a public method does not say that it reads the game's code only: " + exposed);

        // Every symbol of the fixture has an expectation.
        var declared = new SortedSet<string>(StringComparer.Ordinal);
        foreach (string line in File.ReadLines(source))
            foreach (string word in line.Split(new[] { ' ', '!', '(', ')', '|', '&' }, StringSplitOptions.RemoveEmptyEntries))
                if (word.StartsWith("CASE_", StringComparison.Ordinal) && line.TrimStart().StartsWith('#')) declared.Add(word["CASE_".Length..]);
        foreach (string name in declared) Check(cases.ContainsKey(name), $"Fixture symbol CASE_{name} has no expectation.");
        foreach (string name in cases.Keys) Check(declared.Contains(name), $"Expectation {name} has no fixture symbol.");

        // The same patchable edit in a patch generation whose holder name a loaded type already has:
        // the verification refuses to publish it.
        string body = Path.Combine(root, "BODY", "Edits.dll");
        Expect("generation 7", Patcher("generation 7", Path.Combine(root, "generation7"), 7, new[] { new AssemblyPair("Edits", loaded, body) }), "Edits",
            Refused("emit_verification_failed Edits"));

        // Generated code that no method owns is compared with nothing: a difference in it is
        // refused. The diagnostic `owner` hides the owner of a lambda of generic code.
        foreach (string name in new[] { "GENERIC_METHOD_LAMBDA", "GENERIC_TYPE_LAMBDA", "GENERIC_TYPE_THIS_LAMBDA", "GENERIC_TYPE_LOCAL_FUNCTION", "GENERIC_NESTED_LAMBDA" })
        {
            string label = "unowned " + name, type = name == "GENERIC_METHOD_LAMBDA" ? "Edits.GenericMethodLambdas" : name == "GENERIC_NESTED_LAMBDA" ? "Edits.GenericLambdas`1+Inner" : "Edits.GenericLambdas`1";
            Expect(label, Patcher(label, Path.Combine(root, "unowned-" + name), 1, new[] { new AssemblyPair("Edits", loaded, Path.Combine(root, name, "Edits.dll")) }, fault: "owner"), "Edits",
                Refused("unowned_generated_code " + type));
        }
        // The same diagnostic on an edit that it does not hide, and on no edit at all, changes nothing.
        Expect("unowned, plain edit", Patcher("unowned, plain edit", Path.Combine(root, "unowned-body"), 1, new[] { new AssemblyPair("Edits", loaded, body) }, fault: "owner"), "Edits",
            Patch().Redirects("changed Edits.Bodies::Instance()"));

        // A string that names an added method is a call by name that finds nothing. The note says
        // where the string stands, and it is another note (a new revision) when a second place
        // starts to name the method: an edit that only adds Invoke("Blast") is then not answered
        // with a note that merely still stands.
        static (string Revision, string Message) Note(JsonElement? result, string code, string member)
        {
            if (result is { } root)
                foreach (var note in root.GetProperty("assemblies")[0].GetProperty("notes").EnumerateArray())
                    if (note.GetProperty("code").GetString() == code && note.GetProperty("member").GetString() == member)
                        return (note.TryGetProperty("revision", out var revision) ? revision.GetString()! : "", note.GetProperty("message").GetString()!);
            return ("?", "?");
        }
        JsonElement? Named(string name) => Patcher("named " + name, Path.Combine(root, "named-" + name), 1, new[] { new AssemblyPair("Edits", loaded, Path.Combine(root, name, "Edits.dll")) });
        var unnamed = Note(Named("ADD_METHOD_UNNAMED"), "added_method_invisible_by_name", "Edits.Caller::Blast()");
        var namedOnce = Note(Named("ADD_METHOD_NAMED"), "added_method_named_by_string", "Edits.Caller::Blast()");
        var namedTwice = Note(Named("ADD_METHOD_NAMED_TWICE"), "added_method_named_by_string", "Edits.Caller::Blast()");
        Check(unnamed.Revision.Length == 16, $"added method, unnamed: the note has the revision '{unnamed.Revision}', expected that of the method.");
        Check(namedOnce.Revision.Length == 16 && namedOnce.Revision != unnamed.Revision, $"added method, named: the revision '{namedOnce.Revision}' must differ from that of the note without a string, '{unnamed.Revision}'.");
        Check(namedTwice.Revision.Length == 16 && namedTwice.Revision != namedOnce.Revision, $"added method, named twice: the revision '{namedTwice.Revision}' must differ from that of one naming place, '{namedOnce.Revision}'.");
        Check(namedOnce.Message.Contains("\"Blast\" in code of Edits.Caller names", StringComparison.Ordinal) && namedOnce.Message.Contains("does nothing", StringComparison.Ordinal),
            $"added method, named: the note does not say where the string stands and what it means: {namedOnce.Message}");

        // The attribute types the compiler embeds come with the first nullable annotation or
        // `nint` of an assembly and go with the last: neither is an added or removed type.
        string embedded = Fixture("Classify", "Embedded.cs");
        string embeddedWithout = Compile(Path.Combine(root, "embedded", "without", "Embedded.dll"), new[] { embedded });
        string embeddedWith = Compile(Path.Combine(root, "embedded", "with", "Embedded.dll"), new[] { embedded }, define: "CASE_EMBEDDED");
        Expect("embedded attribute types added", Patcher("embedded attribute types added", Path.Combine(root, "embedded-added"), 1, new[] { new AssemblyPair("Embedded", embeddedWithout, embeddedWith) }), "Embedded", Unchanged());
        Expect("embedded attribute types removed", Patcher("embedded attribute types removed", Path.Combine(root, "embedded-removed"), 1, new[] { new AssemblyPair("Embedded", embeddedWith, embeddedWithout) }), "Embedded", Unchanged());

        // A type of its own module named through a reference scoped to the module: the form a
        // post-processor writes when it imports a type of the assembly it rewrites. No compiler
        // writes it, so the test makes it: the reference to the same name in another assembly is
        // given the module as its scope. A body that names the type this way is patched.
        string scopedOther = Compile(Path.Combine(root, "scoped", "ScopedOther.dll"), new[] { Fixture("Classify", "ScopedOther.cs") });
        string scopedSource = Fixture("Classify", "Scoped.cs");
        string scopedLoaded = Compile(Path.Combine(root, "scoped", "loaded", "Scoped.dll"), new[] { scopedSource }, new[] { "other=" + scopedOther });
        string scopedCurrent = Compile(Path.Combine(root, "scoped", "current", "Scoped.dll"), new[] { scopedSource }, new[] { "other=" + scopedOther }, define: "CASE_SCOPED_BODY");
        foreach (string image in new[] { scopedLoaded, scopedCurrent })
            Check(ScopeToModule(image, "Scoped", "Ball"), $"module-scoped type reference: {image} has no reference to Scoped.Ball of the other assembly to rewrite.");
        Expect("module-scoped type reference", Patcher("module-scoped type reference", Path.Combine(root, "scoped-body"), 1, new[] { new AssemblyPair("Scoped", scopedLoaded, scopedCurrent) }), "Scoped",
            Patch().Redirects("changed Scoped.Thrower::Throw()"));

        // Netcode: recognised by the reference of the image to the Netcode runtime, and by nothing
        // else. A request that still names an assembly as one (the member `netcode` of earlier
        // runners, which followed the project's assembly graph) changes nothing: this image
        // references no Netcode, the post-processor returned it as it was, and it is patched.
        Expect("netcode named by the request", Patcher("netcode named by the request", Path.Combine(root, "netcode-flag"), 1, new[] { new AssemblyPair("Edits", loaded, body) }, netcode: true), "Edits",
            Patch().Redirects("changed Edits.Bodies::Instance()"));
        Netcode(root, unityStub, netcodeStub);

        // The real engine module: its classes are recognised by name at the top of the chain.
        string real = Path.Combine(unityData, "PlaybackEngines", "LinuxStandaloneSupport", "Variations", "mono", "Managed", "UnityEngine.CoreModule.dll");
        if (File.Exists(real))
        {
            string realSource = Fixture("Classify", "RealUnity.cs");
            string realLoaded = Compile(Path.Combine(root, "real", "loaded", "Real.dll"), new[] { realSource }, new[] { real });
            string realAdd = Compile(Path.Combine(root, "real", "add", "Real.dll"), new[] { realSource }, new[] { real }, define: "CASE_REAL_ADD");
            string realRemove = Compile(Path.Combine(root, "real", "remove", "Real.dll"), new[] { realSource }, new[] { real }, define: "CASE_REAL_REMOVE");
            Expect("real engine, message added", Patcher("real engine, message added", Path.Combine(root, "real-add"), 1, new[] { new AssemblyPair("Real", realLoaded, realAdd) }), "Real",
                Refused("unity_message_added Real.Mover::Update()"));
            Expect("real engine, message removed", Patcher("real engine, message removed", Path.Combine(root, "real-remove"), 1, new[] { new AssemblyPair("Real", realLoaded, realRemove) }), "Real",
                Patch().Redirects("removed Real.Mover::LateUpdate()"));
            Serialization(root, Path.GetDirectoryName(real)!);
        }
        else Console.WriteLine("note: " + real + " is not installed; the checks against the real engine module were skipped.");
    }

    /// What Unity serializes, decided against the real engine modules lying next to the loaded
    /// assembly as in a Player's Managed directory (Fixtures/Classify/Serial.cs).
    static void Serialization(string root, string managed)
    {
        string[] modules = { "UnityEngine.CoreModule.dll", "UnityEngine.PhysicsModule.dll", "UnityEngine.InputLegacyModule.dll" };
        string source = Fixture("Classify", "Serial.cs");
        string loadedDirectory = Path.Combine(root, "serial", "loaded");
        Directory.CreateDirectory(loadedDirectory);
        var references = modules.Select(m => Path.Combine(managed, m)).ToArray();
        foreach (string module in references) File.Copy(module, Path.Combine(loadedDirectory, Path.GetFileName(module)), overwrite: true);
        string loaded = Compile(Path.Combine(loadedDirectory, "Serial.dll"), new[] { source }, references, unsafeCode: true);
        var cases = new Dictionary<string, Expectation>
        {
            // Long enums and the engine's types it skips: a note, as for any field Unity never serializes.
            ["SER_ENGINE_ACCESS"] = Refused(new[] { "narrow", "bounds", "mask", "hash", "matrix", "cell", "gradient", "curve", "offset", "rect", "tint", "property", "turn", "clicked", "scored", "lazy", "exposed", "texture", "asset" }
                .Select(field => "serialization_changed Serial.Kept::" + field).ToArray()),
            ["SER_FIXED"] = Refused("serialization_changed Serial.Packed::bytes"),
            ["SER_DICT_READONLY"] = Refused("serialization_changed Serial.Shop::prices"),
            ["SER_DICT_ADD"] = Patch().Fields("Serial.Shop::stock", "Serial.Shop::lists", "Serial.Shop::unmarked", "Serial.Shop::set").Redirects("changed Serial.Shop::Count()")
                .Noted("serialized_field_not_restored Serial.Shop::stock", "serialized_field_not_restored Serial.Shop::lists",
                    "added_member_attributes_ignored Serial.Shop::stock", "added_member_attributes_ignored Serial.Shop::lists", "added_member_attributes_ignored Serial.Shop::set"),
            // Dictionaries of dictionaries, and a key Unity cannot write (builder6-6).
            ["SER_DICT_NESTED"] = Refused("serialization_changed Serial.Nest::nested"),
            ["SER_DICT_NESTED_ADD"] = Patch().Fields("Serial.Nest::more", "Serial.Nest::drops").Redirects("changed Serial.Nest::Count()")
                .Noted("serialized_field_not_restored Serial.Nest::more", "serialized_field_not_restored Serial.Nest::drops",
                    "added_member_attributes_ignored Serial.Nest::more", "added_member_attributes_ignored Serial.Nest::drops"),
            // An abstract [Serializable] class is not serialized: a note (builder6-11).
            ["SER_ABSTRACT"] = Unchanged().Noted("accessibility_changed Serial.Loadout::primary"),
        };
        foreach (var (name, expectation) in cases)
        {
            string label = name, directory = Path.Combine(root, "serial", name);
            string current = Compile(Path.Combine(directory, "Serial.dll"), new[] { source }, references, define: name, unsafeCode: true);
            Expect(label, Patcher(label, directory, 1, new[] { new AssemblyPair("Serial", loaded, current) }), "Serial", expectation);
        }
        // The fields of the types Unity skips: patched with the note alone. They stand in the same
        // edit as the refused ones above, so the notes are read from that result's changes.
        var result = JsonDocument.Parse(File.ReadAllText(Path.Combine(root, "serial", "SER_ENGINE_ACCESS", "result.json"))).RootElement;
        var noted = result.GetProperty("assemblies")[0].GetProperty("changes").EnumerateArray()
            .Where(c => c.GetProperty("action").GetString() == "note" && c.GetProperty("code").GetString() == "accessibility_changed")
            .Select(c => c.GetProperty("member").GetString()!).OrderBy(x => x, StringComparer.Ordinal).ToList();
        Same("SER_ENGINE_ACCESS", "accessibility notes", new[] { "wide", "uwide", "routine", "wait", "aim", "aim2", "hit", "touch", "ground", "range", "native", "pool", "routines" }
            .Select(field => "Serial.Skipped::" + field), noted);
    }

    /// Gives the reference to the type `ns.name` of another assembly the module itself as its
    /// scope. The resolution scope is the first column of a TypeRef row, a coded index whose tag
    /// 0 means the Module table.
    static bool ScopeToModule(string path, string ns, string name)
    {
        byte[] image = File.ReadAllBytes(path);
        int offset = -1;
        bool wide = false;
        using (var pe = new System.Reflection.PortableExecutable.PEReader(new MemoryStream(image)))
        {
            var md = System.Reflection.Metadata.PEReaderExtensions.GetMetadataReader(pe);
            foreach (var table in new[] { System.Reflection.Metadata.Ecma335.TableIndex.Module, System.Reflection.Metadata.Ecma335.TableIndex.ModuleRef,
                System.Reflection.Metadata.Ecma335.TableIndex.AssemblyRef, System.Reflection.Metadata.Ecma335.TableIndex.TypeRef })
                wide |= System.Reflection.Metadata.Ecma335.MetadataReaderExtensions.GetTableRowCount(md, table) >= 1 << 14;
            foreach (var handle in md.TypeReferences)
            {
                var reference = md.GetTypeReference(handle);
                if (md.GetString(reference.Name) != name || md.GetString(reference.Namespace) != ns || reference.ResolutionScope.Kind != System.Reflection.Metadata.HandleKind.AssemblyReference) continue;
                offset = pe.PEHeaders.MetadataStartOffset + System.Reflection.Metadata.Ecma335.MetadataReaderExtensions.GetTableMetadataOffset(md, System.Reflection.Metadata.Ecma335.TableIndex.TypeRef)
                    + (System.Reflection.Metadata.Ecma335.MetadataTokens.GetRowNumber(handle) - 1) * System.Reflection.Metadata.Ecma335.MetadataReaderExtensions.GetTableRowSize(md, System.Reflection.Metadata.Ecma335.TableIndex.TypeRef);
            }
        }
        if (offset < 0) return false;
        image[offset] = 4;
        image[offset + 1] = 0;
        if (wide) { image[offset + 2] = 0; image[offset + 3] = 0; }
        File.WriteAllBytes(path, image);
        return true;
    }

    // ---- Netcode for GameObjects ----------------------------------------------------------

    const string Thing = "Net.NetThing", Ping = Thing + "::PingRpc(System.Int32)";

    /// One CASE_NET_* symbol of Fixtures/Classify/Net.cs per kind of edit in an assembly the
    /// Netcode post-processor rewrote. What it registers once is refused, with a code that names
    /// the cause; everything else is patched as in any assembly, the body of an RPC included.
    static Dictionary<string, Expectation> NetcodeCases() => new()
    {
        // Patched: code like any other.
        ["BODY"] = Patch().Redirects("changed " + Thing + "::Value()"),
        ["PLAIN"] = Patch().Redirects("changed Net.Plain::Twice(System.Int32)"),
        ["SPAWN"] = Patch().Redirects("changed " + Thing + "::OnNetworkSpawn()").Noted("spawned_objects_keep_state " + Thing + "::OnNetworkSpawn()"),
        // A method only OnNetworkSpawn calls, and the authority NetworkTransform takes at spawn.
        ["SPAWN_HOOK"] = Patch().Redirects("changed " + Thing + "::Arm()").Noted("spawned_objects_keep_state " + Thing + "::Arm()"),
        ["AUTHORITY"] = Patch().Redirects("changed Net.ClientMover::OnIsServerAuthoritative()").Noted("spawned_objects_keep_state Net.ClientMover::OnIsServerAuthoritative()"),
        // OnNetworkPreSpawn takes the NetworkManager by reference (builder6-1).
        ["PRESPAWN"] = Patch().Redirects("changed " + Thing + "::OnNetworkPreSpawn(Unity.Netcode.NetworkManager&)")
            .Noted("spawned_objects_keep_state " + Thing + "::OnNetworkPreSpawn(Unity.Netcode.NetworkManager&)"),
        // A read permission a property of another class supplies is constructor code: not compared
        // as the written argument is (a documented limit), and told (netcode6-1).
        ["VARIABLE_READ_HELPER"] = Patch().Redirects("changed Net.NetRules::get_HiddenRead()").Noted("constructor_applies_to_new_objects Net.NetRules::get_HiddenRead()"),
        // The initial value and the write permission of a network variable are constructor code;
        // its read permission decides the spawn data both ends read.
        ["VARIABLE_INITIAL"] = Patch().Redirects("changed Net.NetPerm::.ctor()").Noted("constructor_applies_to_new_objects Net.NetPerm::.ctor()"),
        ["VARIABLE_WRITE"] = Patch().Redirects("changed Net.NetPerm::.ctor()").Noted("constructor_applies_to_new_objects Net.NetPerm::.ctor()"),
        ["VARIABLE_READ"] = Refused("netcode_variable_changed Net.NetPerm::Secret"),
        ["LIST_READ"] = Refused("netcode_variable_changed Net.NetPerm::Items"),
        ["METHOD_ADDED"] = Patch().Methods(Thing + "::Helper()").Noted("added_method_invisible_by_name " + Thing + "::Helper()"),
        ["FIELD_ADDED"] = Patch().Redirects("changed " + Thing + "::Value()").Fields(Thing + "::plain").Noted("serialized_field_not_restored " + Thing + "::plain"),
        // Patched: the body of an RPC, while the code in front of it that sends it is as it was.
        ["RPC_BODY"] = Patch().Redirects("changed " + Ping),
        ["RPC_BODY_LOCALS"] = Patch().Redirects("changed " + Ping),
        ["RPC_BODY_LAMBDA"] = Patch().Redirects("changed " + Ping),
        // No change: the registrations in another order. The RPCs are looked up by id, and every
        // serializer belongs to one type.
        ["RPC_REORDERED"] = Unchanged(),
        ["SERIALIZATION_ORDER"] = Unchanged(),
        // Refused: the set of RPCs of a class and their ids.
        ["RPC_ADDED"] = Refused("netcode_rpc_changed " + Thing + "::FreshServerRpc()"),
        ["RPC_REMOVED"] = Refused("netcode_rpc_changed " + Thing + "::DropClientRpc()"),
        ["RPC_RENAMED"] = Refused("netcode_rpc_changed " + Thing + "::DropClientRpc()", "netcode_rpc_changed " + Thing + "::GoneClientRpc()"),
        ["RPC_PARAMETER"] = Refused("netcode_rpc_changed " + Ping, "netcode_rpc_changed " + Thing + "::PingRpc(System.Int64)"),
        ["RPC_NO_LONGER"] = Refused("custom_attributes_changed " + Ping, "netcode_rpc_changed " + Ping),
        // Refused: what an RPC sends, and how it is received.
        ["RPC_ATTRIBUTE"] = Refused("custom_attributes_changed " + Ping, "netcode_rpc_changed " + Ping),
        ["RPC_DELIVERY"] = Refused("custom_attributes_changed " + Ping, "netcode_rpc_changed " + Ping),
        ["RPC_SEND"] = Refused("netcode_rpc_changed " + Ping),
        ["RPC_RECEIVE"] = Refused("netcode_rpc_changed " + Ping),
        ["RPC_SERIALIZATION"] = Refused("netcode_rpc_changed " + Ping),
        ["RPC_PERMISSION"] = Refused("netcode_rpc_changed " + Thing + "::__initializeRpcs()"),
        // Refused: the network variables of a class.
        ["VARIABLE_ADDED"] = Refused("netcode_variable_changed " + Thing + "::Extra"),
        ["VARIABLE_REMOVED"] = Refused("netcode_variable_changed " + Thing + "::Speed", "netcode_serialization_changed __GEN.NetworkVariableSerializationHelper::InitializeSerialization()"),
        ["VARIABLE_RENAMED"] = Refused("netcode_variable_changed " + Thing + "::Count", "netcode_variable_changed " + Thing + "::Total"),
        ["VARIABLE_RETYPED"] = Refused("netcode_variable_changed " + Thing + "::Count"),
        ["VARIABLE_REORDERED"] = Refused("netcode_variable_changed " + Thing + "::__initializeVariables()"),
        // Refused: the rest of what is registered at start-up.
        ["SERIALIZATION"] = Refused("netcode_serialization_changed __GEN.NetworkVariableSerializationHelper::InitializeSerialization()"),
        ["TYPE_NAME"] = Refused("netcode_generated_code_changed " + Thing + "::__getTypeName()"),
        ["UNKNOWN_MEMBER"] = Refused("netcode_generated_code_changed " + Thing + "::__initializeSomethingNew()"),
        ["BRIDGE"] = Refused("netcode_generated_code_changed __GEN.UnityPlayNetcodeInitializers"),
        ["BECAME"] = Refused("base_type_changed Net.Late", "netcode_generated_code_changed Net.Late"),
    };

    static void Netcode(string root, string unityStub, string netcodeStub)
    {
        string source = Fixture("Classify", "Net.cs");
        var references = new[] { unityStub, netcodeStub };
        string loaded = Compile(Path.Combine(root, "net", "loaded", "Net.dll"), new[] { source }, references);
        var cases = NetcodeCases();
        Parallel.ForEach(cases, new ParallelOptions { MaxDegreeOfParallelism = 8 }, pair =>
        {
            string label = "CASE_NET_" + pair.Key, directory = Path.Combine(root, "net", pair.Key);
            try
            {
                string current = Compile(Path.Combine(directory, "Net.dll"), new[] { source }, references, define: label);
                Expect(label, Patcher(label, directory, 1, new[] { new AssemblyPair("Net", loaded, current) }), "Net", pair.Value);
            }
            catch (Exception e) { Check(false, $"{label}: {e.Message}"); }
        });
        var declared = new SortedSet<string>(StringComparer.Ordinal);
        foreach (string line in File.ReadLines(source))
            foreach (string word in line.Split(new[] { ' ', '!', '(', ')', '|', '&' }, StringSplitOptions.RemoveEmptyEntries))
                if (word.StartsWith("CASE_NET_", StringComparison.Ordinal) && line.TrimStart().StartsWith('#')) declared.Add(word["CASE_NET_".Length..]);
        foreach (string name in declared) Check(cases.ContainsKey(name), $"Fixture symbol CASE_NET_{name} has no expectation.");
        foreach (string name in cases.Keys) Check(declared.Contains(name), $"Expectation NET_{name} has no fixture symbol.");

        // Another compilation of the same source means the same: nothing to report.
        string again = Compile(Path.Combine(root, "net", "again", "Net.dll"), new[] { source }, references, define: "NOTHING");
        Check(!File.ReadAllBytes(loaded).AsSpan().SequenceEqual(File.ReadAllBytes(again)), "netcode, no edit: the two compilations must differ in bytes for the test to mean anything.");
        Expect("netcode, no edit", Patcher("netcode, no edit", Path.Combine(root, "net-again"), 1, new[] { new AssemblyPair("Net", loaded, again) }), "Net", Silent());

        // An image that references the Netcode runtime and is not in the post-processor's form:
        // no start-up registration, a NetworkBehaviour without the generated members, an RPC
        // without its handler, an RPC whose body does not start with its send code. No edit of
        // such an assembly is patched, and the reason says what is missing.
        string raw = Fixture("Classify", "NetRaw.cs");
        foreach (var (form, said) in new[] { ("RAW_NO_REGISTRATION", "InitializeSerialization is missing"), ("RAW_BEHAVIOUR", "has no __initializeRpcs"),
            ("RAW_HANDLER", "has 1 RPCs and 0 methods named __rpc_handler_"), ("RAW_HEAD", "does not start with the code that sends it") })
        {
            string rawLoaded = Compile(Path.Combine(root, "raw", form, "loaded", "NetRaw.dll"), new[] { raw }, references, define: form);
            string rawCurrent = Compile(Path.Combine(root, "raw", form, "current", "NetRaw.dll"), new[] { raw }, references, define: form + ";CASE_RAW_BODY");
            string member = form == "RAW_NO_REGISTRATION" ? "NetRaw.Uses::Target()" : "NetRaw.Thing::Value()";
            var refused = Patcher("netcode, " + form, Path.Combine(root, "raw-" + form), 1, new[] { new AssemblyPair("NetRaw", rawLoaded, rawCurrent) });
            Expect("netcode, " + form, refused, "NetRaw", Refused("netcode_assembly " + member));
            Check(Message(refused, "netcode_assembly").Contains(said, StringComparison.Ordinal), $"netcode, {form}: the reason does not say '{said}': {Message(refused, "netcode_assembly")}");
            Expect("netcode, " + form + ", no edit", Patcher("netcode, " + form + ", no edit", Path.Combine(root, "raw-same-" + form), 1, new[] { new AssemblyPair("NetRaw", rawLoaded, rawLoaded) }), "NetRaw", Silent());
        }
        // The loaded assembly is in the form and the new compilation is not: refused the same way.
        string formed = Compile(Path.Combine(root, "raw", "formed", "NetRaw.dll"), new[] { raw }, references, define: "RAW_FORMED");
        string unformed = Compile(Path.Combine(root, "raw", "unformed", "NetRaw.dll"), new[] { raw }, references, define: "RAW_HANDLER;CASE_RAW_BODY");
        Expect("netcode, the new compilation is not in the form", Patcher("netcode, the new compilation is not in the form", Path.Combine(root, "raw-unformed"), 1, new[] { new AssemblyPair("NetRaw", formed, unformed) }), "NetRaw",
            Refused("netcode_assembly NetRaw.Thing::PingRpc(System.Int32)"));

        // The Netcode runtime itself registers more than a game assembly (its messages, its own
        // NetworkBehaviours), and none of it is read: an edit of it is refused.
        string runtimeSource = Fixture("Stubs", "Netcode.cs");
        string runtimeEdited = Compile(Path.Combine(root, "runtime", "Unity.Netcode.Runtime.dll"), new[] { runtimeSource }, new[] { unityStub }, define: "CASE_RUNTIME_BODY");
        var runtime = Patcher("netcode, the runtime itself", Path.Combine(root, "runtime-edit"), 1, new[] { new AssemblyPair("Unity.Netcode.Runtime", netcodeStub, runtimeEdited) });
        Expect("netcode, the runtime itself", runtime, "Unity.Netcode.Runtime", Refused("netcode_assembly Unity.Netcode.NetworkManager::Version()"));
        Check(Message(runtime, "netcode_assembly").Contains("the Netcode runtime itself", StringComparison.Ordinal), $"netcode, the runtime itself: the reason does not say so: {Message(runtime, "netcode_assembly")}");

        // An assembly that starts to use Netcode in ordinary code: the post-processor writes its
        // start-up types also when there is nothing to register, and then there is nothing a
        // running Player lacks. With something to register it is refused; the last use going
        // away is the same edit backwards.
        string first = Fixture("Classify", "NetFirst.cs");
        string without = Compile(Path.Combine(root, "first", "without", "NetFirst.dll"), new[] { first }, references);
        string with = Compile(Path.Combine(root, "first", "with", "NetFirst.dll"), new[] { first }, references, define: "USES");
        string registers = Compile(Path.Combine(root, "first", "registers", "NetFirst.dll"), new[] { first }, references, define: "USES;REGISTERS");
        Check(!References(without, "Unity.Netcode.Runtime") && References(with, "Unity.Netcode.Runtime"), "netcode, first use: only the image that uses the API may reference the runtime.");
        Expect("netcode, first use", Patcher("netcode, first use", Path.Combine(root, "first-use"), 1, new[] { new AssemblyPair("NetFirst", without, with) }), "NetFirst",
            Patch().Redirects("changed NetFirst.Lobby::Hosting()"));
        Expect("netcode, last use", Patcher("netcode, last use", Path.Combine(root, "last-use"), 1, new[] { new AssemblyPair("NetFirst", with, without) }), "NetFirst",
            Patch().Redirects("changed NetFirst.Lobby::Hosting()"));
        Expect("netcode, first use that registers", Patcher("netcode, first use that registers", Path.Combine(root, "first-registers"), 1, new[] { new AssemblyPair("NetFirst", without, registers) }), "NetFirst",
            Refused("netcode_generated_code_changed __GEN.NetworkVariableSerializationHelper"));
        Expect("netcode, last use that registered", Patcher("netcode, last use that registered", Path.Combine(root, "last-registers"), 1, new[] { new AssemblyPair("NetFirst", registers, without) }), "NetFirst",
            Refused("netcode_generated_code_changed __GEN.NetworkVariableSerializationHelper"));
    }

    /// The message of the first reason with this code in a result.
    static string Message(JsonElement? result, string code) =>
        result?.GetProperty("assemblies")[0].GetProperty("unsupported").EnumerateArray().Where(u => u.GetProperty("code").GetString() == code).Select(u => u.GetProperty("message").GetString()).FirstOrDefault() ?? "";

    static bool References(string image, string assembly)
    {
        using var pe = new System.Reflection.PortableExecutable.PEReader(File.OpenRead(image));
        var md = System.Reflection.Metadata.PEReaderExtensions.GetMetadataReader(pe);
        return md.AssemblyReferences.Any(h => md.GetString(md.GetAssemblyReference(h).Name) == assembly);
    }

    static void Quietness()
    {
        string root = Path.Combine(work, "quiet");
        // A comment-only edit gives other file bytes and another MVID, and nothing to report.
        string commentLoaded = Compile(Path.Combine(root, "comment", "loaded", "Quiet.dll"), new[] { Fixture("Quiet", "Comment.v1.cs") });
        string commentCurrent = Compile(Path.Combine(root, "comment", "current", "Quiet.dll"), new[] { Fixture("Quiet", "Comment.v2.cs") });
        Check(!File.ReadAllBytes(commentLoaded).AsSpan().SequenceEqual(File.ReadAllBytes(commentCurrent)), "comment-only: the two compilations must differ in bytes for the test to mean anything.");
        Expect("comment-only", Patcher("comment-only", Path.Combine(root, "comment-only"), 1, new[] { new AssemblyPair("Quiet", commentLoaded, commentCurrent) }), "Quiet", Silent());

        // A new file that sorts first moves every token of the files after it.
        string late = Fixture("Quiet", "Late.cs");
        string sortedLoaded = Compile(Path.Combine(root, "sorted", "loaded", "Sorted.dll"), new[] { late });
        string member = Compile(Path.Combine(root, "sorted", "member", "Sorted.dll"), new[] { Fixture("Quiet", "Early.Member.cs"), late });
        string type = Compile(Path.Combine(root, "sorted", "type", "Sorted.dll"), new[] { Fixture("Quiet", "Early.Type.cs"), late });
        Expect("new file with a member", Patcher("new file with a member", Path.Combine(root, "sorted-member"), 1, new[] { new AssemblyPair("Sorted", sortedLoaded, member) }), "Sorted",
            Patch().Methods("Sorted.Late::FromNewFile()"));
        Expect("new file with a type", Patcher("new file with a type", Path.Combine(root, "sorted-type"), 1, new[] { new AssemblyPair("Sorted", sortedLoaded, type) }), "Sorted",
            Refused("type_added Sorted.Fresh"));
    }

    static void CrossAssembly()
    {
        string root = Path.Combine(work, "cross");
        string coreLoaded = Compile(Path.Combine(root, "loaded", "Core.dll"), new[] { Fixture("Cross", "Core.v1.cs") });
        string coreCurrent = Compile(Path.Combine(root, "current", "Core.dll"), new[] { Fixture("Cross", "Core.v2.cs") });
        string appLoaded = Compile(Path.Combine(root, "loaded", "App.dll"), new[] { Fixture("Cross", "App.v1.cs") }, new[] { coreLoaded });
        string appCurrent = Compile(Path.Combine(root, "current", "App.dll"), new[] { Fixture("Cross", "App.v2.cs") }, new[] { coreCurrent });

        // App uses a method and a field that the same change added to Core.
        var result = Patcher("cross-assembly addition", Path.Combine(root, "addition"), 1, new[] { new AssemblyPair("Core", coreLoaded, coreCurrent), new AssemblyPair("App", appLoaded, appCurrent) });
        Expect("cross-assembly addition", result, "App", Refused("cross_assembly_addition App.Shop::Total(Core.Counter)", "cross_assembly_addition App.Shop::Extra(Core.Counter)"));
        if (result is { } document)
        {
            var core = document.GetProperty("assemblies").EnumerateArray().First(a => a.GetProperty("name").GetString() == "Core");
            Check(core.GetProperty("verdict").GetString() == "patch" && document.GetProperty("verdict").GetString() == "unsupported", "cross-assembly addition: Core is patchable on its own, the change as a whole is not.");
        }

        // Stopwatch derives from Core.Timer, and Core is not part of the request. With Core.dll next
        // to the loaded App.dll the chain is read to its end (a plain class: Update is a method);
        // without it the class may be an engine object, and an added Update is refused.
        string appUpdate = Compile(Path.Combine(root, "update", "App.dll"), new[] { Fixture("Cross", "App.v3.cs") }, new[] { coreLoaded });
        Expect("base in a sibling assembly", Patcher("base in a sibling assembly", Path.Combine(root, "sibling"), 1, new[] { new AssemblyPair("App", appLoaded, appUpdate) }), "App",
            Patch().Methods("App.Stopwatch::Update()"));
        string alone = Path.Combine(root, "alone", "App.dll");
        Directory.CreateDirectory(Path.GetDirectoryName(alone)!);
        File.Copy(appLoaded, alone);
        Expect("base out of reach", Patcher("base out of reach", Path.Combine(root, "unreachable"), 1, new[] { new AssemblyPair("App", alone, appUpdate) }), "App",
            Refused("unity_message_added App.Stopwatch::Update()"));
    }

    /// A string of the game's other code names an added method, or the game computes the names
    /// it calls methods of its objects by (Fixtures/Named).
    static void NamedElsewhere(string unityStub)
    {
        string root = Path.Combine(work, "named");
        string lib = Fixture("Named", "Lib.cs"), game = Fixture("Named", "Game.cs");
        string Lib(string directory, string? define = null) => Compile(Path.Combine(root, directory, "Lib.dll"), new[] { lib }, new[] { unityStub }, define);
        string Game(string directory, string lib, string? define = null) => Compile(Path.Combine(root, directory, "Game.dll"), new[] { game }, new[] { unityStub, lib }, define);
        string libLoaded = Lib("loaded"), libCurrent = Lib("current", "NAMED_ADD");
        string gameLoaded = Game("loaded", libLoaded), gameCurrent = Game("current", libCurrent, "NAMED_CALL");
        const string Hurt = "Lib.Target::Hurt(System.Int32,System.Int32)";
        var named = Patch().Methods("Lib.Target::TakeDamage()", "Lib.Commands::Reset()", Hurt)
            .Noted("added_method_named_by_string Lib.Target::TakeDamage()", "added_method_named_by_string Lib.Commands::Reset()", "added_method_invisible_by_name " + Hurt);
        // Both assemblies change in one request.
        var both = Patcher("named by another entry", Path.Combine(root, "both"), 1, new[] { new AssemblyPair("Lib", libLoaded, libCurrent), new AssemblyPair("Game", gameLoaded, gameCurrent) });
        Expect("named by another entry", both, "Lib", named);
        Expect("named by another entry", both, "Game", Patch().Redirects("changed Game.Shooter::Update()"));
        // The strings stand in an assembly the edit does not touch, next to the loaded one.
        string calling = Lib("sibling-call");
        Game("sibling-call", calling, "NAMED_CALL");
        Expect("named by a sibling", Patcher("named by a sibling", Path.Combine(root, "sibling-call-out"), 1, new[] { new AssemblyPair("Lib", calling, libCurrent) }), "Lib", named);
        // The sibling calls by a name it computes: any added method of an engine object may be meant.
        string computing = Lib("sibling-computed");
        Game("sibling-computed", computing, "NAMED_COMPUTED");
        // A method with two parameters is no target of such a call (builder6-9).
        Expect("computed name", Patcher("computed name", Path.Combine(root, "sibling-computed-out"), 1, new[] { new AssemblyPair("Lib", computing, libCurrent) }), "Lib",
            Patch().Methods("Lib.Target::TakeDamage()", "Lib.Commands::Reset()", Hurt).Noted("added_method_computed_name Lib.Target::TakeDamage()", "added_method_invisible_by_name " + Hurt));
        // Neither: the note of every added method of an engine object.
        Expect("not named", Patcher("not named", Path.Combine(root, "plain-out"), 1, new[] { new AssemblyPair("Lib", libLoaded, libCurrent) }), "Lib",
            Patch().Methods("Lib.Target::TakeDamage()", "Lib.Commands::Reset()", Hurt).Noted("added_method_invisible_by_name Lib.Target::TakeDamage()", "added_method_invisible_by_name " + Hurt));

        // A sibling that references nothing of Lib calls the added method by SendMessage (builder6-5).
        string striking = Lib("sibling-remote");
        Compile(Path.Combine(root, "sibling-remote", "Remote.dll"), new[] { Fixture("Named", "Remote.cs") }, new[] { unityStub });
        Expect("named by a sibling without a reference", Patcher("named by a sibling without a reference", Path.Combine(root, "sibling-remote-out"), 1,
            new[] { new AssemblyPair("Lib", striking, Lib("current-strike", "NAMED_STRIKE")) }), "Lib",
            Patch().Methods("Lib.Target::Strike()").Noted("added_method_named_by_string Lib.Target::Strike()"));

        // A package next to the game sends messages by a name it makes: an added method named like
        // a message may be meant, another is not (builder6-7).
        string messaged = Lib("package");
        Compile(Path.Combine(root, "package", "Unity.Messages.dll"), new[] { Fixture("Named", "Package.cs") }, new[] { unityStub });
        var package = Patcher("package message", Path.Combine(root, "package-out"), 1, new[] { new AssemblyPair("Lib", messaged, Lib("current-message", "NAMED_MESSAGE")) });
        Expect("package message", package, "Lib", Patch().Methods("Lib.Target::OnJump()", "Lib.Target::Jump()")
            .Noted("added_method_computed_name Lib.Target::OnJump()", "added_method_invisible_by_name Lib.Target::Jump()"));
        Check(Message(package, "added_method_computed_name", "Lib.Target::OnJump()").Contains("Unity.Messages.Sender (Unity.Messages)", StringComparison.Ordinal),
            "package message: the note does not name the package's call: " + Message(package, "added_method_computed_name", "Lib.Target::OnJump()"));

        // An override of a hook that a base class of another assembly calls only from Start (builder6-3).
        string hookLib = Lib("hook");
        string orcLoaded = Game("hook", hookLib), orcCurrent = Game("hook-current", hookLib, "NAMED_ORC");
        var hook = Patcher("hook", Path.Combine(root, "hook-out"), 1, new[] { new AssemblyPair("Game", orcLoaded, orcCurrent) });
        Expect("hook", hook, "Game", Patch().Redirects("changed Game.Orc::OnSpawned()").Noted("started_objects_keep_state Game.Orc::OnSpawned()"));
        Check(Message(hook, "started_objects_keep_state", "Game.Orc::OnSpawned()").Contains("Lib.Unit::Start() (Lib)", StringComparison.Ordinal),
            "hook: the note does not name the base class's Start: " + Message(hook, "started_objects_keep_state", "Game.Orc::OnSpawned()"));
    }

    /// The message of a note of the first assembly of a result; "?" when it has none.
    static string Message(JsonElement? result, string code, string member)
    {
        if (result is { } root)
            foreach (var assembly in root.GetProperty("assemblies").EnumerateArray())
                foreach (var note in assembly.GetProperty("notes").EnumerateArray())
                    if (note.GetProperty("code").GetString() == code && note.GetProperty("member").GetString() == member)
                        return note.GetProperty("message").GetString()!;
        return "?";
    }

    static void Interface()
    {
        string directory = Path.Combine(work, "interface");
        Directory.CreateDirectory(directory);
        string bad = Path.Combine(directory, "bad.json");
        File.WriteAllText(bad, "{\"schemaVersion\":2}");
        var (exit, output, error) = Run(dotnet, new[] { patcher, bad });
        Check(exit == 1 && output.Length == 0 && error.Trim().Length > 0, $"A refused request must exit 1 with a message on stderr only (exit {exit}, stdout '{output}', stderr '{error}').");
        File.WriteAllText(bad, "{\"schemaVersion\":1,\"generation\":1,\"output\":\"" + directory + "\",\"assemblies\":[{\"name\":\"X\",\"loaded\":\"/nowhere/X.dll\",\"current\":\"/nowhere/X.dll\"}]}");
        (exit, output, error) = Run(dotnet, new[] { patcher, bad });
        Check(exit == 1 && output.Length == 0 && error.Contains("/nowhere/X.dll", StringComparison.Ordinal), $"A missing assembly must exit 1 and name the file (exit {exit}, stderr '{error}').");

        // A defect of the tool on one assembly is a refusal with its reason: no crash, no patch.
        string loadedEdits = Path.Combine(work, "classify", "loaded", "Edits.dll"), editedEdits = Path.Combine(work, "classify", "BODY", "Edits.dll");
        foreach (string fault in new[] { "diff", "emit" })
        {
            var failed = Patcher("fault in " + fault, Path.Combine(directory, "fault-" + fault), 1, new[] { new AssemblyPair("Edits", loadedEdits, editedEdits) }, fault: fault);
            Expect("fault in " + fault, failed, "Edits", Refused("internal_error Edits"));
            if (failed is { } document)
                Check(document.GetProperty("assemblies")[0].GetProperty("unsupported")[0].GetProperty("message").GetString()!.Contains("InvalidOperationException", StringComparison.Ordinal), $"fault in {fault}: the reason does not carry the exception.");
        }
        // A previous result that is not one is a refused request.
        File.WriteAllText(bad, "{\"schemaVersion\":1}");
        string withPrevious = WriteRequest(Path.Combine(directory, "previous"), 2, new[] { new AssemblyPair("Edits", loadedEdits, editedEdits) }, previous: bad);
        (exit, output, error) = Run(dotnet, new[] { patcher, withPrevious });
        Check(exit == 1 && output.Length == 0 && error.Contains("previous result", StringComparison.Ordinal), $"An unreadable previous result must exit 1 (exit {exit}, stdout '{output}', stderr '{error}').");

        // The code table: lower_snake_case, one meaning each, and every code is produced by a test.
        (exit, output, error) = Run(dotnet, new[] { patcher, "--codes" });
        Check(exit == 0, "--codes failed: " + error);
        if (exit != 0) return;
        var table = JsonDocument.Parse(output).RootElement;
        foreach (string group in new[] { "unsupported", "notes" })
        {
            foreach (var code in table.GetProperty(group).EnumerateObject())
            {
                Check(code.Name.All(c => c is >= 'a' and <= 'z' or '_') && code.Value.GetString()!.Length > 10, $"Code {code.Name} is not lower_snake_case or has no meaning.");
                Check(producedCodes.ContainsKey(group + ":" + code.Name), $"No test produces the {group} code {code.Name}.");
            }
            foreach (string produced in producedCodes.Keys.Where(k => k.StartsWith(group + ":", StringComparison.Ordinal)))
                Check(table.GetProperty(group).TryGetProperty(produced[(group.Length + 1)..], out _), $"The result used the code {produced}, which the code table does not list.");
        }
    }

    // ---- behaviour on Unity's Mono ---------------------------------------------------------

    static (int Exit, string Output, string Error) Mono(string directory, params string[] arguments)
    {
        var environment = new Dictionary<string, string> { ["MONO_PATH"] = bclJit + ":" + directory, ["MONO_CFG_DIR"] = monoEtc };
        // Inlining is off, as in a Player that hot reload starts: a redirect cannot reach a body
        // that the JIT copied into its caller.
        return Run(mono, new[] { "--debug", "-O=-float32,-inline" }.Concat(arguments), directory, environment);
    }

    static string WriteApply(string path, JsonElement result, string runtime)
    {
        var text = new StringBuilder("runtime\t" + runtime + "\n");
        foreach (var a in result.GetProperty("assemblies").EnumerateArray())
        {
            var patch = a.GetProperty("patch");
            if (patch.GetProperty("path").GetString()!.Length == 0) continue;
            string name = a.GetProperty("name").GetString()!;
            text.Append($"patch\t{name}\t{a.GetProperty("loadedMvid").GetString()}\t{patch.GetProperty("path").GetString()}\t{patch.GetProperty("pdb").GetString()}\n");
            foreach (var d in a.GetProperty("detours").EnumerateArray())
                text.Append($"detour\t{name}\t{d.GetProperty("originalToken").GetInt32()}\t{d.GetProperty("replacementToken").GetInt32()}\n");
        }
        File.WriteAllText(path, text.ToString());
        return path;
    }

    static Dictionary<string, string> Lines(string output)
    {
        var map = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (string line in output.Split('\n', StringSplitOptions.RemoveEmptyEntries))
        {
            int at = line.IndexOf('=');
            if (at > 0) map[line[..at]] = line[(at + 1)..];
        }
        return map;
    }

    static void Behaviour(string unityStub)
    {
        string root = Path.Combine(work, "behaviour"), loaded = Path.Combine(root, "loaded"), current = Path.Combine(root, "current");
        string coreLoaded = Compile(Path.Combine(loaded, "Game.Core.dll"), new[] { Fixture("Behaviour", "Core.v1.cs") });
        string coreCurrent = Compile(Path.Combine(current, "Game.Core.dll"), new[] { Fixture("Behaviour", "Core.v2.cs") });
        string gameLoaded = Compile(Path.Combine(loaded, "Game.dll"), new[] { Fixture("Behaviour", "Game.v1.cs"), Fixture("Behaviour", "More.v1.cs") }, new[] { coreLoaded, unityStub });
        string gameCurrent = Compile(Path.Combine(current, "Game.dll"), new[] { Fixture("Behaviour", "Game.v2.cs"), Fixture("Behaviour", "More.v2.cs") }, new[] { coreCurrent, unityStub });
        // The driver is compiled against what the Player loaded and used unchanged with both.
        string driver = Compile(Path.Combine(loaded, "Driver.dll"), new[] { Fixture("Behaviour", "Driver.cs") }, new[] { coreLoaded, gameLoaded, unityStub });
        string runtime = Compile(Path.Combine(loaded, "UnityMicroEditor.HotReload.dll"), new[] { Fixture("Host", "FieldStore.cs") });
        string host = Compile(Path.Combine(loaded, "Host.exe"), new[] { Fixture("Host", "Host.cs") }, target: "exe", library: bclJit);
        foreach (string file in new[] { driver, host, unityStub })
        {
            File.Copy(file, Path.Combine(current, Path.GetFileName(file)), true);
            if (file == unityStub) File.Copy(file, Path.Combine(loaded, Path.GetFileName(file)), true);
        }

        var pairs = new[] { new AssemblyPair("Game.Core", coreLoaded, coreCurrent), new AssemblyPair("Game", gameLoaded, gameCurrent) };
        var result = Patcher("behaviour", Path.Combine(root, "patch"), 3, pairs);
        if (result is not { } document) return;
        Check(document.GetProperty("verdict").GetString() == "patch", "behaviour: the fixture must be patchable: " + string.Join("; ",
            document.GetProperty("assemblies").EnumerateArray().SelectMany(a => a.GetProperty("unsupported").EnumerateArray()).Select(u => u.GetProperty("code").GetString() + " " + u.GetProperty("member").GetString() + ": " + u.GetProperty("message").GetString())));
        if (document.GetProperty("verdict").GetString() != "patch") return;
        var game = document.GetProperty("assemblies").EnumerateArray().First(a => a.GetProperty("name").GetString() == "Game");
        var core = document.GetProperty("assemblies").EnumerateArray().First(a => a.GetProperty("name").GetString() == "Game.Core");
        var detours = Pairs(game, "detours", "kind", "member");
        var notes = Pairs(game, "notes", "code", "member");
        // Unchanged methods whose generated members were renumbered stay out of the patch.
        foreach (string quiet in new[] { "Best()", "CountBelowHealth()", "Evens(System.Int32)", "MakePair()", "Where()", "Secret()" })
            Check(!detours.Any(d => d.EndsWith("Game.Player::" + quiet, StringComparison.Ordinal)), $"behaviour: the unchanged method {quiet} was reported as changed.");
        foreach (string expected in new[] { "changed Game.Player::Damage(System.Int32)", "changed Game.Vec3::.ctor(System.Single,System.Single,System.Single)", "changed Game.Inventory+Slot::Show()",
            "changed Game.Player::Game.IScored.Score()", "removed Game.Spinner::Update()", "lambda Game.Player::MakeAdder(System.Int32) [lambda 0]",
            "lambda Game.Player::BelowHealth() [lambda 0]", "lambda Game.Player::Doubler() [lambda 0]" })
            Check(detours.Contains(expected), $"behaviour: the redirect '{expected}' is missing from [{string.Join(" | ", detours)}].");
        Check(!detours.Any(d => d.Contains("Ticker(System.Int32) [lambda", StringComparison.Ordinal)), "behaviour: a local function that takes its closure by reference was redirected.");
        // Lambdas are redirected exactly where the code that creates them is the same in both versions.
        foreach (string expected in new[] { "changed Game.Figures::Grow(Game.Figure)", "lambda Game.Wiring::Greeter(System.String) [lambda 0]", "lambda Game.Wiring::Numbers(System.Int32) [lambda 0]",
            "lambda Game.Wiring::Twins(" + Lists(2) + ") [lambda 1]", "lambda Game.Wiring::Swapped(" + Lists(2) + ") [lambda 0]", "lambda Game.Wiring::Swapped(" + Lists(2) + ") [lambda 1]",
            "lambda Game.Wiring::Makers() [lambda 0]", "lambda Game.Wiring::Makers() [lambda 1]", "lambda Game.Wiring::Nested(System.Int32) [lambda 0]",
            "lambda Game.Echo::Caller(System.Int32) [lambda 0]", "lambda Game.Echo::Scaler() [lambda 0]", "lambda Game.Echo::Counters(System.Int32) [lambda 0]" })
            Check(detours.Contains(expected), $"behaviour: the redirect '{expected}' is missing from [{string.Join(" | ", detours)}].");
        foreach (string unexpected in new[] { "lambda Game.Wiring::Replaced(", "lambda Game.Wiring::Around(", "lambda Game.Wiring::Twins(" + Lists(2) + ") [lambda 0]", "lambda Game.Wiring::Nested(System.Int32) [lambda 1]", "lambda Game.Echo::Scaler() [lambda 1]" })
            Check(!detours.Any(d => d.StartsWith(unexpected, StringComparison.Ordinal)), $"behaviour: '{unexpected}' was redirected without a proven partner or without an edit.");
        foreach (string expected in new[] { "lambda_not_remapped Game.Wiring::Replaced(" + Lists(3) + ")", "running_coroutines_keep_old_code Game.Shifting::FromIterator(System.Int32)", "running_coroutines_keep_old_code Game.Shifting::FromAsync(System.Int32)" })
            Check(notes.Contains(expected), $"behaviour: the note '{expected}' is missing from [{string.Join(" | ", notes)}].");
        foreach (string unexpected in new[] { "lambda_not_remapped Game.Wiring::Around(System.Int32)", "lambda_not_remapped Game.Wiring::Nested(System.Int32)", "running_coroutines_keep_old_code Game.Shifting::ToIterator(System.Int32)", "running_coroutines_keep_old_code Game.Shifting::ToAsync(System.Int32)" })
            Check(!notes.Contains(unexpected), $"behaviour: the note '{unexpected}' has nothing to tell.");
        // `readonly` on an instance field, and `readonly` added to a static one, is no change at all.
        Check(!game.GetProperty("changes").EnumerateArray().Any(c => c.GetProperty("member").GetString()!.StartsWith("Game.Frozen::", StringComparison.Ordinal) && c.GetProperty("kind").GetString()!.StartsWith("field", StringComparison.Ordinal)),
            "behaviour: a field of Game.Frozen was reported as changed.");
        foreach (string expected in new[] { "lambda_not_remapped Game.Player::MakeCounter(System.Int32)", "lambda_not_remapped Game.Player::Ticker(System.Int32)", "running_coroutines_keep_old_code Game.Player::Countdown(System.Int32)",
            "field_value_reset Game.Player::speed", "existing_instances_keep_default Game.Player::armor", "constructor_applies_to_new_objects Game.Player::.ctor()" })
            Check(notes.Contains(expected), $"behaviour: the note '{expected}' is missing from [{string.Join(" | ", notes)}].");
        Same("behaviour", "added fields of Game", new[] { "Game.Big::twices", "Game.Inventory::<Capacity>k__BackingField", "Game.Player::Struck", "Game.Player::armor", "Game.Player::bonusPool", "Game.Player::offset", "Game.Player::speed", "Game.Player::struck" },
            Singles(game, "addedFields", "member"));
        Same("behaviour", "added fields of Game.Core", new[] { "Game.Core.Counter::step" }, Singles(core, "addedFields", "member"));
        Check(game.GetProperty("patch").GetProperty("pdb").GetString()!.Length > 0, "behaviour: the patch of Game has no PDB.");

        string apply = WriteApply(Path.Combine(root, "apply.txt"), document, runtime);
        string[] loadedFiles = { coreLoaded, gameLoaded, Path.Combine(loaded, Path.GetFileName(unityStub)), driver };
        string[] currentFiles = { coreCurrent, gameCurrent, Path.Combine(current, Path.GetFileName(unityStub)), Path.Combine(current, "Driver.dll") };
        var reference = Mono(current, new[] { Path.Combine(current, "Host.exe"), "reference", "Driver" }.Concat(currentFiles).ToArray());
        var patched = Mono(loaded, new[] { host, "patched", apply, "Driver" }.Concat(loadedFiles).ToArray());
        File.WriteAllText(Path.Combine(root, "reference.txt"), reference.Output + reference.Error);
        File.WriteAllText(Path.Combine(root, "patched.txt"), patched.Output + patched.Error);
        Check(reference.Exit == 0, "behaviour: the new compilation does not run on Mono: " + reference.Error);
        Check(patched.Exit == 0, "behaviour: the patched run failed on Mono: " + patched.Error + patched.Output);
        if (reference.Exit != 0 || patched.Exit != 0) return;

        // State from before the patch: (patched, fresh start).
        var stateful = new Dictionary<string, (string Patched, string Fresh)>
        {
            ["existing.Damage"] = ("94", "104"),          // the added field `armor` reads 0 in an object that already existed
            ["existing.Speed"] = ("0", "2.5"),            // the retyped field is stored anew
            ["existing.Counter"] = ("1", "4"),            // the added field `step` reads 0
            ["stale.Counter"] = ("11", "15"),             // a delegate over a lambda that could not be remapped keeps the old body
            ["running.Countdown"] = ("2,1", "20,10,liftoff"), // an iterator that was already running finishes on the old code
            ["running.Ticker"] = ("4,6", "40,60"),        // ... including its local function, which takes the iterator's state by reference
            ["running.FromIterator"] = ("6", "end"),      // an iterator of a method that is a plain method now
            ["existing.Nested inner"] = ("7", "5007"),    // a closure the old outer lambda filled before the patch
            ["stale.Replaced.heal"] = ("sparkle", ""),    // lambda_not_remapped: the old subscription keeps its lambda, which the new code no longer has
            ["stale.Replaced.death"] = ("", "explode"),   // ... and nothing subscribed the new one before the patch
        };
        var fresh = Lines(reference.Output);
        var live = Lines(patched.Output);
        Check(fresh.Count >= 60, $"behaviour: the scenario printed only {fresh.Count} lines.");
        Same("behaviour", "the labels of both runs", fresh.Keys, live.Keys.OrderBy(x => x, StringComparer.Ordinal).ToList());
        foreach (var (label, value) in fresh)
        {
            if (!live.TryGetValue(label, out var actual)) continue;
            Check(!value.StartsWith("THROWS", StringComparison.Ordinal), $"behaviour: '{label}' throws on the new compilation: {value}");
            if (stateful.TryGetValue(label, out var known))
                Check(actual == known.Patched && value == known.Fresh, $"behaviour: '{label}' is {actual} patched and {value} fresh; expected {known.Patched} and {known.Fresh}.");
            else Check(actual == value, $"behaviour: '{label}' is {actual} with the patch and {value} on the new compilation.");
        }
        // A stack trace through patched code names the edited source file and line.
        int line = Array.FindIndex(File.ReadAllLines(Fixture("Behaviour", "Game.v2.cs")), text => text.Contains("THROW-LINE", StringComparison.Ordinal)) + 1;
        Check(live.GetValueOrDefault("Where") == $"Game.v2.cs:{line}", $"behaviour: the patched frame reads '{live.GetValueOrDefault("Where")}', expected Game.v2.cs:{line}.");
        // The same in an added instance method, whose instructions follow a receiver check.
        line = Array.FindIndex(File.ReadAllLines(Fixture("Behaviour", "More.v2.cs")), text => text.Contains("THROW-LINE-ADDED", StringComparison.Ordinal)) + 1;
        Check(live.GetValueOrDefault("Optional.Trace") == $"More.v2.cs:{line}", $"behaviour: the frame of the added method reads '{live.GetValueOrDefault("Optional.Trace")}', expected More.v2.cs:{line}.");
        Check(live.GetValueOrDefault("Optional.Call null") == "NRE" && live.GetValueOrDefault("Quad.Grow") == "quad:Quad:11:4", "behaviour: a null receiver of an added method must throw, and a derived record must stay derived.");

        // Every body of the new compilation forced into a patch, and each compiled by Mono's JIT.
        var all = Patcher("every body", Path.Combine(root, "all"), 9, new[] { new AssemblyPair("Game.Core", coreCurrent, coreCurrent), new AssemblyPair("Game", gameCurrent, gameCurrent) }, forceAll: true);
        if (all is not { } forced) return;
        Check(forced.GetProperty("verdict").GetString() == "patch", "every body: not patchable: " + forced.GetProperty("assemblies"));
        if (forced.GetProperty("verdict").GetString() != "patch") return;
        string applyAll = WriteApply(Path.Combine(root, "apply-all.txt"), forced, runtime);
        File.Copy(runtime, Path.Combine(current, Path.GetFileName(runtime)), true);
        var jit = Mono(current, new[] { Path.Combine(current, "Host.exe"), "jit", applyAll }.Concat(currentFiles.Take(3)).ToArray());
        Check(jit.Exit == 0 && jit.Output.Contains("failed=0", StringComparison.Ordinal), "every body: Mono could not compile the patch:\n" + jit.Output + jit.Error);
        int count = forced.GetProperty("summary").GetProperty("patchedMethods").GetInt32();
        Check(count >= 70, $"every body: only {count} methods were forced into the patch.");
    }

    // ---- two patches in a row --------------------------------------------------------------

    static void Generations()
    {
        string root = Path.Combine(work, "generations"), source = Fixture("Generations", "Gens.cs");
        string loaded = Compile(Path.Combine(root, "loaded", "Gens.dll"), new[] { source });
        string first = Compile(Path.Combine(root, "first", "Gens.dll"), new[] { source }, define: "GEN_1");
        string second = Compile(Path.Combine(root, "second", "Gens.dll"), new[] { source }, define: "GEN_2");
        AssemblyPair[] To(string current) => new[] { new AssemblyPair("Gens", loaded, current) };
        const string earlier = "earlier_patch_delegates_keep_old_code ", identity = "added_method_delegate_identity ";

        // The first patch lists what only it defines: nothing will ever redirect into these methods.
        var one = Patcher("generation 1", Path.Combine(root, "g1"), 1, To(first));
        string previous = Path.Combine(root, "g1", "result.json");
        Expect("generation 1", one, "Gens", Patch()
            .Redirects("changed Gens.Thing::Edit()", "changed Gens.Thing::Count()", "changed Gens.Thing::Subscribe()", "changed Gens.Thing::Unsubscribe()", "changed Gens.Thing::UseSteady()",
                "changed Gens.Thing::Scaled(System.Int32)", "lambda Gens.Thing::Scaled(System.Int32) [lambda 0]", "changed Gens.Thing::Kept(System.Int32)", "changed Gens.Thing::Steps()")
            .Methods("Gens.Thing::Handler()", "Gens.Thing::Steady(System.Int32)").Fields("Gens.Thing::count")
            .Noted("running_coroutines_keep_old_code Gens.Thing::Steps()"));
        if (one is not { } result) return;
        var members = Pairs(result.GetProperty("assemblies")[0], "patchMembers", "kind", "member");
        foreach (string expected in new[] { "added Gens.Thing::Handler()", "added Gens.Thing::Steady(System.Int32)", "lambda Gens.Thing::Scaled(System.Int32) [lambda 0]", "lambda Gens.Thing::Kept(System.Int32) [lambda 0]" })
            Check(members.Contains(expected), $"generation 1: the patch member '{expected}' is missing from [{string.Join(" | ", members)}].");
        Check(members.Any(m => m.StartsWith("state_machine Gens.Thing::Steps() [", StringComparison.Ordinal) && m.EndsWith("::MoveNext]", StringComparison.Ordinal)), $"generation 1: the state machine of Steps() is not a patch member: [{string.Join(" | ", members)}].");
        Check(members.Any(m => m.StartsWith("closure Gens.Thing::Scaled(System.Int32) [", StringComparison.Ordinal)), "generation 1: the closure class of Scaled() is not a patch member.");
        // A replacement of a loaded method is reached through the loaded method, and is not listed.
        Check(!members.Any(m => m.EndsWith(" Gens.Thing::Edit()", StringComparison.Ordinal) || m.EndsWith(" Gens.Thing::Scaled(System.Int32)", StringComparison.Ordinal)), "generation 1: a replacement of a loaded method is listed as a patch member.");

        // The second patch changes the handler, the lambda of Scaled and the iterator; it keeps
        // Steady and the lambda of Kept. Edit() is as loaded again, so it is not redirected.
        string[] common = { "changed Gens.Thing::Count()", "changed Gens.Thing::Subscribe()", "changed Gens.Thing::Unsubscribe()", "changed Gens.Thing::UseSteady()",
            "changed Gens.Thing::Scaled(System.Int32)", "lambda Gens.Thing::Scaled(System.Int32) [lambda 0]", "changed Gens.Thing::Kept(System.Int32)", "changed Gens.Thing::Steps()" };
        var two = Patcher("generation 2", Path.Combine(root, "g2"), 2, To(second), previous: previous);
        Expect("generation 2", two, "Gens", Patch().Redirects(common).Methods("Gens.Thing::Handler()", "Gens.Thing::Steady(System.Int32)").Fields("Gens.Thing::count")
            .Noted("running_coroutines_keep_old_code Gens.Thing::Steps()",
                earlier + "Gens.Thing::Handler()", earlier + "Gens.Thing::Scaled(System.Int32)", earlier + "Gens.Thing::Steps()", identity + "Gens.Thing::Handler()"));
        // Without the previous result nothing is known about an earlier patch.
        Expect("generation 2, previous unknown", Patcher("generation 2, previous unknown", Path.Combine(root, "g2-alone"), 2, To(second)), "Gens",
            Patch().Redirects(common).Methods("Gens.Thing::Handler()", "Gens.Thing::Steady(System.Int32)").Fields("Gens.Thing::count")
                .Noted("running_coroutines_keep_old_code Gens.Thing::Steps()"));
        // The same code as another patch: its copies do the same, but a delegate of an added method is still another one.
        Expect("generation 2, same code", Patcher("generation 2, same code", Path.Combine(root, "g2-same"), 2, To(first), previous: previous), "Gens",
            Patch().Redirects(common.Append("changed Gens.Thing::Edit()").ToArray()).Methods("Gens.Thing::Handler()", "Gens.Thing::Steady(System.Int32)").Fields("Gens.Thing::count")
                .Noted("running_coroutines_keep_old_code Gens.Thing::Steps()", identity + "Gens.Thing::Handler()"));
        // A previous result that knows nothing of this assembly has nothing to compare.
        string foreign = Path.Combine(root, "foreign.json");
        File.WriteAllText(foreign, "{\"schemaVersion\":1,\"generation\":1,\"assemblies\":[{\"name\":\"Other\",\"patchMembers\":[{\"member\":\"Other.A::B()\",\"kind\":\"added\",\"bodyHash\":\"00\"}]},{\"name\":\"Gens\"}]}");
        Expect("generation 2, previous of another assembly", Patcher("generation 2, previous of another assembly", Path.Combine(root, "g2-foreign"), 2, To(second), previous: foreign), "Gens",
            Patch().Redirects(common).Methods("Gens.Thing::Handler()", "Gens.Thing::Steady(System.Int32)").Fields("Gens.Thing::count")
                .Noted("running_coroutines_keep_old_code Gens.Thing::Steps()"));
        // The revision of a note tells a note that only still stands from one about code that
        // changed again: the iterator differs in the second patch and is the same in a repetition
        // of the first. A note about a field has none, one about the step between two patches
        // names the generation.
        static string Revision(JsonElement? result, string code, string member)
        {
            if (result is not { } root) return "?";
            foreach (var note in root.GetProperty("assemblies")[0].GetProperty("notes").EnumerateArray())
                if (note.GetProperty("code").GetString() == code && note.GetProperty("member").GetString() == member)
                    return note.TryGetProperty("revision", out var revision) ? revision.GetString()! : "";
            return "?";
        }
        var same = Patcher("generation 2, same code again", Path.Combine(root, "g2-same-again"), 2, To(first), previous: previous);
        string stepsOne = Revision(one, "running_coroutines_keep_old_code", "Gens.Thing::Steps()"), stepsTwo = Revision(two, "running_coroutines_keep_old_code", "Gens.Thing::Steps()");
        Check(stepsOne.Length == 16 && stepsOne.All(Uri.IsHexDigit), $"generation 1: the note of Steps() has the revision '{stepsOne}', expected 16 hex digits.");
        Check(stepsTwo.Length == 16 && stepsTwo != stepsOne, $"generation 2: the iterator changed, but its note has the revision '{stepsTwo}' after '{stepsOne}'.");
        Check(Revision(same, "running_coroutines_keep_old_code", "Gens.Thing::Steps()") == stepsOne, "generation 2, same code: the note of the unchanged iterator has another revision.");
        Check(Revision(two, "earlier_patch_delegates_keep_old_code", "Gens.Thing::Handler()") == "g2", "generation 2: a note about the earlier patch does not name its generation as the revision.");
        // Two copies of an added method that code makes a delegate of are a condition that stands
        // from patch to patch: with a generation as its revision the note would be new in every one.
        Check(Revision(two, "added_method_delegate_identity", "Gens.Thing::Handler()") == "" && Revision(same, "added_method_delegate_identity", "Gens.Thing::Handler()") == "",
            $"generation 2: the note about the delegate of an added method has the revisions '{Revision(two, "added_method_delegate_identity", "Gens.Thing::Handler()")}' and '{Revision(same, "added_method_delegate_identity", "Gens.Thing::Handler()")}', expected none.");

        // Everything reverted: nothing to patch, and all the first patch brought stays behind,
        // except the lambda of Kept, which is the loaded one again.
        Expect("generation 2, reverted", Patcher("generation 2, reverted", Path.Combine(root, "g2-reverted"), 2, To(loaded), previous: previous), "Gens",
            Unchanged().Noted(earlier + "Gens.Thing::Handler()", earlier + "Gens.Thing::Steady(System.Int32)", earlier + "Gens.Thing::Scaled(System.Int32)", earlier + "Gens.Thing::Steps()"));

        // On Mono: the facts those notes are about, and what a later patch must keep working.
        if (two is not { } latest || result.GetProperty("verdict").GetString() != "patch" || latest.GetProperty("verdict").GetString() != "patch") return;
        string driver = Compile(Path.Combine(root, "loaded", "Driver.dll"), new[] { Fixture("Generations", "Driver.cs") }, new[] { loaded });
        string runtime = Compile(Path.Combine(root, "loaded", "UnityMicroEditor.HotReload.dll"), new[] { Fixture("Host", "FieldStore.cs") });
        string host = Compile(Path.Combine(root, "loaded", "Host.exe"), new[] { Fixture("Host", "Host.cs") }, target: "exe", library: bclJit);
        string applies = WriteApply(Path.Combine(root, "apply1.txt"), result, runtime) + "," + WriteApply(Path.Combine(root, "apply2.txt"), latest, runtime);
        var run = Mono(Path.Combine(root, "loaded"), host, "generations", applies, "Driver", loaded, driver);
        File.WriteAllText(Path.Combine(root, "patched.txt"), run.Output + run.Error);
        Check(run.Exit == 0, "generations: the run failed on Mono: " + run.Error + run.Output);
        if (run.Exit != 0) return;
        var lines = Lines(run.Output);
        var expectations = new Dictionary<string, string>
        {
            ["0.Edit"] = "e1", ["0.Count"] = "none", ["0.Steady"] = "0", ["0.scaled0"] = "10", ["0.scaled"] = "10", ["0.kept0"] = "101", ["0.steps"] = "0,1",
            ["1.Edit"] = "e2", ["1.Count"] = "count=1", ["1.Steady"] = "2", ["1.scaled0"] = "20", ["1.scaled1"] = "20", ["1.scaled"] = "20", ["1.kept0"] = "101", ["1.kept1"] = "101",
            ["1.steps1"] = "10", ["1.steps"] = "10,11",
            ["2.Edit"] = "e1",            // reverted: the original runs again
            ["2.Count"] = "count=2",      // the added field kept its value
            ["2.Steady"] = "2",
            ["2.scaled0"] = "30",         // a delegate from before any patch follows the loaded lambda, which is redirected anew
            ["2.scaled1"] = "20",         // earlier_patch_delegates_keep_old_code: a delegate the first patch made keeps its lambda
            ["2.scaled"] = "30",
            ["2.kept0"] = "101", ["2.kept1"] = "101",
            ["2.handlers"] = "handler-1", // added_method_delegate_identity: `-=` of the second patch did not remove the first patch's handler
            ["2.steps1"] = "11",          // an iterator the first patch started finishes on it
            ["2.steps"] = "20,21",
        };
        Same("generations", "the labels of the run", expectations.Keys, lines.Keys.OrderBy(x => x, StringComparer.Ordinal).ToList());
        foreach (var (label, value) in expectations)
            Check(lines.GetValueOrDefault(label) == value, $"generations: '{label}' is {lines.GetValueOrDefault(label)}, expected {value}.");
    }

    // ---- entry -----------------------------------------------------------------------------

    static string FindRepository()
    {
        for (var directory = new DirectoryInfo(AppContext.BaseDirectory); directory != null; directory = directory.Parent)
            if (File.Exists(Path.Combine(directory.FullName, "tools", "HotReloadPatcher", "HotReloadPatcher.csproj"))) return directory.FullName;
        throw new InvalidOperationException("The repository root (tools/HotReloadPatcher) was not found above " + AppContext.BaseDirectory);
    }

    private static int Main()
    {
        var watch = Stopwatch.StartNew();
        try
        {
            unityData = Environment.GetEnvironmentVariable("UNITY_DATA") is { Length: > 0 } data ? data
                : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Unity", "Hub", "Editor", "6000.6.3f1", "Editor", "Data");
            dotnet = Path.Combine(unityData, "DotNetSdk", "dotnet");
            csc = Directory.GetFiles(Path.Combine(unityData, "DotNetSdk", "sdk"), "csc.dll", SearchOption.AllDirectories)
                .Where(f => f.Contains(Path.Combine("Roslyn", "bincore"), StringComparison.Ordinal)).OrderBy(x => x, StringComparer.Ordinal).Last();
            string monoRoot = Path.Combine(unityData, "MonoBleedingEdge");
            mono = new[] { Path.Combine(monoRoot, "bin-linux64", "mono"), Path.Combine(monoRoot, "bin", "mono") }.First(File.Exists);
            bclAot = Path.Combine(monoRoot, "lib", "mono", "unityaot-linux");
            bclJit = Path.Combine(monoRoot, "lib", "mono", "unityjit-linux");
            monoEtc = Path.Combine(monoRoot, "etc");
            repository = FindRepository();
            fixtures = Path.Combine(repository, "tests", "HotReloadPatcherTests", "Fixtures");
            work = Path.Combine(AppContext.BaseDirectory, "work");
            if (Directory.Exists(work)) Directory.Delete(work, true);
            Directory.CreateDirectory(work);

            BuildPatcher();
            string unityStub = Compile(Path.Combine(work, "stubs", "UnityEngine.CoreModule.dll"), new[] { Fixture("Stubs", "UnityEngine.cs") });
            string netcodeStub = Compile(Path.Combine(work, "stubs", "Unity.Netcode.Runtime.dll"), new[] { Fixture("Stubs", "Netcode.cs") }, new[] { unityStub });
            Classification(unityStub, netcodeStub);
            Quietness();
            CrossAssembly();
            NamedElsewhere(unityStub);
            Behaviour(unityStub);
            Generations();
            Interface();
            ServedAgainInReverse();
        }
        catch (Exception e)
        {
            failures.Enqueue("The test run stopped: " + e);
        }
        foreach (string failure in failures) Console.WriteLine("FAILED " + failure);
        Console.WriteLine(failures.IsEmpty
            ? $"HotReloadPatcher tests passed: {checks} checks in {watch.Elapsed.TotalSeconds:F1} s."
            : $"HotReloadPatcher tests FAILED: {failures.Count} of {checks} checks in {watch.Elapsed.TotalSeconds:F1} s.");
        return failures.IsEmpty ? 0 : 1;
    }
}
