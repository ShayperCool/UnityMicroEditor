using UnityEngine;

namespace UnityPlay.Compatibility
{
    [DefaultExecutionOrder(-100)]
    public sealed class CompatibilityDynamicCreationProbe : MonoBehaviour
    {
        public CompatibilityDynamicallyCreatedProbe created;
        public bool childAwakeRanBeforeAddReturned;
        public bool childEnableRanBeforeAddReturned;

        private void Awake()
        {
            created = gameObject.AddComponent<CompatibilityDynamicallyCreatedProbe>();
            childAwakeRanBeforeAddReturned = created.awakeCalled;
            childEnableRanBeforeAddReturned = created.enableCalled;
        }
    }

    public sealed class CompatibilityDynamicallyCreatedProbe : MonoBehaviour
    {
        public bool awakeCalled;
        public bool enableCalled;

        private void Awake() { awakeCalled = true; }
        private void OnEnable() { enableCalled = true; }
    }
}
