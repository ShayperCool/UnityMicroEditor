using System;
using System.Collections;
using System.IO;
using UnityEngine;

namespace UnityPlay.Hybrid
{
    [DefaultExecutionOrder(32000)]
    public sealed class HybridProbeDriver : MonoBehaviour
    {
        [SerializeField] private HybridCloneNode prefab;
        [SerializeField] private bool nativeExpected;

        private IEnumerator Start()
        {
            if (prefab == null) throw new InvalidOperationException("Hybrid probe prefab reference was not restored.");
            HybridCloneNode componentClone = Instantiate(prefab);
            GameObject objectClone = Instantiate(prefab.gameObject);
            if (!objectClone.TryGetComponent<HybridCloneNode>(out var objectNode))
                throw new InvalidOperationException("Hybrid GameObject clone lost its authored component.");
            bool sourceRootsMatch = prefab.transform.root == prefab.transform && prefab.Peer.transform.root == prefab.transform;
            bool sourceChildrenMatch = prefab.GetComponentInChildren<Animator>() == prefab.Animator
                && prefab.gameObject.GetComponentInChildren(typeof(Animator)) == prefab.Animator;
            prefab.Peer.gameObject.SetActive(false);
            bool sourceInactiveChildrenMatch = prefab.GetComponentInChildren<Animator>() == null
                && prefab.gameObject.GetComponentInChildren(typeof(Animator), true) == prefab.Animator;
            prefab.Peer.gameObject.SetActive(true);
            yield return null;
            yield return null;
            var proof = new Proof
            {
                schemaVersion = 1,
                nativeExpected = nativeExpected,
                sourceHasTemplateParent = prefab.transform.parent != null,
                sourceActiveSelf = prefab.gameObject.activeSelf,
                sourceActiveInHierarchy = prefab.gameObject.activeInHierarchy,
                sourceAwakeCount = prefab.awakeCount + prefab.Peer.awakeCount,
                sourceRootsMatch = sourceRootsMatch,
                sourceChildrenMatch = sourceChildrenMatch,
                sourceInactiveChildrenMatch = sourceInactiveChildrenMatch,
                cloneRootsMatch = componentClone.Peer.transform.root == componentClone.transform
                    && objectNode.Peer.transform.root == objectNode.transform,
                clonesAreParentless = componentClone.transform.parent == null && objectNode.transform.parent == null,
                clonesAreInLoadedScene = componentClone.gameObject.scene == gameObject.scene && objectNode.gameObject.scene == gameObject.scene,
                clonesAreActive = componentClone.gameObject.activeSelf && componentClone.gameObject.activeInHierarchy
                    && objectNode.gameObject.activeSelf && objectNode.gameObject.activeInHierarchy,
                cyclicReferencesRewired = componentClone.Peer.Peer == componentClone && objectNode.Peer.Peer == objectNode
                    && componentClone.Peer != prefab.Peer && objectNode.Peer != prefab.Peer,
                callbacks = CallbackCount(componentClone) + CallbackCount(objectNode),
                awakeGraphReady = componentClone.awakeReferencesReady && componentClone.Peer.awakeReferencesReady
                    && objectNode.awakeReferencesReady && objectNode.Peer.awakeReferencesReady,
                nativeMeshShared = componentClone.Mesh.sharedMesh == prefab.Mesh.sharedMesh && objectNode.Mesh.sharedMesh == prefab.Mesh.sharedMesh,
                meshVertices = prefab.Mesh.sharedMesh.vertexCount,
                nativeAudioShared = componentClone.Audio.clip == prefab.Audio.clip && objectNode.Audio.clip == prefab.Audio.clip,
                nativeControllerShared = componentClone.Animator.runtimeAnimatorController == prefab.Animator.runtimeAnimatorController
                    && objectNode.Animator.runtimeAnimatorController == prefab.Animator.runtimeAnimatorController,
                audioSamples = prefab.Audio.clip == null ? 0 : prefab.Audio.clip.samples,
                controllerClips = prefab.Animator.runtimeAnimatorController == null ? 0 : prefab.Animator.runtimeAnimatorController.animationClips.Length,
                curveObservationsChecked = CheckNativeCurveSerialization(),
            };
            proof.success = proof.sourceActiveSelf && !proof.sourceActiveInHierarchy && proof.sourceAwakeCount == 0
                && proof.sourceRootsMatch && proof.sourceChildrenMatch && proof.sourceInactiveChildrenMatch && proof.cloneRootsMatch
                && proof.clonesAreParentless && proof.clonesAreInLoadedScene && proof.clonesAreActive
                && proof.cyclicReferencesRewired && proof.callbacks == 12 && proof.awakeGraphReady && proof.nativeMeshShared
                && proof.nativeAudioShared && proof.nativeControllerShared
                && (!nativeExpected || proof.audioSamples > 0 && proof.controllerClips > 0);
            string output = Environment.GetEnvironmentVariable("UNITY_PLAY_HYBRID_PROOF");
            string json = JsonUtility.ToJson(proof);
            Debug.Log("[unity-play-hybrid-proof] " + json);
            if (!string.IsNullOrEmpty(output))
            {
                if (!Path.IsPathRooted(output)) throw new InvalidOperationException("Hybrid proof output must be absolute.");
                Directory.CreateDirectory(Path.GetDirectoryName(output));
                File.WriteAllText(output, json + "\n");
            }
            if (!proof.success) throw new InvalidOperationException("Hybrid prefab compatibility probe failed; inspect its proof JSON.");
        }

        private static int CallbackCount(HybridCloneNode root)
        { return root.awakeCount + root.enableCount + root.startCount + root.Peer.awakeCount + root.Peer.enableCount + root.Peer.startCount; }

        private static int CheckNativeCurveSerialization()
        {
            string path = Environment.GetEnvironmentVariable("UNITY_PLAY_CURVE_OBSERVATIONS");
            if (string.IsNullOrEmpty(path)) return 0;
            if (!Path.IsPathRooted(path)) throw new InvalidOperationException("Native curve observations path must be absolute.");
            var observations = JsonUtility.FromJson<CurveObservations>(File.ReadAllText(path));
            if (observations == null || observations.curveWrapModes == null || observations.curveWrapModes.Length == 0)
                throw new InvalidOperationException("Native curve observations are missing.");
            foreach (var observation in observations.curveWrapModes)
            {
                // EditorJsonUtility adds the native object wrapper; the curve
                // payload itself is decoded by Unity's public JSON serializer.
                var wrapped = JsonUtility.FromJson<EditorCurveRoot>(observation.serializedJson);
                CurveCarrier carrier = wrapped == null ? null : wrapped.MonoBehaviour;
                if (carrier == null) carrier = JsonUtility.FromJson<CurveCarrier>(observation.serializedJson);
                AnimationCurve curve = carrier == null ? null : carrier.curve;
                if (curve == null || curve.length != 2 || Mathf.Abs(curve.Evaluate(.5f) - .5f) > .0001f
                    || (int)curve.preWrapMode != observation.actualPublicPreWrapMode
                    || (int)curve.postWrapMode != observation.actualPublicPostWrapMode)
                    throw new InvalidOperationException("Public AnimationCurve JSON deserialization differs from the native importer observation: " + observation.requestedName);
            }
            return observations.curveWrapModes.Length;
        }

        [Serializable] private sealed class CurveCarrier { public AnimationCurve curve; }
        [Serializable] private sealed class EditorCurveRoot { public CurveCarrier MonoBehaviour; }
        [Serializable] private sealed class CurveObservations { public CurveObservation[] curveWrapModes; }
        [Serializable] private sealed class CurveObservation
        {
            public string requestedName;
            public int actualPublicPreWrapMode;
            public int actualPublicPostWrapMode;
            public string serializedJson;
        }

        [Serializable] private sealed class Proof
        {
            public int schemaVersion;
            public bool success;
            public bool nativeExpected;
            public bool sourceHasTemplateParent;
            public bool sourceActiveSelf;
            public bool sourceActiveInHierarchy;
            public int sourceAwakeCount;
            public bool sourceRootsMatch;
            public bool sourceChildrenMatch;
            public bool sourceInactiveChildrenMatch;
            public bool cloneRootsMatch;
            public bool clonesAreParentless;
            public bool clonesAreInLoadedScene;
            public bool clonesAreActive;
            public bool cyclicReferencesRewired;
            public int callbacks;
            public bool awakeGraphReady;
            public bool nativeMeshShared;
            public int meshVertices;
            public bool nativeAudioShared;
            public bool nativeControllerShared;
            public int audioSamples;
            public int controllerClips;
            public int curveObservationsChecked;
        }
    }
}
