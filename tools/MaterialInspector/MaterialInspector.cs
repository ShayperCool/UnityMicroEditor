// Runs in a standalone AssetImportWorker (rust/src/native/inspector.rs compiles it with the
// compiler of the Unity installation and puts it next to the project's script assemblies).
//
// A build writes a material variant after the engine has built its properties, and building
// them runs the drawers of the shader's properties and the ValidateMaterial of the shader's
// inspector. This importer makes the engine do the same and saves what it leaves: it reads
// each listed variant from its source, touches a property, and writes the material back as
// text. Nothing of an inspector is restated here; the engine calls the project's own
// compiled class.
//
// The shaders of the variants are imported earlier in the same run and recorded in the
// worker's ArtifactDB, so a material finds its shader through its reference and an inspector
// that asks the asset database about the shader (the path of a graph, its importer, the
// metadata beside it) gets the answers it gets in the Editor. The report names, for each
// variant, the class of the inspector and the assembly that defines it; none when no loaded
// assembly does, in which case the engine ran no inspector.
//
// The asset lists one item a line: "colorspace linear" or "colorspace gamma" states the color
// space of the project, which a standalone worker does not take from the project settings and
// in which the engine converts the colors an inspector reads and sets; "pipeline <path>" names
// the render pipeline asset of the project, which decides the inspector of a shader with
// CustomEditorForRenderPipeline; "material <path>" names a variant. Every variant is written twice, under
// Temp/UnityMicroEditorInspect: in "a" over parents as they were read, in "b" after the
// variants among its parents were built first, outermost first. A build may meet a chain in
// either state.
using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using UnityEditor;
using UnityEditor.AssetImporters;
using UnityEditorInternal;
using UnityEngine;
using UnityEngine.Rendering;

namespace UnityMicroEditor.Inspection
{
    [ScriptedImporter(1, "umeinspect")]
    public sealed class MaterialInspector : ScriptedImporter
    {
        const string Output = "Temp/UnityMicroEditorInspect";

        // The assembly that defines a class the engine would take as an inspector: the
        // name as the shader gives it, or in the UnityEditor namespace.
        static string AssemblyOf(string inspector)
        {
            if (string.IsNullOrEmpty(inspector)) return "";
            foreach (var assembly in AppDomain.CurrentDomain.GetAssemblies())
                foreach (var name in new[] { inspector, "UnityEditor." + inspector })
                {
                    var type = assembly.GetType(name, false);
                    if (type != null && typeof(ShaderGUI).IsAssignableFrom(type)) return assembly.GetName().Name;
                }
            return "";
        }

        static void Build(Material material)
        {
            // Any property query builds the properties of a material that has none yet.
            material.HasProperty("_");
        }

        public override void OnImportAsset(AssetImportContext context)
        {
            var report = new StringBuilder();
            var lines = File.ReadAllLines(context.assetPath);
            foreach (var line in lines)
            {
                if (line == "colorspace linear") PlayerSettings.colorSpace = ColorSpace.Linear;
                if (line == "colorspace gamma") PlayerSettings.colorSpace = ColorSpace.Gamma;
                if (!line.StartsWith("pipeline ")) continue;
                RenderPipelineAsset pipeline = null;
                foreach (var loaded in InternalEditorUtility.LoadSerializedFileAndForget(line.Substring(9)))
                    if (loaded is RenderPipelineAsset asset) pipeline = asset;
                if (pipeline == null) throw new InvalidOperationException("no render pipeline asset in " + line.Substring(9));
                GraphicsSettings.defaultRenderPipeline = pipeline;
            }
            var current = GraphicsSettings.currentRenderPipeline;
            report.Append("pipeline\t").Append(current ? current.GetType().FullName : "").Append('\n');
            report.Append("colorspace\t").Append(QualitySettings.activeColorSpace == ColorSpace.Linear ? "linear" : "gamma").Append('\n');
            foreach (var phase in new[] { "a", "b" })
            {
                Directory.CreateDirectory(Output + "/" + phase);
                int index = -1;
                foreach (var line in lines)
                {
                    if (!line.StartsWith("material ")) continue;
                    index++;
                    var path = line.Substring(9);
                    Material material = null;
                    foreach (var loaded in InternalEditorUtility.LoadSerializedFileAndForget(path))
                        if (loaded is Material found) material = found;
                    if (material == null)
                    {
                        if (phase == "a") report.Append("missing\t").Append(index).Append('\n');
                        continue;
                    }
                    if (phase == "b")
                    {
                        var parents = new List<Material>();
                        for (var parent = material.parent; parent != null && parent.parent != null && parents.Count < 64; parent = parent.parent)
                            parents.Add(parent);
                        for (int i = parents.Count - 1; i >= 0; i--) Build(parents[i]);
                    }
                    Build(material);
                    if (phase == "a")
                    {
                        var shader = material.shader;
                        var inspector = shader ? ShaderUtil.GetCurrentCustomEditor(shader) : "";
                        report.Append("material\t").Append(index).Append('\t')
                            .Append(inspector).Append('\t').Append(AssemblyOf(inspector)).Append('\t')
                            .Append(shader ? shader.name : "").Append('\n');
                    }
                    InternalEditorUtility.SaveToSerializedFileAndForget(new UnityEngine.Object[] { material }, Output + "/" + phase + "/" + index + ".mat", true);
                }
            }
            report.Append("done\n");
            File.WriteAllText(Output + "/report.txt", report.ToString());
            var result = new TextAsset(report.ToString());
            context.AddObjectToAsset("report", result);
            context.SetMainObject(result);
        }
    }
}
