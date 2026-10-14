// An assembly whose added methods the code of another assembly names. NAMED_ADD adds them.
using UnityEngine;

namespace Lib
{
    public class Target : MonoBehaviour
    {
        public int hp = 10;
#if NAMED_ADD
        private void TakeDamage() { hp -= 1; }
#endif
    }

    public class Commands
    {
        public int count;
#if NAMED_ADD
        public void Reset() { count = 0; }
#endif
    }
}
