// Compiled against the real UnityEngine.CoreModule of the installation: the engine's own classes
// are recognised by name, without reading the module.
using UnityEngine;

namespace Real
{
    public class Mover : MonoBehaviour
    {
        public int ticks;
        private void Start() { ticks = 1; }
#if CASE_REAL_ADD
        private void Update() { ticks++; }
#endif
#if !CASE_REAL_REMOVE
        private void LateUpdate() { ticks += 2; }
#endif
    }
}
