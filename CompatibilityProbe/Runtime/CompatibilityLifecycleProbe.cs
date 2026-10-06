using System;
using System.Collections.Generic;
using System.Text;
using UnityEngine;
using Object = UnityEngine.Object;

namespace UnityPlay.Compatibility
{
    /// <summary>Records what authored components can observe during lifecycle callbacks.</summary>
    public class CompatibilityLifecycleProbe : MonoBehaviour
    {
        public enum AwakeOperation
        {
            None = 0,
            DisablePeerBehaviour = 1,
            DeactivatePeerRoot = 2,
            DestroyPeerRoot = 3,
            DestroyPeerRootImmediately = 4,
            ActivatePeerRoot = 5,
        }

        [SerializeField] private GameObject peerRoot = null;
        [SerializeField] private Behaviour peerBehaviour = null;
        [SerializeField] private Transform anchor = null;
        [SerializeField] private Object asset = null;
        [SerializeField] private Object[] references = null;
        [SerializeField] private int revision = 0;
        [SerializeField] private bool flag = false;
        [SerializeField] private float scalar = 0;
        [SerializeField] private AwakeOperation awakeOperation = AwakeOperation.None;
        [SerializeField] private bool reactivatePeerOnStart = false;

        private static readonly List<Event> recorded = new List<Event>(64);
        public static IReadOnlyList<Event> Events => recorded;
        public static Event[] CaptureEvents() { return recorded.ToArray(); }
        public static void ResetEvents() { recorded.Clear(); }

        protected void Awake()
        {
            Record("Awake");
            switch (awakeOperation)
            {
                case AwakeOperation.None: return;
                case AwakeOperation.DisablePeerBehaviour:
                    if (peerBehaviour == null) throw new InvalidOperationException("DisablePeerBehaviour requires an authored peerBehaviour.");
                    peerBehaviour.enabled = false;
                    break;
                case AwakeOperation.DeactivatePeerRoot:
                    if (peerRoot == null) throw new InvalidOperationException("DeactivatePeerRoot requires an authored peerRoot.");
                    peerRoot.SetActive(false);
                    break;
                case AwakeOperation.DestroyPeerRoot:
                    if (peerRoot == null) throw new InvalidOperationException("DestroyPeerRoot requires an authored peerRoot.");
                    Destroy(peerRoot);
                    break;
                case AwakeOperation.DestroyPeerRootImmediately:
                    if (peerRoot == null) throw new InvalidOperationException("DestroyPeerRootImmediately requires an authored peerRoot.");
                    DestroyImmediate(peerRoot);
                    break;
                case AwakeOperation.ActivatePeerRoot:
                    if (peerRoot == null) throw new InvalidOperationException("ActivatePeerRoot requires an authored peerRoot.");
                    peerRoot.SetActive(true);
                    break;
                default: throw new InvalidOperationException("Unknown authored AwakeOperation.");
            }
            Record("Awake.After" + awakeOperation);
        }
        protected void OnEnable() { Record("OnEnable"); }
        protected void Start()
        {
            Record("Start");
            if (!reactivatePeerOnStart) return;
            if (peerRoot == null) throw new InvalidOperationException("Reactivation requires an authored peerRoot.");
            peerRoot.SetActive(true);
            Record("Start.AfterReactivatePeer");
        }
        protected void OnDisable() { Record("OnDisable"); }
        protected void OnDestroy() { Record("OnDestroy"); }

        protected void Record(string phase, int derivedRevision = 0)
        {
            if (recorded.Count >= 256) throw new InvalidOperationException("Compatibility lifecycle event limit exceeded.");
            var names = new string[references == null ? 0 : references.Length];
            for (int index = 0; index < names.Length; index++)
                names[index] = references[index] != null ? references[index].GetType().FullName + ":" + references[index].name : "null";
            recorded.Add(new Event
            {
                phase = phase, objectPath = ObjectPath(transform), objectName = name,
                activeSelf = gameObject.activeSelf, activeInHierarchy = gameObject.activeInHierarchy, enabled = enabled,
                isActiveAndEnabled = isActiveAndEnabled,
                parentPath = transform.parent != null ? ObjectPath(transform.parent) : "",
                peerRootPath = peerRoot != null ? ObjectPath(peerRoot.transform) : "",
                peerRootActiveSelf = peerRoot != null && peerRoot.activeSelf,
                peerRootActiveInHierarchy = peerRoot != null && peerRoot.activeInHierarchy,
                peerRootExists = peerRoot != null,
                peerBehaviourExists = peerBehaviour != null,
                peerBehaviourEnabled = peerBehaviour != null && peerBehaviour.enabled,
                peerBehaviourIsActiveAndEnabled = peerBehaviour != null && peerBehaviour.isActiveAndEnabled,
                anchorPath = anchor != null ? ObjectPath(anchor) : "",
                assetName = asset != null ? asset.name : "", assetType = asset != null ? asset.GetType().FullName : "",
                referenceNames = names, revision = revision, flag = flag, scalar = scalar, frameCount = Time.frameCount,
                derivedRevision = derivedRevision, operation = (int)awakeOperation,
            });
        }

        private static string ObjectPath(Transform transform)
        {
            var ancestors = new List<Transform>();
            for (Transform current = transform; current != null; current = current.parent) ancestors.Add(current);
            var path = new StringBuilder();
            for (int index = ancestors.Count - 1; index >= 0; index--)
            {
                if (path.Length != 0) path.Append('/');
                path.Append(ancestors[index].name);
            }
            return path.ToString();
        }

        [Serializable]
        public sealed class Event
        {
            public string phase;
            public string objectPath;
            public string objectName;
            public bool activeSelf;
            public bool activeInHierarchy;
            public bool enabled;
            public bool isActiveAndEnabled;
            public string parentPath;
            public string peerRootPath;
            public bool peerRootActiveSelf;
            public bool peerRootActiveInHierarchy;
            public bool peerRootExists;
            public bool peerBehaviourExists;
            public bool peerBehaviourEnabled;
            public bool peerBehaviourIsActiveAndEnabled;
            public string anchorPath;
            public string assetName;
            public string assetType;
            public string[] referenceNames;
            public int revision;
            public bool flag;
            public float scalar;
            public int frameCount;
            public int derivedRevision;
            public int operation;
        }
    }
}
