using System;
using UnityEngine;

namespace UnityPlay.Hybrid
{
    public sealed class HybridCloneNode : MonoBehaviour
    {
        [SerializeField] private HybridCloneNode peer;
        [SerializeField] private MeshFilter mesh;
        [SerializeField] private AudioSource audio;
        [SerializeField] private Animator animator;
        [SerializeField] private int revision;
        [SerializeField] private bool nativeExpected;
        [NonSerialized] public int awakeCount;
        [NonSerialized] public int enableCount;
        [NonSerialized] public int startCount;
        [NonSerialized] public bool awakeReferencesReady;
        public HybridCloneNode Peer { get { return peer; } }
        public MeshFilter Mesh { get { return mesh; } }
        public AudioSource Audio { get { return audio; } }
        public Animator Animator { get { return animator; } }

        private void Awake()
        {
            awakeCount++;
            awakeReferencesReady = revision == 17 && peer != null && peer.peer == this
                && mesh != null && mesh.sharedMesh != null && audio != null && animator != null
                && (!nativeExpected || audio.clip != null && animator.runtimeAnimatorController != null);
            if (!awakeReferencesReady) throw new InvalidOperationException("Hybrid prefab clone Awake observed an incomplete serialized graph.");
        }
        private void OnEnable() { enableCount++; }
        private void Start() { startCount++; }
    }
}
