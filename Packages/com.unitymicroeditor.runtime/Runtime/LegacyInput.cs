using System;
using System.Collections.Generic;
using UnityEngine;

namespace UnityMicroEditor
{
    /// <summary>
    /// The mouse and keys a remote viewer or an agent gives a Player of Play mode code, which reads
    /// them through the legacy Input (<c>PlayModeEditor.UnityEngine_Input</c>). Every press and
    /// release takes effect on a frame of its own, the frame after it arrived, so that a click that
    /// arrives whole is still seen down, then up, as a frame of the Editor sees it.
    /// </summary>
    internal static class LegacyInput
    {
        /// <summary>Whether the Player's code reads the legacy Input through the bridge: Play mode code does.</summary>
        internal static bool Answers => !string.IsNullOrEmpty(Environment.GetEnvironmentVariable("UME_PLAY_MODE_BUILD_TARGET"));

        sealed class Control
        {
            internal readonly Queue<bool> Pending = new Queue<bool>();
            internal bool Held;
            internal int DownFrame = -1, UpFrame = -1;
        }

        static readonly Dictionary<int, Control> buttons = new Dictionary<int, Control>();
        static readonly Dictionary<KeyCode, Control> keys = new Dictionary<KeyCode, Control>();
        static Vector2 position, pendingScroll, scroll;
        static int frame = -1;
        static float lastEvent = float.NegativeInfinity;

        /// <summary>Whether the viewer's pointer stands for the mouse: it moved lately or holds a button.</summary>
        internal static bool Pointing { get { Advance(); return Time.realtimeSinceStartup - lastEvent < 10 || AnyButtonHeld(); } }
        internal static Vector2 Position { get { Advance(); return position; } }
        internal static Vector2 Scroll { get { Advance(); return scroll; } }

        internal static void Mouse(Vector2 at, int button, bool pressed, Vector2 wheel)
        {
            position = at;
            pendingScroll += wheel;
            lastEvent = Time.realtimeSinceStartup;
            if (button >= 0) Enqueue(buttons, button, pressed);
        }

        internal static void Key(KeyCode key, bool pressed)
        {
            lastEvent = Time.realtimeSinceStartup;
            Enqueue(keys, key, pressed);
        }

        /// <summary>Releases what is held, as a viewer that left lets go.</summary>
        internal static void ReleaseAll()
        {
            foreach (var control in buttons.Values) if (control.Held || control.Pending.Count != 0) control.Pending.Enqueue(false);
            foreach (var control in keys.Values) if (control.Held || control.Pending.Count != 0) control.Pending.Enqueue(false);
        }

        internal static bool Held(int button) { Advance(); return buttons.TryGetValue(button, out var control) && control.Held; }
        internal static bool Down(int button) { Advance(); return buttons.TryGetValue(button, out var control) && control.DownFrame == frame; }
        internal static bool Up(int button) { Advance(); return buttons.TryGetValue(button, out var control) && control.UpFrame == frame; }
        internal static bool Held(KeyCode key) { Advance(); return keys.TryGetValue(key, out var control) && control.Held; }
        internal static bool Down(KeyCode key) { Advance(); return keys.TryGetValue(key, out var control) && control.DownFrame == frame; }
        internal static bool Up(KeyCode key) { Advance(); return keys.TryGetValue(key, out var control) && control.UpFrame == frame; }

        internal static bool AnyHeld
        {
            get { Advance(); foreach (var control in keys.Values) if (control.Held) return true; return AnyButtonHeld(); }
        }

        internal static bool AnyDown
        {
            get
            {
                Advance();
                foreach (var control in keys.Values) if (control.DownFrame == frame) return true;
                foreach (var control in buttons.Values) if (control.DownFrame == frame) return true;
                return false;
            }
        }

        /// <summary>The key the legacy Input names <paramref name="name"/> ("space", "left shift", "a"), or none.</summary>
        internal static KeyCode Key(string name)
        {
            if (string.IsNullOrEmpty(name)) return KeyCode.None;
            string compact = name.Replace(" ", string.Empty);
            if (compact.Length == 1 && char.IsDigit(compact[0])) return KeyCode.Alpha0 + (compact[0] - '0');
            return Enum.TryParse(compact, true, out KeyCode key) ? key : KeyCode.None;
        }

        /// <summary>The key an Input System key name ("Space", "LeftCtrl", "Digit1", "Enter") is in the legacy Input.</summary>
        internal static KeyCode FromInputSystem(string name)
        {
            switch (name.ToLowerInvariant())
            {
                case "enter": return KeyCode.Return;
                case "leftctrl": return KeyCode.LeftControl;
                case "rightctrl": return KeyCode.RightControl;
                case "leftmeta": case "leftcommand": case "leftwindows": return KeyCode.LeftCommand;
                case "rightmeta": case "rightcommand": case "rightwindows": return KeyCode.RightCommand;
                case "numpadenter": return KeyCode.KeypadEnter;
                case "backquote": return KeyCode.BackQuote;
                case "quote": return KeyCode.Quote;
                case "leftbracket": return KeyCode.LeftBracket;
                case "rightbracket": return KeyCode.RightBracket;
                case "contextmenu": return KeyCode.Menu;
            }
            if (name.StartsWith("Digit", StringComparison.OrdinalIgnoreCase) && name.Length == 6 && char.IsDigit(name[5]))
                return KeyCode.Alpha0 + (name[5] - '0');
            if (name.StartsWith("Numpad", StringComparison.OrdinalIgnoreCase) && name.Length == 7 && char.IsDigit(name[6]))
                return KeyCode.Keypad0 + (name[6] - '0');
            return Enum.TryParse(name, true, out KeyCode key) ? key : KeyCode.None;
        }

        static void Enqueue<T>(Dictionary<T, Control> controls, T id, bool pressed)
        {
            if (!controls.TryGetValue(id, out var control)) controls[id] = control = new Control();
            control.Pending.Enqueue(pressed);
        }

        static bool AnyButtonHeld()
        {
            foreach (var control in buttons.Values) if (control.Held) return true;
            return false;
        }

        // The first read of a frame takes, of every control, the next press or release that arrived.
        static void Advance()
        {
            if (Time.frameCount == frame) return;
            frame = Time.frameCount;
            scroll = pendingScroll;
            pendingScroll = Vector2.zero;
            Step(buttons.Values);
            Step(keys.Values);
        }

        static void Step(IEnumerable<Control> controls)
        {
            foreach (var control in controls)
            {
                if (control.Pending.Count == 0) continue;
                bool pressed = control.Pending.Dequeue();
                if (pressed == control.Held) continue;
                control.Held = pressed;
                if (pressed) control.DownFrame = frame; else control.UpFrame = frame;
            }
        }
    }
}
