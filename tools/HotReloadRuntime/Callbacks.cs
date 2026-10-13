using System;
using System.Collections.Generic;
using System.Reflection;

namespace UnityMicroEditor.HotReload
{
    // Game code asks to be told about a code patch with attributes. They are matched by simple name
    // from any namespace, so code written for "Hot Reload for Unity" keeps working without a
    // reference to this tool: InvokeOnHotReload and OnHotReload run after every applied patch,
    // InvokeOnHotReloadLocal runs a method when its own body changed. Only what the loaded game
    // assemblies declare is found; an attribute on a method added by a patch is not visible.
    internal sealed class Callbacks
    {
        private sealed class Callback
        {
            internal MethodInfo Method;
            internal bool TakesPatches;
            internal string Problem;
        }

        private sealed class Scan
        {
            internal readonly List<Callback> Global = new List<Callback>();
            internal readonly HashSet<int> Local = new HashSet<int>();
        }

        private const BindingFlags Declared = BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static | BindingFlags.Instance | BindingFlags.DeclaredOnly;

        private readonly Host host;
        private readonly Dictionary<Assembly, Scan> scans = new Dictionary<Assembly, Scan>();
        private readonly HashSet<MethodBase> warned = new HashSet<MethodBase>();

        internal int Invoked, Errors;

        internal Callbacks(Host host) { this.host = host; }

        /// <summary>Runs on the thread that applied the patch, after every redirect is in place.</summary>
        internal void Run(List<Assembly> gameAssemblies, List<Change> changes)
        {
            Invoked = 0; Errors = 0;
            object patches = null;
            foreach (Assembly assembly in gameAssemblies)
                foreach (Callback callback in ScanOf(assembly).Global)
                {
                    if (callback.Problem != null) { Skip(callback.Method, callback.Problem); continue; }
                    object[] arguments = null;
                    if (callback.TakesPatches) arguments = new[] { patches ?? (patches = host.Patches(changes)) };
                    Call(callback.Method, arguments);
                }
            foreach (Change change in changes)
            {
                // The original carries the redirect, so invoking it runs the new body.
                MethodBase method = change.Original;
                if (change.Removed || method == null || !ScanOf(method.Module.Assembly).Local.Contains(method.MetadataToken)) continue;
                if (method.GetParameters().Length != 0) Skip(method, "A method that runs when it was patched takes no parameters.");
                else Call(method, null);
            }
        }

        private void Call(MethodBase method, object[] arguments)
        {
            if (method.IsStatic) { Invoke(method, null, arguments); return; }
            if (!host.Enumerable(method.DeclaringType))
            { Skip(method, "Live instances can only be found for UnityEngine.Object types; declare the method static."); return; }
            foreach (object target in host.Instances(method.DeclaringType)) Invoke(method, target, arguments);
        }

        private void Invoke(MethodBase method, object target, object[] arguments)
        {
            Invoked++;
            try { method.Invoke(target, arguments); }
            catch (Exception error)
            {
                // Game code failed, not the patch.
                Errors++;
                host.Report(error is TargetInvocationException && error.InnerException != null ? error.InnerException : error, target);
            }
        }

        private void Skip(MethodBase method, string problem)
        {
            Errors++;
            if (warned.Add(method)) host.Warn("callback-skipped " + method.DeclaringType.FullName + "." + method.Name + ": " + problem);
        }

        private Scan ScanOf(Assembly assembly)
        {
            if (scans.TryGetValue(assembly, out Scan scan)) return scan;
            scans.Add(assembly, scan = new Scan());
            Type[] types;
            try { types = assembly.GetTypes(); }
            catch (ReflectionTypeLoadException error) { types = error.Types; }
            foreach (Type type in types)
            {
                if (type == null) continue;
                MethodInfo[] methods;
                try { methods = type.GetMethods(Declared); }
                catch (Exception) { continue; }
                foreach (MethodInfo method in methods)
                {
                    bool global = false, local = false;
                    try
                    {
                        // Most methods carry no attribute at all; this test allocates nothing.
                        if (!method.IsDefined(typeof(Attribute), false)) continue;
                        foreach (CustomAttributeData data in method.GetCustomAttributesData())
                        {
                            string name = data.Constructor.DeclaringType.Name;
                            if (name.EndsWith("Attribute", StringComparison.Ordinal)) name = name.Substring(0, name.Length - 9);
                            if (name == "InvokeOnHotReload" || name == "OnHotReload") global = true;
                            else if (name == "InvokeOnHotReloadLocal") local = true;
                        }
                    }
                    catch (Exception) { continue; }   // an attribute whose type does not load is not one of ours
                    if (local) scan.Local.Add(method.MetadataToken);
                    if (global) scan.Global.Add(Describe(method));
                }
            }
            return scan;
        }

        private Callback Describe(MethodInfo method)
        {
            var callback = new Callback { Method = method };
            ParameterInfo[] parameters = method.GetParameters();
            if (method.ContainsGenericParameters || method.DeclaringType.ContainsGenericParameters)
                callback.Problem = "A generic method cannot be called without type arguments.";
            else if (parameters.Length == 1 && host.TakesPatches(parameters[0].ParameterType)) callback.TakesPatches = true;
            else if (parameters.Length != 0)
                callback.Problem = "The method must take no parameter or one List<UnityMicroEditor.MethodPatch>.";
            return callback;
        }
    }
}
