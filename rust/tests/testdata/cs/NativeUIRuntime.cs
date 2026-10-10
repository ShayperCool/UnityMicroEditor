using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using UnityMicroEditor.Raw;
using UnityEngine;
using UnityEngine.UIElements;
using Object = UnityEngine.Object;

internal static class Program
{
    private const string UI = "11111111111111111111111111111111";
    private const string CSS = "22222222222222222222222222222222";
    private const string FONT = "33333333333333333333333333333333";
    private static int assertions;
    private static void Assert(bool value, string message) { assertions++; if (!value) throw new Exception(message); }
    private static void Reject(Action action, string message)
    {
        try { action(); } catch (InvalidDataException) { assertions++; return; }
        throw new Exception(message);
    }
    private static NativeContentObjectDescriptor Entry(string id, string type) => new NativeContentObjectDescriptor { fileId = id, typeName = type };
    private static NativeUIBindingDescriptor Binding(string owner = "90", string path = "references.RefIds[0].data.m_Stylesheets[0]", string guid = CSS, string id = "74", string rid = "1000")
        => new NativeUIBindingDescriptor { ownerFileId = owner, propertyPath = path, targetGuid = guid, targetFileId = id, managedReferenceId = rid };
    private static Dictionary<string, object> Map(string json) => (Dictionary<string, object>)RawJson.Parse(json);
    private static NativeContentFileDescriptor TreeDescriptor() => new NativeContentFileDescriptor
    {
        guid = UI,
        objects = new[] { Entry("90", typeof(VisualTreeAsset).FullName), Entry("10", typeof(StyleSheet).FullName) },
        dependencies = new[] { new NativeContentDependencyDescriptor { guid = CSS } },
        uiBindings = new[] { Binding(), Binding(path: "inlineSheet", guid: UI, id: "10", rid: null) }
    };
    private static Dictionary<string, Object> OriginalObjects()
    {
        var inline = new StyleSheet { name = "inline", Json = Map("{\"colors\":[0.4],\"assets\":[]}") };
        var tree = new VisualTreeAsset { name = "Lobby", Json = Map("{\"contentHash\":77,\"inlineSheet\":{\"instanceID\":" + inline.ID + "},\"references\":{\"version\":2,\"RefIds\":[{\"rid\":-2,\"data\":{}},{\"rid\":1000,\"data\":{\"m_VisualTreeAsset\":{\"instanceID\":0},\"m_Stylesheets\":[{\"instanceID\":0}]}}]}}") };
        ((Dictionary<string, object>)NativeUIRuntime.Get(tree.Json, "references.RefIds[1].data.m_VisualTreeAsset"))["instanceID"] = (long)tree.ID;
        return new Dictionary<string, Object> { [UI + ":90"] = tree, [UI + ":10"] = inline };
    }
    private static void Main()
    {
        var font = new Font { name = "font" };
        var sheet = new StyleSheet { name = "Terminal", Json = Map("{\"assets\":[{\"instanceID\":0}],\"contentHash\":984,\"colors\":[0.75],\"strings\":[\"é\\nAmber\"]}") };
        string originalSheet = JsonUtility.ToJson(sheet);
        var cssObjects = new Dictionary<string, Object> { [CSS + ":74"] = sheet };
        var cssOwned = new List<Object>();
        var cssDescriptor = new NativeContentFileDescriptor
        {
            guid = CSS, objects = new[] { Entry("74", typeof(StyleSheet).FullName) },
            dependencies = new[] { new NativeContentDependencyDescriptor { guid = FONT } },
            uiBindings = new[] { Binding(owner: "74", path: "assets[0]", guid: FONT, id: "128", rid: null) }
        };
        NativeUIRuntime.Prepare(cssDescriptor, cssObjects, (g, i) => font, cssOwned);
        var cssFinal = (StyleSheet)cssObjects[CSS + ":74"];
        Assert(cssFinal != sheet && cssFinal.Initialized && cssOwned.Count == 1, "stylesheet needs one owned final initialized clone");
        Assert((long)NativeUIRuntime.Get(cssFinal.Json, "assets[0].instanceID") == font.ID, "exact native font must be bound");
        Assert((long)cssFinal.Json["contentHash"] == 984 && (double)NativeUIRuntime.Get(cssFinal.Json, "colors[0]") == 0.75, "imported style data must remain unchanged");
        Assert(JsonUtility.ToJson(sheet) == originalSheet && !sheet.Destroyed, "native original stylesheet must not be changed or destroyed");

        var descriptor = TreeDescriptor();
        var objects = OriginalObjects();
        Object originalTree = objects[UI + ":90"], originalInline = objects[UI + ":10"];
        string originalTreeJson = JsonUtility.ToJson(originalTree);
        var owned = new List<Object>();
        NativeUIRuntime.Prepare(descriptor, objects, (g, i) => g == UI ? objects[g + ":" + i] : cssObjects[g + ":" + i], owned);
        var final = (VisualTreeAsset)objects[UI + ":90"];
        Assert(final.Initialized && final.name == "Lobby" && owned.Count == 2, "final VTA lifecycle must run after overwrite; only final clones retained");
        Assert((long)NativeUIRuntime.Get(final.Json, "references.RefIds[1].data.m_Stylesheets[0].instanceID") == cssFinal.ID, "native rid must resolve past the runtime sentinel");
        Assert((long)NativeUIRuntime.Get(final.Json, "inlineSheet.instanceID") == objects[UI + ":10"].ID, "local inline stylesheet must point at its initialized clone");
        Assert((long)NativeUIRuntime.Get(final.Json, "references.RefIds[1].data.m_VisualTreeAsset.instanceID") == final.ID, "VTA self-owner links must identify the final clone");
        Assert(JsonUtility.ToJson(originalTree) == originalTreeJson && !originalTree.Destroyed && !originalInline.Destroyed, "native VTA and inline data must remain untouched");
        Assert((long)final.Json["contentHash"] == 77, "unbound fields must survive clone and overwrite");

        string identity = NativeUIRuntime.Identity(descriptor);
        descriptor.uiBindings[0].targetFileId = "75";
        Assert(identity != NativeUIRuntime.Identity(descriptor), "same native payload with different UI identity must invalidate cache");
        descriptor.uiBindings[0].targetFileId = "74";
        descriptor.uiBindings[0].managedReferenceId = "1001";
        Assert(identity != NativeUIRuntime.Identity(descriptor), "managed registry guard must participate in cache identity");
        descriptor.uiBindings[0].managedReferenceId = "1000";
        var empty = new NativeContentFileDescriptor { guid = CSS, objects = cssDescriptor.objects };
        string emptyIdentity = NativeUIRuntime.Identity(empty);
        empty.uiBindings = Array.Empty<NativeUIBindingDescriptor>();
        Assert(emptyIdentity == NativeUIRuntime.Identity(empty), "omitted and empty binding lists have identical meaning");

        var failedOwned = new List<Object>();
        var missing = OriginalObjects();
        Reject(() => NativeUIRuntime.Prepare(TreeDescriptor(), missing, (g, i) => null, failedOwned), "missing published target must fail before publication");
        NativeUIRuntime.Release(failedOwned);
        Assert(failedOwned.Count == 0 && !missing[UI + ":90"].Destroyed, "failed preparation must clean clones without destroying originals");
        var registry = Map("{\"references\":{\"RefIds\":[{\"rid\":1000,\"data\":{}},{\"rid\":1000,\"data\":{}}]}}");
        Reject(() => NativeUIRuntime.ResolvePath(registry, Binding()), "duplicate rid cannot silently select an arbitrary node");
        registry = Map("{\"references\":{\"RefIds\":[{\"rid\":1001,\"data\":{}}]}}");
        Reject(() => NativeUIRuntime.ResolvePath(registry, Binding()), "missing rid cannot silently use the native index");
        Reject(() => NativeUIRuntime.Get(final.Json, "references.RefIds[99999].data"), "missing path index must fail closed");
        Reject(() => NativeUIRuntime.Get(final.Json, "references..RefIds[0]"), "malformed path must fail closed");
        Reject(() => NativeUIRuntime.Get(final.Json, "inlineSheet\n"), "trailing newline cannot bypass the exact path grammar");
        var invalid = TreeDescriptor(); invalid.uiBindings[0].managedReferenceId = null;
        Reject(() => NativeUIRuntime.Validate(invalid), "managed paths require exact rid guard");
        invalid = TreeDescriptor(); invalid.uiBindings[0].targetGuid = FONT;
        Reject(() => NativeUIRuntime.Validate(invalid), "undeclared dependency cannot be rebound");
        invalid = TreeDescriptor(); invalid.uiBindings[0].ownerFileId = "999";
        Reject(() => NativeUIRuntime.Validate(invalid), "unpublished owner cannot be rebound");
        invalid = TreeDescriptor(); invalid.uiBindings[0].targetFileId = "074";
        Reject(() => NativeUIRuntime.Validate(invalid), "noncanonical native IDs cannot be rebound");
        invalid = TreeDescriptor(); invalid.uiBindings = new[] { invalid.uiBindings[0], invalid.uiBindings[0] };
        Reject(() => NativeUIRuntime.Validate(invalid), "duplicate property bindings cannot overwrite one another");

        JsonUtility.IgnoreOverwrite = true;
        failedOwned.Clear(); missing = OriginalObjects();
        Reject(() => NativeUIRuntime.Prepare(TreeDescriptor(), missing, (g, i) => g == UI ? missing[g + ":" + i] : cssFinal, failedOwned), "public Unity overwrite readback must verify retained references");
        JsonUtility.IgnoreOverwrite = false; NativeUIRuntime.Release(failedOwned);
        Assert(!missing[UI + ":90"].Destroyed, "failed readback must keep native source alive");
        Object.DropFinalReferences = true;
        failedOwned.Clear(); missing = OriginalObjects();
        Reject(() => NativeUIRuntime.Prepare(TreeDescriptor(), missing, (g, i) => g == UI ? missing[g + ":" + i] : cssFinal, failedOwned), "final clone reference loss must fail before publication");
        Object.DropFinalReferences = false; NativeUIRuntime.Release(failedOwned);
        NativeUIRuntime.Release(owned); NativeUIRuntime.Release(cssOwned);
        Assert(final.Destroyed && cssFinal.Destroyed && !font.Destroyed && !sheet.Destroyed && !originalInline.Destroyed, "owned asset cleanup must destroy only clones");
        Console.WriteLine("Native UI runtime assertions: " + assertions);
    }
}

namespace UnityEngine
{
    public class Object
    {
        private static int next;
        public readonly int ID = ++next;
        public string name = "asset";
        public Dictionary<string, object> Json = new Dictionary<string, object>();
        public bool Destroyed, Initialized, Overwritten;
        public static bool DropFinalReferences;
        public static Object Instantiate(Object value)
        {
            var clone = (Object)Activator.CreateInstance(value.GetType());
            clone.name = value.name;
            clone.Json = (Dictionary<string, object>)RawJson.Parse(RawJson.Write(value.Json));
            Remap(clone.Json, value.ID, clone.ID);
            clone.Initialized = true;
            if (DropFinalReferences && value.Overwritten) NullRefs(clone.Json);
            return clone;
        }
        private static void Remap(object value, int from, int to)
        {
            if (value is Dictionary<string, object> map)
            {
                if (map.Count == 1 && map.TryGetValue("instanceID", out object id) && id is long n && n == from) map["instanceID"] = (long)to;
                else foreach (object entry in map.Values) Remap(entry, from, to);
            }
            else if (value is List<object> list) foreach (object entry in list) Remap(entry, from, to);
        }
        private static void NullRefs(object value)
        {
            if (value is Dictionary<string, object> map)
            {
                if (map.Count == 1 && map.ContainsKey("instanceID")) map["instanceID"] = 0L;
                else foreach (object entry in map.Values) NullRefs(entry);
            }
            else if (value is List<object> list) foreach (object entry in list) NullRefs(entry);
        }
        public static void DestroyImmediate(Object value) { value.Destroyed = true; }
    }
    public sealed class Font : Object { }
    public struct Color { public static Color white => default; }
    public struct Vector2 { public static Vector2 one => default; }
    public static class JsonUtility
    {
        public static bool IgnoreOverwrite;
        public static string ToJson(object value)
        {
            if (value is Object asset) return RawJson.Write(asset.Json);
            var reference = (Object)value.GetType().GetField("target", BindingFlags.Public | BindingFlags.Instance).GetValue(value);
            return "{\"target\":{\"instanceID\":" + reference.ID + "}}";
        }
        public static void FromJsonOverwrite(string json, object target)
        {
            if (IgnoreOverwrite) return;
            var asset = (Object)target;
            asset.Json = (Dictionary<string, object>)RawJson.Parse(json);
            asset.Overwritten = true;
            asset.Initialized = false;
        }
    }
}
namespace UnityEngine.UIElements
{
    public sealed class StyleSheet : Object { }
    public sealed class VisualTreeAsset : Object { }
}
