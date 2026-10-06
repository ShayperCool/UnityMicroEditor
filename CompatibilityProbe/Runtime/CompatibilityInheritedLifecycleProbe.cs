using UnityEngine;

namespace UnityPlay.Compatibility
{
    public class CompatibilityInheritedLifecycleProbe : CompatibilityLifecycleProbe
    {
        [SerializeField] private int derivedRevision = 0;

        protected new void Awake()
        {
            Record("DerivedAwake.BeforeBase", derivedRevision);
            base.Awake();
            Record("DerivedAwake.AfterBase", derivedRevision);
        }
    }
}
