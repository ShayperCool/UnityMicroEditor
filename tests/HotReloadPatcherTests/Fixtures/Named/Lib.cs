// An assembly whose added methods the code of another assembly names. NAMED_ADD adds them;
// NAMED_STRIKE adds one a sibling without a reference calls by SendMessage; NAMED_FIELD adds a
// field a class of another assembly inherits; NAMED_MESSAGE adds methods a package's message may
// mean.
using UnityEngine;

namespace Lib
{
    public class Target : MonoBehaviour
    {
        public int hp = 10;
#if NAMED_ADD
        private void TakeDamage() { hp -= 1; }
        // Two parameters: no call by a computed name reaches it.
        private void Hurt(int amount, int times) { hp -= amount * times; }
#endif
#if NAMED_STRIKE
        private void Strike() { hp -= 5; }
#endif
#if NAMED_FIELD
        public int armor;
#endif
#if NAMED_MESSAGE
        private void OnJump() { hp += 1; }
        private void Jump() { hp += 2; }
#endif
    }

    public class Commands
    {
        public int count;
#if NAMED_ADD
        public void Reset() { count = 0; }
#endif
    }

    // A base class whose Start calls a hook that the game overrides in another assembly.
    public abstract class Unit : MonoBehaviour
    {
        public int hp;
        private void Start() { OnSpawned(); }
        protected abstract void OnSpawned();
    }
}
