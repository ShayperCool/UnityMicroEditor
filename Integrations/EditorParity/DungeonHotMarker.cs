// Validation-only new-source discovery probe for an explicitly owned copy.
using Dungeon.CLIPlay;
using UnityEngine;

namespace AgentParity.Validation
{
    public static class DungeonHotMarker
    {
        [AgentTool("dungeon.hot_marker", Description = "Confirm a newly created script is compiled and its attributed tool executes in the live Dungeon Player.", ReadOnly = true)]
        public static DungeonHotMarkerResult Read()
        {
            return new DungeonHotMarkerResult {
                tag = "new-guid-sdk-hot-v1", frame = Time.frameCount,
                unityVersion = Application.unityVersion
            };
        }
    }

    public sealed class DungeonHotMarkerResult
    {
        public string tag, unityVersion;
        public int frame;
    }
}
