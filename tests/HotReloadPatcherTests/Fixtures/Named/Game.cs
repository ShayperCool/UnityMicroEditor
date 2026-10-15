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

    // Unity writes the fields Target declares into the documents of this class too.
    public class Boss : Lib.Target { }

    // The hook Lib.Unit's Start calls, once per object.
    public class Orc : Lib.Unit
    {
#if NAMED_ORC
        protected override void OnSpawned() { hp = 40; }
#else
        protected override void OnSpawned() { hp = 10; }
#endif
    }
}
