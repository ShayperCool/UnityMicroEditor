using System;
using System.Collections.Generic;
using System.Threading;
using UnityMicroEditor;
using UnityEngine;
using UnityEngine.EventSystems;
using UnityEngine.InputSystem;

namespace AgentParity
{
    public sealed class AdvancedAgentProbe : MonoBehaviour, IPointerDownHandler, IPointerUpHandler, IPointerClickHandler
    {
        public int value = 5;
        public int updates, fixedUpdates, lateUpdates, keyPresses, mousePresses;
        public int pointerDowns, pointerUps, clicks, toolCalls;
        private static AdvancedAgentProbe current;
        private static int mainThread, dtoConstructors;

        public enum Mode { Normal, Alternate }
        [Serializable] public sealed class Change
        {
            [AgentToolArgument(Minimum = 1, Maximum = 10)] public int amount;
            [AgentToolArgument(MinLength = 1, MaxLength = 32)] public string message;
            [AgentToolArgument(Required = false)] public Mode mode;
            public Change() { dtoConstructors++; }
        }
        [Serializable] public sealed class Counts
        {
            public int value, updates, fixedUpdates, lateUpdates, keyPresses, mousePresses;
            public int pointerDowns, pointerUps, clicks, toolCalls, dtoConstructors;
            public bool sameMainThread, persistent;
        }

        private void Awake() { current = this; mainThread = Thread.CurrentThread.ManagedThreadId; dtoConstructors = 0; }
        private void OnDestroy() { if (current == this) current = null; }
        private void Update()
        {
            updates++;
            if (Keyboard.current != null && Keyboard.current.aKey.wasPressedThisFrame) keyPresses++;
            if (Mouse.current != null && Mouse.current.leftButton.wasPressedThisFrame) mousePresses++;
        }
        private void FixedUpdate() { fixedUpdates++; }
        private void LateUpdate() { lateUpdates++; }
        public void OnPointerDown(PointerEventData data) { pointerDowns++; }
        public void OnPointerUp(PointerEventData data) { pointerUps++; }
        public void OnPointerClick(PointerEventData data) { clicks++; }

        [AgentTool("advanced.read", Description = "Read actual game callbacks and input handler counters.", ReadOnly = true)]
        public static Counts Read() { return Capture(current); }

        [AgentTool("advanced.reset", Description = "Reset fixture values and callback counters at a paused comparison checkpoint.")]
        public static Counts ResetCheckpoint()
        {
            if (current == null) throw new InvalidOperationException("The fixture has no live probe.");
            current.value = 5;
            current.updates = current.fixedUpdates = current.lateUpdates = 0;
            current.keyPresses = current.mousePresses = 0;
            current.pointerDowns = current.pointerUps = current.clicks = 0;
            current.toolCalls = 0; dtoConstructors = 0;
            return Capture(current);
        }

        [AgentTool("advanced.bump", Description = "Increase the fixture's value with validated DTO arguments.")]
        public static Counts Bump(Change change) { current.value += change.amount; current.toolCalls++; return Capture(current); }

        [AgentTool("advanced.instance", Description = "Invoke the explicitly selected live component.")]
        public Counts BumpInstance(Change change) { value += change.amount; toolCalls++; return Capture(this); }

        [AgentTool("advanced.persist", Description = "Move the live fixture Canvas root into DontDestroyOnLoad for persistent receiver assertions.")]
        public Counts PersistRoot()
        { UnityEngine.Object.DontDestroyOnLoad(transform.root.gameObject); return Capture(this); }

        [AgentTool("advanced.throw", Description = "Prove execution failures retain their observable effect.")]
        public static Counts ThrowAfterChange() { current.toolCalls++; throw new InvalidOperationException("intentional AdvancedParity execution failure"); }

        [AgentTool("advanced.invalid_output")]
        public static Dictionary<string, object> InvalidOutput()
        { current.toolCalls++; return new Dictionary<string, object> { ["foreignObject"] = current }; }

        [AgentTool("advanced.collision")] public static Counts CollisionOne() { return Capture(current); }
        [AgentTool("advanced.collision")] public static Counts CollisionTwo() { return Capture(current); }
        [AgentTool("advanced.unsupported")] public static Counts Unsupported(Type type) { return Capture(current); }
        [AgentTool("advanced.private_hidden")] private static Counts Hidden() { return Capture(current); }

        private static Counts Capture(AdvancedAgentProbe value)
        {
            if (value == null) throw new InvalidOperationException("The fixture has no live probe.");
            return new Counts { value = value.value, updates = value.updates, fixedUpdates = value.fixedUpdates,
                lateUpdates = value.lateUpdates, keyPresses = value.keyPresses, mousePresses = value.mousePresses,
                pointerDowns = value.pointerDowns, pointerUps = value.pointerUps, clicks = value.clicks,
                toolCalls = value.toolCalls, dtoConstructors = dtoConstructors,
                sameMainThread = Thread.CurrentThread.ManagedThreadId == mainThread,
                persistent = value.gameObject.scene.name == "DontDestroyOnLoad" };
        }
    }
}
