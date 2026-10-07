using System;
using System.Collections.Generic;
using UnityEngine;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private static ParticleSystem.MinMaxCurve ParticleCurve(object value)
        {
            var map = Map(value);
            int mode = Int(map, "minMaxState");
            if (mode < 0 || mode > 3) throw new InvalidOperationException("Unsupported ParticleSystem curve mode: " + mode);
            var curve = new ParticleSystem.MinMaxCurve(Float(map, "scalar", 1)) { mode = (ParticleSystemCurveMode)mode };
            curve.constantMin = Float(map, "minScalar"); curve.constantMax = Float(map, "scalar", 1);
            curve.curveMultiplier = Float(map, "scalar", 1);
            curve.curveMin = Curve(Value(map, "minCurve", null)); curve.curveMax = Curve(Value(map, "maxCurve", null));
            return curve;
        }
        private static ParticleSystem.MinMaxGradient ParticleGradient(object value)
        {
            var map = Map(value);
            int mode = Int(map, "minMaxState");
            if (mode < 0 || mode > 4) throw new InvalidOperationException("Unsupported ParticleSystem gradient mode: " + mode);
            var gradient = new ParticleSystem.MinMaxGradient(Rgba(Value(map, "maxColor", null), Color.white))
            { mode = (ParticleSystemGradientMode)mode };
            gradient.colorMin = Rgba(Value(map, "minColor", null), Color.white); gradient.colorMax = Rgba(Value(map, "maxColor", null), Color.white);
            if (map.TryGetValue("minGradient", out object min)) gradient.gradientMin = ColorGradient(min);
            if (map.TryGetValue("maxGradient", out object max)) gradient.gradientMax = ColorGradient(max);
            return gradient;
        }

        private static bool SameParticleCurve(ParticleSystem.MinMaxCurve actual, ParticleSystem.MinMaxCurve expected)
        {
            if (actual.mode != expected.mode) return false;
            switch (expected.mode)
            {
                case ParticleSystemCurveMode.Constant: return actual.constant.Equals(expected.constant);
                case ParticleSystemCurveMode.TwoConstants: return actual.constantMin.Equals(expected.constantMin) && actual.constantMax.Equals(expected.constantMax);
                case ParticleSystemCurveMode.Curve: return actual.curveMultiplier.Equals(expected.curveMultiplier) && SameParticleAnimationCurve(actual.curveMax, expected.curveMax);
                case ParticleSystemCurveMode.TwoCurves: return actual.curveMultiplier.Equals(expected.curveMultiplier) && SameParticleAnimationCurve(actual.curveMin, expected.curveMin) && SameParticleAnimationCurve(actual.curveMax, expected.curveMax);
                default: return false;
            }
        }

        private static bool SameParticleAnimationCurve(AnimationCurve actual, AnimationCurve expected)
        {
            if (actual == null || expected == null) return actual == null && expected == null;
            if (actual.preWrapMode != expected.preWrapMode || actual.postWrapMode != expected.postWrapMode || actual.length != expected.length) return false;
            for (int index = 0; index < expected.length; index++)
            {
                Keyframe a = actual[index], b = expected[index];
                if (!a.time.Equals(b.time) || !a.value.Equals(b.value) || !a.inTangent.Equals(b.inTangent) || !a.outTangent.Equals(b.outTangent) ||
                    a.weightedMode != b.weightedMode || !a.inWeight.Equals(b.inWeight) || !a.outWeight.Equals(b.outWeight)) return false;
            }
            return true;
        }

        private static bool SameParticleGradient(ParticleSystem.MinMaxGradient actual, ParticleSystem.MinMaxGradient expected)
        {
            if (actual.mode != expected.mode) return false;
            switch (expected.mode)
            {
                case ParticleSystemGradientMode.Color: return actual.color.Equals(expected.color);
                case ParticleSystemGradientMode.TwoColors: return actual.colorMin.Equals(expected.colorMin) && actual.colorMax.Equals(expected.colorMax);
                case ParticleSystemGradientMode.Gradient:
                case ParticleSystemGradientMode.RandomColor: return SameParticleColorGradient(actual.gradientMax, expected.gradientMax);
                case ParticleSystemGradientMode.TwoGradients: return SameParticleColorGradient(actual.gradientMin, expected.gradientMin) && SameParticleColorGradient(actual.gradientMax, expected.gradientMax);
                default: return false;
            }
        }

        private static bool SameParticleColorGradient(Gradient actual, Gradient expected)
        {
            if (actual == null || expected == null) return actual == null && expected == null;
            if (actual.mode != expected.mode || actual.colorSpace != expected.colorSpace) return false;
            GradientColorKey[] a = actual.colorKeys, b = expected.colorKeys;
            GradientAlphaKey[] aa = actual.alphaKeys, ba = expected.alphaKeys;
            if (a.Length != b.Length || aa.Length != ba.Length) return false;
            for (int index = 0; index < b.Length; index++)
                if (!a[index].color.Equals(b[index].color) || !a[index].time.Equals(b[index].time)) return false;
            for (int index = 0; index < ba.Length; index++)
                if (!aa[index].alpha.Equals(ba[index].alpha) || !aa[index].time.Equals(ba[index].time)) return false;
            return true;
        }

        private sealed partial class ImportContext
        {
            private void ConfigureParticles(ParticleSystem system, Dictionary<string, object> map, RawSceneObject record)
            {
                system.Stop(true, ParticleSystemStopBehavior.StopEmittingAndClear);
                foreach (var pair in map)
                {
                    if (!(pair.Value is Dictionary<string, object> module) || !Bool(module, "enabled") || pair.Key == "InitialModule" ||
                        pair.Key == "EmissionModule" || pair.Key == "ShapeModule" || pair.Key == "ColorModule" || pair.Key == "SizeModule") continue;
                    throw Error(record, "Enabled ParticleSystem module requires a public API adapter: " + pair.Key);
                }
                var main = system.main;
                var initial = Map(Value(map, "InitialModule", null));
                main.duration = Float(map, "lengthInSec", 5); main.loop = Bool(map, "looping", true);
                main.prewarm = Bool(map, "prewarm"); main.playOnAwake = Bool(map, "playOnAwake", true);
                main.simulationSpeed = Float(map, "simulationSpeed", 1); main.useUnscaledTime = Bool(map, "useUnscaledTime");
                main.stopAction = (ParticleSystemStopAction)Int(map, "stopAction"); main.cullingMode = (ParticleSystemCullingMode)Int(map, "cullingMode");
                main.ringBufferMode = (ParticleSystemRingBufferMode)Int(map, "ringBufferMode");
                main.ringBufferLoopRange = Vec2(Value(map, "ringBufferLoopRange", null), new Vector2(0, 1));
                main.emitterVelocityMode = (ParticleSystemEmitterVelocityMode)Int(map, "emitterVelocityMode", 1);
                int space = Int(map, "moveWithTransform", 1);
                main.simulationSpace = space == 1 ? ParticleSystemSimulationSpace.Local : space == 0 ? ParticleSystemSimulationSpace.World : ParticleSystemSimulationSpace.Custom;
                main.customSimulationSpace = Resolve(Reference(Value(map, "moveWithCustomTransform", null))) as Transform;
                main.scalingMode = (ParticleSystemScalingMode)Int(map, "scalingMode", 1);
                main.startDelay = ParticleCurve(Value(map, "startDelay", null));
                main.startLifetime = ParticleCurve(Value(initial, "startLifetime", null));
                main.startSpeed = ParticleCurve(Value(initial, "startSpeed", null));
                main.startColor = ParticleGradient(Value(initial, "startColor", null));
                main.startSize3D = Bool(initial, "size3D"); main.startSizeX = ParticleCurve(Value(initial, "startSize", null));
                main.startSizeY = ParticleCurve(Value(initial, "startSizeY", null)); main.startSizeZ = ParticleCurve(Value(initial, "startSizeZ", null));
                main.startRotation3D = Bool(initial, "rotation3D"); main.startRotationX = ParticleCurve(Value(initial, "startRotationX", null));
                main.startRotationY = ParticleCurve(Value(initial, "startRotationY", null)); main.startRotationZ = ParticleCurve(Value(initial, "startRotation", null));
                main.flipRotation = Float(initial, "randomizeRotationDirection"); main.gravityModifier = ParticleCurve(Value(initial, "gravityModifier", null));
                main.maxParticles = Int(initial, "maxNumParticles", 1000);
                uint seed = Bits(map, "randomSeed");
                bool autoSeed = Bool(map, "autoRandomSeed", true);
                // randomSeed disables automatic seeding; restore the authored flag last.
                system.randomSeed = seed; system.useAutoRandomSeed = autoSeed;
                if (system.randomSeed != seed || system.useAutoRandomSeed != autoSeed)
                    throw Error(record, "ParticleSystem random seed readback differs from the authored settings.");
                var emission = system.emission; emission.enabled = false;
                var shape = system.shape; shape.enabled = false;
                var velocity = system.velocityOverLifetime; velocity.enabled = false;
                var limitVelocity = system.limitVelocityOverLifetime; limitVelocity.enabled = false;
                var inheritVelocity = system.inheritVelocity; inheritVelocity.enabled = false;
                var lifetime = system.lifetimeByEmitterSpeed; lifetime.enabled = false;
                var force = system.forceOverLifetime; force.enabled = false;
                var color = system.colorOverLifetime; color.enabled = false;
                var colorBySpeed = system.colorBySpeed; colorBySpeed.enabled = false;
                var size = system.sizeOverLifetime; size.enabled = false;
                var sizeBySpeed = system.sizeBySpeed; sizeBySpeed.enabled = false;
                var rotation = system.rotationOverLifetime; rotation.enabled = false;
                var rotationBySpeed = system.rotationBySpeed; rotationBySpeed.enabled = false;
                var external = system.externalForces; external.enabled = false;
                var noise = system.noise; noise.enabled = false;
                var collision = system.collision; collision.enabled = false;
                var trigger = system.trigger; trigger.enabled = false;
                var subEmitters = system.subEmitters; subEmitters.enabled = false;
                var texture = system.textureSheetAnimation; texture.enabled = false;
                var lights = system.lights; lights.enabled = false;
                var trails = system.trails; trails.enabled = false;
                var custom = system.customData; custom.enabled = false;
                ConfigureParticleEmission(system, Map(Value(map, "EmissionModule", null)), record);
                ConfigureParticleShape(system, Map(Value(map, "ShapeModule", null)), record);
                ConfigureParticleColor(system, Map(Value(map, "ColorModule", null)), record);
                ConfigureParticleSize(system, Map(Value(map, "SizeModule", null)), record);
            }

            private void ConfigureParticleEmission(ParticleSystem system, Dictionary<string, object> map, RawSceneObject record)
            {
                if (!Bool(map, "enabled")) return;
                if (Int(map, "serializedVersion", 4) != 4) throw Error(record, "Unsupported ParticleSystem emission serialization version.");
                var module = system.emission;
                var time = ParticleCurve(Value(map, "rateOverTime", null));
                var distance = ParticleCurve(Value(map, "rateOverDistance", null));
                var sourceBursts = Value(map, "m_Bursts", null) as List<object> ?? new List<object>();
                if (Int(map, "m_BurstCount") != sourceBursts.Count) throw Error(record, "ParticleSystem burst count does not match its source array.");
                var bursts = new ParticleSystem.Burst[sourceBursts.Count];
                for (int index = 0; index < bursts.Length; index++)
                {
                    var burst = Map(sourceBursts[index]);
                    if (Int(burst, "serializedVersion", 2) != 2) throw Error(record, "Unsupported ParticleSystem burst serialization version.");
                    bursts[index] = new ParticleSystem.Burst(Float(burst, "time"), ParticleCurve(Value(burst, "countCurve", null)),
                        Int(burst, "cycleCount", 1), Float(burst, "repeatInterval", 1)) { probability = Float(burst, "probability", 1) };
                }
                module.rateOverTime = time; module.rateOverDistance = distance;
                module.SetBursts(bursts); module.enabled = true;
                var observed = new ParticleSystem.Burst[bursts.Length];
                if (!SameParticleCurve(module.rateOverTime, time) || !SameParticleCurve(module.rateOverDistance, distance) ||
                    !module.enabled || module.burstCount != bursts.Length || module.GetBursts(observed) != bursts.Length)
                    throw Error(record, "ParticleSystem emission readback differs from the authored settings.");
                for (int index = 0; index < bursts.Length; index++)
                    if (!observed[index].time.Equals(bursts[index].time) || observed[index].cycleCount != bursts[index].cycleCount ||
                        !observed[index].repeatInterval.Equals(bursts[index].repeatInterval) || !observed[index].probability.Equals(bursts[index].probability) ||
                        !SameParticleCurve(observed[index].count, bursts[index].count))
                        throw Error(record, "ParticleSystem burst readback differs from the authored settings at index " + index + ".");
            }

            private void ConfigureParticleShape(ParticleSystem system, Dictionary<string, object> map, RawSceneObject record)
            {
                if (!Bool(map, "enabled")) return;
                if (Int(map, "serializedVersion", 6) != 6) throw Error(record, "Unsupported ParticleSystem shape serialization version.");
                int type = Int(map, "type");
                // Implement the two authored geometric shapes in the project. Mesh/sprite emission and legacy shell modes need their own adapters.
                if (type != (int)ParticleSystemShapeType.Cone && type != (int)ParticleSystemShapeType.Box)
                    throw Error(record, "Unsupported ParticleSystem emission shape: " + type);
                foreach (string field in new[] { "m_Mesh", "m_MeshRenderer", "m_SkinnedMeshRenderer", "m_Sprite", "m_SpriteRenderer", "m_Texture" })
                    if (Reference(Value(map, field, null)).fileID != "0") throw Error(record, "ParticleSystem shape source reference requires an adapter: " + field);
                if (Int(map, "placementMode") != 0) throw Error(record, "Unsupported ParticleSystem shape placement mode.");
                var radius = Map(Value(map, "radius", null));
                var arc = Map(Value(map, "arc", null));
                int radiusMode = Int(radius, "mode"), arcMode = Int(arc, "mode");
                if (radiusMode < 0 || radiusMode > 3 || arcMode < 0 || arcMode > 3) throw Error(record, "Unsupported ParticleSystem shape distribution mode.");
                var module = system.shape;
                module.shapeType = (ParticleSystemShapeType)type;
                module.angle = Float(map, "angle", 25); module.length = Float(map, "length", 5);
                module.donutRadius = Float(map, "donutRadius", .2f);
                module.radius = Float(radius, "value", 1); module.radiusThickness = Float(map, "radiusThickness", 1);
                module.radiusMode = (ParticleSystemShapeMultiModeValue)radiusMode; module.radiusSpread = Float(radius, "spread");
                var radiusSpeed = ParticleCurve(Value(radius, "speed", null)); module.radiusSpeed = radiusSpeed;
                module.arc = Float(arc, "value", 360); module.arcMode = (ParticleSystemShapeMultiModeValue)arcMode; module.arcSpread = Float(arc, "spread");
                var arcSpeed = ParticleCurve(Value(arc, "speed", null)); module.arcSpeed = arcSpeed;
                module.boxThickness = Vec3(Value(map, "boxThickness", null), Vector3.zero);
                module.position = Vec3(Value(map, "m_Position", null), Vector3.zero);
                module.rotation = Vec3(Value(map, "m_Rotation", null), Vector3.zero);
                module.scale = Vec3(Value(map, "m_Scale", null), Vector3.one);
                module.alignToDirection = Bool(map, "alignToDirection");
                module.randomDirectionAmount = Float(map, "randomDirectionAmount");
                module.sphericalDirectionAmount = Float(map, "sphericalDirectionAmount");
                module.randomPositionAmount = Float(map, "randomPositionAmount");
                module.enabled = true;
                if (!module.enabled || (int)module.shapeType != type || !module.angle.Equals(Float(map, "angle", 25)) || !module.length.Equals(Float(map, "length", 5)) ||
                    !module.donutRadius.Equals(Float(map, "donutRadius", .2f)) || !module.radius.Equals(Float(radius, "value", 1)) ||
                    !module.radiusThickness.Equals(Float(map, "radiusThickness", 1)) || (int)module.radiusMode != radiusMode || (int)module.arcMode != arcMode ||
                    !module.radiusSpread.Equals(Float(radius, "spread")) || !module.arc.Equals(Float(arc, "value", 360)) || !module.arcSpread.Equals(Float(arc, "spread")) ||
                    !SameParticleCurve(module.radiusSpeed, radiusSpeed) || !SameParticleCurve(module.arcSpeed, arcSpeed) ||
                    !module.boxThickness.Equals(Vec3(Value(map, "boxThickness", null), Vector3.zero)) || !module.position.Equals(Vec3(Value(map, "m_Position", null), Vector3.zero)) ||
                    !module.rotation.Equals(Vec3(Value(map, "m_Rotation", null), Vector3.zero)) || !module.scale.Equals(Vec3(Value(map, "m_Scale", null), Vector3.one)) ||
                    module.alignToDirection != Bool(map, "alignToDirection") || !module.randomDirectionAmount.Equals(Float(map, "randomDirectionAmount")) ||
                    !module.sphericalDirectionAmount.Equals(Float(map, "sphericalDirectionAmount")) || !module.randomPositionAmount.Equals(Float(map, "randomPositionAmount")))
                    throw Error(record, "ParticleSystem shape readback differs from the authored settings.");
            }

            private void ConfigureParticleColor(ParticleSystem system, Dictionary<string, object> map, RawSceneObject record)
            {
                if (!Bool(map, "enabled")) return;
                var source = ParticleGradient(Value(map, "gradient", null));
                var module = system.colorOverLifetime;
                module.color = source; module.enabled = true;
                if (!module.enabled || !SameParticleGradient(module.color, source)) throw Error(record, "ParticleSystem color-over-lifetime readback differs from the authored settings.");
            }

            private void ConfigureParticleSize(ParticleSystem system, Dictionary<string, object> map, RawSceneObject record)
            {
                if (!Bool(map, "enabled")) return;
                var x = ParticleCurve(Value(map, "curve", null));
                var y = ParticleCurve(Value(map, "y", null)); var z = ParticleCurve(Value(map, "z", null));
                var module = system.sizeOverLifetime;
                module.x = x; module.y = y; module.z = z;
                module.separateAxes = Bool(map, "separateAxes"); module.enabled = true;
                if (!module.enabled || module.separateAxes != Bool(map, "separateAxes") || !SameParticleCurve(module.x, x) ||
                    !SameParticleCurve(module.y, y) || !SameParticleCurve(module.z, z))
                    throw Error(record, "ParticleSystem size-over-lifetime readback differs from the authored settings.");
            }

            private void ConfigureParticleRenderer(ParticleSystemRenderer renderer, Dictionary<string, object> map, RawSceneObject record)
            {
                ConfigureRenderer(renderer, record);
                renderer.renderMode = (ParticleSystemRenderMode)Int(map, "m_RenderMode");
                renderer.sortMode = (ParticleSystemSortMode)Int(map, "m_SortMode");
                renderer.minParticleSize = Float(map, "m_MinParticleSize"); renderer.maxParticleSize = Float(map, "m_MaxParticleSize", .5f);
                renderer.cameraVelocityScale = Float(map, "m_CameraVelocityScale"); renderer.velocityScale = Float(map, "m_VelocityScale");
                renderer.lengthScale = Float(map, "m_LengthScale", 2); renderer.sortingFudge = Float(map, "m_SortingFudge");
                renderer.normalDirection = Float(map, "m_NormalDirection", 1); renderer.shadowBias = Float(map, "m_ShadowBias");
                renderer.alignment = (ParticleSystemRenderSpace)Int(map, "m_RenderAlignment");
                renderer.pivot = Vec3(Value(map, "m_Pivot", null), Vector3.zero); renderer.flip = Vec3(Value(map, "m_Flip", null), Vector3.zero);
                renderer.enableGPUInstancing = Bool(map, "m_EnableGPUInstancing", true); renderer.allowRoll = Bool(map, "m_AllowRoll", true);
                renderer.freeformStretching = Bool(map, "m_FreeformStretching"); renderer.rotateWithStretchDirection = Bool(map, "m_RotateWithStretchDirection", true);
                if (Bool(map, "m_UseCustomVertexStreams") || Bool(map, "m_UseCustomTrailVertexStreams"))
                    throw Error(record, "Custom ParticleSystem vertex streams require an explicit adapter.");
                var meshes = new List<Mesh>();
                foreach (string field in new[] { "m_Mesh", "m_Mesh1", "m_Mesh2", "m_Mesh3" })
                {
                    Mesh mesh = Resolve(Reference(Value(map, field, null))) as Mesh;
                    if (mesh != null) meshes.Add(mesh);
                }
                if (meshes.Count != 0) renderer.SetMeshes(meshes.ToArray());
            }
        }
    }
}
