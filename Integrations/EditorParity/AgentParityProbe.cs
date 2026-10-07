using UnityEngine;

namespace AgentParity
{
    public sealed class AgentParityProbe : MonoBehaviour
    {
        public int revision = 1;
        public bool toggled = true;
        public Vector3 marker = new Vector3(1, 2, 3);
        public int awakeCount;
        public int enableCount;
        public int startCount;

        private void Awake() { awakeCount++; Debug.Log("[agent-parity] awake revision=" + revision); }
        private void OnEnable() { enableCount++; }
        private void Start() { startCount++; Debug.Log("[agent-parity] start revision=" + revision); }
    }
}
