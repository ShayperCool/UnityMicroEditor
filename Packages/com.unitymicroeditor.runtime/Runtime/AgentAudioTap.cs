using System;
using UnityEngine;
using UnityEngine.Scripting;

namespace UnityMicroEditor
{
    // Sits on the object of the scene's active AudioListener while a remote session streams.
    // Unity hands a component there the final mix of the Player's audio on the audio thread
    // and plays what the component leaves in the buffer, so the sound is copied, not changed.
    [Preserve]
    public sealed class AgentAudioTap : MonoBehaviour
    {
        internal volatile Action<float[], int> Sink;

        private void OnAudioFilterRead(float[] data, int channels) { Sink?.Invoke(data, channels); }
    }
}
