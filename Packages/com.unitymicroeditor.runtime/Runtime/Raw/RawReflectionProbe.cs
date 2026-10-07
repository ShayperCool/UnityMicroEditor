using System;
using System.Collections.Generic;
using UnityEngine;
using UnityEngine.Rendering;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private sealed partial class ImportContext
        {
            private void ConfigureReflectionProbe(ReflectionProbe probe, Dictionary<string, object> data, RawSceneObject record)
            {
                if (Int(data, "m_Type") != 0)
                    throw Error(record, "Only cubemap ReflectionProbe components are supported.");
                if (!Bool(data, "m_UseOcclusionCulling", true))
                    throw Error(record, "ReflectionProbe occlusion culling has no public runtime setter; only the native default is supported.");
                probe.mode = (ReflectionProbeMode)Int(data, "m_Mode");
                probe.refreshMode = (ReflectionProbeRefreshMode)Int(data, "m_RefreshMode");
                probe.timeSlicingMode = (ReflectionProbeTimeSlicingMode)Int(data, "m_TimeSlicingMode");
                probe.resolution = Int(data, "m_Resolution", 128);
                probe.size = Vec3(Value(data, "m_BoxSize", null), Vector3.one);
                probe.center = Vec3(Value(data, "m_BoxOffset", null), Vector3.zero);
                probe.nearClipPlane = Float(data, "m_NearClip", .3f);
                probe.farClipPlane = Float(data, "m_FarClip", 1000);
                probe.shadowDistance = Float(data, "m_ShadowDistance", 100);
                probe.clearFlags = (ReflectionProbeClearFlags)Int(data, "m_ClearFlags", 1);
                probe.backgroundColor = Rgba(Value(data, "m_BackGroundColor", null), Color.black);
                probe.cullingMask = unchecked((int)Bits(Map(Value(data, "m_CullingMask", null)), "m_Bits", uint.MaxValue));
                probe.intensity = Float(data, "m_IntensityMultiplier", 1);
                probe.blendDistance = Float(data, "m_BlendDistance");
                probe.hdr = Bool(data, "m_HDR", true);
                probe.boxProjection = Bool(data, "m_BoxProjection");
                probe.renderDynamicObjects = Bool(data, "m_RenderDynamicObjects");
                probe.importance = Int(data, "m_Importance", 1);
                Texture custom = Resolve(Reference(Value(data, "m_CustomBakedTexture", null))) as Texture;
                probe.customBakedTexture = custom;
                if (data.ContainsKey("m_BakedTexture"))
                    probe.bakedTexture = Resolve(Reference(data["m_BakedTexture"])) as Texture;
                probe.enabled = Bool(data, "m_Enabled", true);
                if (probe.customBakedTexture != custom || probe.size != Vec3(Value(data, "m_BoxSize", null), Vector3.one)
                    || probe.center != Vec3(Value(data, "m_BoxOffset", null), Vector3.zero)
                    || probe.mode != (ReflectionProbeMode)Int(data, "m_Mode")
                    || probe.resolution != Int(data, "m_Resolution", 128)
                    || Math.Abs(probe.intensity - Float(data, "m_IntensityMultiplier", 1)) > .00001f)
                    throw Error(record, "ReflectionProbe public API readback differs from source settings.");
            }
        }
    }
}
