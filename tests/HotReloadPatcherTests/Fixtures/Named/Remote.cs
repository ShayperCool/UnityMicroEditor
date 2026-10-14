// An assembly of the game that calls a method of Lib by SendMessage and references neither Lib
// nor anything of it: a call by name needs no reference (builder6-5).
using UnityEngine;

namespace Remote
{
    public class Shooter : MonoBehaviour
    {
        public Component hit;
        private void Update() { hit.SendMessage("Strike"); }
    }
}
