using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private sealed partial class ImportContext
        {
            private readonly Dictionary<Mesh, RawNativeMeshRetention> nativeMeshRetentions = new Dictionary<Mesh, RawNativeMeshRetention>();
            private Dictionary<string, HashSet<RawMeshBakeProfile>> requestedNativeMeshProfiles;

            private static void ValidateNativeMeshRetention(NativeContentFileDescriptor descriptor)
            {
                if (descriptor.meshCpuRetention == null) return;
                var ids = new HashSet<string>(StringComparer.Ordinal);
                foreach (NativeMeshCpuRetentionDescriptor retained in descriptor.meshCpuRetention)
                {
                    if (retained == null || !ids.Add(retained.fileId)) throw new InvalidDataException("Native Mesh CPU retention identity is invalid.");
                    bool matched = false;
                    foreach (NativeContentObjectDescriptor item in descriptor.objects)
                        if (item.fileId == retained.fileId && item.typeName == "UnityEngine.Mesh") { matched = true; break; }
                    if (!matched) throw new InvalidDataException("Native Mesh CPU retention must identify a published Mesh: " + retained.fileId);
                }
            }

            private static void InitializeNativeMeshRetention(NativeContentHandle handle, NativeContentFileDescriptor descriptor)
            {
                if (descriptor.meshCpuRetention == null) return;
                foreach (NativeMeshCpuRetentionDescriptor retained in descriptor.meshCpuRetention)
                {
                    var mesh = (Mesh)handle.Objects[AssetKey(descriptor.guid, retained.fileId)];
                    if (!mesh.isReadable) throw new InvalidDataException("Native Mesh CPU retention readback failed: " + descriptor.guid + "/" + retained.fileId);
                    handle.MeshRetentions.Add(retained.fileId, new RawNativeMeshRetention(mesh, retained.originalIsReadable));
                }
            }

            private bool NativeMeshRetentionMatches(NativeContentHandle handle, NativeContentFileDescriptor descriptor)
            {
                if (handle.MeshRetentions.Count != (descriptor.meshCpuRetention == null ? 0 : descriptor.meshCpuRetention.Length)) return false;
                if (descriptor.meshCpuRetention == null) return true;
                if (requestedNativeMeshProfiles == null) requestedNativeMeshProfiles = RequestedNativeMeshProfiles();
                foreach (NativeMeshCpuRetentionDescriptor retained in descriptor.meshCpuRetention)
                {
                    if (!handle.MeshRetentions.TryGetValue(retained.fileId, out RawNativeMeshRetention state)
                        || state.OriginalIsReadable != retained.originalIsReadable) return false;
                    if (!state.Released) continue;
                    if (requestedNativeMeshProfiles.TryGetValue(AssetKey(descriptor.guid, retained.fileId), out HashSet<RawMeshBakeProfile> required))
                        foreach (RawMeshBakeProfile profile in required) if (!state.Baked.Contains(profile)) return false;
                }
                return true;
            }

            private Dictionary<string, HashSet<RawMeshBakeProfile>> RequestedNativeMeshProfiles()
            {
                var requested = new Dictionary<string, HashSet<RawMeshBakeProfile>>(StringComparer.Ordinal);
                Add(manifest.objects);
                if (manifest.assets != null)
                    foreach (RawSceneAsset asset in manifest.assets) if (asset.kind == "prefab") Add(asset.objects);
                return requested;

                void Add(RawSceneObject[] records)
                {
                    if (records == null) return;
                    foreach (RawSceneObject record in records)
                    {
                        if (record.classId != 64) continue;
                        var data = Map(RawJson.Parse(record.dataJson));
                        ReferenceData mesh = Reference(Value(data, "m_Mesh", null));
                        if (mesh == null || IsNull(mesh.fileID) || string.IsNullOrEmpty(mesh.guid)) continue;
                        string key = AssetKey(mesh.guid, mesh.fileID);
                        if (!requested.TryGetValue(key, out HashSet<RawMeshBakeProfile> profiles))
                        { profiles = new HashSet<RawMeshBakeProfile>(); requested.Add(key, profiles); }
                        profiles.Add(new RawMeshBakeProfile(Bool(data, "m_Convex"), (MeshColliderCookingOptions)Int(data, "m_CookingOptions", 30)));
                    }
                }
            }

            private void RegisterNativeMeshRetention(IEnumerable<NativeContentHandle> handles)
            {
                foreach (NativeContentHandle handle in handles)
                    foreach (RawNativeMeshRetention retained in handle.MeshRetentions.Values)
                        nativeMeshRetentions[retained.Mesh] = retained;
            }

            private void PrepareNativeMeshCollider(MeshCollider collider, Mesh mesh)
            {
                if (mesh != null && nativeMeshRetentions.TryGetValue(mesh, out RawNativeMeshRetention retained))
                    RawMeshCooking.Prepare(retained, collider);
            }

            private void RestoreNativeMeshReadability()
            {
                Exception failure = null;
                foreach (RawNativeMeshRetention retained in nativeMeshRetentions.Values)
                    RawCleanup.Run(() => RawMeshCooking.Restore(retained), ref failure);
                if (failure != null) throw new InvalidOperationException("Native mesh readability restoration failed.", failure);
            }
        }
    }
}
