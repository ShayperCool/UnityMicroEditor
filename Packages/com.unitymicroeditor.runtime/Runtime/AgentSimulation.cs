using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.LowLevel;

namespace Dungeon.CLIPlay
{
    /// <summary>Gates game PlayerLoop stages while the bridge and renderer continue to run.</summary>
    internal sealed class AgentSimulation : IDisposable
    {
        private sealed class UpdateMarker { }
        private sealed class FixedMarker { }
        private sealed class LateMarker { }
        private readonly Type[] stageTypes = { typeof(UnityEngine.PlayerLoop.FixedUpdate), typeof(UnityEngine.PlayerLoop.PreUpdate),
            typeof(UnityEngine.PlayerLoop.Update), typeof(UnityEngine.PlayerLoop.PreLateUpdate) };
        private readonly Dictionary<Type, PlayerLoopSystem> stages = new Dictionary<Type, PlayerLoopSystem>();
        private bool installed, paused, stepping;
        private float previousTimeScale, previousCaptureDeltaTime;
        private bool previousAudioPause;
        private long updates, fixedTicks, lateUpdates, completedSteps;
        private long startUpdates, startFixedTicks, startLateUpdates;
        private int startFrame;
        internal bool Paused => paused;
        internal bool Stepping => stepping;
        internal long CompletedSteps => completedSteps;
        internal string LastStepError { get; private set; }

        internal void Pause()
        {
            if (stepping) throw new InvalidOperationException("A simulation step is still running.");
            EnsureInstalled();
            if (paused) return;
            previousTimeScale = Time.timeScale; previousCaptureDeltaTime = Time.captureDeltaTime; previousAudioPause = AudioListener.pause;
            Time.timeScale = 0; AudioListener.pause = true; SetEnabled(false); paused = true;
        }

        internal void Resume()
        {
            if (stepping) throw new InvalidOperationException("A simulation step is still running.");
            if (!paused) return;
            SetEnabled(true); Time.timeScale = previousTimeScale; Time.captureDeltaTime = previousCaptureDeltaTime;
            AudioListener.pause = previousAudioPause; paused = false;
        }

        internal void Step()
        {
            if (!paused || stepping) throw new InvalidOperationException("A single simulation step requires an idle paused simulation.");
            if (Time.fixedDeltaTime <= 0 || Time.fixedDeltaTime > Time.maximumDeltaTime)
                throw new InvalidDataException("fixedDeltaTime must be positive and no greater than maximumDeltaTime for one fixed tick.");
            startUpdates = updates; startFixedTicks = fixedTicks; startLateUpdates = lateUpdates; startFrame = Time.frameCount;
            LastStepError = null; Time.captureDeltaTime = Time.fixedDeltaTime; Time.timeScale = 1;
            AudioListener.pause = previousAudioPause; SetEnabled(true); stepping = true;
        }

        // Called by the bridge at the end of every live/render frame, before it
        // accepts new requests. SetPlayerLoop applies on the next full iteration.
        internal bool CompleteAtEndOfFrame()
        {
            if (!stepping || Time.frameCount <= startFrame) return false;
            SetEnabled(false); Time.timeScale = 0; Time.captureDeltaTime = previousCaptureDeltaTime; AudioListener.pause = true;
            stepping = false; completedSteps++;
            if (updates - startUpdates != 1 || lateUpdates - startLateUpdates != 1 || fixedTicks - startFixedTicks != 1)
                LastStepError = "Single-step invariant failed: update=" + (updates - startUpdates) + ", fixed=" + (fixedTicks - startFixedTicks) + ", late=" + (lateUpdates - startLateUpdates) + ".";
            return true;
        }

        internal Dictionary<string, object> Status()
        {
            var gated = new List<object>(); foreach (Type type in stageTypes) gated.Add(type.FullName);
            return new Dictionary<string, object> { ["paused"] = paused, ["stepping"] = stepping,
                ["semantics"] = "gated_game_player_loop", ["stepSemantics"] = "one_update_one_fixed_tick_one_late_update",
                ["bridgeAndRenderFramesContinue"] = true, ["nativeFrameCountAdvances"] = true,
                ["gameUpdateFrames"] = updates, ["gameFixedTicks"] = fixedTicks, ["gameLateFrames"] = lateUpdates,
                ["completedSteps"] = completedSteps, ["gatedStages"] = gated,
                ["fixedDeltaTime"] = Time.fixedDeltaTime, ["stepError"] = LastStepError,
                ["audioListenerPaused"] = AudioListener.pause };
        }

        private void EnsureInstalled()
        {
            if (installed) return;
            PlayerLoopSystem loop = PlayerLoop.GetCurrentPlayerLoop();
            if (!FrameLoop.AppendToStage(ref loop, typeof(UnityEngine.PlayerLoop.Update), typeof(UpdateMarker), () => updates++)
                || !FrameLoop.AppendToStage(ref loop, typeof(UnityEngine.PlayerLoop.FixedUpdate), typeof(FixedMarker), () => fixedTicks++)
                || !FrameLoop.AppendToStage(ref loop, typeof(UnityEngine.PlayerLoop.PreLateUpdate), typeof(LateMarker), () => lateUpdates++))
                throw new InvalidOperationException("The current PlayerLoop lacks required game stages.");
            if (loop.subSystemList == null) throw new InvalidOperationException("The current PlayerLoop has no root stages.");
            foreach (PlayerLoopSystem stage in loop.subSystemList)
                foreach (Type expected in stageTypes) if (stage.type == expected) stages.Add(expected, stage);
            if (stages.Count != stageTypes.Length) { stages.Clear(); throw new InvalidOperationException("The current PlayerLoop profile lacks the required simulation stages."); }
            PlayerLoop.SetPlayerLoop(loop); installed = true;
        }

        private void SetEnabled(bool enabled)
        {
            PlayerLoopSystem loop = PlayerLoop.GetCurrentPlayerLoop();
            var children = new List<PlayerLoopSystem>(loop.subSystemList ?? Array.Empty<PlayerLoopSystem>());
            children.RemoveAll(value => stages.ContainsKey(value.type));
            if (enabled)
            {
                int beforePostLate = children.FindIndex(value => value.type == typeof(UnityEngine.PlayerLoop.PostLateUpdate));
                if (beforePostLate < 0) throw new InvalidOperationException("The bridge/render PlayerLoop stage was replaced.");
                foreach (Type type in stageTypes) children.Insert(beforePostLate++, stages[type]);
            }
            loop.subSystemList = children.ToArray(); PlayerLoop.SetPlayerLoop(loop);
        }

        public void Dispose()
        {
            if (!installed) return;
            if (paused)
            {
                stepping = false; SetEnabled(true); Time.timeScale = previousTimeScale; Time.captureDeltaTime = previousCaptureDeltaTime;
                AudioListener.pause = previousAudioPause; paused = false;
            }
            PlayerLoopSystem loop = PlayerLoop.GetCurrentPlayerLoop();
            bool changed = FrameLoop.Remove(ref loop, typeof(UpdateMarker));
            changed = FrameLoop.Remove(ref loop, typeof(FixedMarker)) || changed;
            changed = FrameLoop.Remove(ref loop, typeof(LateMarker)) || changed;
            if (changed) PlayerLoop.SetPlayerLoop(loop); installed = false;
        }
    }
}
