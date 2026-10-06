using System;
using System.Collections.Generic;
using System.Globalization;
using System.Reflection;
using System.Text;
using UnityEngine;
using UnityEngine.SceneManagement;
using UnityEngine.Rendering;
using Object = UnityEngine.Object;

namespace UnityPlay.Compatibility
{
    // The same observer runs in the Editor reference and the raw Player. It uses
    // public scene/component APIs and Unity's serializer, including for private
    // serialized references. No importer assumptions are used to produce expected data.
    public static class SceneSnapshot
    {
        [Serializable] public sealed class Snapshot
        {
            public int schemaVersion = 1;
            public string engineVersion;
            public string sceneName;
            public ObjectRecord[] objects;
            public AssetRecord[] assets;
            public string[] diagnostics;
        }
        [Serializable] public sealed class ObjectRecord
        {
            public string path;
            public int rootIndex;
            public int siblingIndex;
            public string name;
            public bool activeSelf;
            public bool activeInHierarchy;
            public int layer;
            public string tag;
            public Vector3 localPosition;
            public Quaternion localRotation;
            public Vector3 localScale;
            public ComponentRecord[] components;
        }
        [Serializable] public sealed class ComponentRecord
        {
            public int index;
            public string typeName;
            public string captureKind;
            public string serializedJson;
            public ReferenceRecord[] references;
            public CameraRecord camera;
            public LightRecord light;
            public MeshRecord mesh;
            public RendererRecord renderer;
            public ColliderRecord collider;
            public RigidbodyRecord rigidbody;
        }
        [Serializable] public sealed class ReferenceRecord
        {
            public string property;
            public string identity;
            public string typeName;
            public string name;
            public bool resolved;
        }
        [Serializable] public sealed class CameraRecord
        {
            public bool enabled;
            public int clearFlags;
            public Color backgroundColor;
            public float nearClipPlane;
            public float farClipPlane;
            public float fieldOfView;
            public bool orthographic;
            public float orthographicSize;
            public float depth;
            public int cullingMask;
            public Rect rect;
            public bool allowHDR;
            public bool allowMSAA;
            public bool allowDynamicResolution;
            public int targetDisplay;
            public int renderingPath;
            public bool usePhysicalProperties;
            public Vector2 lensShift;
            public string targetTexture;
        }
        [Serializable] public sealed class LightRecord
        {
            public bool enabled;
            public int type;
            public Color color;
            public float intensity;
            public float range;
            public float spotAngle;
            public float innerSpotAngle;
            public int shadows;
            public float shadowStrength;
            public float shadowBias;
            public float shadowNormalBias;
            public float shadowNearPlane;
            public int shadowResolution;
            public int shadowCustomResolution;
            public int cullingMask;
            public uint renderingLayerMask;
            public float bounceIntensity;
            public bool useColorTemperature;
            public float colorTemperature;
            public string cookie;
        }
        [Serializable] public sealed class MeshRecord
        {
            public string identity;
            public string name;
            public int vertexCount;
            public int subMeshCount;
            public int[] indexCounts;
            public Bounds bounds;
            public bool isReadable;
        }
        [Serializable] public sealed class RendererRecord
        {
            public bool enabled;
            public int shadowCastingMode;
            public bool receiveShadows;
            public int lightProbeUsage;
            public int reflectionProbeUsage;
            public uint renderingLayerMask;
            public int sortingLayerID;
            public int sortingOrder;
            public int lightmapIndex;
            public Vector4 lightmapScaleOffset;
            public string probeAnchor;
            public string lightProbeProxyVolumeOverride;
            public MaterialRecord[] materials;
        }
        [Serializable] public sealed class MaterialRecord
        {
            public string identity;
            public string name;
            public string shaderName;
            public Color color;
            public bool hasColor;
            public int renderQueue;
            public string[] shaderKeywords;
            public bool enableInstancing;
            public bool doubleSidedGI;
            public int globalIlluminationFlags;
            public TexturePropertyRecord[] textures;
            public FloatPropertyRecord[] floats;
            public VectorPropertyRecord[] vectors;
            public ColorPropertyRecord[] colors;
        }
        [Serializable] public sealed class FloatPropertyRecord { public string property; public string kind; public float value; }
        [Serializable] public sealed class VectorPropertyRecord { public string property; public Vector4 value; }
        [Serializable] public sealed class ColorPropertyRecord { public string property; public Color value; }
        [Serializable] public sealed class TexturePropertyRecord
        {
            public string property;
            public string identity;
            public Vector2 scale;
            public Vector2 offset;
        }
        [Serializable] public sealed class ColliderRecord
        {
            public string shape;
            public bool enabled;
            public bool isTrigger;
            public float contactOffset;
            public int includeLayers;
            public int excludeLayers;
            public int layerOverridePriority;
            public bool providesContacts;
            public string sharedMaterial;
            public Vector3 center;
            public Vector3 size;
            public float radius;
            public float height;
            public int direction;
        }
        [Serializable] public sealed class RigidbodyRecord
        {
            public float mass;
            public float linearDamping;
            public float angularDamping;
            public bool useGravity;
            public bool isKinematic;
            public int interpolation;
            public int constraints;
            public int collisionDetectionMode;
            public Vector3 centerOfMass;
            public Vector3 inertiaTensor;
            public Quaternion inertiaTensorRotation;
        }
        [Serializable] public sealed class AssetRecord
        {
            public string identity;
            public string typeName;
            public string name;
            public string scalarJson;
            public ReferenceRecord[] references;
            public TextureRecord texture;
            public MaterialRecord material;
            public MeshRecord mesh;
        }
        [Serializable] public sealed class TextureRecord
        {
            public int width;
            public int height;
            public int dimension;
            public int graphicsFormat;
            public bool isSRGBFormat;
            public string format;
            public int mipmapCount;
            public bool isReadable;
            public int filterMode;
            public int wrapModeU;
            public int wrapModeV;
            public int anisoLevel;
            public float mipMapBias;
            public bool sampleAvailable;
            public Color firstPixel;
            public Color centerPixel;
            public Color lastPixel;
            public string sampleError;
        }
        [Serializable] private sealed class ObjectReference { public Object target; }

        public static Snapshot Capture(Scene scene)
        {
            if (!scene.IsValid() || !scene.isLoaded) throw new ArgumentException("Snapshot requires a loaded valid scene.", nameof(scene));
            return new Observer(scene).Capture();
        }

        // Reports must not depend on Unity registering nested DTO type trees in
        // dynamically loaded assemblies. Script state still uses JsonUtility;
        // this writer only emits our already captured, public observation data.
        public static string ToJson(object report, bool pretty = false)
        {
            var writer = new ReportWriter(pretty);
            writer.Write(report, 0);
            return writer.ToString();
        }

        private sealed class ReportWriter
        {
            private readonly StringBuilder output = new StringBuilder();
            private readonly bool pretty;
            private readonly HashSet<object> ancestors = new HashSet<object>();
            public ReportWriter(bool pretty) { this.pretty = pretty; }
            public override string ToString() { return output.ToString(); }

            public void Write(object value, int depth)
            {
                if (depth > 128) throw new InvalidOperationException("Report nesting exceeds observer limit.");
                if (value == null) { output.Append("null"); return; }
                if (value is string text) { Quote(text); return; }
                if (value is bool flag) { output.Append(flag ? "true" : "false"); return; }
                if (value is float single)
                {
                    if (float.IsNaN(single) || float.IsInfinity(single)) throw new InvalidOperationException("Report contains a nonfinite float.");
                    output.Append(single.ToString("R", CultureInfo.InvariantCulture)); return;
                }
                if (value is double number)
                {
                    if (double.IsNaN(number) || double.IsInfinity(number)) throw new InvalidOperationException("Report contains a nonfinite double.");
                    output.Append(number.ToString("R", CultureInfo.InvariantCulture)); return;
                }
                Type type = value.GetType();
                if (type == typeof(byte) || type == typeof(sbyte) || type == typeof(short) || type == typeof(ushort)
                    || type == typeof(int) || type == typeof(uint) || type == typeof(long) || type == typeof(ulong))
                { output.Append(Convert.ToString(value, CultureInfo.InvariantCulture)); return; }
                if (value is Vector2 vector2) { Values(depth, new[] { "x", "y" }, vector2.x, vector2.y); return; }
                if (value is Vector3 vector3) { Values(depth, new[] { "x", "y", "z" }, vector3.x, vector3.y, vector3.z); return; }
                if (value is Vector4 vector4) { Values(depth, new[] { "x", "y", "z", "w" }, vector4.x, vector4.y, vector4.z, vector4.w); return; }
                if (value is Quaternion rotation) { Values(depth, new[] { "x", "y", "z", "w" }, rotation.x, rotation.y, rotation.z, rotation.w); return; }
                if (value is Color color) { Values(depth, new[] { "r", "g", "b", "a" }, color.r, color.g, color.b, color.a); return; }
                if (value is Rect rect) { Values(depth, new[] { "x", "y", "width", "height" }, rect.x, rect.y, rect.width, rect.height); return; }
                if (value is Bounds bounds) { Values(depth, new[] { "center", "size" }, bounds.center, bounds.size); return; }

                if (!type.IsValueType && !ancestors.Add(value)) throw new InvalidOperationException("Report contains a cyclic DTO reference.");
                try
                {
                    if (value is Array array)
                    {
                        if (array.Rank != 1) throw new InvalidOperationException("Report supports one-dimensional arrays only.");
                        output.Append('[');
                        for (int index = 0; index < array.Length; index++)
                        { Entry(index, depth); Write(array.GetValue(index), depth + 1); }
                        End(']', array.Length, depth); return;
                    }
                    string owner = type.DeclaringType == null ? "" : type.DeclaringType.FullName;
                    bool knownDTO = owner == "UnityPlay.Compatibility.SceneSnapshot" || owner == "UnityPlay.Compatibility.ReferenceCapture"
                        || owner == "UnityPlay.Compatibility.SnapshotDriver" || owner == "UnityPlay.Compatibility.CompatibilityLifecycleProbe";
                    if (!knownDTO || type.Namespace != typeof(SceneSnapshot).Namespace
                        || !Attribute.IsDefined(type, typeof(SerializableAttribute), false))
                        throw new InvalidOperationException("Unsupported report DTO type " + type.FullName + ".");
                    // Only public fields declared by our own DTOs are inspected.
                    // Engine objects and foreign/private fields never enter here.
                    FieldInfo[] fields = type.GetFields(BindingFlags.Public | BindingFlags.Instance | BindingFlags.DeclaredOnly);
                    Array.Sort(fields, (left, right) => string.CompareOrdinal(left.Name, right.Name));
                    output.Append('{'); int count = 0;
                    foreach (FieldInfo field in fields)
                    {
                        if (Attribute.IsDefined(field, typeof(NonSerializedAttribute), false)) continue;
                        Entry(count++, depth); Quote(field.Name); Colon(); Write(field.GetValue(value), depth + 1);
                    }
                    End('}', count, depth);
                }
                finally { if (!type.IsValueType) ancestors.Remove(value); }
            }

            private void Values(int depth, string[] names, params object[] values)
            {
                output.Append('{');
                for (int index = 0; index < names.Length; index++)
                { Entry(index, depth); Quote(names[index]); Colon(); Write(values[index], depth + 1); }
                End('}', names.Length, depth);
            }
            private void Entry(int index, int depth)
            { if (index != 0) output.Append(','); if (pretty) { output.Append('\n'); output.Append(' ', (depth + 1) * 2); } }
            private void Colon() { output.Append(':'); if (pretty) output.Append(' '); }
            private void End(char closing, int count, int depth)
            { if (pretty && count != 0) { output.Append('\n'); output.Append(' ', depth * 2); } output.Append(closing); }
            private void Quote(string text) { output.Append(Json.Quote(text)); }
        }

        private sealed class Observer
        {
            private readonly Scene scene;
            private readonly List<ObjectRecord> records = new List<ObjectRecord>();
            private readonly List<GameObject> objects = new List<GameObject>();
            private readonly Dictionary<Object, string> identities = new Dictionary<Object, string>();
            private readonly Dictionary<Object, AssetRecord> assets = new Dictionary<Object, AssetRecord>();
            private readonly List<string> diagnostics = new List<string>();
            private string referenceShape;

            public Observer(Scene scene) { this.scene = scene; }

            public Snapshot Capture()
            {
                int rootIndex = 0;
                foreach (GameObject root in scene.GetRootGameObjects())
                {
                    if (IsToolContainer(root)) continue;
                    Collect(root.transform, rootIndex.ToString(CultureInfo.InvariantCulture) + ":" + root.name, rootIndex, rootIndex);
                    rootIndex++;
                }
                if (objects.Count != 0)
                {
                    referenceShape = Json.ReferenceShape(ReferenceJson(objects[0]));
                    if (referenceShape == null) diagnostics.Add("Engine object-reference JSON has no recognized map shape; canonical serialized references are unavailable.");
                }
                for (int index = 0; index < objects.Count; index++) CaptureComponents(objects[index], records[index]);
                var assetRecords = new List<AssetRecord>(assets.Values);
                assetRecords.Sort((left, right) => string.CompareOrdinal(left.identity, right.identity));
                return new Snapshot { engineVersion = Application.unityVersion, sceneName = scene.name,
                    objects = records.ToArray(), assets = assetRecords.ToArray(), diagnostics = diagnostics.ToArray() };
            }

            private static bool IsToolContainer(GameObject root)
            {
                if (root.name != "Raw scene imported assets") return false;
                foreach (Component component in root.GetComponents<Component>())
                    if (component != null && component.GetType().FullName == "Dungeon.CLIPlay.Raw.RawSceneResources") return true;
                return false;
            }

            private void Collect(Transform transform, string path, int rootIndex, int siblingIndex)
            {
                GameObject owner = transform.gameObject;
                identities.Add(owner, "scene:" + path);
                Component[] components = owner.GetComponents<Component>();
                for (int index = 0; index < components.Length; index++)
                    if (components[index] != null) identities.Add(components[index], "component:" + path + ":" + components[index].GetType().FullName + "[" + index + "]");
                objects.Add(owner);
                records.Add(new ObjectRecord { path = path, rootIndex = rootIndex, siblingIndex = siblingIndex, name = owner.name,
                    activeSelf = owner.activeSelf, activeInHierarchy = owner.activeInHierarchy, layer = owner.layer, tag = owner.tag,
                    localPosition = transform.localPosition, localRotation = transform.localRotation, localScale = transform.localScale });
                for (int index = 0; index < transform.childCount; index++)
                {
                    Transform child = transform.GetChild(index);
                    Collect(child, path + "/" + index.ToString(CultureInfo.InvariantCulture) + ":" + child.name, rootIndex, index);
                }
            }

            private void CaptureComponents(GameObject owner, ObjectRecord record)
            {
                Component[] components = owner.GetComponents<Component>();
                var result = new ComponentRecord[components.Length];
                for (int index = 0; index < components.Length; index++)
                {
                    Component component = components[index];
                    var item = new ComponentRecord { index = index, typeName = component == null ? "<missing-script>" : component.GetType().FullName,
                        captureKind = "unknown-json", serializedJson = "{}", references = Array.Empty<ReferenceRecord>() };
                    result[index] = item;
                    if (component == null) { diagnostics.Add(record.path + " has a missing component at index " + index); continue; }
                    if (component is Camera camera)
                    {
                        item.captureKind = "camera";
                        item.camera = new CameraRecord { enabled = camera.enabled, clearFlags = (int)camera.clearFlags,
                            backgroundColor = camera.backgroundColor, nearClipPlane = camera.nearClipPlane, farClipPlane = camera.farClipPlane,
                            fieldOfView = camera.fieldOfView, orthographic = camera.orthographic, orthographicSize = camera.orthographicSize,
                            depth = camera.depth, cullingMask = camera.cullingMask, rect = camera.rect, allowHDR = camera.allowHDR,
                            allowMSAA = camera.allowMSAA, allowDynamicResolution = camera.allowDynamicResolution, targetDisplay = camera.targetDisplay,
                            renderingPath = (int)camera.renderingPath, usePhysicalProperties = camera.usePhysicalProperties,
                            lensShift = camera.lensShift, targetTexture = Identity(camera.targetTexture) };
                    }
                    else if (component is Light light)
                    {
                        item.captureKind = "light";
                        item.light = new LightRecord { enabled = light.enabled, type = (int)light.type, color = light.color, intensity = light.intensity,
                            range = light.range, spotAngle = light.spotAngle, innerSpotAngle = light.innerSpotAngle,
                            shadows = (int)light.shadows, shadowStrength = light.shadowStrength, shadowBias = light.shadowBias,
                            shadowNormalBias = light.shadowNormalBias, shadowNearPlane = light.shadowNearPlane,
                            shadowResolution = (int)light.shadowResolution, shadowCustomResolution = light.shadowCustomResolution,
                            cullingMask = light.cullingMask, renderingLayerMask = unchecked((uint)light.renderingLayerMask),
                            bounceIntensity = light.bounceIntensity, useColorTemperature = light.useColorTemperature,
                            colorTemperature = light.colorTemperature, cookie = Identity(light.cookie) };
                    }
                    else if (component is MeshFilter filter) { item.captureKind = "mesh-filter"; item.mesh = Mesh(filter.sharedMesh); }
                    else if (component is MeshRenderer renderer)
                    {
                        item.captureKind = "mesh-renderer";
                        Material[] shared = renderer.sharedMaterials;
                        var materials = new MaterialRecord[shared.Length];
                        for (int materialIndex = 0; materialIndex < shared.Length; materialIndex++) materials[materialIndex] = Material(shared[materialIndex]);
                        item.renderer = new RendererRecord { enabled = renderer.enabled, shadowCastingMode = (int)renderer.shadowCastingMode,
                            receiveShadows = renderer.receiveShadows, lightProbeUsage = (int)renderer.lightProbeUsage,
                            reflectionProbeUsage = (int)renderer.reflectionProbeUsage, renderingLayerMask = renderer.renderingLayerMask,
                            sortingLayerID = renderer.sortingLayerID, sortingOrder = renderer.sortingOrder,
                            lightmapIndex = renderer.lightmapIndex, lightmapScaleOffset = renderer.lightmapScaleOffset,
                            probeAnchor = Identity(renderer.probeAnchor), lightProbeProxyVolumeOverride = Identity(renderer.lightProbeProxyVolumeOverride), materials = materials };
                    }
                    else if (component is Collider collider)
                    {
                        item.captureKind = "collider";
                        item.collider = new ColliderRecord { shape = component.GetType().Name, enabled = collider.enabled,
                            isTrigger = collider.isTrigger, contactOffset = collider.contactOffset, includeLayers = collider.includeLayers.value,
                            excludeLayers = collider.excludeLayers.value, layerOverridePriority = collider.layerOverridePriority,
                            providesContacts = collider.providesContacts, sharedMaterial = Identity(collider.sharedMaterial) };
                        if (collider is BoxCollider box) { item.collider.center = box.center; item.collider.size = box.size; }
                        else if (collider is SphereCollider sphere) { item.collider.center = sphere.center; item.collider.radius = sphere.radius; }
                        else if (collider is CapsuleCollider capsule)
                        { item.collider.center = capsule.center; item.collider.radius = capsule.radius; item.collider.height = capsule.height; item.collider.direction = capsule.direction; }
                    }
                    else if (component is Rigidbody body)
                    {
                        item.captureKind = "rigidbody";
                        item.rigidbody = new RigidbodyRecord { mass = body.mass, linearDamping = body.linearDamping, angularDamping = body.angularDamping,
                            useGravity = body.useGravity, isKinematic = body.isKinematic, interpolation = (int)body.interpolation,
                            constraints = (int)body.constraints, collisionDetectionMode = (int)body.collisionDetectionMode,
                            centerOfMass = body.centerOfMass, inertiaTensor = body.inertiaTensor, inertiaTensorRotation = body.inertiaTensorRotation };
                    }
                    else if (component is Transform) item.captureKind = "transform";
                    else if (component is MonoBehaviour) item.captureKind = "script-json";
                    if (component is MonoBehaviour || item.captureKind == "unknown-json")
                    {
                        try { item.serializedJson = CanonicalJson(JsonUtility.ToJson(component), record.path + ":" + item.typeName, out item.references); }
                        catch (Exception error) { diagnostics.Add(record.path + ":" + item.typeName + " serializer: " + error.GetType().Name + ": " + error.Message); }
                    }
                }
                record.components = result;
            }

            private string Identity(Object value)
            {
                if (value == null) return "null";
                if (identities.TryGetValue(value, out string identity)) return identity;
                identity = "asset:" + value.GetType().FullName + ":" + value.name;
                // A label is deliberately not a GUID claim. Two different objects
                // sharing a name remain visible rather than silently conflated.
                foreach (var pair in assets)
                    if (pair.Key != value && pair.Value.identity == identity)
                        diagnostics.Add("Ambiguous asset identity " + identity + "; compare content records as well as names.");
                identities.Add(value, identity);
                var record = new AssetRecord { identity = identity, typeName = value.GetType().FullName, name = value.name,
                    scalarJson = "{}", references = Array.Empty<ReferenceRecord>() };
                assets.Add(value, record);
                if (value is Texture texture) record.texture = Texture(texture);
                else if (value is Material material) record.material = Material(material);
                else if (value is Mesh mesh) record.mesh = Mesh(mesh);
                else if (value is ScriptableObject)
                {
                    try { record.scalarJson = CanonicalJson(JsonUtility.ToJson(value), identity, out record.references); }
                    catch (Exception error) { diagnostics.Add(identity + " serializer: " + error.GetType().Name + ": " + error.Message); }
                }
                else record.scalarJson = JsonUtility.ToJson(value);
                return identity;
            }

            private MeshRecord Mesh(Mesh mesh)
            {
                if (mesh == null) return null;
                string identity = Identity(mesh);
                var counts = new int[mesh.subMeshCount];
                for (int index = 0; index < counts.Length; index++) counts[index] = checked((int)mesh.GetIndexCount(index));
                return new MeshRecord { identity = identity, name = mesh.name, vertexCount = mesh.vertexCount,
                    subMeshCount = mesh.subMeshCount, indexCounts = counts, bounds = mesh.bounds, isReadable = mesh.isReadable };
            }

            private MaterialRecord Material(Material material)
            {
                if (material == null) return null;
                string identity = Identity(material);
                string[] names = material.GetTexturePropertyNames();
                Array.Sort(names, StringComparer.Ordinal);
                var properties = new TexturePropertyRecord[names.Length];
                for (int index = 0; index < names.Length; index++) properties[index] = new TexturePropertyRecord { property = names[index],
                    identity = Identity(material.GetTexture(names[index])), scale = material.GetTextureScale(names[index]), offset = material.GetTextureOffset(names[index]) };
                var floats = new List<FloatPropertyRecord>();
                var vectors = new List<VectorPropertyRecord>();
                var colors = new List<ColorPropertyRecord>();
                if (material.shader != null)
                    for (int propertyIndex = 0; propertyIndex < material.shader.GetPropertyCount(); propertyIndex++)
                    {
                        string propertyName = material.shader.GetPropertyName(propertyIndex);
                        ShaderPropertyType propertyType = material.shader.GetPropertyType(propertyIndex);
                        if (propertyType == ShaderPropertyType.Float || propertyType == ShaderPropertyType.Range)
                            floats.Add(new FloatPropertyRecord { property = propertyName, kind = propertyType.ToString(), value = material.GetFloat(propertyName) });
                        else if (propertyType == ShaderPropertyType.Int)
                            floats.Add(new FloatPropertyRecord { property = propertyName, kind = propertyType.ToString(), value = material.GetInteger(propertyName) });
                        else if (propertyType == ShaderPropertyType.Vector) vectors.Add(new VectorPropertyRecord { property = propertyName, value = material.GetVector(propertyName) });
                        else if (propertyType == ShaderPropertyType.Color) colors.Add(new ColorPropertyRecord { property = propertyName, value = material.GetColor(propertyName) });
                    }
                floats.Sort((left, right) => string.CompareOrdinal(left.property, right.property));
                vectors.Sort((left, right) => string.CompareOrdinal(left.property, right.property));
                colors.Sort((left, right) => string.CompareOrdinal(left.property, right.property));
                string[] keywords = material.shaderKeywords;
                Array.Sort(keywords, StringComparer.Ordinal);
                return new MaterialRecord { identity = identity, name = material.name, shaderName = material.shader == null ? "null" : material.shader.name,
                    hasColor = material.HasProperty("_Color"), color = material.HasProperty("_Color") ? material.color : Color.clear,
                    renderQueue = material.renderQueue, shaderKeywords = keywords, enableInstancing = material.enableInstancing,
                    doubleSidedGI = material.doubleSidedGI, globalIlluminationFlags = (int)material.globalIlluminationFlags, textures = properties,
                    floats = floats.ToArray(), vectors = vectors.ToArray(), colors = colors.ToArray() };
            }

            private static TextureRecord Texture(Texture texture)
            {
                var result = new TextureRecord { width = texture.width, height = texture.height, dimension = (int)texture.dimension,
                    graphicsFormat = (int)texture.graphicsFormat, isSRGBFormat = UnityEngine.Experimental.Rendering.GraphicsFormatUtility.IsSRGBFormat(texture.graphicsFormat), filterMode = (int)texture.filterMode, wrapModeU = (int)texture.wrapModeU,
                    wrapModeV = (int)texture.wrapModeV, anisoLevel = texture.anisoLevel, mipMapBias = texture.mipMapBias,
                    sampleError = "Not a readable Texture2D." };
                if (texture is Texture2D image)
                {
                    result.format = image.format.ToString(); result.mipmapCount = image.mipmapCount; result.isReadable = image.isReadable;
                    if (image.isReadable)
                    {
                        try
                        {
                            result.firstPixel = image.GetPixel(0, 0); result.centerPixel = image.GetPixel(image.width / 2, image.height / 2);
                            result.lastPixel = image.GetPixel(image.width - 1, image.height - 1); result.sampleAvailable = true; result.sampleError = "";
                        }
                        catch (Exception error) { result.sampleError = error.GetType().Name + ": " + error.Message; }
                    }
                }
                return result;
            }

            private static string ReferenceJson(Object value)
            {
                string wrapper = JsonUtility.ToJson(new ObjectReference { target = value });
                return Json.Field(wrapper, "target");
            }

            private string CanonicalJson(string source, string context, out ReferenceRecord[] references)
            {
                var found = new List<ReferenceRecord>();
                string result = Json.Rewrite(source, (chunk, property) =>
                {
                    if (referenceShape == null || Json.ReferenceShape(chunk) != referenceShape) return null;
                    Object target;
                    try { target = JsonUtility.FromJson<ObjectReference>("{\"target\":" + chunk + "}").target; }
                    catch (Exception error)
                    {
                        diagnostics.Add(context + ":" + property + " reference decode: " + error.GetType().Name + ": " + error.Message);
                        return null;
                    }
                    string identity = target == null ? "null" : Identity(target);
                    found.Add(new ReferenceRecord { property = property, identity = identity,
                        typeName = target == null ? "null" : target.GetType().FullName, name = target == null ? "" : target.name, resolved = target != null });
                    return "{\"reference\":" + Json.Quote(identity) + "}";
                });
                references = found.ToArray();
                return result;
            }
        }

        // This reads JSON spans, rather than engine-private fields. References are
        // recognized from an actual public serializer wrapper for this version.
        private static class Json
        {
            public static string Quote(string value)
            {
                var text = new StringBuilder("\"");
                foreach (char character in value)
                {
                    switch (character)
                    {
                        case '"': text.Append("\\\""); break;
                        case '\\': text.Append("\\\\"); break;
                        case '\b': text.Append("\\b"); break;
                        case '\f': text.Append("\\f"); break;
                        case '\n': text.Append("\\n"); break;
                        case '\r': text.Append("\\r"); break;
                        case '\t': text.Append("\\t"); break;
                        default:
                            if (character < ' ' || char.IsSurrogate(character)) text.Append("\\u").Append(((int)character).ToString("x4", CultureInfo.InvariantCulture));
                            else text.Append(character);
                            break;
                    }
                }
                return text.Append('"').ToString();
            }
            [Serializable] private sealed class Text { public string value = ""; }
            public static string Field(string source, string name)
            {
                int index = 0; SkipSpace(source, ref index); Require(source, ref index, '{');
                while (index < source.Length)
                {
                    SkipSpace(source, ref index); if (source[index] == '}') break;
                    string key = ReadString(source, ref index); SkipSpace(source, ref index); Require(source, ref index, ':'); SkipSpace(source, ref index);
                    int start = index; SkipValue(source, ref index);
                    if (key == name) return source.Substring(start, index - start);
                    SkipSpace(source, ref index); if (source[index] == '}') break; Require(source, ref index, ',');
                }
                return null;
            }
            public static string ReferenceShape(string source)
            {
                if (string.IsNullOrEmpty(source) || source[0] != '{') return null;
                int index = 1; var keys = new List<string>();
                while (index < source.Length)
                {
                    SkipSpace(source, ref index); if (source[index] == '}') break;
                    keys.Add(ReadString(source, ref index)); SkipSpace(source, ref index); Require(source, ref index, ':'); SkipValue(source, ref index);
                    SkipSpace(source, ref index); if (source[index] == '}') break; Require(source, ref index, ',');
                }
                keys.Sort(StringComparer.Ordinal);
                return string.Join("|", keys);
            }
            public static string Rewrite(string source, Func<string, string, string> replace)
            { int index = 0; return RewriteValue(source, ref index, "", replace, 0); }
            private static string RewriteValue(string source, ref int index, string property, Func<string, string, string> replace, int depth)
            {
                if (depth > 128) throw new InvalidOperationException("JSON nesting exceeds observer limit.");
                SkipSpace(source, ref index); int start = index;
                if (source[index] == '{')
                {
                    int end = index; SkipValue(source, ref end);
                    string replacement = replace(source.Substring(index, end - index), property);
                    if (replacement != null) { index = end; return replacement; }
                    index++; var text = new StringBuilder("{"); bool first = true;
                    while (true)
                    {
                        SkipSpace(source, ref index); if (source[index] == '}') { index++; break; }
                        int keyStart = index; string key = ReadString(source, ref index); int keyEnd = index;
                        SkipSpace(source, ref index); Require(source, ref index, ':');
                        if (!first) text.Append(','); first = false;
                        text.Append(source, keyStart, keyEnd - keyStart).Append(':');
                        text.Append(RewriteValue(source, ref index, string.IsNullOrEmpty(property) ? key : property + "." + key, replace, depth + 1));
                        SkipSpace(source, ref index); if (source[index] == '}') { index++; break; } Require(source, ref index, ',');
                    }
                    return text.Append('}').ToString();
                }
                if (source[index] == '[')
                {
                    index++; var text = new StringBuilder("["); int arrayIndex = 0;
                    while (true)
                    {
                        SkipSpace(source, ref index); if (source[index] == ']') { index++; break; }
                        if (arrayIndex != 0) text.Append(',');
                        text.Append(RewriteValue(source, ref index, property + "[" + arrayIndex + "]", replace, depth + 1)); arrayIndex++;
                        SkipSpace(source, ref index); if (source[index] == ']') { index++; break; } Require(source, ref index, ',');
                    }
                    return text.Append(']').ToString();
                }
                SkipValue(source, ref index); return source.Substring(start, index - start);
            }
            private static void SkipValue(string source, ref int index)
            {
                SkipSpace(source, ref index);
                if (source[index] == '"') { ReadString(source, ref index); return; }
                if (source[index] == '{' || source[index] == '[')
                {
                    char end = source[index++] == '{' ? '}' : ']';
                    while (true)
                    {
                        SkipSpace(source, ref index); if (source[index] == end) { index++; return; }
                        if (source[index] == ',' || source[index] == ':') { index++; continue; }
                        SkipValue(source, ref index);
                    }
                }
                while (index < source.Length && source[index] != ',' && source[index] != '}' && source[index] != ']' && !char.IsWhiteSpace(source[index])) index++;
            }
            private static string ReadString(string source, ref int index)
            {
                int start = index; Require(source, ref index, '"');
                while (index < source.Length)
                {
                    char value = source[index++];
                    if (value == '"')
                    {
                        // Property paths only need the decoded key. The same public
                        // serializer handles escaping instead of bespoke Unicode logic.
                        string key = source.Substring(start, index - start);
                        return JsonUtility.FromJson<Text>("{\"value\":" + key + "}").value;
                    }
                    if (value == '\\') index++;
                }
                throw new InvalidOperationException("Unterminated serializer JSON string.");
            }
            private static void SkipSpace(string source, ref int index) { while (index < source.Length && char.IsWhiteSpace(source[index])) index++; }
            private static void Require(string source, ref int index, char expected)
            { if (index >= source.Length || source[index++] != expected) throw new InvalidOperationException("Malformed serializer JSON."); }
        }
    }
}
