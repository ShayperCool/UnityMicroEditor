// The game's other assembly. NAMED_CALL names the methods Lib adds by a string; NAMED_COMPUTED
// calls methods of its objects by a name it computes.
using UnityEngine;

namespace Game
{
    public class Shooter : MonoBehaviour
    {
        public Lib.Target target;
        public Lib.Commands commands;
        public string kind = "Pinged";
        private void Update()
        {
#if NAMED_CALL
            target.SendMessage("TakeDamage");
            typeof(Lib.Commands).GetMethod("Reset");
#elif NAMED_COMPUTED
            target.SendMessage("On" + kind);
#else
            target.hp--;
#endif
        }
    }
}
