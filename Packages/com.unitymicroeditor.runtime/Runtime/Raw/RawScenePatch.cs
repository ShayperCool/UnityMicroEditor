using System;
using System.Collections.Generic;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    [Serializable]
    public sealed class RawScenePatch
    {
        public int schemaVersion;
        public string baseRevision;
        public string revision;
        public RawSceneObject[] objects;
    }

    public static partial class RawSceneLoader
    {
        private const int PatchSceneRootsClass = 1660057539;

        public sealed partial class LiveSession
        {
            public Scene ApplyPatch(string path, string expectedHash, string expectedRevision)
            {
                if (disposed) throw new ObjectDisposedException(nameof(LiveSession));
                LastReloadRestartRequired = false;
                byte[] payload;
                using (var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read))
                using (var bytes = new MemoryStream())
                {
                    var buffer = new byte[16384];
                    int count;
                    while ((count = input.Read(buffer, 0, buffer.Length)) != 0)
                    {
                        if (bytes.Length + count > 2 * 1024 * 1024) throw new InvalidDataException("Scene patch exceeds the size limit.");
                        bytes.Write(buffer, 0, count);
                    }
                    payload = bytes.ToArray();
                }
                using (var hash = SHA256.Create())
                    if (BitConverter.ToString(hash.ComputeHash(payload)).Replace("-", string.Empty).ToLowerInvariant() != expectedHash)
                        throw new InvalidDataException("Scene patch changed after publication.");
                var patch = JsonUtility.FromJson<RawScenePatch>(new UTF8Encoding(false, true).GetString(payload));
                if (patch == null || patch.schemaVersion != 1 || patch.objects == null || patch.objects.Length == 0
                    || patch.baseRevision != Revision || string.IsNullOrEmpty(patch.revision) || patch.revision != expectedRevision || patch.revision == Revision)
                    throw new InvalidDataException("Scene patch schema or base revision differs from the live scene.");
                current.ValidatePatch(patch);
                LastReloadRestartRequired = true;
                current.ApplyPatch(patch);
                Revision = patch.revision;
                CurrentRevision = Revision;
                ReloadCount++;
                PreviousSceneUnload = null;
                LastReloadRestartRequired = false;
                return Scene;
            }
        }

        private sealed partial class ImportContext
        {
            internal void ValidatePatch(RawScenePatch patch)
            {
                var records = PatchRecords();
                var changed = new HashSet<string>(StringComparer.Ordinal);
                foreach (RawSceneObject record in patch.objects)
                {
                    if (record == null || string.IsNullOrEmpty(record.fileId) || !changed.Add(record.fileId)
                        || (record.classId != 1 && record.classId != 4 && record.classId != PatchSceneRootsClass) || record.skipMissingScript)
                        throw new InvalidDataException("A scene patch supports unique GameObjects, Transforms and SceneRoots.");
                    var map = PatchData(record);
                    if (records.TryGetValue(record.fileId, out RawSceneObject previous))
                    {
                        if (record.classId != previous.classId || record.gameObjectId != previous.gameObjectId
                            || record.sourcePath != previous.sourcePath || record.guid != previous.guid || record.typeName != previous.typeName
                            || record.skipMissingScript != previous.skipMissingScript)
                            throw new InvalidDataException("A scene patch cannot change object identity.");
                        var oldMap = PatchData(previous);
                        string[] allowed = record.classId == 4
                            ? new[] { "m_LocalPosition", "m_LocalRotation", "m_LocalScale", "m_LocalEulerAnglesHint", "m_Children" }
                            : record.classId == 1 ? new[] { "m_Name", "m_IsActive", "m_Layer", "m_TagString" } : new[] { "m_Roots" };
                        foreach (string name in allowed) { oldMap.Remove(name); map.Remove(name); }
                        if (!PatchEqual(oldMap, map)) throw new InvalidDataException("The patch contains unsupported properties or reparenting.");
                        if (record.classId != PatchSceneRootsClass && (!sceneObjects.TryGetValue(record.fileId, out Object live) || live == null))
                            throw new InvalidDataException("The patched object no longer exists in the running scene.");
                    }
                    records[record.fileId] = record;
                }
                foreach (RawSceneObject record in patch.objects)
                {
                    if (record.classId == PatchSceneRootsClass) continue;
                    if (record.classId == 1)
                    {
                        var data = ReadData<GameObjectData>(record.dataJson);
                        if (data.m_Layer < 0 || data.m_Layer > 31 || (data.m_IsActive != 0 && data.m_IsActive != 1))
                            throw new InvalidDataException("Patch GameObject settings are invalid.");
                        ValidatePatchTag(string.IsNullOrEmpty(data.m_TagString) ? "Untagged" : data.m_TagString);
                        if (!sceneObjects.ContainsKey(record.fileId))
                        {
                            var components = ReadData<PatchGameObjectComponents>(record.dataJson).m_Component;
                            if (components == null || components.Length != 1 || components[0] == null || components[0].component == null
                                || !records.TryGetValue(components[0].component.fileID, out RawSceneObject transform)
                                || transform.classId != 4 || transform.gameObjectId != record.fileId || !changed.Contains(transform.fileId)
                                || sceneObjects.ContainsKey(transform.fileId))
                                throw new InvalidDataException("A new patch GameObject must contain exactly its new Transform.");
                        }
                    }
                    else
                    {
                        var data = ReadData<TransformData>(record.dataJson);
                        ReferenceData ownerReference = ReadData<PatchTransformOwner>(record.dataJson).m_GameObject;
                        if (!records.TryGetValue(record.gameObjectId, out RawSceneObject owner) || owner.classId != 1
                            || ownerReference == null || ownerReference.fileID != record.gameObjectId
                            || (!sceneObjects.ContainsKey(record.fileId) && sceneObjects.ContainsKey(record.gameObjectId)))
                            throw new InvalidDataException("Patch Transform owner is invalid.");
                        if (!Finite(data.m_LocalPosition) || !Finite(data.m_LocalScale)
                            || !Finite(data.m_LocalRotation.x) || !Finite(data.m_LocalRotation.y)
                            || !Finite(data.m_LocalRotation.z) || !Finite(data.m_LocalRotation.w))
                            throw new InvalidDataException("Patch Transform contains a nonfinite value.");
                    }
                }
                ValidatePatchHierarchy(records);
                ValidatePatchLiveChildren(patch);
            }

            private void ValidatePatchLiveChildren(RawScenePatch patch)
            {
                var previous = PatchRecords();
                foreach (RawSceneObject record in patch.objects)
                {
                    if (record.classId != 4 || previous.TryGetValue(record.fileId, out RawSceneObject old)
                        && !PatchFieldChanged(old, record, "m_Children")) continue;
                    sceneObjects.TryGetValue(record.fileId, out Object parentValue);
                    var parent = parentValue as Transform;
                    foreach (ReferenceData reference in ReadData<TransformData>(record.dataJson).m_Children)
                    {
                        // New children get their authored parent before sibling order
                        // is applied. Existing children must still agree at runtime.
                        if (!previous.ContainsKey(reference.fileID)) continue;
                        if (!sceneObjects.TryGetValue(reference.fileID, out Object value)
                            || !(value is Transform child) || child == null || parent == null || child.parent != parent)
                            throw new InvalidDataException("A patched child was removed or reparented by the running game.");
                    }
                }
            }

            private void ValidatePatchTag(string tag)
            {
                if (tag == "Untagged") return;
                foreach (Object value in sceneObjects.Values)
                    if (value is GameObject probe && probe != null)
                    {
                        try { probe.CompareTag(tag); return; }
                        catch (UnityException exception) { throw new InvalidDataException("Patch tag is not declared by this Player: " + tag, exception); }
                    }
                throw new InvalidDataException("Patch tag cannot be verified without a live GameObject.");
            }

            private static void ValidatePatchHierarchy(Dictionary<string, RawSceneObject> records)
            {
                var transforms = new Dictionary<string, TransformData>(StringComparer.Ordinal);
                var roots = new HashSet<string>(StringComparer.Ordinal);
                var listed = new Dictionary<string, string>(StringComparer.Ordinal);
                ReferenceData[] rootList = null;
                int rootRecords = 0;
                foreach (RawSceneObject record in records.Values)
                {
                    if (IsTransform(record.classId))
                    {
                        var data = ReadData<TransformData>(record.dataJson);
                        if (data.m_Father == null || string.IsNullOrEmpty(data.m_Father.fileID) || !string.IsNullOrEmpty(data.m_Father.guid)
                            || data.m_Children == null) throw new InvalidDataException("Patch hierarchy descriptors are incomplete or external.");
                        transforms.Add(record.fileId, data);
                        if (IsNull(data.m_Father.fileID)) roots.Add(record.fileId);
                    }
                    else if (record.classId == PatchSceneRootsClass)
                    {
                        rootRecords++;
                        rootList = ReadData<PatchSceneRootsData>(record.dataJson).m_Roots;
                    }
                }
                foreach (var pair in transforms)
                {
                    string parent = pair.Value.m_Father.fileID;
                    if (!IsNull(parent) && !transforms.ContainsKey(parent)) throw new InvalidDataException("Patch Transform parent is missing.");
                    var children = new HashSet<string>(StringComparer.Ordinal);
                    foreach (ReferenceData reference in pair.Value.m_Children)
                    {
                        if (reference == null || string.IsNullOrEmpty(reference.fileID) || !string.IsNullOrEmpty(reference.guid)
                            || !children.Add(reference.fileID) || !transforms.TryGetValue(reference.fileID, out TransformData child)
                            || child.m_Father.fileID != pair.Key || listed.ContainsKey(reference.fileID))
                            throw new InvalidDataException("Patch Transform child/parent records disagree.");
                        listed.Add(reference.fileID, pair.Key);
                    }
                    var visited = new HashSet<string>(StringComparer.Ordinal) { pair.Key };
                    while (!IsNull(parent))
                    {
                        if (!visited.Add(parent)) throw new InvalidDataException("Patch Transform hierarchy contains a cycle.");
                        if (!transforms.TryGetValue(parent, out TransformData ancestor)) throw new InvalidDataException("Patch ancestor is missing.");
                        parent = ancestor.m_Father.fileID;
                    }
                }
                foreach (var pair in transforms)
                    if (!IsNull(pair.Value.m_Father.fileID) && (!listed.TryGetValue(pair.Key, out string owner) || owner != pair.Value.m_Father.fileID))
                        throw new InvalidDataException("Patch Transform is absent from its parent's child list.");
                if (rootRecords > 1) throw new InvalidDataException("A scene may have only one SceneRoots record.");
                if (rootRecords == 1)
                {
                    if (rootList == null || rootList.Length != roots.Count) throw new InvalidDataException("SceneRoots must list every root Transform.");
                    var seen = new HashSet<string>(StringComparer.Ordinal);
                    foreach (ReferenceData reference in rootList)
                        if (reference == null || !string.IsNullOrEmpty(reference.guid) || !roots.Contains(reference.fileID) || !seen.Add(reference.fileID))
                            throw new InvalidDataException("SceneRoots contains a duplicate or non-root object.");
                }
            }

            internal void ApplyPatch(RawScenePatch patch)
            {
                var previous = PatchRecords();
                var records = new Dictionary<string, RawSceneObject>(previous, StringComparer.Ordinal);
                foreach (RawSceneObject record in patch.objects)
                {
                    if (record.classId == 1 && !previous.ContainsKey(record.fileId))
                    {
                        var gameObject = new GameObject(ReadData<GameObjectData>(record.dataJson).m_Name);
                        ownedSceneObjects.Add(gameObject);
                        gameObject.SetActive(false);
                        SceneManager.MoveGameObjectToScene(gameObject, scene);
                        sceneObjects.Add(record.fileId, gameObject);
                    }
                    records[record.fileId] = record;
                }
                foreach (RawSceneObject record in patch.objects)
                    if (record.classId == 4 && !previous.ContainsKey(record.fileId))
                        sceneObjects.Add(record.fileId, ((GameObject)sceneObjects[record.gameObjectId]).transform);
                foreach (RawSceneObject record in patch.objects)
                {
                    if (record.classId == PatchSceneRootsClass) continue;
                    previous.TryGetValue(record.fileId, out RawSceneObject old);
                    if (record.classId == 4)
                    {
                        if (old == null) ConfigureTransform(record);
                        else
                        {
                            var data = ReadData<TransformData>(record.dataJson);
                            var target = (Transform)sceneObjects[record.fileId];
                            if (PatchFieldChanged(old, record, "m_LocalPosition")) target.localPosition = data.m_LocalPosition;
                            if (PatchFieldChanged(old, record, "m_LocalRotation")) target.localRotation = data.m_LocalRotation;
                            if (PatchFieldChanged(old, record, "m_LocalScale")) target.localScale = data.m_LocalScale;
                        }
                    }
                    else
                    {
                        var data = ReadData<GameObjectData>(record.dataJson);
                        var target = (GameObject)sceneObjects[record.fileId];
                        if (old == null || PatchFieldChanged(old, record, "m_Name")) target.name = data.m_Name;
                        if (old == null || PatchFieldChanged(old, record, "m_Layer")) target.layer = data.m_Layer;
                        if (old == null || PatchFieldChanged(old, record, "m_TagString")) target.tag = string.IsNullOrEmpty(data.m_TagString) ? "Untagged" : data.m_TagString;
                    }
                }
                foreach (RawSceneObject record in patch.objects)
                    if (record.classId == 4 && (!previous.TryGetValue(record.fileId, out RawSceneObject old) || PatchFieldChanged(old, record, "m_Children")))
                        ConfigureTransformChildren(record);
                foreach (RawSceneObject record in patch.objects)
                    if (record.classId == PatchSceneRootsClass)
                    {
                        var roots = ReadData<PatchSceneRootsData>(record.dataJson).m_Roots;
                        for (int index = 0; index < roots.Length; index++)
                        {
                            var target = (Transform)sceneObjects[roots[index].fileID];
                            // An unchanged source parent must not undo runtime reparenting.
                            if (target != null && target.parent == null) target.SetSiblingIndex(index);
                        }
                    }
                foreach (RawSceneObject record in patch.objects)
                    if (record.classId == 1 && (!previous.TryGetValue(record.fileId, out RawSceneObject old) || PatchFieldChanged(old, record, "m_IsActive")))
                        ((GameObject)sceneObjects[record.fileId]).SetActive(ReadData<GameObjectData>(record.dataJson).m_IsActive != 0);
                var values = new List<RawSceneObject>(records.Count);
                foreach (RawSceneObject record in currentRecords) values.Add(records[record.fileId]);
                foreach (RawSceneObject record in patch.objects) if (!previous.ContainsKey(record.fileId)) values.Add(record);
                currentRecords = values.ToArray();
                manifest.objects = currentRecords;
            }

            private Dictionary<string, RawSceneObject> PatchRecords()
            {
                var result = new Dictionary<string, RawSceneObject>(StringComparer.Ordinal);
                foreach (RawSceneObject record in currentRecords) result.Add(record.fileId, record);
                return result;
            }
            private static Dictionary<string, object> PatchData(RawSceneObject record)
            {
                if (!(RawJson.Parse(record.dataJson) is Dictionary<string, object> map)) throw new InvalidDataException("Patch data must be a JSON map.");
                return map;
            }
            private static bool PatchFieldChanged(RawSceneObject old, RawSceneObject next, string field)
            {
                PatchData(old).TryGetValue(field, out object before);
                PatchData(next).TryGetValue(field, out object after);
                return !PatchEqual(before, after);
            }
            private static bool PatchEqual(object first, object second)
            {
                if (ReferenceEquals(first, second)) return true;
                if (first == null || second == null) return false;
                if (first is Dictionary<string, object> left && second is Dictionary<string, object> right)
                {
                    if (left.Count != right.Count) return false;
                    foreach (var item in left) if (!right.TryGetValue(item.Key, out object value) || !PatchEqual(item.Value, value)) return false;
                    return true;
                }
                if (first is List<object> a && second is List<object> b)
                {
                    if (a.Count != b.Count) return false;
                    for (int index = 0; index < a.Count; index++) if (!PatchEqual(a[index], b[index])) return false;
                    return true;
                }
                if ((first is long || first is double) && (second is long || second is double)) return Convert.ToDouble(first) == Convert.ToDouble(second);
                return first.Equals(second);
            }
            private static bool Finite(float value) { return !float.IsNaN(value) && !float.IsInfinity(value); }
            private static bool Finite(Vector3 value) { return Finite(value.x) && Finite(value.y) && Finite(value.z); }
        }

        [Serializable] private sealed class PatchGameObjectComponents { public PatchComponent[] m_Component; }
        [Serializable] private sealed class PatchComponent { public ReferenceData component; }
        [Serializable] private sealed class PatchSceneRootsData { public ReferenceData[] m_Roots; }
        [Serializable] private sealed class PatchTransformOwner { public ReferenceData m_GameObject; }
    }
}
