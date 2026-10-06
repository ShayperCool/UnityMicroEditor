using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;

namespace Dungeon.CLIPlay.Raw
{
    internal readonly struct RawMeshBakeProfile : IEquatable<RawMeshBakeProfile>
    {
        internal readonly bool Convex;
        internal readonly MeshColliderCookingOptions Options;
        internal RawMeshBakeProfile(bool convex, MeshColliderCookingOptions options) { Convex = convex; Options = options; }
        public bool Equals(RawMeshBakeProfile other) => Convex == other.Convex && Options == other.Options;
        public override bool Equals(object value) => value is RawMeshBakeProfile other && Equals(other);
        public override int GetHashCode() => unchecked(((int)Options * 397) ^ (Convex ? 1 : 0));
    }

    // Geometry comes from the native importer unchanged. The temporary CPU copy
    // exists only until Unity has prepared the authored collider profiles.
    internal sealed class RawNativeMeshRetention
    {
        internal readonly Mesh Mesh;
        internal readonly bool OriginalIsReadable;
        internal readonly HashSet<RawMeshBakeProfile> Baked = new HashSet<RawMeshBakeProfile>();
        internal bool Released;
        internal RawNativeMeshRetention(Mesh mesh, bool originalIsReadable) { Mesh = mesh; OriginalIsReadable = originalIsReadable; }
    }

    internal static class RawMeshCooking
    {
        internal static void Prepare(RawNativeMeshRetention retained, MeshCollider collider)
        {
            if (!retained.OriginalIsReadable) RequireShareableTransform(collider);
            var profile = new RawMeshBakeProfile(collider.convex, collider.cookingOptions);
            if (retained.Baked.Contains(profile)) return;
            if (!retained.Mesh.isReadable)
                throw new InvalidDataException("Native mesh needs a fresh CPU projection for collider cooking: " + retained.Mesh.name);
            RequireNoNativeErrors(() => Physics.BakeMesh(retained.Mesh.GetEntityId(), profile.Convex, profile.Options), "Mesh collider baking", retained.Mesh);
            retained.Baked.Add(profile);
        }

        internal static void Restore(RawNativeMeshRetention retained)
        {
            if (retained.OriginalIsReadable || retained.Released) return;
            RequireNoNativeErrors(() => retained.Mesh.UploadMeshData(true), "Mesh CPU retention release", retained.Mesh);
            if (retained.Mesh.isReadable) throw new InvalidDataException("Native mesh authored readability was not restored: " + retained.Mesh.name);
            retained.Released = true;
        }

        private static void RequireShareableTransform(MeshCollider collider)
        {
            Matrix4x4 matrix = collider.transform.localToWorldMatrix;
            Vector4 x = matrix.GetColumn(0), y = matrix.GetColumn(1), z = matrix.GetColumn(2);
            double xx = Dot(x, x), yy = Dot(y, y), zz = Dot(z, z);
            bool skewed = !Orthogonal(x, y, xx, yy) || !Orthogonal(x, z, xx, zz) || !Orthogonal(y, z, yy, zz);
            if (skewed || collider.convex && matrix.determinant < 0)
                throw new InvalidDataException("Authored unreadable mesh collider requires transform-specific cooking: " + collider.name);
        }

        private static double Dot(Vector4 left, Vector4 right) => (double)left.x * right.x + (double)left.y * right.y + (double)left.z * right.z;
        private static bool Orthogonal(Vector4 first, Vector4 second, double firstLength, double secondLength)
        {
            double product = firstLength * secondLength;
            return product > 0 && !double.IsInfinity(product) && !double.IsNaN(product)
                && Math.Abs(Dot(first, second)) <= 0.00001 * Math.Sqrt(product);
        }

        private static void RequireNoNativeErrors(Action operation, string action, Mesh mesh)
        {
            string failure = null;
            var errorLock = new object();
            Application.LogCallback capture = (message, stack, type) =>
            {
                if (type != LogType.Error && type != LogType.Exception && type != LogType.Assert) return;
                lock (errorLock) if (failure == null) failure = message;
            };
            Application.logMessageReceivedThreaded += capture;
            try
            {
                operation();
                lock (errorLock) if (failure != null) throw new InvalidDataException(action + " failed for " + mesh.name + ": " + failure);
            }
            finally { Application.logMessageReceivedThreaded -= capture; }
        }
    }
}
