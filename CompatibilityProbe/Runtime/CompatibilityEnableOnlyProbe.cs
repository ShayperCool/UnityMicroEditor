using UnityEngine;

namespace UnityPlay.Compatibility
{
    [DefaultExecutionOrder(100)]
    public sealed class CompatibilityEnableOnlyProbe : MonoBehaviour
    {
        public bool wasEnabled;
        public bool wasStarted;

        private void OnEnable() { wasEnabled = true; }
        private void Start() { wasStarted = true; }
    }
}
