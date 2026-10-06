using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Security;
using System.Text.Json;
using System.Text.Json.Serialization;
using UnityEngine;
using UnityEngine.LowLevel;

internal static class Program
{
    private const int MaxCapturedCharacters = 4096;
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        WriteIndented = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull
    };

    public static async Task<int> Main(string[] args)
    {
        if (args.Length == 2 && args[0] == "--child" && TryParseProbe(args[1], out var childProbe))
            return RunChild(childProbe);

        if (args.Length != 0)
        {
            WriteJson(new { status = "invalid-arguments", usage = "dotnet ManagedProbe.dll" });
            return 2;
        }

        try
        {
            var results = new List<ProbeResult>();
            foreach (var probe in Enum.GetValues<Probe>())
                results.Add(await RunIsolated(probe));

            var managed = results[0];
            var nativeAvailable = results.Skip(1).All(result => result.Status == "success");
            WriteJson(new
            {
                schemaVersion = 1,
                experiment = "unity-managed-outside-engine",
                status = "completed",
                host = RuntimeInformation.FrameworkDescription,
                unityCoreAssembly = Path.Combine(AppContext.BaseDirectory, "UnityEngine.CoreModule.dll"),
                pureManagedAvailable = managed.Status == "success",
                nativeRuntimeAvailable = nativeAvailable,
                enginePlayModeStarted = false,
                interpretation = nativeAvailable
                    ? "These sampled APIs returned successfully; this does not establish scene loading or Play Mode."
                    : managed.Status == "success"
                        ? "The sampled Unity managed math works independently. Native-backed APIs require Unity runtime initialization; loading managed DLLs alone does not start Play Mode."
                        : "The managed Vector3 sample did not execute successfully on this host. Loading managed DLLs alone does not start Play Mode.",
                probes = results
            });
            return managed.Status == "success" ? 0 : 1;
        }
        catch (Exception error)
        {
            WriteJson(new { status = "experiment-infrastructure-failed", error = ErrorInfo.From(error) });
            return 3;
        }
    }

    private static async Task<ProbeResult> RunIsolated(Probe probe)
    {
        var host = Environment.ProcessPath;
        if (string.IsNullOrEmpty(host))
            throw new InvalidOperationException("Cannot locate the dotnet host for isolated probes.");

        var start = new ProcessStartInfo(host)
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
            CreateNoWindow = true,
            WorkingDirectory = AppContext.BaseDirectory
        };
        start.ArgumentList.Add(Path.Combine(AppContext.BaseDirectory, "ManagedProbe.dll"));
        start.ArgumentList.Add("--child");
        start.ArgumentList.Add(ProbeName(probe));

        using var process = Process.Start(start);
        if (process == null)
            throw new InvalidOperationException("The isolated probe process did not start.");

        // Drain both pipes before waiting. A native crash may write a large diagnostic.
        var stdoutTask = CaptureBounded(process.StandardOutput);
        var stderrTask = CaptureBounded(process.StandardError);
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
        var timedOut = false;
        try
        {
            await process.WaitForExitAsync(timeout.Token);
        }
        catch (OperationCanceledException)
        {
            timedOut = true;
            process.Kill(entireProcessTree: true);
            await process.WaitForExitAsync();
        }

        var stdout = await stdoutTask;
        var stderr = await stderrTask;
        ProbeResult? result = null;
        if (!timedOut && process.ExitCode == 0)
        {
            try
            {
                result = JsonSerializer.Deserialize<ProbeResult>(stdout, JsonOptions);
            }
            catch (JsonException)
            {
                // Preserve malformed output rather than treating a failed child as success.
            }
        }

        if (result == null || result.Id != ProbeName(probe))
        {
            return new ProbeResult
            {
                Id = ProbeName(probe),
                Kind = ProbeKind(probe),
                Status = timedOut ? "timeout" : "process-failed",
                ExitCode = process.ExitCode,
                Stdout = EmptyToNull(stdout),
                Stderr = EmptyToNull(stderr)
            };
        }

        return result with { ExitCode = process.ExitCode, Stderr = EmptyToNull(stderr) };
    }

    private static int RunChild(Probe probe)
    {
        try
        {
            // Keep each call in a separate method. JIT/runtime refusal then reaches this catch.
            var observation = probe switch
            {
                Probe.Vector3 => ProbeVector3(),
                Probe.GameObject => ProbeGameObject(),
                Probe.Time => ProbeTime(),
                Probe.PlayerLoop => ProbePlayerLoop(),
                _ => throw new ArgumentOutOfRangeException(nameof(probe))
            };
            WriteJson(new ProbeResult
            {
                Id = ProbeName(probe),
                Kind = ProbeKind(probe),
                Status = "success",
                Observation = JsonSerializer.SerializeToElement(observation, JsonOptions)
            });
        }
        catch (Exception error)
        {
            WriteJson(new ProbeResult
            {
                Id = ProbeName(probe),
                Kind = ProbeKind(probe),
                Status = ClassifyFailure(error),
                Error = ErrorInfo.From(error)
            });
        }

        // A valid negative observation is a completed experiment, not a crashed process.
        return 0;
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static object ProbeVector3()
    {
        var a = new Vector3(3, 4, 0);
        var b = new Vector3(1, 2, 3);
        var sum = a + b;
        var dot = Vector3.Dot(a, b);
        var cross = Vector3.Cross(a, b);
        var magnitude = a.magnitude;
        var normalized = a.normalized;
        var correct = sum.x == 4 && sum.y == 6 && sum.z == 3
                      && dot == 11 && cross.x == 12 && cross.y == -9 && cross.z == 2
                      && Math.Abs(magnitude - 5) < 0.00001f
                      && Math.Abs(normalized.x - 0.6f) < 0.00001f
                      && Math.Abs(normalized.y - 0.8f) < 0.00001f;
        if (!correct)
            throw new InvalidOperationException("Vector3 results did not match the expected values.");

        return new
        {
            sum = Coordinates(sum),
            dot,
            cross = Coordinates(cross),
            magnitude,
            normalized = Coordinates(normalized),
            expectedValuesMatched = correct
        };
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static object ProbeGameObject()
    {
        // A one-off diagnostic object exists only if an engine happens to be initialized.
        var instance = new GameObject("ManagedProbe");
        return new { createdObject = instance.name };
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static object ProbeTime() => new { time = Time.time, deltaTime = Time.deltaTime };

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static object ProbePlayerLoop()
    {
        var loop = PlayerLoop.GetCurrentPlayerLoop();
        return new { rootType = loop.type?.FullName, rootSubsystemCount = loop.subSystemList?.Length ?? 0 };
    }

    private static float[] Coordinates(Vector3 vector) => [vector.x, vector.y, vector.z];

    private static string ClassifyFailure(Exception error) => error switch
    {
        SecurityException => "runtime-refused-internal-call",
        InvalidProgramException => "runtime-refused-internal-call",
        MissingMethodException => "missing-native-internal-call",
        EntryPointNotFoundException => "missing-native-entry-point",
        DllNotFoundException => "missing-native-library",
        FileNotFoundException => "missing-managed-dependency",
        FileLoadException => "managed-assembly-load-failed",
        BadImageFormatException => "managed-assembly-incompatible",
        _ => "managed-exception"
    };

    private static async Task<string> CaptureBounded(StreamReader stream)
    {
        var buffer = new char[1024];
        var captured = new System.Text.StringBuilder();
        var discarded = false;
        int count;
        while ((count = await stream.ReadAsync(buffer)) != 0)
        {
            var remaining = MaxCapturedCharacters - captured.Length;
            if (remaining > 0)
                captured.Append(buffer, 0, Math.Min(remaining, count));
            if (count > remaining)
                discarded = true;
        }

        if (discarded)
            captured.Append("\n[output truncated]");
        return captured.ToString();
    }

    private static bool TryParseProbe(string name, out Probe probe)
    {
        foreach (var candidate in Enum.GetValues<Probe>())
        {
            if (ProbeName(candidate) == name)
            {
                probe = candidate;
                return true;
            }
        }

        probe = default;
        return false;
    }

    private static string ProbeName(Probe probe) => probe switch
    {
        Probe.Vector3 => "managed.vector3",
        Probe.GameObject => "native.gameobject",
        Probe.Time => "native.time",
        Probe.PlayerLoop => "native.playerloop",
        _ => throw new ArgumentOutOfRangeException(nameof(probe))
    };

    private static string ProbeKind(Probe probe) => probe == Probe.Vector3 ? "pure-managed" : "native-backed";
    private static string? EmptyToNull(string value) => string.IsNullOrWhiteSpace(value) ? null : value;
    private static void WriteJson<T>(T value) => Console.WriteLine(JsonSerializer.Serialize(value, JsonOptions));

    private enum Probe { Vector3, GameObject, Time, PlayerLoop }

    private sealed record ProbeResult
    {
        public string Id { get; init; } = "";
        public string Kind { get; init; } = "";
        public string Status { get; init; } = "";
        public int? ExitCode { get; init; }
        public JsonElement? Observation { get; init; }
        public ErrorInfo? Error { get; init; }
        public string? Stdout { get; init; }
        public string? Stderr { get; init; }
    }

    private sealed record ErrorInfo(string Type, string Message)
    {
        public static ErrorInfo From(Exception error) => new(error.GetType().FullName ?? error.GetType().Name, error.Message);
    }
}
