using System;

namespace UnityMicroEditor.Raw
{
    // Unity's native Material color storage/readback can round a channel.
    // The pinned Linear Player returned 2–9 ULP for
    // Prop_Strap and Decor_SignFace. Thirty-two ULP permits that float32 roundtrip and rejects substantial
    // differences without a scale-dependent visual tolerance.
    internal static class RawFloatReadback
    {
        internal static bool SameColorChannel(float expected, float actual)
        {
            if (float.IsNaN(expected) || float.IsNaN(actual)
                || float.IsInfinity(expected) || float.IsInfinity(actual)) return false;
            if (expected == actual) return true;
            int expectedBits = BitConverter.SingleToInt32Bits(expected);
            int actualBits = BitConverter.SingleToInt32Bits(actual);
            if ((expectedBits < 0) != (actualBits < 0)) return false;
            return Math.Abs((long)expectedBits - actualBits) <= 32;
        }
    }
}
