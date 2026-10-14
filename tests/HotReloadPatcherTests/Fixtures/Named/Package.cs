// A package of the engine (its name starts with Unity.) that sends a message by a name it makes,
// as an input component sends On<Action> (builder6-7).
using UnityEngine;

namespace Unity.Messages
{
    public class Sender : MonoBehaviour
    {
        public string action = "Jump";
        public Component receiver;
        private void Update() { receiver.SendMessage("On" + action); }
    }
}
