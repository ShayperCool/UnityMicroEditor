using System;
using System.Collections.Generic;
using System.IO;
using Dungeon.CLIPlay.Raw;
using Dungeon.CLIPlay;

internal static class Program
{
    private static int assertions;
    private static void Main()
    {
        const string source = "{\"name\":\"Тест \\u263a\\n\\t\\\\\\\"\",\"references\":[{\"fileID\":\"9223372036854775807\",\"guid\":\"0011\"}],\"min\":-9223372036854775808,\"fraction\":1.25e-2,\"enabled\":true,\"empty\":null}";
        var map = (Dictionary<string, object>)RawJson.Parse(source);
        Check((string)map["name"] == "Тест ☺\n\t\\\"", "Unicode and escaped strings");
        Check((long)map["min"] == long.MinValue, "64-bit integer precision");
        Check(Math.Abs((double)map["fraction"] - .0125) < 1e-12, "Invariant exponent parsing");
        string roundtrip = RawJson.Write(map);
        var recovered = (Dictionary<string, object>)RawJson.Parse(roundtrip);
        var reference = (Dictionary<string, object>)((List<object>)recovered["references"])[0];
        Check((string)reference["fileID"] == "9223372036854775807", "Reference ID precision survives roundtrip");
        Check(RawJson.Write(RawJson.Parse(roundtrip)) == roundtrip, "Stable nested roundtrip");
        foreach (string bad in new[] { "", "{\"x\":1,\"x\":2}", "{\"x\":1}x", "[1,]", "01", "1.", "1e", "NaN", "1e9999", "\"\\u00\"", "\"\\z\"", "\"\n\"", "truefalse" })
        {
            bool rejected = false;
            try { RawJson.Parse(bad); }
            catch (InvalidDataException) { rejected = true; }
            Check(rejected, "Malformed JSON rejected: " + bad.Replace("\n", "\\n"));
        }
        bool bounded = false;
        try { RawJson.Parse(new string('[', 140) + "0" + new string(']', 140)); }
        catch (InvalidDataException) { bounded = true; }
        Check(bounded, "Nesting bound");
        string report = Path.Combine(Path.GetTempPath(), "raw-report.json");
        string manifest = Path.Combine(Path.GetTempPath(), "raw-scene.json");
        Check(CliPlayArguments.TryParse(new[] { "--unity-cli-report", report, "--unity-cli-frames", "3",
                  "--unity-cli-raw-manifest", manifest }, out var arguments)
              && arguments.RawManifestPath == manifest, "Raw manifest argument");
        foreach (string[] extra in new[] {
            new[] { "--unity-cli-raw-manifest", "relative.json" },
            new[] { "--unity-cli-raw-manifest", report },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-scene", "SampleScene" },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-content", manifest + ".content" },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-raw-manifest", manifest },
        })
        {
            bool rejected = false;
            var argv = new List<string> { "--unity-cli-report", report, "--unity-cli-frames", "3" };
            argv.AddRange(extra);
            try { CliPlayArguments.TryParse(argv.ToArray(), out _); }
            catch (FormatException) { rejected = true; }
            Check(rejected, "Invalid raw arguments rejected");
        }
        var roots = new[] { "script", "camera", "mesh" };
        var scriptRoots = new HashSet<string> { "script" };
        var dependencies = new Dictionary<string, HashSet<string>>
        { ["script"] = new HashSet<string> { "camera", "mesh" }, ["camera"] = new HashSet<string>(), ["mesh"] = new HashSet<string>() };
        string[] order = RawActivationPlanner.Order(roots, scriptRoots, dependencies);
        Check(string.Join(",", order) == "camera,mesh,script", "Referenced native roots activate before script root");
        order = RawActivationPlanner.Order(roots, scriptRoots, new Dictionary<string, HashSet<string>>());
        Check(string.Join(",", order) == "camera,mesh,script", "Unconstrained native roots activate first");
        dependencies["camera"].Add("script");
        bool cycleRejected = false;
        try { RawActivationPlanner.Order(roots, scriptRoots, dependencies); }
        catch (InvalidDataException) { cycleRejected = true; }
        Check(cycleRejected, "Active root reference cycle is rejected");
        order = RawActivationPlanner.Order(roots, scriptRoots, dependencies, true);
        Check(string.Join(",", order) == "script,camera,mesh", "Verified deferred lifecycle preserves authored roots despite reference cycles");
        string reload = Path.Combine(Path.GetTempPath(), "raw-reload.json");
        Check(CliPlayArguments.TryParse(new[] { "--unity-cli-report", report, "--unity-cli-frames", "3",
                  "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-raw-reload", reload }, out var reloadArguments)
              && reloadArguments.RawReloadRequestPath == reload, "Interactive raw reload endpoint argument");
        foreach (string[] extra in new[] {
            new[] { "--unity-cli-raw-reload", reload },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-raw-reload", reload },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-raw-reload", "relative.json" },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-raw-reload", manifest },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-raw-reload", report },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-raw-reload", report + ".ready.json" },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-raw-reload", reload, "--unity-cli-raw-reload", reload },
        })
        {
            bool rejected = false;
            var argv = new List<string> { "--unity-cli-report", report, "--unity-cli-frames", "3" };
            argv.AddRange(extra);
            try { CliPlayArguments.TryParse(argv.ToArray(), out _); }
            catch (FormatException) { rejected = true; }
            Check(rejected, "Invalid live reload arguments rejected");
        }
        TestReloadIdentity();
        TestLifecycle();
        Console.WriteLine("Raw runtime tests passed: " + assertions + " assertions.");
    }

    private static void TestReloadIdentity()
    {
        var original = new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha" },
            new Dictionary<string, string> { ["/cache/content"] = "pack-sha:crc:registry" });
        original.RequireCompatible(new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha" },
            new Dictionary<string, string> { ["/cache/content"] = "pack-sha:crc:registry" }));
        Check(true, "Unchanged code/native identity allows a different scene revision");
        foreach (var changed in new[] {
            new RawReloadIdentity("/another-source", new Dictionary<string, string> { ["Game"] = "dll-sha" }, new Dictionary<string, string> { ["/cache/content"] = "pack-sha:crc:registry" }),
            new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "changed" }, new Dictionary<string, string> { ["/cache/content"] = "pack-sha:crc:registry" }),
            new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha", ["Added"] = "dll-sha" }, new Dictionary<string, string> { ["/cache/content"] = "pack-sha:crc:registry" }),
            new RawReloadIdentity("/source", new Dictionary<string, string>(), new Dictionary<string, string> { ["/cache/content"] = "pack-sha:crc:registry" }),
            new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha" }, new Dictionary<string, string> { ["/cache/content"] = "changed:crc:registry" }),
            new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha" }, new Dictionary<string, string>()),
        })
        {
            bool rejected = false;
            try { original.RequireCompatible(changed); }
            catch (InvalidDataException) { rejected = true; }
            Check(rejected, "Changed source project/code/native identity requires restart");
        }
        string file = Path.GetTempFileName();
        try
        {
            File.WriteAllText(file, "immutable");
            string hash = RawReloadIdentity.HashFile(file);
            Check(hash.Length == 64 && RawReloadIdentity.HashFile(file) == hash, "File identity stable SHA256");
            File.WriteAllText(file, "changed");
            Check(RawReloadIdentity.HashFile(file) != hash, "Changed file content invalidates identity");
        }
        finally { File.Delete(file); }
    }

    private sealed class Target
    {
        public bool alive = true, active = true, enabled = true;
    }

    private static RawLifecycleQueue<Target> Queue()
    { return new RawLifecycleQueue<Target>(target => target.alive, target => target.active, target => target.enabled); }

    private static void TestLifecycle()
    {
        var a = new Target(); var b = new Target(); var events = new List<string>();
        var queue = Queue();
        queue.Register(a);
        Check(queue.ContainsRegisteredTarget(a) && !queue.ContainsRegisteredTarget(b),
            "Scene registry distinguishes authored graph components from dynamically created components");
        queue.Enqueue(a, 0, () =>
        {
            Check(!queue.IsCollecting, "Collection predicate is false during callback dispatch");
            Check(!queue.ShouldCollect(a, 0), "Explicit same-target base Awake remains direct");
            Check(queue.ShouldCollect(b, 0), "Reentrant peer Awake remains intercepted");
            events.Add("a.awake");
            queue.Enqueue(b, 0, () => events.Add("duplicate.awake"), 0);
        }, -100);
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Enqueue(a, 1, () => events.Add("a.enable"), -100);
        queue.Enqueue(b, 1, () => events.Add("b.enable"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake,a.enable,b.awake,b.enable", "Execution order precedes native sequence; peer callbacks remain deduplicated");

        a = new Target(); b = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () =>
        {
            Check(queue.TryGetAwakeState(a, out bool ownAwake) && ownAwake,
                "Current component has begun authored Awake");
            Check(queue.TryGetAwakeState(b, out bool peerAwake) && !peerAwake,
                "Peer activation compatibility remains false before its authored Awake");
            events.Add("a.awake");
        }, 0);
        queue.Enqueue(a, 1, () => events.Add("a.enable"), 0);
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Enqueue(b, 1, () => events.Add("b.enable"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake,a.enable,b.awake,b.enable", "Equal-order callbacks preserve native Awake/OnEnable interleaving");

        a = new Target(); b = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () =>
        {
            b.enabled = false;
            queue.Enqueue(b, 2, () => events.Add("spurious.disable"), 0);
            events.Add("a.awake");
        }, -100);
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Enqueue(b, 1, () => events.Add("spurious.enable"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake,b.awake", "Disabled peer still receives Awake but stale enable/disable bodies are suppressed");

        a = new Target(); b = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () =>
        {
            queue.Enqueue(b, 3, () => events.Add("spurious.destroy"), 0);
            b.alive = false; events.Add("a.awake");
        }, -100);
        queue.Enqueue(b, 0, () => events.Add("spurious.awake"), 0);
        queue.Enqueue(b, 1, () => events.Add("spurious.enable"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake", "Peer destroyed before authored Awake receives no stale bodies");

        a = new Target(); b = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("a.awake"), 0);
        queue.Enqueue(a, 1, () =>
        {
            events.Add("a.enable"); a.enabled = false;
            queue.Enqueue(a, 2, () => events.Add("a.disable"), 0);
            queue.Enqueue(a, 3, () => events.Add("a.destroy"), 0);
            a.alive = false;
        }, 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake,a.enable,a.disable,a.destroy", "Reentrant disable/destroy follow delivered callbacks");

        a = new Target { active = false }; events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("spurious.awake"), 0);
        queue.Enqueue(a, 1, () => events.Add("spurious.enable"), 0);
        queue.Complete();
        Check(events.Count == 0, "Inactive objects receive neither Awake nor enable bodies");

        a = new Target(); b = new Target(); var later = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () =>
        {
            events.Add("a.awake"); b.active = false;
            queue.Enqueue(b, 2, () => events.Add("spurious.disable"), 0);
        }, -100);
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Enqueue(b, 1, () => events.Add("b.enable"), 0);
        queue.Enqueue(later, 0, () =>
        {
            events.Add("later.awake"); b.active = true;
            queue.Enqueue(b, 1, () => events.Add("b.enable"), 0);
        }, 100);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake,later.awake,b.awake,b.enable", "Deactivated peer retains undelivered Awake when a later callback reactivates it");

        a = new Target(); b = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () =>
        {
            events.Add("a.awake"); b.enabled = false;
            queue.Enqueue(b, 2, () => events.Add("spurious.disable"), 0);
            b.enabled = true;
            queue.Enqueue(b, 1, () => events.Add("b.new.enable"), 0);
        }, -100);
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Enqueue(b, 1, () => events.Add("spurious.old.enable"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake,b.awake,b.new.enable", "Disable/enable replaces a stale pending enable exactly once");

        a = new Target { enabled = false }; events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("a.awake"), 0);
        queue.Enqueue(a, 1, () => events.Add("spurious.enable"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.awake", "Disabled behaviour on an active object receives Awake only");

        a = new Target(); b = new Target(); events.Clear(); int errors = 0;
        var expected = new InvalidOperationException("authored callback failure");
        queue = new RawLifecycleQueue<Target>(target => target.alive, target => target.active, target => target.enabled,
            (exception, target) =>
            {
                errors++;
                Check(ReferenceEquals(exception, expected) && ReferenceEquals(target, a), "Exception reporting retains the original exception and component context");
            });
        queue.Enqueue(a, 0, () => { throw expected; }, -100);
        queue.Enqueue(b, 0, () =>
        {
            Check(!queue.ShouldCollect(b, 0) && queue.ShouldCollect(a, 0), "Dispatch context is restored after a failing callback");
            events.Add("b.awake");
        }, 0);
        queue.Enqueue(a, 1, () => events.Add("a.enable"), -100);
        queue.Complete();
        Check(errors == 1 && string.Join(",", events) == "a.enable,b.awake", "Reported callback exception does not prevent subsequent lifecycle callbacks");

        a = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => { }, 0); queue.Complete();
        queue.Enqueue(a, 3, () =>
        {
            Check(a.alive, "Destroy body runs synchronously while the native object is still alive");
            events.Add("a.destroy");
        }, 0);
        a.alive = false;
        Check(string.Join(",", events) == "a.destroy", "A script with a synthetic empty Awake still receives its destroy body");

        a = new Target { active = false, enabled = false }; b = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("late.awake"), 0);
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Complete();
        Check(queue.HasPendingAwake && !queue.HasPendingActiveAwake && queue.HasPendingAwakeFor(a),
            "Deactivated pending Awake survives completion without polling an inactive object");
        Check(!queue.ShouldCollect(b, 1) && queue.ShouldCollect(a, 1),
            "Completed queue intercepts only components with pending Awake");
        a.active = true;
        Check(queue.HasPendingActiveAwake, "Explicit SetActive notification detects a disabled active peer");
        queue.FlushPending();
        Check(string.Join(",", events) == "b.awake,late.awake" && !queue.HasPendingAwake,
            "Late activation releases Awake exactly once even for a disabled behaviour without OnEnable");
        queue.FlushPending();
        Check(events.Count == 2, "Repeated activation notifications do not duplicate Awake");

        a = new Target { active = false }; events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("late.awake"), 0);
        queue.Complete(); a.active = true;
        queue.Enqueue(a, 1, () => events.Add("late.enable"), 0);
        queue.FlushPending();
        Check(string.Join(",", events) == "late.awake,late.enable", "Late native OnEnable remains behind the retained Awake");

        a = new Target { active = false }; events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("spurious.awake"), 0);
        queue.Complete();
        queue.Enqueue(a, 3, () => events.Add("spurious.destroy"), 0);
        a.alive = false; queue.FlushPending();
        Check(events.Count == 0 && !queue.HasPendingAwake,
            "Destroying a deferred component before activation suppresses both Awake and OnDestroy and clears retained state");

        a = new Target(); b = new Target { active = false }; events.Clear(); queue = Queue();
        queue.Register(b);
        queue.Enqueue(a, 0, () =>
        {
            events.Add("a.before"); b.active = true;
            queue.Enqueue(b, 0, () => events.Add("b.awake"), 100);
            queue.Enqueue(b, 1, () => events.Add("b.enable"), 100);
            events.Add("a.after");
        }, -100);
        queue.Complete();
        Check(string.Join(",", events) == "a.before,b.awake,b.enable,a.after",
            "Fresh native Awake and OnEnable from reentrant activation run before the caller continues");

        a = new Target(); b = new Target { enabled = false }; later = new Target(); events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () =>
        {
            events.Add("a.before"); b.active = true;
            queue.FlushActivated(target => ReferenceEquals(target, b));
            events.Add("a.after");
        }, -100);
        b.active = false;
        queue.Enqueue(b, 0, () => events.Add("b.awake"), 0);
        queue.Enqueue(later, 0, () => events.Add("later.awake"), 0);
        queue.Complete();
        Check(string.Join(",", events) == "a.before,b.awake,a.after,later.awake",
            "Targeted SetActive release handles disabled retained peers without advancing unrelated roots");

        a = new Target(); b = new Target { active = false }; events.Clear(); queue = Queue();
        queue.Enqueue(a, 0, () => events.Add("a.awake"), 0);
        queue.Enqueue(b, 0, () =>
        {
            Check(!queue.ShouldCollect(a, 1), "Completed graph does not intercept already awakened peers with stale historical enable state");
            Check(!queue.TryGetAwakeState(a, out _), "Completed graph uses native activity for already awakened peers during another delayed Awake");
            events.Add("b.awake");
        }, 0);
        queue.Complete(); b.active = true;
        queue.FlushActivated(target => ReferenceEquals(target, b));
        Check(string.Join(",", events) == "a.awake,b.awake", "Delayed callbacks resume with isolated graph state");
    }
    private static void Check(bool passed, string message)
    {
        assertions++;
        if (!passed) throw new InvalidOperationException(message);
    }
}
