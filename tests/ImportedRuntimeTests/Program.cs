using System;
using System.Collections.Generic;
using System.IO;
using UnityMicroEditor.Raw;

internal static class Program
{
    private static int checks;
    private static Dictionary<string, object> Fixture()
    {
        return (Dictionary<string, object>)RawJson.Parse("{\"prefab\":null,\"entries\":[{\"value\":null,\"keep\":17}],\"nested\":{\"targets\":[null,null]},\"keep\":42}");
    }
    private static void Check(bool condition, string message)
    { checks++; if (!condition) throw new InvalidOperationException(message); }
    private static void Main()
    {
        object reference = RawJson.Parse("{\"instanceID\":910000000000000001}");
        foreach (string path in new[] { "prefab", "entries[0].value", "entries.Array.data[0].value", "nested.targets[1]", "nested.targets.Array.data[1]" })
        {
            var root = Fixture();
            ImportedAssetRuntimePaths.Set(root, path, reference);
            Check((long)root["keep"] == 42, "Unrelated root field must be preserved.");
            var entries = (List<object>)root["entries"];
            Check((long)((Dictionary<string, object>)entries[0])["keep"] == 17, "Unrelated array-entry field must be preserved.");
            Check(RawJson.Write(root).Contains("910000000000000001"), "Reference must retain its complete 64-bit engine identity.");
        }
        foreach (string path in new[] { "", ".prefab", "prefab.", "entries[-1].value", "entries[2].value", "missing", "entries[0].missing", "entries.Array.data[0].value]", "entries[0]value", "entries.[0].value", "[0]", "nested.targets[1]]", "entries..value" })
        {
            var root = Fixture(); string original = RawJson.Write(root); bool rejected = false;
            try { ImportedAssetRuntimePaths.Set(root, path, reference); }
            catch (InvalidDataException) { rejected = true; }
            Check(rejected, "Invalid or unresolved path must fail: " + path);
            Check(RawJson.Write(root) == original, "Rejected path must not mutate the serialized graph: " + path);
        }
        Console.WriteLine(checks + " imported binding path assertions passed.");
    }
}
