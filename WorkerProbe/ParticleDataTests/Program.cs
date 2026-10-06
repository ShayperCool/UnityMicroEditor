using System;
using System.Collections.Generic;
using Dungeon.CLIPlay.Raw;
using UnityEngine;

namespace Dungeon.CLIPlay.Raw
{
    public static partial class RawSceneLoader
    {
        private static void Check(bool condition, string message) { if (!condition) throw new Exception(message); }
        private static void Reject(Action action, string message)
        {
            try { action(); } catch (InvalidOperationException) { return; }
            throw new Exception(message);
        }
        public static void TestManagedParticleData()
        {
            var count = ParticleCurve(RawJson.Parse("{\"minMaxState\":3,\"minScalar\":12,\"scalar\":26}"));
            Check(count.mode == ParticleSystemCurveMode.TwoConstants && count.constantMin == 12 && count.constantMax == 26, "Sparks source burst 12..26 must survive conversion");
            var burst = new ParticleSystem.Burst(0, count, 1, 1.7f) { probability = 1 };
            Check(burst.time == 0 && burst.cycleCount == 1 && burst.repeatInterval == 1.7f && burst.probability == 1 && SameParticleCurve(burst.count, count), "Unity Burst public contract must retain source count/cycles/interval/probability");
            var zeroRate = ParticleCurve(RawJson.Parse("{\"minMaxState\":0,\"minScalar\":10,\"scalar\":0}"));
            Check(zeroRate.constant == 0 && zeroRate.mode == ParticleSystemCurveMode.Constant, "Source rateOverTime=0 must not pick unused minScalar=10");
            Check(!SameParticleCurve(count, zeroRate), "Curve mode mismatch must reject");
            var changed = count; changed.constantMin = 13;
            Check(!SameParticleCurve(count, changed), "Burst min count changes must reject");
            changed = count; changed.constantMax = 27;
            Check(!SameParticleCurve(count, changed), "Burst max count changes must reject");
            foreach (int invalid in new[] { -1, 4, int.MaxValue })
                Reject(() => ParticleCurve(new Dictionary<string, object> { { "minMaxState", invalid } }), "Unknown curve mode must reject before native curve construction");
            var tint = ParticleGradient(RawJson.Parse("{\"minMaxState\":2,\"minColor\":{\"r\":0.25,\"g\":0.5,\"b\":1,\"a\":0},\"maxColor\":{\"r\":1,\"g\":2,\"b\":3,\"a\":1}}"));
            Check(tint.mode == ParticleSystemGradientMode.TwoColors && tint.colorMin == new Color(.25f, .5f, 1, 0) && tint.colorMax == new Color(1, 2, 3, 1), "TwoColors must preserve HDR RGB and alpha independently");
            var changedTint = tint; changedTint.colorMin = new Color(.25f, .5f, 1, .1f);
            Check(!SameParticleGradient(tint, changedTint), "Gradient alpha changes must reject");
            foreach (int invalid in new[] { -1, 5, int.MaxValue })
                Reject(() => ParticleGradient(new Dictionary<string, object> { { "minMaxState", invalid } }), "Unknown gradient mode must reject before native gradient construction");
            Check(Bits(new Dictionary<string, object> { { "randomSeed", 4294967295L } }, "randomSeed") == uint.MaxValue, "Unsigned seed must preserve all 32 bits");
            Check((int)ParticleSystemShapeType.Cone == 4 && (int)ParticleSystemShapeType.Box == 5, "Pinned SDK serialized shape IDs changed");
        }
    }
}

internal static class Program
{
    private static int Main()
    {
        RawSceneLoader.TestManagedParticleData();
        Console.WriteLine("Managed Particle data regression passed; native module readback is checked in Player by the adapter.");
        return 0;
    }
}
