// Reference capture, run by the tool's developers only, with a licensed Editor:
// writes the settings managers of a project and the managers a build derives
// (Library/BuildSettings.asset, MonoManager.asset, ScriptMapper) in the Player
// format with their type trees, as one file. The tool ships what it reads off
// that file per release (`player-file manager-pack`); a user's preparation
// never runs this.
//
//   Unity -batchmode -nographics -projectPath <empty project of the release> \
//     -executeMethod UnityMicroEditor.ReferenceCapture.ManagerCapture.Run \
//     -captureOutput <directory> -quit
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using UnityEditor;
using UnityEditor.Build.Content;
using UnityEngine;

namespace UnityMicroEditor.ReferenceCapture
{
    public static class ManagerCapture
    {
        const BuildTarget Target = BuildTarget.StandaloneLinux64;
        const string InternalName = "globalgamemanagers";

        public static void Run()
        {
            string[] args = Environment.GetCommandLineArgs();
            int at = Array.IndexOf(args, "-captureOutput");
            if (at < 0 || at + 1 >= args.Length) throw new ArgumentException("-captureOutput <directory> is required");
            string output = Path.GetFullPath(args[at + 1]);
            Directory.CreateDirectory(output);
            var files = Directory.GetFiles("ProjectSettings", "*.asset").OrderBy(f => f, StringComparer.Ordinal).ToList();
            files.AddRange(new[] { "Library/BuildSettings.asset", "Library/MonoManager.asset", "Library/ScriptMapper" });
            var ids = new List<ObjectIdentifier>();
            var log = new System.Text.StringBuilder();
            foreach (string file in files)
            {
                if (!File.Exists(file)) { log.Append("missing ").Append(file).Append('\n'); continue; }
                ObjectIdentifier[] found;
                try { found = ContentBuildInterface.GetPlayerObjectIdentifiersInSerializedFile(file, Target); }
                catch (Exception error) { log.Append("refused ").Append(file).Append(' ').Append(error.Message).Append('\n'); continue; }
                foreach (var id in found)
                {
                    var type = ContentBuildInterface.GetTypeForObject(id);
                    log.Append("object ").Append(file).Append(' ').Append(id.localIdentifierInFile).Append(' ').Append(type == null ? "?" : type.FullName).Append('\n');
                    // A package's settings asset (6000.6's ProjectAuditorSettings) is a
                    // script object no Player holds; writing it fails the file.
                    if (type == typeof(MonoBehaviour) || (type != null && type.IsSubclassOf(typeof(ScriptableObject)))) continue;
                    ids.Add(id);
                }
            }
            var command = new WriteCommand { fileName = "managers.player", internalName = InternalName, serializeObjects = new List<SerializationInfo>() };
            var map = new BuildReferenceMap();
            long index = 1;
            foreach (var id in ids)
            {
                command.serializeObjects.Add(new SerializationInfo { serializationObject = id, serializationIndex = index });
                map.AddMapping(InternalName, index, id);
                index++;
            }
            var global = ContentBuildInterface.GetGlobalUsageFromGraphicsSettings();
            var usage = new BuildUsageTagSet();
            var all = ids.ToArray();
            ContentBuildInterface.CalculateBuildUsageTags(all, all, global, usage);
            var result = ContentBuildInterface.WriteSerializedFile(output, new WriteParameters
            {
                writeCommand = command,
                settings = new BuildSettings { target = Target, group = BuildTargetGroup.Standalone, buildFlags = ContentBuildFlags.None },
                globalUsage = global,
                usageSet = usage,
                referenceMap = map,
            });
            foreach (var file in result.resourceFiles)
                log.Append("file ").Append(file.fileAlias).Append(' ').Append(Path.GetFileName(file.fileName)).Append('\n');
            log.Append("unity ").Append(Application.unityVersion).Append('\n');
            File.WriteAllText(Path.Combine(output, "capture.txt"), log.ToString());
        }
    }
}
