using System;
using System.Collections.Generic;
using System.IO;
using UnityMicroEditor.Raw;

namespace UnityEngine
{
    [AttributeUsage(AttributeTargets.Field)] public sealed class SerializeField : Attribute { }
}

internal enum Reward : byte { One = 1, Nine = 9, Ten = 10 }
internal enum Wider : int { One = 1 }
internal enum Signed : sbyte { MinusOne = -1 }
[Serializable] internal sealed class Grammar { public List<Reward> closets = new List<Reward>(); }
[Serializable] internal sealed class Rule { public List<Reward> preferredZones = new List<Reward>(); }
[Serializable] internal struct StructRule { public Reward[] zones; public StructRule(Reward[] values) { zones = values; } }
[Serializable] internal class BaseData { public byte[] inheritedBytes = Array.Empty<byte>(); }
[Serializable] internal sealed class Fixture : BaseData
{
    public List<Reward> rareRewards = new List<Reward>();
    public Reward[] commonRewards = Array.Empty<Reward>();
    public Grammar grammar = new Grammar();
    public List<Rule> essentialSystems = new List<Rule>();
    public List<StructRule> structRules = new List<StructRule>();
    public List<byte> bytes = new List<byte>();
    public List<Wider> wider = new List<Wider>();
    public int[] integers = Array.Empty<int>();
    public uint[] unsigned = Array.Empty<uint>();
    public float[] floats = Array.Empty<float>();
    public List<Signed> signed = new List<Signed>();
    public List<string> strings = new List<string>();
    public string literal = "";
    [NonSerialized] public byte[] ignored = Array.Empty<byte>();
    [UnityEngine.SerializeField] private byte[] serializedPrivate = Array.Empty<byte>();
    public int PrivateCount => serializedPrivate.Length;
    public object reference = new object();
}

internal static class Program
{
    private static void Check(bool condition, string message) { if (!condition) throw new Exception(message); }
    private static Dictionary<string, object> Parse(string json) => (Dictionary<string, object>)RawJson.Parse(json);
    private static void Reject(Action action, string message)
    {
        try { action(); } catch (InvalidDataException) { return; }
        throw new Exception(message);
    }

    private static int Main()
    {
        const string json = "{\"rareRewards\":\"090ay\",\"commonRewards\":\"0b0c0dy\",\"grammar\":{\"closets\":\"080507y\"},\"essentialSystems\":[{\"preferredZones\":\"110812y\"},{\"preferredZones\":[1,2]}],\"inheritedBytes\":\"00aAFFy\",\"serializedPrivate\":\"ffy\",\"literal\":\"090ay\",\"strings\":[\"110812y\"],\"ignored\":\"gggy\",\"reference\":{\"fileID\":\"123\",\"guid\":\"090ay\"}}";
        var data = Parse(json);
        var expectations = RawCompactCollections.Normalize(typeof(Fixture), data);
        Check(expectations.Count == 6, "Nested, inherited and SerializeField metadata collections must be found");
        Check(RawJson.Write(data["rareRewards"]) == "[9,10]" && RawJson.Write(data["commonRewards"]) == "[11,12,13]", "Actual MissionCatalog encoded rewards changed");
        Check(RawJson.Write(((Dictionary<string, object>)data["grammar"])["closets"]) == "[8,5,7]", "Actual Grammar closets encoding changed");
        Check(RawJson.Write(data["inheritedBytes"]) == "[0,170,255]", "Byte range, uppercase hex and inherited fields must survive");
        Check((string)data["literal"] == "090ay" && RawJson.Write(data["strings"]) == "[\"110812y\"]", "Ordinary strings must never decode by their suffix");
        Check((string)data["ignored"] == "gggy", "NonSerialized fields must not be inspected");
        Check((string)((Dictionary<string, object>)data["reference"])["guid"] == "090ay", "PPtr payload must remain untouched");
        RawCompactCollections.Verify(expectations, RawJson.Parse(RawJson.Write(data)));
        var wrong = Parse(RawJson.Write(data)); wrong["commonRewards"] = new List<object> { 11L, 12L };
        Reject(() => RawCompactCollections.Verify(expectations, wrong), "Lost collection value must fail readback");
        wrong = Parse(RawJson.Write(data)); wrong["rareRewards"] = new List<object> { 9L, 11L };
        Reject(() => RawCompactCollections.Verify(expectations, wrong), "Changed collection value must fail readback");
        wrong = Parse(RawJson.Write(data)); wrong.Remove("grammar");
        Reject(() => RawCompactCollections.Verify(expectations, wrong), "Lost nested collection must fail readback");
        foreach (string malformed in new[] { "0y", "010y", "ggyy", "00u", "00", "xx", "" })
            Reject(() => RawCompactCollections.Normalize(typeof(Fixture), new Dictionary<string, object> { { "rareRewards", malformed } }), "Invalid UInt8 compact data must reject");
        foreach (string field in new[] { "wider", "signed", "strings" })
            Reject(() => RawCompactCollections.Normalize(typeof(Fixture), new Dictionary<string, object> { { field, "ffy" } }), "Unverified element type must reject");
        var empty = Parse("{\"rareRewards\":\"y\",\"commonRewards\":[],\"grammar\":null}");
        var emptyExpectations = RawCompactCollections.Normalize(typeof(Fixture), empty);
        Check(emptyExpectations.Count == 1 && RawJson.Write(empty["rareRewards"]) == "[]", "Empty compact bytes must restore an empty list");
        RawCompactCollections.Verify(emptyExpectations, empty);
        var structs = Parse("{\"structRules\":[{\"zones\":\"01ffy\"}],\"bytes\":\"00ffy\"}");
        var structExpectations = RawCompactCollections.Normalize(typeof(Fixture), structs);
        Check(structExpectations.Count == 2 && RawJson.Write(structs["bytes"]) == "[0,255]", "List<byte> and nested List<struct> must restore");
        RawCompactCollections.Verify(structExpectations, structs);
        var intData = Parse("{\"wider\":\"000000000100000002000000i\",\"integers\":\"ffffffff00000080ffffff7f01000000i\"}");
        var intExpectations = RawCompactCollections.Normalize(typeof(Fixture), intData);
        Check(intExpectations.Count == 2 && RawJson.Write(intData["wider"]) == "[0,1,2]", "Actual ShipArchetype sizes must decode as Int32 enums");
        Check(RawJson.Write(intData["integers"]) == "[-1,-2147483648,2147483647,1]", "Int32 signed bounds and little-endian byte order must survive");
        RawCompactCollections.Verify(intExpectations, intData);
        foreach (string invalid in new[] { "0i", "00i", "000000i", "gg000000i", "00000000u", "00000000y", "" })
            Reject(() => RawCompactCollections.Normalize(typeof(Fixture), new Dictionary<string, object> { { "integers", invalid } }), "Invalid or wrong-suffix Int32 data must reject");
        foreach (string field in new[] { "unsigned", "floats" })
            Reject(() => RawCompactCollections.Normalize(typeof(Fixture), new Dictionary<string, object> { { field, "00000000i" } }), "Unverified uint/float decoding must reject");
        var intEmpty = Parse("{\"integers\":\"i\"}");
        var intEmptyExpectations = RawCompactCollections.Normalize(typeof(Fixture), intEmpty);
        Check(RawJson.Write(intEmpty["integers"]) == "[]", "Empty Int32 compact array must restore");
        RawCompactCollections.Verify(intEmptyExpectations, intEmpty);
        Reject(() => RawCompactCollections.Normalize(typeof(Fixture), new Dictionary<string, object> { { "bytes", new string('0', 2 * 1024 * 1024 + 2) + "y" } }), "Oversized collection must fail before allocation");
        Console.WriteLine("Typed compact UInt8/Int32 collection regression passed.");
        return 0;
    }
}
