using System;
using System.Collections;
using System.IO;
using UnityEngine;

namespace Dungeon.CLIPlay.Benchmarks
{
    // Copied only into a disposable Dungeon source project by benchmark_hybrid.py.
    [DefaultExecutionOrder(32000)]
    public sealed class UnityPlayHybridBenchmarkProbe : MonoBehaviour
    {
        [SerializeField] private Transform sceneTarget = null;
        [SerializeField] private DungeonShootingTarget dungeonTarget = null;
        [SerializeField] private AudioClip nativeAudio = null;

        private static int CodeRevision() { return 1; } // UNITY_PLAY_BENCHMARK_CODE

        private IEnumerator Start()
        {
            if (sceneTarget == null || dungeonTarget == null || nativeAudio == null)
                throw new InvalidOperationException("Hybrid benchmark lost an authored scene/game/audio reference.");
            if (!nativeAudio.LoadAudioData())
                throw new InvalidOperationException("Hybrid benchmark could not load its native AudioClip data.");
            yield return null;
            yield return null;
            int remainingFrames = 90;
            while (nativeAudio.loadState == AudioDataLoadState.Loading && remainingFrames-- > 0)
                yield return null;
            if (nativeAudio.loadState != AudioDataLoadState.Loaded)
                throw new InvalidOperationException("Hybrid benchmark native AudioClip did not become readable.");
            var values = new float[Mathf.Min(1024, nativeAudio.samples) * nativeAudio.channels];
            if (values.Length == 0 || !nativeAudio.GetData(values, 0))
                throw new InvalidOperationException("Hybrid benchmark AudioClip.GetData failed. Use a decompressed importer asset.");
            uint fingerprint = 2166136261;
            double squareSum = 0;
            unchecked
            {
                foreach (float value in values)
                {
                    fingerprint = (fingerprint ^ (uint)Mathf.RoundToInt(value * 32768f)) * 16777619;
                    squareSum += value * value;
                }
            }
            var proof = new Proof
            {
                schemaVersion = 1,
                codeRevision = CodeRevision(),
                sceneName = gameObject.scene.name,
                sceneTargetName = sceneTarget.name,
                sceneTargetLocalPosition = sceneTarget.localPosition,
                sceneReferencesRestored = sceneTarget.gameObject.scene == gameObject.scene
                    && dungeonTarget.gameObject.scene == gameObject.scene,
                dungeonTargetName = dungeonTarget.name,
                dungeonTargetType = dungeonTarget.GetType().FullName,
                dungeonTargetAwakeObserved = Mathf.Abs(dungeonTarget.Health - dungeonTarget.maxHealth) < .0001f,
                audioName = nativeAudio.name,
                audioSamples = nativeAudio.samples,
                audioChannels = nativeAudio.channels,
                audioFrequency = nativeAudio.frequency,
                audioFingerprint = fingerprint.ToString("x8"),
                audioRms = Math.Sqrt(squareSum / values.Length),
                audioReadValues = values.Length,
                observedUtcUnixMilliseconds = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
            };
            proof.success = proof.sceneName == "SampleScene" && proof.sceneReferencesRestored
                && proof.dungeonTargetAwakeObserved && proof.audioSamples > 0 && proof.audioRms > 1e-8;
            string output = Environment.GetEnvironmentVariable("UNITY_PLAY_HYBRID_BENCHMARK_PROOF");
            if (string.IsNullOrEmpty(output) || !Path.IsPathRooted(output))
                throw new InvalidOperationException("Hybrid benchmark requires an absolute proof output path.");
            Directory.CreateDirectory(Path.GetDirectoryName(output));
            string json = JsonUtility.ToJson(proof);
            File.WriteAllText(output, json + "\n");
            Debug.Log("[unity-play-hybrid-benchmark-proof] " + json);
            if (!proof.success)
                throw new InvalidOperationException("Hybrid benchmark scene/game/audio proof failed.");
        }

        [Serializable] private sealed class Proof
        {
            public int schemaVersion;
            public bool success;
            public int codeRevision;
            public string sceneName;
            public string sceneTargetName;
            public Vector3 sceneTargetLocalPosition;
            public bool sceneReferencesRestored;
            public string dungeonTargetName;
            public string dungeonTargetType;
            public bool dungeonTargetAwakeObserved;
            public string audioName;
            public int audioSamples;
            public int audioChannels;
            public int audioFrequency;
            public string audioFingerprint;
            public double audioRms;
            public int audioReadValues;
            public long observedUtcUnixMilliseconds;
        }
    }
}
