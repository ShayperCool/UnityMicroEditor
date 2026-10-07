using System;
using UnityEngine.LowLevel;

namespace UnityMicroEditor
{
    /// <summary>Edits a copy of the public PlayerLoop without replacing existing engine or package systems.</summary>
    public static class FrameLoop
    {
        public static bool AppendEndOfFrame(ref PlayerLoopSystem loop, Type marker, PlayerLoopSystem.UpdateFunction callback)
        {
            return AppendToStage(ref loop, typeof(UnityEngine.PlayerLoop.PostLateUpdate), marker, callback);
        }

        public static bool AppendToStage(ref PlayerLoopSystem loop, Type stage, Type marker, PlayerLoopSystem.UpdateFunction callback)
        {
            if (stage == null) throw new ArgumentNullException(nameof(stage));
            if (marker == null) throw new ArgumentNullException(nameof(marker));
            if (callback == null) throw new ArgumentNullException(nameof(callback));
            if (loop.type == stage)
            {
                PlayerLoopSystem[] previous = loop.subSystemList;
                int count = previous == null ? 0 : previous.Length;
                var systems = new PlayerLoopSystem[count + 1];
                if (count > 0) Array.Copy(previous, systems, count);
                systems[count] = new PlayerLoopSystem { type = marker, updateDelegate = callback };
                loop.subSystemList = systems;
                return true;
            }

            if (loop.subSystemList == null) return false;
            for (int index = 0; index < loop.subSystemList.Length; index++)
            {
                PlayerLoopSystem child = loop.subSystemList[index];
                if (!AppendToStage(ref child, stage, marker, callback)) continue;
                loop.subSystemList[index] = child;
                return true;
            }
            return false;
        }

        public static bool Remove(ref PlayerLoopSystem loop, Type marker)
        {
            if (marker == null) throw new ArgumentNullException(nameof(marker));
            PlayerLoopSystem[] previous = loop.subSystemList;
            if (previous == null) return false;

            bool changed = false;
            int retained = 0;
            for (int index = 0; index < previous.Length; index++)
            {
                if (previous[index].type == marker)
                {
                    changed = true;
                    continue;
                }
                PlayerLoopSystem child = previous[index];
                if (Remove(ref child, marker))
                {
                    previous[index] = child;
                    changed = true;
                }
                retained++;
            }
            if (retained != previous.Length)
            {
                var systems = new PlayerLoopSystem[retained];
                int destination = 0;
                for (int index = 0; index < previous.Length; index++)
                {
                    if (previous[index].type != marker) systems[destination++] = previous[index];
                }
                loop.subSystemList = systems;
            }
            return changed;
        }
    }
}
