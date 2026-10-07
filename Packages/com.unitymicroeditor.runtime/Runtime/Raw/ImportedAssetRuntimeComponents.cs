using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEngine;
using UnityEngine.Audio;
using UnityEngine.Rendering;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private static Type NativeComponentType(int classId)
        {
            switch (classId)
            {
                case 4: return typeof(Transform);
                case 20: return typeof(Camera);
                case 23: return typeof(MeshRenderer);
                case 33: return typeof(MeshFilter);
                case 54: return typeof(Rigidbody);
                case 64: return typeof(MeshCollider);
                case 65: return typeof(BoxCollider);
                case 81: return typeof(AudioListener);
                case 82: return typeof(AudioSource);
                case 95: return typeof(Animator);
                case 108: return typeof(Light);
                case 120: return typeof(LineRenderer);
                case 135: return typeof(SphereCollider);
                case 136: return typeof(CapsuleCollider);
                case 137: return typeof(SkinnedMeshRenderer);
                case 143: return typeof(CharacterController);
                case 198: return typeof(ParticleSystem);
                case 199: return typeof(ParticleSystemRenderer);
                case 205: return typeof(LODGroup);
                case 215: return typeof(ReflectionProbe);
                case 222: return typeof(CanvasRenderer);
                case 223: return typeof(Canvas);
                case 224: return typeof(RectTransform);
                case 225: return typeof(CanvasGroup);
                default: return null;
            }
        }

        private static Dictionary<string, object> Map(object value)
        { return value as Dictionary<string, object> ?? new Dictionary<string, object>(StringComparer.Ordinal); }
        private static object Value(Dictionary<string, object> data, string key, object fallback)
        { return data.TryGetValue(key, out object value) && value != null ? value : fallback; }
        private static int Int(Dictionary<string, object> data, string key, int fallback = 0)
        { return Convert.ToInt32(Value(data, key, fallback), CultureInfo.InvariantCulture); }
        private static float Float(Dictionary<string, object> data, string key, float fallback = 0)
        { return Convert.ToSingle(Value(data, key, fallback), CultureInfo.InvariantCulture); }
        private static uint Bits(Dictionary<string, object> data, string key, uint fallback = 0)
        { return unchecked((uint)Convert.ToInt64(Value(data, key, fallback), CultureInfo.InvariantCulture)); }
        private static bool Bool(Dictionary<string, object> data, string key, bool fallback = false)
        { return Int(data, key, fallback ? 1 : 0) != 0; }
        private static Vector2 Vec2(object value, Vector2 fallback)
        { var map = Map(value); return new Vector2(Float(map, "x", fallback.x), Float(map, "y", fallback.y)); }
        private static Vector3 Vec3(object value, Vector3 fallback)
        { var map = Map(value); return new Vector3(Float(map, "x", fallback.x), Float(map, "y", fallback.y), Float(map, "z", fallback.z)); }
        private static Color Rgba(object value, Color fallback)
        { var map = Map(value); return new Color(Float(map, "r", fallback.r), Float(map, "g", fallback.g), Float(map, "b", fallback.b), Float(map, "a", fallback.a)); }
        private static ReferenceData Reference(object value)
        {
            var map = Map(value);
            return new ReferenceData { fileID = Convert.ToString(Value(map, "fileID", "0"), CultureInfo.InvariantCulture),
                guid = Convert.ToString(Value(map, "guid", ""), CultureInfo.InvariantCulture) };
        }
        private static AnimationCurve Curve(object value)
        {
            if (value == null) return new AnimationCurve();
            if (!(value is Dictionary<string, object>)) throw new InvalidDataException("Serialized AnimationCurve must be a JSON map.");
            // Native serialized infinity values are not WrapMode enum values.
            // Let Unity deserialize its own keyframes, tangents and wrap modes.
            var carrier = JsonUtility.FromJson<CurveCarrier>(RawJson.Write(new Dictionary<string, object> { { "curve", value } }));
            if (carrier == null || carrier.curve == null) throw new InvalidDataException("Unity refused the serialized AnimationCurve payload.");
            return carrier.curve;
        }
        [Serializable] private sealed class CurveCarrier { public AnimationCurve curve; }
        private static Gradient ColorGradient(object value)
        {
            var map = Map(value);
            int colors = Int(map, "m_NumColorKeys", 2), alpha = Int(map, "m_NumAlphaKeys", 2);
            if (colors < 1 || colors > 8 || alpha < 1 || alpha > 8) throw new InvalidDataException("Invalid serialized gradient key count.");
            var colorKeys = new GradientColorKey[colors];
            var alphaKeys = new GradientAlphaKey[alpha];
            for (int index = 0; index < colors; index++)
                colorKeys[index] = new GradientColorKey(Rgba(Value(map, "key" + index, null), Color.white), Float(map, "ctime" + index) / 65535f);
            for (int index = 0; index < alpha; index++)
                alphaKeys[index] = new GradientAlphaKey(Rgba(Value(map, "key" + index, null), Color.white).a, Float(map, "atime" + index) / 65535f);
            int mode = Int(map, "m_Mode"), colorSpace = Int(map, "m_ColorSpace", -1);
            if (mode < 0 || mode > 2 || colorSpace < -1 || colorSpace > 1) throw new InvalidDataException("Unsupported serialized gradient mode or color space.");
            var gradient = new Gradient { mode = (GradientMode)mode, colorSpace = (ColorSpace)colorSpace };
            gradient.SetKeys(colorKeys, alphaKeys);
            return gradient;
        }

        private sealed partial class ImportContext
        {
            private void ConfigureRectTransform(RectTransform transform, RawSceneObject record)
            {
                var map = Map(RawJson.Parse(record.dataJson));
                transform.anchorMin = Vec2(Value(map, "m_AnchorMin", null), new Vector2(.5f, .5f));
                transform.anchorMax = Vec2(Value(map, "m_AnchorMax", null), new Vector2(.5f, .5f));
                transform.pivot = Vec2(Value(map, "m_Pivot", null), new Vector2(.5f, .5f));
                transform.sizeDelta = Vec2(Value(map, "m_SizeDelta", null), new Vector2(100, 100));
                transform.anchoredPosition = Vec2(Value(map, "m_AnchoredPosition", null), Vector2.zero);
                Vector3 position = transform.localPosition;
                position.z = Vec3(Value(map, "m_LocalPosition", null), Vector3.zero).z;
                transform.localPosition = position;
            }

            private void ConfigureRenderer(Renderer renderer, RawSceneObject record)
            {
                var data = ReadData<RendererData>(record.dataJson);
                renderer.enabled = data.m_Enabled != 0;
                renderer.shadowCastingMode = (ShadowCastingMode)data.m_CastShadows;
                renderer.receiveShadows = data.m_ReceiveShadows != 0;
                renderer.lightProbeUsage = (LightProbeUsage)data.m_LightProbeUsage;
                renderer.reflectionProbeUsage = (ReflectionProbeUsage)data.m_ReflectionProbeUsage;
                var materials = new Material[data.m_Materials == null ? 0 : data.m_Materials.Length];
                for (int index = 0; index < materials.Length; index++) materials[index] = (Material)Resolve(data.m_Materials[index]);
                renderer.sharedMaterials = materials;
                var map = Map(RawJson.Parse(record.dataJson));
                renderer.motionVectorGenerationMode = (MotionVectorGenerationMode)Int(map, "m_MotionVectors", 1);
                renderer.allowOcclusionWhenDynamic = Bool(map, "m_DynamicOccludee", true);
                renderer.renderingLayerMask = Bits(map, "m_RenderingLayerMask", 1);
                renderer.sortingLayerID = Int(map, "m_SortingLayerID");
                renderer.sortingOrder = Int(map, "m_SortingOrder");
                renderer.probeAnchor = Resolve(Reference(Value(map, "m_ProbeAnchor", null))) as Transform;
                renderer.lightProbeProxyVolumeOverride = Resolve(Reference(Value(map, "m_LightProbeVolumeOverride", null))) as GameObject;
            }

            private void ConfigureCollider(Collider collider, Dictionary<string, object> map, bool trigger = true)
            {
                collider.enabled = Bool(map, "m_Enabled", true);
                if (trigger) collider.isTrigger = Bool(map, "m_IsTrigger");
                collider.sharedMaterial = Resolve(Reference(Value(map, "m_Material", null))) as PhysicsMaterial;
                collider.includeLayers = unchecked((int)Bits(Map(Value(map, "m_IncludeLayers", null)), "m_Bits"));
                collider.excludeLayers = unchecked((int)Bits(Map(Value(map, "m_ExcludeLayers", null)), "m_Bits"));
                collider.layerOverridePriority = Int(map, "m_LayerOverridePriority");
                collider.providesContacts = Bool(map, "m_ProvidesContacts");
            }

            private void ConfigureNativeComponent(RawSceneObject record)
            {
                if (!IsComponent(record.classId) || IsTransform(record.classId)) return;
                var map = Map(RawJson.Parse(record.dataJson));
                Component component = (Component)sceneObjects[record.fileId];
                switch (record.classId)
                {
                    case 64:
                    {
                        var collider = (MeshCollider)component;
                        collider.cookingOptions = (MeshColliderCookingOptions)Int(map, "m_CookingOptions", 30);
                        collider.convex = Bool(map, "m_Convex");
                        var mesh = Resolve(Reference(Value(map, "m_Mesh", null))) as Mesh;
                        PrepareNativeMeshCollider(collider, mesh);
                        collider.sharedMesh = mesh;
                        ConfigureCollider(collider, map); break;
                    }
                    case 81: ((AudioListener)component).enabled = Bool(map, "m_Enabled", true); break;
                    case 82:
                    {
                        var audio = (AudioSource)component;
                        audio.enabled = Bool(map, "m_Enabled", true);
                        audio.clip = Resolve(Reference(Value(map, "m_audioClip", null))) as AudioClip;
                        audio.resource = Resolve(Reference(Value(map, "m_Resource", null))) as AudioResource;
                        if (audio.resource == null && audio.clip == null)
                            audio.clip = Resolve(Reference(Value(map, "m_audioClip", null))) as AudioClip;
                        audio.outputAudioMixerGroup = Resolve(Reference(Value(map, "OutputAudioMixerGroup", null))) as AudioMixerGroup;
                        audio.playOnAwake = Bool(map, "m_PlayOnAwake", true);
                        audio.volume = Float(map, "m_Volume", 1); audio.pitch = Float(map, "m_Pitch", 1);
                        audio.loop = Bool(map, "Loop"); audio.mute = Bool(map, "Mute");
                        audio.spatialize = Bool(map, "Spatialize"); audio.spatializePostEffects = Bool(map, "SpatializePostEffects");
                        audio.priority = Int(map, "Priority", 128); audio.dopplerLevel = Float(map, "DopplerLevel", 1);
                        audio.minDistance = Float(map, "MinDistance", 1); audio.maxDistance = Float(map, "MaxDistance", 500);
                        audio.panStereo = Float(map, "Pan2D"); audio.rolloffMode = (AudioRolloffMode)Int(map, "rolloffMode");
                        audio.bypassEffects = Bool(map, "BypassEffects"); audio.bypassListenerEffects = Bool(map, "BypassListenerEffects");
                        audio.bypassReverbZones = Bool(map, "BypassReverbZones");
                        SetAudioCurve(audio, map, "rolloffCustomCurve", AudioSourceCurveType.CustomRolloff);
                        SetAudioCurve(audio, map, "panLevelCustomCurve", AudioSourceCurveType.SpatialBlend);
                        SetAudioCurve(audio, map, "spreadCustomCurve", AudioSourceCurveType.Spread);
                        SetAudioCurve(audio, map, "reverbZoneMixCustomCurve", AudioSourceCurveType.ReverbZoneMix);
                        break;
                    }
                    case 95:
                    {
                        var animator = (Animator)component;
                        animator.enabled = Bool(map, "m_Enabled", true);
                        animator.avatar = Resolve(Reference(Value(map, "m_Avatar", null))) as Avatar;
                        animator.runtimeAnimatorController = Resolve(Reference(Value(map, "m_Controller", null))) as RuntimeAnimatorController;
                        animator.cullingMode = (AnimatorCullingMode)Int(map, "m_CullingMode");
                        animator.updateMode = (AnimatorUpdateMode)Int(map, "m_UpdateMode");
                        animator.applyRootMotion = Bool(map, "m_ApplyRootMotion");
                        animator.stabilizeFeet = Bool(map, "m_StabilizeFeet");
                        animator.animatePhysics = Bool(map, "m_AnimatePhysics");
                        animator.keepAnimatorStateOnDisable = Bool(map, "m_KeepAnimatorStateOnDisable");
                        animator.writeDefaultValuesOnDisable = Bool(map, "m_WriteDefaultValuesOnDisable");
                        break;
                    }
                    case 120:
                    {
                        var line = (LineRenderer)component; ConfigureRenderer(line, record);
                        var parameters = Map(Value(map, "m_Parameters", null));
                        line.widthMultiplier = Float(parameters, "widthMultiplier", 1);
                        if (parameters.ContainsKey("widthCurve")) line.widthCurve = Curve(parameters["widthCurve"]);
                        if (parameters.ContainsKey("colorGradient")) line.colorGradient = ColorGradient(parameters["colorGradient"]);
                        line.numCornerVertices = Int(parameters, "numCornerVertices"); line.numCapVertices = Int(parameters, "numCapVertices");
                        line.alignment = (LineAlignment)Int(parameters, "alignment"); line.textureMode = (LineTextureMode)Int(parameters, "textureMode");
                        line.textureScale = Vec2(Value(parameters, "textureScale", null), Vector2.one);
                        line.shadowBias = Float(parameters, "shadowBias", .5f); line.generateLightingData = Bool(parameters, "generateLightingData");
                        line.useWorldSpace = Bool(map, "m_UseWorldSpace", true); line.loop = Bool(map, "m_Loop");
                        var positions = Value(map, "m_Positions", null) as List<object> ?? new List<object>();
                        line.positionCount = positions.Count;
                        for (int index = 0; index < positions.Count; index++) line.SetPosition(index, Vec3(positions[index], Vector3.zero));
                        break;
                    }
                    case 137:
                    {
                        var renderer = (SkinnedMeshRenderer)component; ConfigureRenderer(renderer, record);
                        renderer.sharedMesh = Resolve(Reference(Value(map, "m_Mesh", null))) as Mesh;
                        renderer.rootBone = Resolve(Reference(Value(map, "m_RootBone", null))) as Transform;
                        var bones = Value(map, "m_Bones", null) as List<object> ?? new List<object>();
                        var transforms = new Transform[bones.Count];
                        for (int index = 0; index < bones.Count; index++) transforms[index] = Resolve(Reference(bones[index])) as Transform;
                        renderer.bones = transforms; renderer.quality = (SkinQuality)Int(map, "m_Quality");
                        renderer.updateWhenOffscreen = Bool(map, "m_UpdateWhenOffscreen"); renderer.skinnedMotionVectors = Bool(map, "m_SkinnedMotionVectors", true);
                        var bounds = Map(Value(map, "m_AABB", null));
                        renderer.localBounds = new Bounds(Vec3(Value(bounds, "m_Center", null), Vector3.zero), Vec3(Value(bounds, "m_Extent", null), Vector3.zero) * 2);
                        if (Value(map, "m_BlendShapeWeights", null) is List<object> weights)
                            for (int index = 0; index < weights.Count; index++) renderer.SetBlendShapeWeight(index, Convert.ToSingle(weights[index], CultureInfo.InvariantCulture));
                        break;
                    }
                    case 143:
                    {
                        var controller = (CharacterController)component; ConfigureCollider(controller, map, false);
                        controller.height = Float(map, "m_Height", 2); controller.radius = Float(map, "m_Radius", .5f);
                        controller.center = Vec3(Value(map, "m_Center", null), Vector3.zero); controller.slopeLimit = Float(map, "m_SlopeLimit", 45);
                        controller.stepOffset = Float(map, "m_StepOffset", .3f); controller.skinWidth = Float(map, "m_SkinWidth", .08f);
                        controller.minMoveDistance = Float(map, "m_MinMoveDistance", .001f); break;
                    }
                    case 198: ConfigureParticles((ParticleSystem)component, map, record); break;
                    case 199: ConfigureParticleRenderer((ParticleSystemRenderer)component, map, record); break;
                    case 205:
                    {
                        var group = (LODGroup)component;
                        var authored = Value(map, "m_LODs", null) as List<object> ?? new List<object>();
                        var lods = new LOD[authored.Count];
                        for (int index = 0; index < authored.Count; index++)
                        {
                            var level = Map(authored[index]);
                            var references = Value(level, "renderers", null) as List<object> ?? new List<object>();
                            var renderers = new Renderer[references.Count];
                            for (int renderer = 0; renderer < references.Count; renderer++)
                                renderers[renderer] = Resolve(Reference(Value(Map(references[renderer]), "renderer", references[renderer]))) as Renderer;
                            lods[index] = new LOD(Float(level, "screenRelativeHeight"), renderers)
                            { fadeTransitionWidth = Float(level, "fadeTransitionWidth") };
                        }
                        group.SetLODs(lods);
                        group.localReferencePoint = Vec3(Value(map, "m_LocalReferencePoint", null), Vector3.zero);
                        group.size = Float(map, "m_Size", 1); group.fadeMode = (LODFadeMode)Int(map, "m_FadeMode");
                        group.animateCrossFading = Bool(map, "m_AnimateCrossFading"); group.enabled = Bool(map, "m_Enabled", true);
                        break;
                    }
                    case 215: ConfigureReflectionProbe((ReflectionProbe)component, map, record); break;
                    case 222:
                        ((CanvasRenderer)component).cullTransparentMesh = Bool(map, "m_CullTransparentMesh", true); break;
                    case 223:
                    {
                        var canvas = (Canvas)component;
                        canvas.enabled = Bool(map, "m_Enabled", true); canvas.renderMode = (RenderMode)Int(map, "m_RenderMode");
                        canvas.worldCamera = Resolve(Reference(Value(map, "m_Camera", null))) as Camera;
                        canvas.planeDistance = Float(map, "m_PlaneDistance", 100); canvas.pixelPerfect = Bool(map, "m_PixelPerfect");
                        canvas.overrideSorting = Bool(map, "m_OverrideSorting"); canvas.sortingLayerID = Int(map, "m_SortingLayerID");
                        canvas.sortingOrder = Int(map, "m_SortingOrder"); canvas.targetDisplay = Int(map, "m_TargetDisplay");
                        canvas.additionalShaderChannels = (AdditionalCanvasShaderChannels)Int(map, "m_AdditionalShaderChannelsFlag"); break;
                    }
                    case 225:
                    {
                        var group = (CanvasGroup)component; group.enabled = Bool(map, "m_Enabled", true);
                        group.alpha = Float(map, "m_Alpha", 1); group.interactable = Bool(map, "m_Interactable", true);
                        group.blocksRaycasts = Bool(map, "m_BlocksRaycasts", true); group.ignoreParentGroups = Bool(map, "m_IgnoreParentGroups"); break;
                    }
                    default: throw Error(record, "Native component has no public API adapter.");
                }
            }

            private static void SetAudioCurve(AudioSource audio, Dictionary<string, object> map, string field, AudioSourceCurveType kind)
            { if (map.TryGetValue(field, out object curve)) audio.SetCustomCurve(kind, Curve(curve)); }
        }
    }
}
