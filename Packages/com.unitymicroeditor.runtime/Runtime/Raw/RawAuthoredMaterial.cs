using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEngine;
using UnityEngine.Rendering;
using Object = UnityEngine.Object;

namespace Dungeon.CLIPlay.Raw
{
    public static partial class RawSceneLoader
    {
        private static class RawAuthoredMaterial
        {
            private static Dictionary<string, object> Map(object value) => value as Dictionary<string, object>
                ?? throw new InvalidDataException("Material property must be a map.");
            private static List<object> List(object value) => value as List<object>
                ?? throw new InvalidDataException("Material property must be a list.");
            private static IEnumerable<object> IntProperties(Dictionary<string, object> saved) =>
                saved.TryGetValue("m_Ints", out object value) ? List(value) : Array.Empty<object>();
            private static int Int(object value) => Convert.ToInt32(value, CultureInfo.InvariantCulture);
            private static float Float(object value) => Convert.ToSingle(value, CultureInfo.InvariantCulture);
            private static ReferenceData Reference(object value) => JsonUtility.FromJson<ReferenceData>(RawJson.Write(value));
            private static bool SameColor(Color expected, Color actual) => RawFloatReadback.SameColorChannel(expected.r, actual.r)
                && RawFloatReadback.SameColorChannel(expected.g, actual.g) && RawFloatReadback.SameColorChannel(expected.b, actual.b)
                && RawFloatReadback.SameColorChannel(expected.a, actual.a);

            internal static void Validate(RawSceneAsset asset)
            {
                var data = Map(RawJson.Parse(asset.dataJson));
                if (asset.classId != 21 || Int(data["serializedVersion"]) != 8
                    || IsNull(Reference(data["m_Shader"]).fileID))
                    throw new InvalidDataException("Unsupported authored material: " + asset.path);
                ReferenceData parent = Reference(data["m_Parent"]);
                if (!IsNull(parent.fileID) && (string.IsNullOrEmpty(parent.guid)
                    || !data.TryGetValue("m_ModifiedSerializedProperties", out object mask) || Int(mask) != 0))
                    throw new InvalidDataException("Unsupported material variant parent or serialized-property mask: " + asset.path);
                var saved = Map(data["m_SavedProperties"]);
                foreach (string group in new[] { "m_TexEnvs", "m_Ints", "m_Floats", "m_Colors" })
                    foreach (object item in group == "m_Ints" ? IntProperties(saved) : List(saved[group]))
                        if (Map(item).Count != 1) throw new InvalidDataException("Malformed material property: " + asset.path);
            }

            internal static ReferenceData Parent(RawSceneAsset asset) => Reference(Map(RawJson.Parse(asset.dataJson))["m_Parent"]);

            private static void VerifyParentCopy(Material parent, Material copy, string path)
            {
                if (parent.shader != copy.shader || parent.renderQueue != copy.renderQueue
                    || parent.rawRenderQueue != copy.rawRenderQueue || parent.enableInstancing != copy.enableInstancing
                    || parent.doubleSidedGI != copy.doubleSidedGI || parent.globalIlluminationFlags != copy.globalIlluminationFlags
                    || !new HashSet<string>(copy.shaderKeywords, StringComparer.Ordinal).SetEquals(parent.shaderKeywords))
                    throw new InvalidDataException("Material variant inherited header differs: " + path);
                for (int pass = 0; pass < parent.passCount; pass++)
                {
                    string name = parent.GetPassName(pass);
                    if (parent.GetShaderPassEnabled(name) != copy.GetShaderPassEnabled(name))
                        throw new InvalidDataException("Material variant inherited pass differs: " + path + ":" + name);
                }
                Shader shader = parent.shader;
                for (int property = 0; property < shader.GetPropertyCount(); property++)
                {
                    int id = shader.GetPropertyNameId(property);
                    bool same;
                    switch (shader.GetPropertyType(property))
                    {
                        case ShaderPropertyType.Color: same = parent.GetColor(id).Equals(copy.GetColor(id)); break;
                        case ShaderPropertyType.Vector: same = parent.GetVector(id).Equals(copy.GetVector(id)); break;
                        case ShaderPropertyType.Float:
                        case ShaderPropertyType.Range: same = parent.GetFloat(id).Equals(copy.GetFloat(id)); break;
                        case ShaderPropertyType.Int: same = parent.GetInteger(id) == copy.GetInteger(id); break;
                        case ShaderPropertyType.Texture:
                            same = parent.GetTexture(id) == copy.GetTexture(id)
                                && parent.GetTextureScale(id).Equals(copy.GetTextureScale(id))
                                && parent.GetTextureOffset(id).Equals(copy.GetTextureOffset(id)); break;
                        default: throw new InvalidDataException("Unsupported inherited shader property type: " + path);
                    }
                    if (!same) throw new InvalidDataException("Material variant inherited property differs: " + path + ":" + shader.GetPropertyName(property));
                }
            }

            internal static Material Create(RawSceneAsset asset, Func<ReferenceData, Object> resolve)
            {
                var data = Map(RawJson.Parse(asset.dataJson));
                Shader shader = resolve(Reference(data["m_Shader"])) as Shader;
                if (shader == null || !shader.isSupported)
                    throw new InvalidDataException("Authored material needs a compiled supported Shader: " + asset.path);
                ReferenceData parentReference = Reference(data["m_Parent"]);
                Material parent = null;
                if (!IsNull(parentReference.fileID))
                {
                    parent = resolve(parentReference) as Material;
                    if (parent == null || parent.shader != shader)
                        throw new InvalidDataException("Material variant parent or Shader identity differs: " + asset.path);
                }
                var material = parent != null ? new Material(parent) : new Material(shader);
                material.name = asset.name;
                try
                {
                    if (parent != null) VerifyParentCopy(parent, material, asset.path);
                    else
                    {
                        material.enableInstancing = Int(data["m_EnableInstancingVariants"]) != 0;
                        material.doubleSidedGI = Int(data["m_DoubleSidedGI"]) != 0;
                        material.globalIlluminationFlags = (MaterialGlobalIlluminationFlags)Int(data["m_LightmapFlags"]);
                        material.renderQueue = Int(data["m_CustomRenderQueue"]);
                        var keywords = List(data["m_ValidKeywords"]);
                        var keywordNames = new string[keywords.Count];
                        for (int i = 0; i < keywords.Count; i++) keywordNames[i] = (string)keywords[i];
                        material.shaderKeywords = keywordNames;
                        if (!new HashSet<string>(material.shaderKeywords, StringComparer.Ordinal).SetEquals(keywordNames))
                            throw new InvalidDataException("Material keyword readback differs: " + asset.path);
                        if (data.TryGetValue("stringTagMap", out object tags))
                            foreach (var tag in Map(tags))
                            {
                                material.SetOverrideTag(tag.Key, (string)tag.Value);
                                if (material.GetTag(tag.Key, false, string.Empty) != (string)tag.Value)
                                    throw new InvalidDataException("Material tag readback differs: " + asset.path + ":" + tag.Key);
                            }
                        foreach (object pass in List(data["disabledShaderPasses"]))
                        {
                            material.SetShaderPassEnabled((string)pass, false);
                            if (material.GetShaderPassEnabled((string)pass))
                                throw new InvalidDataException("Material pass readback differs: " + asset.path + ":" + pass);
                        }
                        int expectedQueue = Int(data["m_CustomRenderQueue"]);
                        if (material.renderQueue != (expectedQueue == -1 ? shader.renderQueue : expectedQueue))
                            throw new InvalidDataException("Material render queue readback differs: " + asset.path);
                    }
                    var saved = Map(data["m_SavedProperties"]);
                    foreach (object item in List(saved["m_Floats"]))
                        foreach (var property in Map(item))
                        {
                            int id = Shader.PropertyToID(property.Key);
                            float value = Float(property.Value);
                            material.SetFloat(id, value);
                            if (material.HasProperty(id) && !material.GetFloat(id).Equals(value))
                                throw new InvalidDataException("Material float readback differs: " + asset.path + ":" + property.Key);
                        }
                    foreach (object item in IntProperties(saved))
                        foreach (var property in Map(item))
                        {
                            int id = Shader.PropertyToID(property.Key), value = Int(property.Value);
                            material.SetInteger(id, value);
                            if (material.HasProperty(id) && material.GetInteger(id) != value)
                                throw new InvalidDataException("Material integer readback differs: " + asset.path + ":" + property.Key);
                        }
                    foreach (object item in List(saved["m_Colors"]))
                        foreach (var property in Map(item))
                        {
                            var color = Map(property.Value);
                            int id = Shader.PropertyToID(property.Key);
                            var value = new Color(Float(color["r"]), Float(color["g"]), Float(color["b"]), Float(color["a"]));
                            int vectorPropertyIndex = shader.FindPropertyIndex(property.Key);
                            if (vectorPropertyIndex >= 0 && shader.GetPropertyType(vectorPropertyIndex) == ShaderPropertyType.Vector)
                            {
                                // Unity stores Vector shader properties in m_Colors too.
                                // Hidden particle fade vectors legitimately contain +Infinity.
                                Vector4 vector = value;
                                material.SetVector(id, vector);
                                if (!material.GetVector(id).Equals(vector))
                                    throw new InvalidDataException("Material vector readback differs: " + asset.path + ":" + property.Key);
                                continue;
                            }
                            material.SetColor(id, value);
                            if (material.HasProperty(id) && !SameColor(value, material.GetColor(id)))
                            {
                                int propertyIndex = shader.FindPropertyIndex(property.Key);
                                throw new InvalidDataException("Material color readback differs: " + asset.path + ":" + property.Key
                                    + " expected=" + value.ToString("R", CultureInfo.InvariantCulture)
                                    + " actual=" + material.GetColor(id).ToString("R", CultureInfo.InvariantCulture)
                                    + " vector=" + material.GetVector(id).ToString("R", CultureInfo.InvariantCulture)
                                    + " type=" + (propertyIndex >= 0 ? shader.GetPropertyType(propertyIndex).ToString() : "missing")
                                    + " flags=" + (propertyIndex >= 0 ? shader.GetPropertyFlags(propertyIndex).ToString() : "missing")
                                    + " shader=" + shader.name + " colorSpace=" + QualitySettings.activeColorSpace);
                            }
                        }
                    foreach (object item in List(saved["m_TexEnvs"]))
                        foreach (var property in Map(item))
                        {
                            var texture = Map(property.Value);
                            int id = Shader.PropertyToID(property.Key);
                            ReferenceData reference = Reference(texture["m_Texture"]);
                            Object resolved = resolve(reference);
                            if (!IsNull(reference.fileID) && (resolved == null || !(resolved is Texture)))
                                throw new InvalidDataException("Material texture reference did not resolve to a Texture: " + asset.path + ":" + property.Key);
                            var value = resolved as Texture;
                            material.SetTexture(id, value);
                            var scale = Map(texture["m_Scale"]); var offset = Map(texture["m_Offset"]);
                            var expectedScale = new Vector2(Float(scale["x"]), Float(scale["y"]));
                            var expectedOffset = new Vector2(Float(offset["x"]), Float(offset["y"]));
                            material.SetTextureScale(id, expectedScale);
                            material.SetTextureOffset(id, expectedOffset);
                            if (material.HasProperty(id) && (material.GetTexture(id) != value
                                || !material.GetTextureScale(id).Equals(expectedScale) || !material.GetTextureOffset(id).Equals(expectedOffset)))
                                throw new InvalidDataException("Material texture readback differs: " + asset.path + ":" + property.Key);
                        }
                    return material;
                }
                catch { Object.Destroy(material); throw; }
            }
        }
    }
}
