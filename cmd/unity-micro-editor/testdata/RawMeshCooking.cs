using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using Dungeon.CLIPlay.Raw;
using UnityEngine;

internal static class Program
{
    private static int assertions;
    internal static void Assert(bool condition, string message) { assertions++; if (!condition) throw new Exception(message); }
    internal static void Reject(Action operation, string message)
    {
        try { operation(); } catch (InvalidDataException) { assertions++; return; }
        throw new Exception(message);
    }
    private static MeshCollider Collider(bool convex = false, int options = 30) => new MeshCollider { convex = convex, cookingOptions = (MeshColliderCookingOptions)options };

    private static void Main()
    {
        var mesh = new Mesh();
        var retained = new RawNativeMeshRetention(mesh, false);
        RawMeshCooking.Prepare(retained, Collider());
        RawMeshCooking.Prepare(retained, Collider());
        Assert(Physics.BakeCalls == 1 && retained.Baked.Count == 1, "shared mesh profile should bake once");
        RawMeshCooking.Prepare(retained, Collider(true));
        RawMeshCooking.Prepare(retained, Collider(false, 24));
        Assert(Physics.BakeCalls == 3 && retained.Baked.Count == 3, "convex and cooking options need exact separate profiles");
        RawMeshCooking.Restore(retained);
        Assert(!mesh.isReadable && retained.Released && mesh.Uploads == 1, "authored non-readable semantics must be restored");
        Assert(mesh.GeometryRevision == 17, "physics preparation must leave geometry unchanged");
        RawMeshCooking.Restore(retained);
        RawMeshCooking.Prepare(retained, Collider());
        Assert(mesh.Uploads == 1 && Physics.BakeCalls == 3, "same baked profile should work after CPU discard");
        Reject(() => RawMeshCooking.Prepare(retained, Collider(false, 0)), "new profile must require a fresh projected native revision");

        var readable = new RawNativeMeshRetention(new Mesh(), true);
        RawMeshCooking.Prepare(readable, Collider());
        RawMeshCooking.Restore(readable);
        Assert(readable.Mesh.isReadable && !readable.Released && readable.Mesh.Uploads == 0, "authored readable meshes must stay readable");

        var failed = new RawNativeMeshRetention(new Mesh(), false);
        Physics.Fail = true;
        Reject(() => RawMeshCooking.Prepare(failed, Collider()), "native cooking errors must fail closed");
        Assert(failed.Baked.Count == 0 && Application.Listeners == 0, "failed bake must not enter the cache or leak listener");
        Physics.Fail = false;
        RawMeshCooking.Prepare(failed, Collider());
        Assert(failed.Baked.Count == 1, "failed profile must be retryable");

        var uploadFailure = new RawNativeMeshRetention(new Mesh { FailUpload = true }, false);
        Reject(() => RawMeshCooking.Restore(uploadFailure), "native upload errors must fail closed");
        Assert(!uploadFailure.Released && Application.Listeners == 0, "failed upload cannot claim restored state");
        var ignoredUpload = new RawNativeMeshRetention(new Mesh { IgnoreUpload = true }, false);
        Reject(() => RawMeshCooking.Restore(ignoredUpload), "restored public isReadable must be verified");

        var skew = Collider();
        skew.transform.localToWorldMatrix = new Matrix4x4(new Vector4(1, 0, 0), new Vector4(1, 1, 0), new Vector4(0, 0, 1), 1);
        Reject(() => RawMeshCooking.Prepare(new RawNativeMeshRetention(new Mesh(), false), skew), "unreadable skewed shape cannot reuse generic collision data");
        RawMeshCooking.Prepare(new RawNativeMeshRetention(new Mesh(), true), skew);
        var mirrored = Collider(true);
        mirrored.transform.localToWorldMatrix = new Matrix4x4(new Vector4(-1, 0, 0), new Vector4(0, 1, 0), new Vector4(0, 0, 1), -1);
        Reject(() => RawMeshCooking.Prepare(new RawNativeMeshRetention(new Mesh(), false), mirrored), "convex negative scale cannot reuse generic collision data");
        mirrored.convex = false;
        RawMeshCooking.Prepare(new RawNativeMeshRetention(new Mesh(), false), mirrored);
        Assert(Application.Listeners == 0, "all native listeners must be released");
        RawSceneLoader.NativeMeshTests();
        Console.WriteLine("Native mesh cooking assertions: " + assertions);
    }
}

namespace Dungeon.CLIPlay.Raw
{
    internal sealed class RawSceneManifest { internal RawSceneObject[] objects; internal RawSceneAsset[] assets; }
    internal sealed class RawSceneObject { internal int classId; internal string dataJson; }
    internal sealed class RawSceneAsset { internal string kind; internal RawSceneObject[] objects; }
    internal sealed class NativeContentFileDescriptor
    {
        internal string guid;
        internal NativeContentObjectDescriptor[] objects;
        internal NativeMeshCpuRetentionDescriptor[] meshCpuRetention;
    }
    internal sealed class NativeContentObjectDescriptor { internal string fileId, typeName; }
    internal sealed class NativeMeshCpuRetentionDescriptor { internal string fileId; internal bool originalIsReadable; }

    public static partial class RawSceneLoader
    {
        private sealed class NativeContentHandle
        {
            internal readonly Dictionary<string, object> Objects = new Dictionary<string, object>();
            internal readonly Dictionary<string, RawNativeMeshRetention> MeshRetentions = new Dictionary<string, RawNativeMeshRetention>();
        }
        private sealed class ReferenceData { internal string guid, fileID; }
        private static string AssetKey(string guid, string id) => guid + ":" + id;
        private static bool IsNull(string id) => string.IsNullOrEmpty(id) || id == "0";
        private static Dictionary<string, object> Map(object value) => value as Dictionary<string, object> ?? new Dictionary<string, object>();
        private static object Value(Dictionary<string, object> map, string key, object fallback) => map.TryGetValue(key, out object value) ? value : fallback;
        private static int Int(Dictionary<string, object> map, string key, int fallback = 0) => Convert.ToInt32(Value(map, key, fallback), CultureInfo.InvariantCulture);
        private static bool Bool(Dictionary<string, object> map, string key) => Int(map, key) != 0;
        private static ReferenceData Reference(object value)
        {
            if (!(value is Dictionary<string, object> map)) return null;
            return new ReferenceData { guid = Convert.ToString(Value(map, "guid", null), CultureInfo.InvariantCulture), fileID = Convert.ToString(Value(map, "fileID", null), CultureInfo.InvariantCulture) };
        }
        private sealed partial class ImportContext
        {
            private readonly RawSceneManifest manifest;
            internal ImportContext(RawSceneManifest manifest) { this.manifest = manifest; }
            internal static void RunNativeMeshTests()
            {
            const string guid = "1234567890abcdef1234567890abcdef", id = "-349857";
            var handle = new NativeContentHandle();
            var mesh = new Mesh();
            handle.Objects.Add(AssetKey(guid, id), mesh);
            var descriptor = new NativeContentFileDescriptor { guid = guid,
                objects = new[] { new NativeContentObjectDescriptor { fileId = id, typeName = "UnityEngine.Mesh" } },
                meshCpuRetention = new[] { new NativeMeshCpuRetentionDescriptor { fileId = id, originalIsReadable = false } } };
            ImportContext.ValidateNativeMeshRetention(descriptor);
            ImportContext.InitializeNativeMeshRetention(handle, descriptor);
            RawSceneObject Record(int options) => new RawSceneObject { classId = 64, dataJson = "{\"m_Mesh\":{\"guid\":\"" + guid + "\",\"fileID\":" + id + "},\"m_Convex\":0,\"m_CookingOptions\":" + options + "}" };
            var context = new ImportContext(new RawSceneManifest { objects = new[] { Record(30) }, assets = Array.Empty<RawSceneAsset>() });
            context.RegisterNativeMeshRetention(new[] { handle });
            context.PrepareNativeMeshCollider(new MeshCollider { cookingOptions = (MeshColliderCookingOptions)30 }, mesh);
            context.RestoreNativeMeshReadability();
            Program.Assert(!mesh.isReadable && context.NativeMeshRetentionMatches(handle, descriptor), "shared native cache must reuse exact baked source profile after CPU discard");
            var changed = new ImportContext(new RawSceneManifest { objects = new[] { Record(24) }, assets = Array.Empty<RawSceneAsset>() });
            Program.Assert(!changed.NativeMeshRetentionMatches(handle, descriptor), "new collider profile must reload a fresh native projection");
            var changedPrefab = new ImportContext(new RawSceneManifest { objects = Array.Empty<RawSceneObject>(), assets = new[] { new RawSceneAsset { kind = "prefab", objects = new[] { Record(0) } } } });
            Program.Assert(!changedPrefab.NativeMeshRetentionMatches(handle, descriptor), "raw prefab collider changes must also invalidate cached CPU-discarded mesh");
            descriptor.meshCpuRetention[0].originalIsReadable = true;
            Program.Assert(!context.NativeMeshRetentionMatches(handle, descriptor), "authored readability metadata is part of native cache identity");
            descriptor.meshCpuRetention[0].originalIsReadable = false;
            descriptor.meshCpuRetention = new[] { descriptor.meshCpuRetention[0], descriptor.meshCpuRetention[0] };
            Program.Reject(() => ImportContext.ValidateNativeMeshRetention(descriptor), "duplicate retained mesh IDs must fail");
            descriptor.meshCpuRetention = new[] { new NativeMeshCpuRetentionDescriptor { fileId = "unknown" } };
            Program.Reject(() => ImportContext.ValidateNativeMeshRetention(descriptor), "retained unknown mesh ID must fail");
            descriptor.meshCpuRetention[0].fileId = id;
            descriptor.objects[0].typeName = "UnityEngine.Texture2D";
            Program.Reject(() => ImportContext.ValidateNativeMeshRetention(descriptor), "retention cannot identify a non-Mesh object");
            var partial = new ImportContext(new RawSceneManifest { objects = Array.Empty<RawSceneObject>(), assets = Array.Empty<RawSceneAsset>() });
            var firstFailed = new Mesh { FailUpload = true };
            var secondRestored = new Mesh();
            partial.nativeMeshRetentions.Add(firstFailed, new RawNativeMeshRetention(firstFailed, false));
            partial.nativeMeshRetentions.Add(secondRestored, new RawNativeMeshRetention(secondRestored, false));
            Exception cleanupFailure = null;
            RawCleanup.Run(partial.RestoreNativeMeshReadability, ref cleanupFailure);
            Program.Assert(cleanupFailure is InvalidOperationException && !secondRestored.isReadable,
                "failed-load restoration must attempt every mesh despite one native cleanup error");
            Program.Assert(firstFailed.isReadable && !partial.nativeMeshRetentions[firstFailed].Released,
                "failed native release must retain retryable CPU state rather than claim success");
            firstFailed.FailUpload = false;
            partial.RestoreNativeMeshReadability();
            Program.Assert(!firstFailed.isReadable && secondRestored.Uploads == 1,
                "failed-load cleanup retry must release only meshes still holding temporary CPU copies");
            }
        }
        internal static void NativeMeshTests() => ImportContext.RunNativeMeshTests();
    }
}

namespace UnityEngine
{
    internal enum MeshColliderCookingOptions { None = 0 }
    internal readonly struct EntityId { }
    internal sealed class Mesh
    {
        internal string name = "Exact native geometry";
        internal bool isReadable = true;
        internal int Uploads;
        internal readonly int GeometryRevision = 17;
        internal bool FailUpload, IgnoreUpload;
        internal EntityId GetEntityId() => default;
        internal void UploadMeshData(bool discard)
        {
            Uploads++;
            if (FailUpload) { Application.Emit("upload failure"); return; }
            if (!IgnoreUpload) isReadable = !discard;
        }
    }
    internal sealed class MeshCollider
    {
        internal string name = "Authored collider";
        internal bool convex;
        internal MeshColliderCookingOptions cookingOptions;
        internal readonly Transform transform = new Transform();
    }
    internal sealed class Transform { internal Matrix4x4 localToWorldMatrix = new Matrix4x4(new Vector4(1, 0, 0), new Vector4(0, 1, 0), new Vector4(0, 0, 1), 1); }
    internal readonly struct Vector4
    {
        internal readonly float x, y, z;
        internal Vector4(float x, float y, float z) { this.x = x; this.y = y; this.z = z; }
    }
    internal readonly struct Matrix4x4
    {
        private readonly Vector4 x, y, z;
        internal readonly float determinant;
        internal Matrix4x4(Vector4 x, Vector4 y, Vector4 z, float determinant) { this.x = x; this.y = y; this.z = z; this.determinant = determinant; }
        internal Vector4 GetColumn(int index) => index == 0 ? x : index == 1 ? y : z;
    }
    internal static class Physics
    {
        internal static bool Fail;
        internal static int BakeCalls;
        internal static void BakeMesh(EntityId mesh, bool convex, MeshColliderCookingOptions options)
        { BakeCalls++; if (Fail) Application.Emit("cooking failure"); }
    }
    internal enum LogType { Error, Exception, Assert, Warning, Log }
    internal static class Application
    {
        internal delegate void LogCallback(string message, string stack, LogType type);
        private static LogCallback callbacks;
        internal static int Listeners;
        internal static event LogCallback logMessageReceivedThreaded
        {
            add { callbacks += value; Listeners++; }
            remove { callbacks -= value; Listeners--; }
        }
        internal static void Emit(string message) => callbacks?.Invoke(message, string.Empty, LogType.Error);
    }
}
