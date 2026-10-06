using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
using UnityEngine;
using UnityEngine.UIElements;
using Object = UnityEngine.Object;

namespace Dungeon.CLIPlay.Raw
{
    // Physical ContentFiles retain the exact imported UI data, but their Editor
    // GUID references are not linked by the Player's global asset database.
    // Bind only verified native PPtr slots through public Unity serialization.
    internal static class NativeUIRuntime
    {
        private static readonly Regex PathPattern = new Regex(@"\A[A-Za-z_][A-Za-z0-9_]*(?:\[[0-9]{1,5}\])?(?:\.[A-Za-z_][A-Za-z0-9_]*(?:\[[0-9]{1,5}\])?)*\z", RegexOptions.CultureInvariant);
        private static readonly Regex ManagedPattern = new Regex(@"^references\.RefIds\[([0-9]{1,5})\]\.data\.", RegexOptions.CultureInvariant);
        private static readonly Regex PathTokens = new Regex(@"([A-Za-z_][A-Za-z0-9_]*)|\[([0-9]+)\]", RegexOptions.CultureInvariant);

        [Serializable] private sealed class ReferenceCarrier { public Object target; }

        internal static string Identity(NativeContentFileDescriptor descriptor)
        {
            var result = new StringBuilder("native-ui-v1\n");
            foreach (NativeUIBindingDescriptor binding in descriptor.uiBindings ?? Array.Empty<NativeUIBindingDescriptor>())
                result.Append(binding.ownerFileId).Append('\n').Append(binding.propertyPath).Append('\n')
                    .Append(binding.targetGuid).Append('\n').Append(binding.targetFileId).Append('\n')
                    .Append(binding.managedReferenceId ?? string.Empty).Append('\n');
            return result.ToString();
        }

        internal static void Validate(NativeContentFileDescriptor descriptor)
        {
            NativeUIBindingDescriptor[] bindings = descriptor.uiBindings ?? Array.Empty<NativeUIBindingDescriptor>();
            if (bindings.Length > 4096) throw new InvalidDataException("Native UI binding count exceeds bounds.");
            var owners = new Dictionary<string, string>(StringComparer.Ordinal);
            foreach (NativeContentObjectDescriptor entry in descriptor.objects) owners.Add(entry.fileId, entry.typeName);
            var dependencies = new HashSet<string>(StringComparer.Ordinal);
            foreach (NativeContentDependencyDescriptor entry in descriptor.dependencies ?? Array.Empty<NativeContentDependencyDescriptor>())
                if (entry != null && !entry.global) dependencies.Add(entry.guid);
            var seen = new HashSet<string>(StringComparer.Ordinal);
            foreach (NativeUIBindingDescriptor binding in bindings)
            {
                if (binding == null || !CanonicalID(binding.ownerFileId) || !owners.TryGetValue(binding.ownerFileId, out string type)
                    || !UIType(type) || !ValidPath(binding.propertyPath) || !CanonicalID(binding.targetFileId)
                    || !CanonicalGUID(binding.targetGuid)) throw new InvalidDataException("Native UI binding identity/path is invalid.");
                if (binding.targetGuid == descriptor.guid)
                {
                    if (!owners.TryGetValue(binding.targetFileId, out string localType) || localType != typeof(StyleSheet).FullName)
                        throw new InvalidDataException("Local native UI bindings must identify a published StyleSheet.");
                }
                else if (!dependencies.Contains(binding.targetGuid))
                    throw new InvalidDataException("Native UI binding target is not a declared native dependency.");
                if (!seen.Add(binding.ownerFileId + ":" + binding.propertyPath))
                    throw new InvalidDataException("Duplicate native UI binding property.");
                Match managed = ManagedPattern.Match(binding.propertyPath);
                if (binding.propertyPath.StartsWith("references.", StringComparison.Ordinal))
                {
                    if (!managed.Success || !CanonicalID(binding.managedReferenceId) || binding.managedReferenceId[0] == '-'
                        || int.Parse(managed.Groups[1].Value, CultureInfo.InvariantCulture) > 16383)
                        throw new InvalidDataException("Native UI managed binding requires an exact registry identity.");
                }
                else if (!string.IsNullOrEmpty(binding.managedReferenceId))
                    throw new InvalidDataException("Native UI nonmanaged binding contains a registry identity.");
            }
        }

        internal static void Prepare(NativeContentFileDescriptor descriptor, Dictionary<string, Object> objects,
            Func<string, string, Object> resolve, List<Object> owned)
        {
            Validate(descriptor);
            // Local inline StyleSheets must be initialized before their VTA clone
            // is linked. External dependencies have already completed this step.
            for (int pass = 0; pass < 2; pass++)
                foreach (NativeContentObjectDescriptor entry in descriptor.objects)
                {
                    bool sheet = entry.typeName == typeof(StyleSheet).FullName;
                    bool tree = entry.typeName == typeof(VisualTreeAsset).FullName;
                    if (pass == 0 ? !sheet : !tree) continue;
                    string key = Key(descriptor.guid, entry.fileId);
                    Object original = objects[key];
                    Object intermediate = Object.Instantiate(original);
                    if (intermediate == null || intermediate.GetType() != original.GetType())
                        throw new InvalidDataException("Unity failed to clone a native UI asset.");
                    owned.Add(intermediate);
                    intermediate.name = original.name;
                    Dictionary<string, object> map = Snapshot(intermediate);
                    var expected = new List<KeyValuePair<NativeUIBindingDescriptor, object>>();
                    foreach (NativeUIBindingDescriptor binding in descriptor.uiBindings ?? Array.Empty<NativeUIBindingDescriptor>())
                    {
                        if (binding.ownerFileId != entry.fileId) continue;
                        Object target = resolve(binding.targetGuid, binding.targetFileId);
                        if (target == null) throw new InvalidDataException("Native UI binding target was not loaded: " + binding.targetGuid + "/" + binding.targetFileId);
                        object reference = Reference(target);
                        string path = ResolvePath(map, binding);
                        RequireReference(Get(map, path), reference, binding.propertyPath);
                        ImportedAssetRuntimePaths.Set(map, path, reference);
                        expected.Add(new KeyValuePair<NativeUIBindingDescriptor, object>(binding, reference));
                    }
                    if (expected.Count != 0)
                    {
                        JsonUtility.FromJsonOverwrite(RawJson.Write(map), intermediate);
                        Verify(Snapshot(intermediate), expected);
                    }
                    // FromJsonOverwrite can replace managed VTA nodes and remove
                    // their nonserialized owner links. Cloning the final data
                    // invokes Unity's normal Awake/OnEnable initialization.
                    Object final = Object.Instantiate(intermediate);
                    if (final == null || final.GetType() != original.GetType())
                        throw new InvalidDataException("Unity failed to initialize a native UI asset clone.");
                    owned.Add(final);
                    final.name = original.name;
                    Verify(Snapshot(final), expected);
                    objects[key] = final;
                    Object.DestroyImmediate(intermediate);
                    owned.Remove(intermediate);
                }
        }

        internal static void Release(List<Object> owned)
        {
            Exception failure = null;
            for (int index = owned.Count - 1; index >= 0; index--)
            {
                Object value = owned[index];
                if (value != null) RawCleanup.Run(() => Object.DestroyImmediate(value), ref failure);
            }
            owned.Clear();
            if (failure != null) throw new InvalidOperationException("Native UI clone cleanup failed.", failure);
        }

        private static Dictionary<string, object> Snapshot(Object value)
        {
            var map = RawJson.Parse(JsonUtility.ToJson(value)) as Dictionary<string, object>;
            if (map == null) throw new InvalidDataException("Unity did not serialize the native UI asset.");
            return map;
        }

        private static object Reference(Object value)
        {
            var map = RawJson.Parse(JsonUtility.ToJson(new ReferenceCarrier { target = value })) as Dictionary<string, object>;
            if (map == null || !map.TryGetValue("target", out object reference) || !(reference is Dictionary<string, object>))
                throw new InvalidDataException("Unity did not serialize the native UI target reference.");
            return reference;
        }

        private static void RequireReference(object observed, object expected, string path)
        {
            if (!(observed is Dictionary<string, object> actual) || !(expected is Dictionary<string, object> target)
                || actual.Count != 1 || target.Count != 1)
                throw new InvalidDataException("Native UI binding slot is not a serialized Object reference: " + path);
            foreach (var pair in target)
                if (!actual.TryGetValue(pair.Key, out object id) || !(id is long))
                    throw new InvalidDataException("Native UI binding slot has an unsupported Object reference encoding: " + path);
        }

        private static void Verify(Dictionary<string, object> map, List<KeyValuePair<NativeUIBindingDescriptor, object>> expected)
        {
            foreach (var binding in expected)
                if (RawJson.Write(Get(map, ResolvePath(map, binding.Key))) != RawJson.Write(binding.Value))
                    throw new InvalidDataException("Unity did not retain a native UI binding: " + binding.Key.propertyPath);
        }

        internal static string ResolvePath(Dictionary<string, object> map, NativeUIBindingDescriptor binding)
        {
            if (string.IsNullOrEmpty(binding.managedReferenceId)) return binding.propertyPath;
            Match prefix = ManagedPattern.Match(binding.propertyPath);
            if (!prefix.Success) throw new InvalidDataException("Native UI managed binding path is invalid.");
            var registry = Get(map, "references.RefIds") as List<object>;
            if (registry == null || registry.Count > 16384) throw new InvalidDataException("Native UI managed registry is missing or oversized.");
            int found = -1;
            for (int index = 0; index < registry.Count; index++)
            {
                if (!(registry[index] is Dictionary<string, object> entry) || !entry.TryGetValue("rid", out object rid) || !(rid is long id))
                    throw new InvalidDataException("Native UI managed registry entry has no exact identity.");
                if (id.ToString(CultureInfo.InvariantCulture) != binding.managedReferenceId) continue;
                if (found >= 0) throw new InvalidDataException("Native UI managed registry identity is duplicated.");
                found = index;
            }
            if (found < 0) throw new InvalidDataException("Native UI managed registry identity is missing: " + binding.managedReferenceId);
            return "references.RefIds[" + found.ToString(CultureInfo.InvariantCulture) + "].data."
                + binding.propertyPath.Substring(prefix.Length);
        }

        internal static object Get(Dictionary<string, object> root, string path)
        {
            if (!ValidPath(path)) throw new InvalidDataException("Native UI JSON path is invalid.");
            object current = root;
            foreach (Match token in PathTokens.Matches(path))
            {
                if (token.Groups[1].Success)
                {
                    if (!(current is Dictionary<string, object> map) || !map.TryGetValue(token.Groups[1].Value, out current))
                        throw new InvalidDataException("Native UI serialized field is missing: " + path);
                }
                else
                {
                    int index = int.Parse(token.Groups[2].Value, CultureInfo.InvariantCulture);
                    if (!(current is List<object> values) || index >= values.Count)
                        throw new InvalidDataException("Native UI serialized array item is missing: " + path);
                    current = values[index];
                }
            }
            return current;
        }

        private static bool ValidPath(string path) { return path != null && path.Length <= 1024 && PathPattern.IsMatch(path); }
        private static bool UIType(string name) { return name == typeof(StyleSheet).FullName || name == typeof(VisualTreeAsset).FullName; }
        private static bool CanonicalID(string id)
        {
            return long.TryParse(id, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long value)
                && value != 0 && value.ToString(CultureInfo.InvariantCulture) == id;
        }
        private static bool CanonicalGUID(string guid)
        {
            if (guid == null || guid.Length != 32 || guid == "00000000000000000000000000000000") return false;
            foreach (char c in guid) if (c < '0' || c > '9' && (c < 'a' || c > 'f')) return false;
            return true;
        }
        private static string Key(string guid, string id) { return guid + ":" + id; }
    }
}
