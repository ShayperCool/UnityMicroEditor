using System;
using System.Collections.Generic;
using System.IO;
using UnityMicroEditor.Raw;
using UnityMicroEditor;

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
        string control = Path.Combine(Path.GetDirectoryName(report), "control");
        Check(CliPlayArguments.TryParse(new[] { "--unity-cli-report", report, "--unity-cli-frames", "3",
            "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-control", control }, out var controlArguments)
            && controlArguments.ControlDirectory == control, "Agent control is bound to an interactive raw session's report directory");
        foreach (string[] extra in new[] {
            new[] { "--unity-cli-control", control },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-control", control },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-control", "relative" },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-control", Path.Combine(control, "outside") },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-control", control, "--unity-cli-control", control },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-control", report },
            new[] { "--unity-cli-raw-manifest", manifest, "--unity-cli-interactive", "--unity-cli-control", report + ".ready.json" },
        })
        {
            bool rejected = false;
            var argv = new List<string> { "--unity-cli-report", report, "--unity-cli-frames", "3" }; argv.AddRange(extra);
            try { CliPlayArguments.TryParse(argv.ToArray(), out _); } catch (FormatException) { rejected = true; }
            Check(rejected, "Invalid agent-control lifecycle/path arguments rejected");
        }
        Console.WriteLine("Raw runtime tests passed: " + assertions + " assertions.");
    }

    private static void TestReloadIdentity()
    {
        var original = new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha" });
        original.RequireCompatible(new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha" }));
        Check(true, "Unchanged code identity allows a different scene revision");
        foreach (var changed in new[] {
            new RawReloadIdentity("/another-source", new Dictionary<string, string> { ["Game"] = "dll-sha" }),
            new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "changed" }),
            new RawReloadIdentity("/source", new Dictionary<string, string> { ["Game"] = "dll-sha", ["Added"] = "dll-sha" }),
            new RawReloadIdentity("/source", new Dictionary<string, string>()),
        })
        {
            bool rejected = false;
            try { original.RequireCompatible(changed); }
            catch (InvalidDataException) { rejected = true; }
            Check(rejected, "Changed source project or code requires restart");
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

    private static void Check(bool passed, string message)
    {
        assertions++;
        if (!passed) throw new InvalidOperationException(message);
    }
}
