using System;
using System.Collections.Generic;
using System.Globalization;
using UnityEngine;

namespace Dungeon.CLIPlay.Raw
{
    // Link the actual Particle adapter without constructing Unity native objects.
    // Curve/Gradient native creation is deliberately unavailable in this SDK test.
    public sealed class RawSceneObject { }
    public static partial class RawSceneLoader
    {
        private struct ReferenceData { public string fileID; }
        private static Dictionary<string, object> Map(object value) => value as Dictionary<string, object> ?? new Dictionary<string, object>();
        private static object Value(Dictionary<string, object> data, string key, object fallback) => data.TryGetValue(key, out object value) && value != null ? value : fallback;
        private static int Int(Dictionary<string, object> data, string key, int fallback = 0) => Convert.ToInt32(Value(data, key, fallback), CultureInfo.InvariantCulture);
        private static uint Bits(Dictionary<string, object> data, string key, uint fallback = 0) => unchecked((uint)Convert.ToInt64(Value(data, key, fallback), CultureInfo.InvariantCulture));
        private static float Float(Dictionary<string, object> data, string key, float fallback = 0) => Convert.ToSingle(Value(data, key, fallback), CultureInfo.InvariantCulture);
        private static bool Bool(Dictionary<string, object> data, string key, bool fallback = false) => Int(data, key, fallback ? 1 : 0) != 0;
        private static Vector2 Vec2(object value, Vector2 fallback) { var map = Map(value); return new Vector2(Float(map, "x", fallback.x), Float(map, "y", fallback.y)); }
        private static Vector3 Vec3(object value, Vector3 fallback) { var map = Map(value); return new Vector3(Float(map, "x", fallback.x), Float(map, "y", fallback.y), Float(map, "z", fallback.z)); }
        private static Color Rgba(object value, Color fallback) { var map = Map(value); return new Color(Float(map, "r", fallback.r), Float(map, "g", fallback.g), Float(map, "b", fallback.b), Float(map, "a", fallback.a)); }
        private static ReferenceData Reference(object value) => new ReferenceData { fileID = Convert.ToString(Value(Map(value), "fileID", "0"), CultureInfo.InvariantCulture) };
        private static AnimationCurve Curve(object value)
        {
            if (value == null || Value(Map(value), "m_Curve", null) is List<object> keys && keys.Count == 0) return null;
            throw new InvalidOperationException("Native AnimationCurve requires a Player test.");
        }
        private static Gradient ColorGradient(object value) => throw new InvalidOperationException("Native Gradient requires a Player test.");
        private static Exception Error(RawSceneObject record, string message) => new InvalidOperationException(message);
        private sealed partial class ImportContext
        {
            private UnityEngine.Object Resolve(ReferenceData reference) => throw new InvalidOperationException("Native asset resolution requires a Player test.");
            private void ConfigureRenderer(ParticleSystemRenderer renderer, RawSceneObject record) => throw new InvalidOperationException("Native renderer requires a Player test.");
        }
    }
}
