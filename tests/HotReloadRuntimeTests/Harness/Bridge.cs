// The harness compiles the bridge's own CodeReload.cs (and RawJson.cs) and drives every request
// through its Poll, as a Player does. Only the atomic file write comes from here, because the real
// one shares a file with report types that need the engine.
using System;
using System.IO;
using System.Text;

namespace UnityMicroEditor
{
    internal static class CliPlayArtifact
    {
        internal static void WriteText(string path, string text)
        {
            string temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            File.WriteAllText(temporary, text, new UTF8Encoding(false));
            if (File.Exists(path)) File.Replace(temporary, path, null);
            else File.Move(temporary, path);
        }
    }
}
