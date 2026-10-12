using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;
using UnityEngine;

namespace UnityMicroEditor
{
    internal sealed class AgentInputExecutionException : InvalidOperationException
    { internal AgentInputExecutionException(string message, Exception inner) : base(message, inner) { } }
    /// <summary>Uses public optional package APIs. It does not inject legacy Input or native OS events.</summary>
    internal sealed class AgentInput : IDisposable
    {
        private readonly Func<string> projectRoot;
        private readonly HashSet<string> heldKeys = new HashSet<string>(StringComparer.Ordinal);
        private object keyboard, mouse;
        private ushort mouseButtons;
        private Vector2 mousePosition;
        private long queuedEvents, uiClicks;
        internal AgentInput(Func<string> projectRoot) { this.projectRoot = projectRoot; }
        internal Vector2 MousePosition => mousePosition;

        internal Dictionary<string, object> Capabilities()
        {
            Type input = PackageType("UnityEngine.InputSystem.InputSystem", "Unity.InputSystem");
            Type events = PackageType("UnityEngine.EventSystems.EventSystem", "UnityEngine.UI");
            int mode = InputMode(out string profileError);
            return new Dictionary<string, object> { ["inputSystemAvailable"] = input != null, ["activeInputHandling"] = mode < 0 ? null : (object)mode,
                ["profileError"] = profileError, ["keyMouseInjectionSupported"] = input != null && (mode == 1 || mode == 2),
                ["inputSystemAssemblyVersion"] = input == null ? null : input.Assembly.GetName().Version.ToString(),
                ["legacyInputInjection"] = false, ["nativeOsInputInjection"] = false,
                ["deviceSemantics"] = "owned_virtual_keyboard_and_mouse", ["eventSemantics"] = "queued_for_next_input_system_update",
                ["uiPointerAvailable"] = events != null, ["uiSemantics"] = "targeted_public_pointer_down_up_click",
                ["queuedEvents"] = queuedEvents, ["uiClicks"] = uiClicks };
        }

        internal object Execute(Dictionary<string, object> arguments, GameObject target)
        {
            string action = Text(arguments, "action");
            switch (action)
            {
                case "key": return Key(arguments);
                case "mouse": return Mouse(arguments);
                case "ui_click": return Click(arguments, target);
                default: throw new InvalidDataException("Input action must be key, mouse or ui_click.");
            }
        }

        private object Key(Dictionary<string, object> arguments)
        {
            RequireInputProfile();
            Type input = RequirePackage("UnityEngine.InputSystem.InputSystem", "Unity.InputSystem");
            Type keyType = RequirePackage("UnityEngine.InputSystem.Key", "Unity.InputSystem");
            Type stateType = RequirePackage("UnityEngine.InputSystem.LowLevel.KeyboardState", "Unity.InputSystem");
            string name = Text(arguments, "key"); bool pressed = Boolean(arguments, "pressed");
            object key;
            try { key = Enum.Parse(keyType, name, true); } catch (Exception) { throw new InvalidDataException("Unknown InputSystem key: " + name); }
            if (!Enum.IsDefined(keyType, key) || Convert.ToInt32(key, CultureInfo.InvariantCulture) == 0) throw new InvalidDataException("InputSystem key must identify a real key.");
            string canonical = key.ToString(); if (pressed) heldKeys.Add(canonical); else heldKeys.Remove(canonical);
            Array keys = Array.CreateInstance(keyType, heldKeys.Count); int index = 0;
            foreach (string held in heldKeys) keys.SetValue(Enum.Parse(keyType, held, false), index++);
            ConstructorInfo constructor = stateType.GetConstructor(new[] { keyType.MakeArrayType() });
            if (constructor == null) throw new InvalidOperationException("The InputSystem profile has no public KeyboardState key constructor.");
            object state = constructor.Invoke(new object[] { keys });
            try { if (keyboard == null) keyboard = AddDevice(input, "Keyboard", "Unity Micro Editor keyboard"); Queue(input, keyboard, stateType, state); queuedEvents++; }
            catch (Exception error) { throw new AgentInputExecutionException("Input key failed after public device/event execution began: " + error.Message, error); }
            return new Dictionary<string, object> { ["action"] = "key", ["key"] = canonical, ["pressed"] = pressed,
                ["queued"] = true, ["eventSequence"] = queuedEvents, ["delivery"] = "next_input_system_update" };
        }

        private object Mouse(Dictionary<string, object> arguments)
        {
            RequireInputProfile();
            Type input = RequirePackage("UnityEngine.InputSystem.InputSystem", "Unity.InputSystem");
            Type stateType = RequirePackage("UnityEngine.InputSystem.LowLevel.MouseState", "Unity.InputSystem");
            Vector2 position = mousePosition;
            if (arguments.TryGetValue("position", out object raw)) position = Vector(raw);
            ushort buttons = mouseButtons;
            string button = Text(arguments, "button");
            if (!string.IsNullOrEmpty(button))
            {
                Type buttonType = RequirePackage("UnityEngine.InputSystem.LowLevel.MouseButton", "Unity.InputSystem");
                object value;
                try { value = Enum.Parse(buttonType, button, true); } catch (Exception) { throw new InvalidDataException("Unknown mouse button: " + button); }
                int bit = Convert.ToInt32(value, CultureInfo.InvariantCulture);
                if (!Enum.IsDefined(buttonType, value) || bit < 0 || bit > 4) throw new InvalidDataException("Unsupported mouse button.");
                bool pressed = Boolean(arguments, "pressed"); ushort mask = (ushort)(1 << bit);
                buttons = pressed ? (ushort)(buttons | mask) : (ushort)(buttons & ~mask);
            }
            Vector2 scroll = arguments.TryGetValue("scroll", out object scrollRaw) ? Vector(scrollRaw) : Vector2.zero;
            object state = Activator.CreateInstance(stateType);
            SetField(stateType, state, "position", position); SetField(stateType, state, "delta", position - mousePosition);
            SetField(stateType, state, "scroll", scroll); SetField(stateType, state, "buttons", buttons);
            try { if (mouse == null) mouse = AddDevice(input, "Mouse", "Unity Micro Editor mouse"); Queue(input, mouse, stateType, state); mousePosition = position; mouseButtons = buttons; queuedEvents++; }
            catch (Exception error) { throw new AgentInputExecutionException("Input mouse failed after public device/event execution began: " + error.Message, error); }
            return new Dictionary<string, object> { ["action"] = "mouse", ["position"] = new Dictionary<string, object> { ["x"] = position.x, ["y"] = position.y },
                ["buttons"] = (long)buttons, ["queued"] = true, ["eventSequence"] = queuedEvents, ["delivery"] = "next_input_system_update" };
        }

        private object Click(Dictionary<string, object> arguments, GameObject target)
        {
            if (target == null || !target.activeInHierarchy) throw new InvalidDataException("A UI click requires a live active target object.");
            Type eventSystemType = RequirePackage("UnityEngine.EventSystems.EventSystem", "UnityEngine.UI");
            Type pointerType = RequirePackage("UnityEngine.EventSystems.PointerEventData", "UnityEngine.UI");
            Type executeType = RequirePackage("UnityEngine.EventSystems.ExecuteEvents", "UnityEngine.UI");
            object eventSystem = PublicProperty(eventSystemType, "current").GetValue(null, null);
            if (eventSystem == null) throw new InvalidOperationException("The scene has no active EventSystem.");
            Vector2 position = arguments.TryGetValue("position", out object raw) ? Vector(raw) : Vector2.zero;
            object data = Activator.CreateInstance(pointerType, new[] { eventSystem });
            SetProperty(pointerType, data, "position", position); SetProperty(pointerType, data, "pressPosition", position);
            SetProperty(pointerType, data, "pointerId", -1); SetProperty(pointerType, data, "clickCount", 1);
            SetProperty(pointerType, data, "eligibleForClick", true);
            Type buttonType = pointerType.GetNestedType("InputButton", BindingFlags.Public);
            if (buttonType == null) throw new InvalidOperationException("The UI profile lacks public PointerEventData.InputButton.");
            string button = Text(arguments, "button"); if (string.IsNullOrEmpty(button)) button = "Left";
            object buttonValue;
            try { buttonValue = Enum.Parse(buttonType, button, true); } catch (Exception) { throw new InvalidDataException("Unknown UI button: " + button); }
            if (!Enum.IsDefined(buttonType, buttonValue)) throw new InvalidDataException("Unknown UI button.");
            SetProperty(pointerType, data, "button", buttonValue);
            Type clickInterface = RequirePackage("UnityEngine.EventSystems.IPointerClickHandler", "UnityEngine.UI");
            MethodInfo getHandler = GenericMethod(executeType, "GetEventHandler", 1).MakeGenericMethod(clickInterface);
            var receiver = getHandler.Invoke(null, new object[] { target }) as GameObject;
            if (receiver == null) throw new InvalidOperationException("The target has no active public pointer-click handler in its hierarchy.");
            SetProperty(pointerType, data, "pointerPress", receiver); SetProperty(pointerType, data, "rawPointerPress", target);
            try
            {
                Dispatch(executeType, "IPointerDownHandler", "pointerDownHandler", receiver, data);
                Dispatch(executeType, "IPointerUpHandler", "pointerUpHandler", receiver, data);
                bool clicked = Dispatch(executeType, "IPointerClickHandler", "pointerClickHandler", receiver, data);
                if (!clicked) throw new InvalidOperationException("The pointer-click handler became unavailable during delivery.");
                uiClicks++;
            }
            catch (Exception error) { throw new AgentInputExecutionException("UI input failed after pointer delivery began: " + error.Message, error); }
            return new Dictionary<string, object> { ["action"] = "ui_click", ["targetPath"] = AgentRuntimeSnapshot.PathOf(target.transform),
                ["handlerPath"] = AgentRuntimeSnapshot.PathOf(receiver.transform), ["delivered"] = true,
                ["delivery"] = "targeted_pointer_down_up_click", ["clickSequence"] = uiClicks };
        }

        /// <summary>Releases every key and mouse button the virtual devices hold down.</summary>
        internal void ReleaseAll()
        {
            Type input = PackageType("UnityEngine.InputSystem.InputSystem", "Unity.InputSystem"); if (input == null) return;
            if (keyboard != null && heldKeys.Count > 0)
            {
                Type stateType = RequirePackage("UnityEngine.InputSystem.LowLevel.KeyboardState", "Unity.InputSystem");
                heldKeys.Clear(); Queue(input, keyboard, stateType, Activator.CreateInstance(stateType)); queuedEvents++;
            }
            if (mouse != null && mouseButtons != 0)
            {
                Type stateType = RequirePackage("UnityEngine.InputSystem.LowLevel.MouseState", "Unity.InputSystem");
                object state = Activator.CreateInstance(stateType);
                SetField(stateType, state, "position", mousePosition); SetField(stateType, state, "buttons", (ushort)0);
                mouseButtons = 0; Queue(input, mouse, stateType, state); queuedEvents++;
            }
        }

        private static bool Dispatch(Type executeType, string interfaceName, string property, GameObject receiver, object data)
        {
            Type contract = RequirePackage("UnityEngine.EventSystems." + interfaceName, "UnityEngine.UI");
            object handler = PublicProperty(executeType, property).GetValue(null, null);
            return (bool)GenericMethod(executeType, "Execute", 3).MakeGenericMethod(contract).Invoke(null, new[] { (object)receiver, data, handler });
        }

        private static object AddDevice(Type input, string layout, string name)
        {
            MethodInfo add = input.GetMethod("AddDevice", BindingFlags.Public | BindingFlags.Static, null, new[] { typeof(string), typeof(string), typeof(string) }, null);
            if (add == null) throw new InvalidOperationException("The InputSystem profile has no public named-layout AddDevice API.");
            object device = add.Invoke(null, new object[] { layout, name, null });
            if (device == null) throw new InvalidOperationException("InputSystem did not create the requested virtual device.");
            MethodInfo current = device.GetType().GetMethod("MakeCurrent", BindingFlags.Public | BindingFlags.Instance, null, Type.EmptyTypes, null);
            if (current == null) throw new InvalidOperationException("InputSystem virtual device has no public MakeCurrent API.");
            current.Invoke(device, null); return device;
        }
        private static void Queue(Type input, object device, Type stateType, object state)
        {
            MethodInfo method = GenericMethod(input, "QueueStateEvent", 3).MakeGenericMethod(stateType);
            try { method.Invoke(null, new[] { device, state, (object)(-1d) }); }
            catch (TargetInvocationException error) { throw new InvalidOperationException("Public InputSystem QueueStateEvent failed: " + (error.InnerException ?? error).Message, error.InnerException ?? error); }
        }
        private static MethodInfo GenericMethod(Type type, string name, int parameters)
        {
            MethodInfo found = null;
            foreach (MethodInfo method in type.GetMethods(BindingFlags.Public | BindingFlags.Static))
                if (method.Name == name && method.IsGenericMethodDefinition && method.GetGenericArguments().Length == 1 && method.GetParameters().Length == parameters)
                { if (found != null) throw new InvalidOperationException("Public package API is ambiguous: " + name); found = method; }
            return found ?? throw new InvalidOperationException("Public package API is unavailable: " + name);
        }
        private static PropertyInfo PublicProperty(Type type, string name)
        { PropertyInfo value = type.GetProperty(name, BindingFlags.Public | BindingFlags.Static | BindingFlags.Instance); if (value == null || value.GetGetMethod(false) == null) throw new InvalidOperationException("Public package property is unavailable: " + name); return value; }
        private static void SetProperty(Type type, object target, string name, object value)
        { PropertyInfo property = PublicProperty(type, name); if (property.GetSetMethod(false) == null) throw new InvalidOperationException("Public package property is not writable: " + name); property.SetValue(target, value, null); }
        private static void SetField(Type type, object target, string name, object value)
        { FieldInfo field = type.GetField(name, BindingFlags.Public | BindingFlags.Instance); if (field == null || field.IsInitOnly) throw new InvalidOperationException("Public package state field is unavailable: " + name); field.SetValue(target, value); }
        internal static Type PackageType(string fullName, string assemblyName)
        { foreach (Assembly assembly in UnityEngine.Assemblies.CurrentAssemblies.GetLoadedAssemblies()) if (assembly.GetName().Name == assemblyName) return assembly.GetType(fullName, false); return null; }
        private static Type RequirePackage(string name, string assembly)
        { return PackageType(name, assembly) ?? throw new InvalidOperationException("Input action requires the explicit project package assembly " + assembly + ". Legacy Input and OS input injection are unsupported."); }
        private static string Text(Dictionary<string, object> arguments, string name) { return arguments.TryGetValue(name, out object value) ? value as string ?? string.Empty : string.Empty; }
        private static bool Boolean(Dictionary<string, object> arguments, string name) { if (!arguments.TryGetValue(name, out object value) || !(value is bool)) throw new InvalidDataException("Expected a boolean: " + name); return (bool)value; }
        private static Vector2 Vector(object value)
        {
            if (!(value is Dictionary<string, object> map)) throw new InvalidDataException("Expected a two-dimensional position/scroll object.");
            return new Vector2(Number(map, "x"), Number(map, "y"));
        }
        private static float Number(Dictionary<string, object> map, string name)
        {
            if (!map.TryGetValue(name, out object value) || (!(value is long) && !(value is double))) throw new InvalidDataException("Expected a numeric input coordinate: " + name);
            double number = Convert.ToDouble(value, CultureInfo.InvariantCulture); if (double.IsNaN(number) || double.IsInfinity(number) || Math.Abs(number) > 1000000) throw new InvalidDataException("Input coordinate exceeds bounds."); return (float)number;
        }
        private void RequireInputProfile()
        {
            int mode = InputMode(out string error);
            if (mode != 1 && mode != 2) throw new InvalidOperationException("Keyboard/mouse injection requires the explicit target's InputSystem activeInputHandler profile (1 or 2). Legacy-only/unknown profiles are unsupported. " + error);
        }
        private int InputMode(out string error)
        {
            error = null; string root = projectRoot();
            if (string.IsNullOrEmpty(root)) { error = "Target project root is unavailable."; return -1; }
            string file = Path.Combine(root, "ProjectSettings", "ProjectSettings.asset");
            try
            {
                var info = new FileInfo(file);
                if (!info.Exists || info.Length > 4 * 1024 * 1024 || (info.Attributes & FileAttributes.ReparsePoint) != 0)
                { error = "Target ProjectSettings is absent, oversized or a symlink."; return -1; }
                foreach (string line in File.ReadLines(file))
                {
                    string value = line.Trim(); const string key = "activeInputHandler:";
                    if (!value.StartsWith(key, StringComparison.Ordinal)) continue;
                    if (int.TryParse(value.Substring(key.Length).Trim(), NumberStyles.None, CultureInfo.InvariantCulture, out int mode) && mode >= 0 && mode <= 2) return mode;
                    error = "Target activeInputHandler is invalid."; return -1;
                }
                error = "Target activeInputHandler is not declared."; return -1;
            }
            catch (Exception failure) { error = failure.Message; return -1; }
        }
        public void Dispose()
        {
            Type input = PackageType("UnityEngine.InputSystem.InputSystem", "Unity.InputSystem"); if (input == null) return;
            Type deviceType = PackageType("UnityEngine.InputSystem.InputDevice", "Unity.InputSystem"); if (deviceType == null) return;
            MethodInfo remove = input.GetMethod("RemoveDevice", BindingFlags.Public | BindingFlags.Static, null, new[] { deviceType }, null);
            if (remove != null) { if (keyboard != null) remove.Invoke(null, new[] { keyboard }); if (mouse != null) remove.Invoke(null, new[] { mouse }); }
            keyboard = null; mouse = null; heldKeys.Clear();
        }
    }
}
