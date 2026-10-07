using System;
using System.IO;
using System.Security.Cryptography;
using UnityEngine;

namespace UnityPlay.WorkerProbe
{
    // References are authored in the raw fixture and populated by the production
    // loader. Start runs after the loader's ordinary scene activation.
    public sealed class NativeContentProductionProof : MonoBehaviour
    {
        public MeshFilter meshFilter;
        public MeshRenderer meshRenderer;
        public AudioSource audioSource;

        private void Start()
        {
            var report = new ProofReport { unityVersion = Application.unityVersion };
            try
            {
                if (meshFilter == null || audioSource == null)
                    throw new InvalidDataException("Raw scene component references were not restored.");
                Mesh mesh = meshFilter.sharedMesh;
                AudioClip clip = audioSource.clip;
                if (mesh == null || clip == null) throw new InvalidDataException("Native Mesh/AudioClip references were not restored.");
                report.meshVertices = mesh.vertexCount;
                report.meshIndices = checked((int)mesh.GetIndexCount(0));
                report.meshSubMeshes = mesh.subMeshCount;
                report.materialRestored = meshRenderer != null && meshRenderer.sharedMaterial != null;
                report.audioSamples = clip.samples;
                report.audioChannels = clip.channels;
                report.audioFrequency = clip.frequency;
                report.audioLoadState = clip.loadState.ToString();
                report.audioLoadType = clip.loadType.ToString();
                if (report.meshVertices != 24 || report.meshIndices != 36 || report.meshSubMeshes != 1)
                    throw new InvalidDataException("The real Dungeon R_Floor mesh differs from its importer oracle.");
                if (clip.loadState != AudioDataLoadState.Loaded)
                    throw new InvalidDataException("Native audio resource was not loaded; GetData is intentionally skipped.");
                if (clip.loadType == AudioClipLoadType.DecompressOnLoad)
                {
                    var pcm = new float[checked(clip.samples * clip.channels)];
                    report.audioGetData = clip.GetData(pcm, 0);
                    if (!report.audioGetData) throw new InvalidDataException("Native audio GetData failed.");
                    var bytes = new byte[checked(sizeof(int) + pcm.Length * sizeof(float))];
                    Buffer.BlockCopy(BitConverter.GetBytes(pcm.Length), 0, bytes, 0, sizeof(int));
                    Buffer.BlockCopy(pcm, 0, bytes, sizeof(int), bytes.Length - sizeof(int));
                    using (var hash = SHA256.Create()) report.audioPCMHash = BitConverter.ToString(hash.ComputeHash(bytes)).Replace("-", "").ToLowerInvariant();
                    if (report.audioPCMHash != "ad4d61350a59160ef1b86a1c6cf0dfc6f496872863076d68886e61170ec32f44")
                        throw new InvalidDataException("PCM differs from the Editor importer oracle.");
                }
                else
                {
                    if (clip.loadType != AudioClipLoadType.CompressedInMemory || clip.samples != 44100 || clip.channels != 1 || clip.frequency != 44100)
                        throw new InvalidDataException("The real Dungeon door_crank_loop native audio identity differs from its importer output.");
                    audioSource.Play();
                    report.audioPlayRequested = true;
                    report.audioIsPlaying = audioSource.isPlaying;
                }
                report.success = true;
            }
            catch (Exception error) { report.error = error.ToString(); Debug.LogError("[native-content-production-proof] " + report.error); }
            string path = Environment.GetEnvironmentVariable("UNITY_PLAY_NATIVE_CONTENT_PROOF_REPORT");
            if (string.IsNullOrEmpty(path) || !Path.IsPathRooted(path)) throw new InvalidDataException("Production proof report must be an absolute environment path.");
            File.WriteAllText(path, JsonUtility.ToJson(report, true));
        }

        [Serializable]
        private sealed class ProofReport
        {
            public bool success;
            public string unityVersion, error;
            public int meshVertices, meshIndices, meshSubMeshes;
            public bool materialRestored;
            public int audioSamples, audioChannels, audioFrequency;
            public string audioLoadState, audioLoadType, audioPCMHash;
            public bool audioGetData, audioPlayRequested, audioIsPlaying;
        }
    }
}
