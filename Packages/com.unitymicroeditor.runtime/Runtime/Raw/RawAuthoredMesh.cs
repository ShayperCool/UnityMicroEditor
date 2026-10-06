using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEngine;
using UnityEngine.Rendering;

namespace Dungeon.CLIPlay.Raw
{
    // Restores a source-authored Mesh.asset's existing geometry. No
    // interchange model import, compression, recalculation or optimization.
    internal static class RawAuthoredMesh
    {
        private const int MaximumBytes = 64 << 20;
        private const MeshUpdateFlags Flags = MeshUpdateFlags.DontRecalculateBounds | MeshUpdateFlags.DontValidateIndices;

        internal static void Validate(RawSceneAsset asset) => Parse(asset);

        internal static Mesh Create(RawSceneAsset asset)
        {
            Source source = Parse(asset);
            var mesh = new Mesh { name = source.data.m_Name };
            try
            {
                mesh.SetVertexBufferParams(source.data.m_VertexData.m_VertexCount, source.attributes);
                if (mesh.vertexBufferCount != source.streams.Length)
                    throw Error(asset, "Unity's public vertex layout differs from the authored buffer.");
                for (int attribute = 0; attribute < source.data.m_VertexData.m_Channels.Length; attribute++)
                {
                    Channel channel = source.data.m_VertexData.m_Channels[attribute];
                    if (channel.dimension == 0) continue;
                    if (mesh.GetVertexAttributeOffset((VertexAttribute)attribute) != channel.offset
                        || mesh.GetVertexAttributeStream((VertexAttribute)attribute) != channel.stream
                        || mesh.GetVertexAttributeDimension((VertexAttribute)attribute) != channel.dimension
                        || (int)mesh.GetVertexAttributeFormat((VertexAttribute)attribute) != channel.format)
                        throw Error(asset, "Unity changed an authored vertex attribute layout.");
                }
                for (int stream = 0; stream < source.streams.Length; stream++)
                {
                    if (mesh.GetVertexBufferStride(stream) != source.strides[stream])
                        throw Error(asset, "Unity changed an authored vertex stream stride.");
                    mesh.SetVertexBufferData(source.streams[stream], 0, 0, source.streams[stream].Length, stream, Flags);
                }
                if (source.bindposes.Length != 0) mesh.bindposes = source.bindposes;
                mesh.SetIndexBufferParams(source.indices.Length / source.indexBytes, (IndexFormat)source.data.m_IndexFormat);
                mesh.SetIndexBufferData(source.indices, 0, 0, source.indices.Length, Flags);
                mesh.subMeshCount = source.data.m_SubMeshes.Length;
                for (int index = 0; index < source.data.m_SubMeshes.Length; index++)
                {
                    Submesh submesh = source.data.m_SubMeshes[index];
                    mesh.SetSubMesh(index, new SubMeshDescriptor(submesh.firstByte / source.indexBytes, submesh.indexCount, (MeshTopology)submesh.topology)
                    {
                        baseVertex = submesh.baseVertex, firstVertex = submesh.firstVertex, vertexCount = submesh.vertexCount,
                        bounds = submesh.localAABB.Bounds,
                    }, Flags);
                }
                mesh.bounds = source.data.m_LocalAABB.Bounds;
                VerifyReadback(asset, mesh, source);
                if (source.data.m_IsReadable == 0) mesh.UploadMeshData(true);
                return mesh;
            }
            catch
            {
                UnityEngine.Object.Destroy(mesh);
                throw;
            }
        }

        private static void VerifyReadback(RawSceneAsset asset, Mesh mesh, Source source)
        {
            if (mesh.vertexCount != source.data.m_VertexData.m_VertexCount || mesh.subMeshCount != source.data.m_SubMeshes.Length
                || !SameBounds(mesh.bounds, source.data.m_LocalAABB.Bounds))
                throw Error(asset, "Mesh vertex/submesh count or bounds differ after restore.");
            using (Mesh.MeshDataArray views = Mesh.AcquireReadOnlyMeshData(mesh))
            {
                Mesh.MeshData view = views[0];
                var indices = view.GetIndexData<byte>();
                if (indices.Length != source.indices.Length)
                    throw Error(asset, "Mesh buffer lengths differ after restore.");
                for (int stream = 0; stream < source.streams.Length; stream++)
                {
                    var vertices = view.GetVertexData<byte>(stream);
                    if (vertices.Length != source.streams[stream].Length) throw Error(asset, "Vertex stream length changed.");
                    for (int index = 0; index < vertices.Length; index++)
                        if (vertices[index] != source.streams[stream][index]) throw Error(asset, "Unity changed authored vertex bytes.");
                }
                for (int index = 0; index < source.indices.Length; index++)
                    if (indices[index] != source.indices[index]) throw Error(asset, "Unity changed authored index bytes.");
            }
            if (source.bindposes.Length != 0)
            {
                Matrix4x4[] bindposes = mesh.bindposes;
                if (bindposes.Length != source.bindposes.Length) throw Error(asset, "Bindpose count changed.");
                for (int index = 0; index < bindposes.Length; index++)
                    if (!bindposes[index].Equals(source.bindposes[index])) throw Error(asset, "Unity changed authored bindpose floats.");
                BoneWeight[] weights = mesh.boneWeights;
                if (weights.Length != mesh.vertexCount) throw Error(asset, "Public skinning weights are unavailable.");
                for (int vertex = 0; vertex < weights.Length; vertex++)
                {
                    BoneWeight expected = source.Weight(vertex);
                    BoneWeight actual = weights[vertex];
                    if (!actual.weight0.Equals(expected.weight0) || !actual.weight1.Equals(expected.weight1)
                        || !actual.weight2.Equals(expected.weight2) || !actual.weight3.Equals(expected.weight3)
                        || actual.boneIndex0 != expected.boneIndex0 || actual.boneIndex1 != expected.boneIndex1
                        || actual.boneIndex2 != expected.boneIndex2 || actual.boneIndex3 != expected.boneIndex3)
                        throw Error(asset, "Public skinning weight/bone-index readback differs from authored bytes.");
                }
            }
            for (int index = 0; index < source.data.m_SubMeshes.Length; index++)
            {
                Submesh expected = source.data.m_SubMeshes[index];
                SubMeshDescriptor actual = mesh.GetSubMesh(index);
                if (actual.indexStart != expected.firstByte / source.indexBytes || actual.indexCount != expected.indexCount
                    || (int)actual.topology != expected.topology || actual.baseVertex != expected.baseVertex
                    || actual.firstVertex != expected.firstVertex || actual.vertexCount != expected.vertexCount
                    || !SameBounds(actual.bounds, expected.localAABB.Bounds))
                    throw Error(asset, "Unity changed an authored submesh descriptor.");
            }
        }

        private static Source Parse(RawSceneAsset asset)
        {
            if (asset.classId != 43 || asset.typeName != "UnityEngine.Mesh" || string.IsNullOrEmpty(asset.fileId))
                throw Error(asset, "Raw Mesh identity/type is invalid.");
            var map = RawJson.Parse(asset.dataJson) as Dictionary<string, object>;
            if (map == null) throw Error(asset, "Mesh data must be a JSON object.");
            RejectUnsupported(asset, map);
            Data data = JsonUtility.FromJson<Data>(asset.dataJson);
            if (data == null || data.serializedVersion != 12 || data.m_MeshCompression != 0
                || data.m_VertexData == null || data.m_VertexData.serializedVersion != 3
                || data.m_VertexData.m_VertexCount <= 0 || data.m_VertexData.m_VertexCount > 4 * 1024 * 1024
                || data.m_VertexData.m_Channels == null || data.m_VertexData.m_Channels.Length != 14
                || data.m_SubMeshes == null || data.m_SubMeshes.Length == 0 || data.m_SubMeshes.Length > 65535
                || data.m_IndexFormat < 0 || data.m_IndexFormat > 1 || data.m_LocalAABB == null
                || data.m_IsReadable < 0 || data.m_IsReadable > 1)
                throw Error(asset, "Unsupported or missing pinned static Mesh layout.");
            var source = new Source { data = data, indexBytes = data.m_IndexFormat == 0 ? 2 : 4 };
            source.vertices = Hex(asset, data.m_VertexData._typelessdata);
            source.indices = Hex(asset, data.m_IndexBuffer);
            if (source.vertices.Length != data.m_VertexData.m_DataSize || source.indices.Length == 0 || source.indices.Length % source.indexBytes != 0)
                throw Error(asset, "Source Mesh buffer lengths do not match their metadata.");
            var attributes = new List<VertexAttributeDescriptor>();
            var strides = new int[4];
            int maximumStream = 0;
            for (int index = 0; index < data.m_VertexData.m_Channels.Length; index++)
            {
                Channel channel = data.m_VertexData.m_Channels[index];
                if (channel == null || channel.dimension < 0 || channel.dimension > 4)
                    throw Error(asset, "Packed or malformed vertex attribute dimensions.");
                if (channel.dimension == 0) continue;
                if (channel.stream < 0 || channel.stream > 3 || channel.offset != strides[channel.stream]
                    || !(channel.format == 0 || channel.format == 2 || channel.format == 10))
                    throw Error(asset, "Only tightly packed float32/UNorm8/UInt32 vertex attributes are supported.");
                if (index >= 12 && (data.m_BindPose == null || data.m_BindPose.Length == 0 || channel.dimension != 4
                    || index == 12 && channel.format != 0 || index == 13 && channel.format != 10))
                    throw Error(asset, "Only fixed-four skinning with float32 weights/UInt32 bone indices is supported.");
                attributes.Add(new VertexAttributeDescriptor((VertexAttribute)index, (VertexAttributeFormat)channel.format, channel.dimension, channel.stream));
                strides[channel.stream] = checked(strides[channel.stream] + (channel.format == 2 ? 1 : 4) * channel.dimension);
                maximumStream = Math.Max(maximumStream, channel.stream);
            }
            if (data.m_VertexData.m_Channels[0].dimension != 3 || data.m_VertexData.m_Channels[0].format != 0)
                throw Error(asset, "Source Mesh needs float32 position3.");
            source.attributes = attributes.ToArray(); source.strides = new int[maximumStream + 1];
            source.streams = new byte[maximumStream + 1][];
            int sourceOffset = 0;
            for (int stream = 0; stream <= maximumStream; stream++)
            {
                if (strides[stream] == 0) throw Error(asset, "Sparse source vertex streams are unsupported.");
                int aligned = checked((sourceOffset + 15) & ~15);
                for (int pad = sourceOffset; pad < aligned; pad++)
                    if (pad >= source.vertices.Length || source.vertices[pad] != 0) throw Error(asset, "Nonzero vertex stream alignment padding.");
                sourceOffset = aligned;
                int size = checked(strides[stream] * data.m_VertexData.m_VertexCount);
                if (sourceOffset > source.vertices.Length - size) throw Error(asset, "Vertex stream exceeds source byte range.");
                source.streams[stream] = new byte[size]; source.strides[stream] = strides[stream];
                Array.Copy(source.vertices, sourceOffset, source.streams[stream], 0, size); sourceOffset += size;
            }
            if (sourceOffset != source.vertices.Length) throw Error(asset, "Source vertex buffer contains unsupported trailing data.");
            source.bindposes = new Matrix4x4[data.m_BindPose == null ? 0 : data.m_BindPose.Length];
            if (source.bindposes.Length > 1024) throw Error(asset, "Bindpose count exceeds supported bounds.");
            if (source.bindposes.Length != 0 && (data.m_VertexData.m_Channels[12].dimension != 4 || data.m_VertexData.m_Channels[13].dimension != 4))
                throw Error(asset, "Skinned Mesh has no fixed-four weight/index channels.");
            for (int index = 0; index < source.bindposes.Length; index++)
            {
                if (data.m_BindPose[index] == null) throw Error(asset, "Missing source bindpose matrix.");
                Matrix4x4 matrix = data.m_BindPose[index].Matrix;
                for (int element = 0; element < 16; element++)
                    if (float.IsNaN(matrix[element]) || float.IsInfinity(matrix[element])) throw Error(asset, "Nonfinite source bindpose.");
                source.bindposes[index] = matrix;
            }
            if (source.bindposes.Length != 0)
                for (int vertex = 0; vertex < data.m_VertexData.m_VertexCount; vertex++)
                {
                    BoneWeight weight = source.Weight(vertex);
                    foreach (float value in new[] { weight.weight0, weight.weight1, weight.weight2, weight.weight3 })
                        if (float.IsNaN(value) || float.IsInfinity(value) || value < 0 || value > 1) throw Error(asset, "Invalid source bone weight.");
                    foreach (int bone in new[] { weight.boneIndex0, weight.boneIndex1, weight.boneIndex2, weight.boneIndex3 })
                        if (bone < 0 || bone >= source.bindposes.Length) throw Error(asset, "Source bone index exceeds bindpose range.");
                }
            ValidateBounds(asset, data.m_LocalAABB);
            for (int index = 0; index < data.m_SubMeshes.Length; index++)
            {
                Submesh submesh = data.m_SubMeshes[index];
                if (submesh == null || submesh.serializedVersion != 2 || submesh.firstByte < 0
                    || submesh.firstByte % source.indexBytes != 0 || submesh.indexCount < 0
                    || (long)submesh.firstByte + (long)submesh.indexCount * source.indexBytes > source.indices.Length
                    || submesh.firstVertex < 0 || submesh.vertexCount < 0
                    || (long)submesh.firstVertex + submesh.vertexCount > data.m_VertexData.m_VertexCount
                    || submesh.localAABB == null || !Topology(submesh.topology, submesh.indexCount))
                    throw Error(asset, "Invalid source submesh range/topology.");
                ValidateBounds(asset, submesh.localAABB);
                for (int item = 0; item < submesh.indexCount; item++)
                {
                    int offset = submesh.firstByte + item * source.indexBytes;
                    uint vertex = source.indexBytes == 2 ? (uint)(source.indices[offset] | source.indices[offset + 1] << 8)
                        : (uint)(source.indices[offset] | source.indices[offset + 1] << 8 | source.indices[offset + 2] << 16 | source.indices[offset + 3] << 24);
                    long absolute = (long)vertex + submesh.baseVertex;
                    if (absolute < submesh.firstVertex || absolute >= (long)submesh.firstVertex + submesh.vertexCount)
                        throw Error(asset, "Source submesh indices address vertices outside its range.");
                }
            }
            return source;
        }

        private static bool Topology(int topology, int count) => topology == 0 && count % 3 == 0
            || topology == 2 && count % 4 == 0 || topology == 3 && count % 2 == 0 || topology == 4 || topology == 5;

        private static void RejectUnsupported(RawSceneAsset asset, Dictionary<string, object> map)
        {
            if (Map(map, "m_VariableBoneCountWeights").TryGetValue("m_Data", out object weights) && weights != null && weights.ToString() != "")
                throw Error(asset, "Variable-count bone weights are unsupported.");
            // Bone-name hashes/root hash and per-bone AABB are native importer
            // metadata, with no public Mesh setter. Rendering uses the restored
            // bindposes/weights plus the renderer's authored bones/localBounds.
            if (map.TryGetValue("m_BindPose", out object poseValue) && poseValue is List<object> poses && poses.Count > 0)
            {
                if (Hex(asset, Text(map, "m_BoneNameHashes")).Length != poses.Count * 4
                    || !map.TryGetValue("m_BonesAABB", out object aabbs) || !(aabbs is List<object> boneBounds) || boneBounds.Count != poses.Count)
                    throw Error(asset, "Importer bone metadata does not match bindpose count.");
            }
            else if (Text(map, "m_BoneNameHashes") != "" || Integer(map, "m_RootBoneNameHash") != 0
                || map.TryGetValue("m_BonesAABB", out object boundsValue) && boundsValue is List<object> bounds && bounds.Count != 0)
                throw Error(asset, "Bone metadata has no bindposes.");
            foreach (object value in Map(map, "m_Shapes").Values)
                if (value is List<object> shapes && shapes.Count != 0) throw Error(asset, "Blend shapes are unsupported.");
            foreach (var field in Map(map, "m_CompressedMesh"))
            {
                if (field.Key == "m_UVInfo") { if (Convert.ToInt64(field.Value, CultureInfo.InvariantCulture) != 0) throw Error(asset, "Packed Mesh UV info is unsupported."); continue; }
                if (!(field.Value is Dictionary<string, object> packed)) throw Error(asset, "Malformed compressed Mesh metadata.");
                if (Integer(packed, "m_NumItems") != 0 || Text(packed, "m_Data") != "") throw Error(asset, "Packed Mesh data is unsupported.");
            }
            Dictionary<string, object> stream = Map(map, "m_StreamData");
            if (Integer(stream, "offset") != 0 || Integer(stream, "size") != 0 || Text(stream, "path") != "")
                throw Error(asset, "Streamed Mesh data is unsupported.");
            Dictionary<string, object> lod = Map(map, "m_MeshLodInfo");
            if (Integer(lod, "m_NumLevels") > 1) throw Error(asset, "Multi-level Mesh LOD data is unsupported.");
        }

        private static Dictionary<string, object> Map(Dictionary<string, object> value, string name) =>
            value.TryGetValue(name, out object field) && field is Dictionary<string, object> map ? map : new Dictionary<string, object>();
        private static string Text(Dictionary<string, object> value, string name) => value.TryGetValue(name, out object field) && field != null ? Convert.ToString(field, CultureInfo.InvariantCulture) : "";
        private static long Integer(Dictionary<string, object> value, string name) => value.TryGetValue(name, out object field) && field != null ? Convert.ToInt64(field, CultureInfo.InvariantCulture) : 0;

        private static byte[] Hex(RawSceneAsset asset, string text)
        {
            text = text ?? "";
            if (text.EndsWith("y", StringComparison.Ordinal)) text = text.Substring(0, text.Length - 1);
            if (text.EndsWith("u", StringComparison.Ordinal)) text = text.Substring(0, text.Length - 1);
            if ((text.Length & 1) != 0 || text.Length > MaximumBytes * 2) throw Error(asset, "Mesh byte string length exceeds bounds.");
            var bytes = new byte[text.Length / 2];
            for (int index = 0; index < bytes.Length; index++)
                if (!byte.TryParse(text.Substring(index * 2, 2), NumberStyles.HexNumber, CultureInfo.InvariantCulture, out bytes[index]))
                    throw Error(asset, "Invalid Mesh hex byte data.");
            return bytes;
        }

        private static void ValidateBounds(RawSceneAsset asset, AABB bounds)
        {
            if (!Finite(bounds.m_Center) || !Finite(bounds.m_Extent) || bounds.m_Extent.x < 0 || bounds.m_Extent.y < 0 || bounds.m_Extent.z < 0)
                throw Error(asset, "Mesh bounds must be finite and have nonnegative extents.");
        }
        private static bool Finite(Vector3 value) => !float.IsNaN(value.x) && !float.IsInfinity(value.x)
            && !float.IsNaN(value.y) && !float.IsInfinity(value.y) && !float.IsNaN(value.z) && !float.IsInfinity(value.z);
        private static bool SameBounds(Bounds a, Bounds b) => a.center.Equals(b.center) && a.extents.Equals(b.extents);
        private static InvalidDataException Error(RawSceneAsset asset, string reason) => new InvalidDataException("Raw Mesh " + asset.path + ": " + reason);

        private sealed class Source
        {
            public Data data;
            public VertexAttributeDescriptor[] attributes;
            public byte[] vertices, indices;
            public byte[][] streams;
            public int[] strides;
            public Matrix4x4[] bindposes;
            public int indexBytes;
            public BoneWeight Weight(int vertex)
            {
                Channel weight = data.m_VertexData.m_Channels[12], bone = data.m_VertexData.m_Channels[13];
                byte[] weights = streams[weight.stream], bones = streams[bone.stream];
                int w = vertex * strides[weight.stream] + weight.offset, b = vertex * strides[bone.stream] + bone.offset;
                return new BoneWeight
                {
                    weight0 = BitConverter.ToSingle(weights, w), weight1 = BitConverter.ToSingle(weights, w + 4),
                    weight2 = BitConverter.ToSingle(weights, w + 8), weight3 = BitConverter.ToSingle(weights, w + 12),
                    boneIndex0 = checked((int)BitConverter.ToUInt32(bones, b)), boneIndex1 = checked((int)BitConverter.ToUInt32(bones, b + 4)),
                    boneIndex2 = checked((int)BitConverter.ToUInt32(bones, b + 8)), boneIndex3 = checked((int)BitConverter.ToUInt32(bones, b + 12)),
                };
            }
        }
        [Serializable] private sealed class Data
        {
            public int serializedVersion, m_MeshCompression, m_IndexFormat, m_IsReadable;
            public string m_Name, m_IndexBuffer;
            public VertexData m_VertexData;
            public Submesh[] m_SubMeshes;
            public AABB m_LocalAABB;
            public MatrixData[] m_BindPose;
        }
        [Serializable] private sealed class VertexData
        {
            public int serializedVersion, m_VertexCount, m_DataSize;
            public string _typelessdata;
            public Channel[] m_Channels;
        }
        [Serializable] private sealed class Channel { public int stream, offset, format, dimension; }
        [Serializable] private sealed class Submesh
        {
            public int serializedVersion, firstByte, indexCount, topology, baseVertex, firstVertex, vertexCount;
            public AABB localAABB;
        }
        [Serializable] private sealed class AABB
        {
            public Vector3 m_Center, m_Extent;
            public Bounds Bounds => new Bounds(m_Center, m_Extent * 2);
        }
        [Serializable] private sealed class MatrixData
        {
            public float e00, e01, e02, e03, e10, e11, e12, e13, e20, e21, e22, e23, e30, e31, e32, e33;
            public Matrix4x4 Matrix => new Matrix4x4(new Vector4(e00, e10, e20, e30), new Vector4(e01, e11, e21, e31),
                new Vector4(e02, e12, e22, e32), new Vector4(e03, e13, e23, e33));
        }
    }
}
