// Test host for Unity's standalone Mono. It plays the Player's part in the patcher tests: load the
// game assemblies, load the patch assemblies, redirect the originals by overwriting the start of
// their compiled code with a jump, then run the driver.
//
//   mono Host.exe reference <driver type> <assembly.dll>...             new compilation, no patch
//   mono Host.exe patched <apply.txt> <driver type> <assembly.dll>...   loaded assemblies + patch
//   mono Host.exe jit <apply.txt> <assembly.dll>...                     compile every method of the patches
//   mono Host.exe generations <apply1.txt>,<apply2.txt>,... <driver type> <assembly.dll>...
//                                                                       Step(0), then per patch: apply it, Step(n)
//
// apply.txt is the patch builder's result as tab-separated lines (this runtime has no JSON reader):
//   runtime <UnityMicroEditor.HotReload.dll>
//   patch   <assembly name> <loaded MVID> <patch.dll> <patch.pdb or empty>
//   detour  <assembly name> <original token> <replacement token>
//
// The redirect here is deliberately minimal (one thread, nothing running): the real applier with
// its gated writes is tools/HotReloadRuntime. Like the applier, a later patch replaces the earlier
// one entirely: an original that it no longer redirects gets its own first bytes back.
using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;

static class Host
{
    static readonly Dictionary<string, Assembly> games = new Dictionary<string, Assembly>();
    static readonly Dictionary<string, Assembly> patches = new Dictionary<string, Assembly>();
    static readonly Dictionary<IntPtr, byte[]> saved = new Dictionary<IntPtr, byte[]>();
    static HashSet<IntPtr> redirected = new HashSet<IntPtr>();
    const int JumpBytes = 12;

    static int Main(string[] args)
    {
        try
        {
            string mode = args[0];
            int at = 1;
            string apply = mode == "reference" ? null : args[at++];
            string driverName = mode == "jit" ? null : args[at++];
            Type driver = null;
            for (; at < args.Length; at++)
            {
                Assembly assembly = Assembly.LoadFrom(args[at]);
                games[assembly.GetName().Name] = assembly;
                if (driverName != null && driver == null) driver = assembly.GetType(driverName);
            }
            if (mode == "jit") { Apply(apply, redirect: false); return CompileAll(); }
            if (driver == null) throw new InvalidOperationException("The driver type " + driverName + " was not found.");
            if (mode == "generations")
            {
                MethodInfo step = driver.GetMethod("Step");
                Console.Out.Write((string)step.Invoke(null, new object[] { 0 }));
                string[] files = apply.Split(',');
                for (int i = 0; i < files.Length; i++)
                {
                    Apply(files[i], redirect: true);
                    Console.Out.Write((string)step.Invoke(null, new object[] { i + 1 }));
                }
                return 0;
            }
            driver.GetMethod("Before").Invoke(null, null);
            if (apply != null) Apply(apply, redirect: true);
            Console.Out.Write((string)driver.GetMethod("After").Invoke(null, null));
            return 0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine("HOST FAILED: " + e);
            return 2;
        }
    }

    static void Apply(string file, bool redirect)
    {
        var originals = new List<MethodBase>();
        var replacements = new List<MethodBase>();
        patches.Clear();
        foreach (string line in File.ReadAllLines(file))
        {
            string[] p = line.Split('\t');
            if (p[0] == "runtime") Assembly.LoadFrom(p[1]);
            else if (p[0] == "patch")
            {
                // A token only means what the builder saw while the loaded module is that very build.
                if (games[p[1]].ManifestModule.ModuleVersionId != new Guid(p[2])) throw new InvalidOperationException("The loaded " + p[1] + " is not the build the patch was made for.");
                byte[] image = File.ReadAllBytes(p[3]);
                patches[p[1]] = p[4].Length > 0 ? Assembly.Load(image, File.ReadAllBytes(p[4])) : Assembly.Load(image);
            }
            else if (p[0] == "detour")
            {
                originals.Add(games[p[1]].ManifestModule.ResolveMethod(int.Parse(p[2])));
                replacements.Add(patches[p[1]].ManifestModule.ResolveMethod(int.Parse(p[3])));
            }
        }
        if (!redirect) return;
        // Everything is compiled before anything is written: access and IL errors surface here.
        var from = new IntPtr[originals.Count];
        var to = new IntPtr[originals.Count];
        for (int i = 0; i < originals.Count; i++)
        {
            to[i] = replacements[i].MethodHandle.GetFunctionPointer();
            from[i] = originals[i].MethodHandle.GetFunctionPointer();
        }
        var next = new HashSet<IntPtr>(from);
        foreach (IntPtr original in redirected)
            if (!next.Contains(original)) Marshal.Copy(saved[original], 0, original, JumpBytes);
        for (int i = 0; i < from.Length; i++)
        {
            if (!saved.ContainsKey(from[i]))
            {
                var bytes = new byte[JumpBytes];
                Marshal.Copy(from[i], bytes, 0, JumpBytes);
                saved[from[i]] = bytes;
            }
            Jump(from[i], to[i]);
        }
        redirected = next;
    }

    static void Jump(IntPtr from, IntPtr to)
    {
        long delta = to.ToInt64() - (from.ToInt64() + 5);
        if (delta >= int.MinValue && delta <= int.MaxValue)
        {
            Marshal.WriteByte(from, 0, 0xE9); // jmp rel32
            Marshal.WriteInt32(from, 1, (int)delta);
            return;
        }
        Marshal.WriteByte(from, 0, 0x48); Marshal.WriteByte(from, 1, 0xB8); // mov rax, imm64
        Marshal.WriteInt64(from, 2, to.ToInt64());
        Marshal.WriteByte(from, 10, 0xFF); Marshal.WriteByte(from, 11, 0xE0); // jmp rax
    }

    /// Mono resolves every token of a body while compiling it, so an invalid body, a missing member
    /// or a malformed signature of a patch surfaces here.
    static int CompileAll()
    {
        const BindingFlags all = BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Instance | BindingFlags.Static | BindingFlags.DeclaredOnly;
        int compiled = 0, failed = 0;
        var kinds = new SortedDictionary<string, int>();
        foreach (Assembly patch in patches.Values)
        {
            foreach (Type type in patch.GetTypes())
            {
                var methods = new List<MethodBase>(type.GetMethods(all));
                methods.AddRange(type.GetConstructors(all));
                foreach (MethodBase method in methods)
                {
                    if (method.IsAbstract || method.ContainsGenericParameters) continue;
                    try { method.MethodHandle.GetFunctionPointer(); compiled++; }
                    catch (Exception e)
                    {
                        failed++;
                        int count;
                        kinds.TryGetValue(e.GetType().Name, out count);
                        kinds[e.GetType().Name] = count + 1;
                        if (failed <= 20) Console.Out.WriteLine("FAILED " + type.FullName + "::" + method.Name + " -> " + e.GetType().Name + ": " + e.Message.Split('\n')[0]);
                    }
                }
            }
        }
        foreach (var kind in kinds) Console.Out.WriteLine(kind.Key + "=" + kind.Value);
        Console.Out.WriteLine("compiled=" + compiled + " failed=" + failed);
        return failed == 0 ? 0 : 3;
    }
}
