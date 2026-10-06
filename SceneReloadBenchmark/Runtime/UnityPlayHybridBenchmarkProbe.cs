using System;
using System.Collections;
using System.Diagnostics;
using System.IO;
using UnityEngine;
using Debug = UnityEngine.Debug;

namespace Dungeon.CLIPlay.Benchmarks
{
    // Copied into a disposable SampleScene project, never into the authored game.
    [DefaultExecutionOrder(32000)]
    public sealed class UnityPlayHybridBenchmarkProbe : MonoBehaviour
    {
        [SerializeField] private Transform sceneTarget = null;
        [SerializeField] private DungeonShootingTarget dungeonTarget = null;
        [SerializeField] private AudioClip nativeAudio = null;
        [SerializeField] private Mesh nativeMesh = null;
        [SerializeField] private Material nativeMaterial = null;
        [SerializeField] private Texture2D nativeTexture = null;
        [SerializeField] private UnityPlaySceneReloadData data = null;
        [SerializeField] private int iterationRevision;
        [SerializeField] private GameObject[] addedObjects = null;

        private bool awakeReferencesRestored;

        private void Awake()
        {
            awakeReferencesRestored = sceneTarget != null && dungeonTarget != null
                && nativeAudio != null && nativeMesh != null && nativeMaterial != null
                && nativeTexture != null && data != null && addedObjects != null;
            if (!awakeReferencesRestored)
                throw new InvalidOperationException("Scene reload lost a serialized reference before Awake.");
            foreach (GameObject item in addedObjects)
                if (item == null || item.scene != gameObject.scene)
                    throw new InvalidOperationException("Added authored object was not restored before Awake.");
        }

        private IEnumerator Start()
        {
            // The proof observes actual scene values after a completed GPU frame.
            yield return new WaitForEndOfFrame();
            var names = new string[addedObjects.Length];
            for (int index = 0; index < names.Length; index++)
                names[index] = addedObjects[index].name;
            var proof = new Proof
            {
                schemaVersion = 1,
                pid = Process.GetCurrentProcess().Id,
                manifestRevision = Dungeon.CLIPlay.Raw.RawSceneLoader.CurrentRevision,
                iterationRevision = iterationRevision,
                sceneName = gameObject.scene.name,
                awakeReferencesRestored = awakeReferencesRestored,
                sceneReferencesRestored = sceneTarget.gameObject.scene == gameObject.scene
                    && dungeonTarget.gameObject.scene == gameObject.scene,
                sceneTargetLocalPosition = sceneTarget.localPosition,
                dungeonTargetAwakeObserved = Mathf.Abs(dungeonTarget.Health - dungeonTarget.maxHealth) < .0001f,
                addedObjectNames = names,
                dataRevision = data.Revision,
                nativeAudioEntityId = nativeAudio.GetEntityId().ToString(),
                nativeMeshEntityId = nativeMesh.GetEntityId().ToString(),
                nativeMaterialEntityId = nativeMaterial.GetEntityId().ToString(),
                nativeTextureEntityId = nativeTexture.GetEntityId().ToString(),
                observedUtcUnixMilliseconds = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                graphicsDeviceName = SystemInfo.graphicsDeviceName,
                graphicsDeviceType = SystemInfo.graphicsDeviceType.ToString(),
            };
            proof.success = proof.sceneName == "SampleScene" && proof.awakeReferencesRestored
                && proof.sceneReferencesRestored && proof.dungeonTargetAwakeObserved
                && nativeAudio.samples > 0 && nativeMesh.vertexCount > 0;
            string output = Environment.GetEnvironmentVariable("UNITY_PLAY_SCENE_RELOAD_PROOF");
            if (string.IsNullOrEmpty(output) || !Path.IsPathRooted(output))
                throw new InvalidOperationException("Scene reload benchmark requires an absolute proof path.");
            Directory.CreateDirectory(Path.GetDirectoryName(output));
            string json = JsonUtility.ToJson(proof);
            File.WriteAllText(output + ".tmp", json + "\n");
            if (File.Exists(output)) File.Delete(output);
            File.Move(output + ".tmp", output);
            Debug.Log("[unity-play-scene-reload-proof] " + json);
            if (!proof.success)
                throw new InvalidOperationException("Scene reload runtime proof failed.");
        }

        [Serializable] private sealed class Proof
        {
            public int schemaVersion;
            public bool success;
            public int pid;
            public string manifestRevision;
            public int iterationRevision;
            public string sceneName;
            public bool awakeReferencesRestored;
            public bool sceneReferencesRestored;
            public bool dungeonTargetAwakeObserved;
            public Vector3 sceneTargetLocalPosition;
            public string[] addedObjectNames;
            public int dataRevision;
            public string nativeAudioEntityId;
            public string nativeMeshEntityId;
            public string nativeMaterialEntityId;
            public string nativeTextureEntityId;
            public long observedUtcUnixMilliseconds;
            public string graphicsDeviceName;
            public string graphicsDeviceType;
        }
    }
}
