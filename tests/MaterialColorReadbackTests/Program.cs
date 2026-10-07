using System;
using Dungeon.CLIPlay.Raw;

internal static class Program
{
    private static void Check(bool condition, string message)
    {
        if (!condition) throw new Exception(message);
    }

    private static int Main()
    {
        // Captured native Dungeon/PixelLit MainColor in Linear space on 6000.6.3f1.
        Check(RawFloatReadback.SameColorChannel(.82f, .82f), "exact native red");
        Check(RawFloatReadback.SameColorChannel(.47f, .4699999f), "captured native green roundtrip");
        Check(RawFloatReadback.SameColorChannel(.16f, .159999967f), "captured native blue roundtrip");
        Check(RawFloatReadback.SameColorChannel(.05f, .0499999821f), "captured sign face red roundtrip");
        Check(RawFloatReadback.SameColorChannel(.06f, .0599999651f), "captured sign face blue roundtrip");
        foreach (float value in new[] { .16f, .47f, .82f, -0.5f, 1000f })
        {
            int bits = BitConverter.SingleToInt32Bits(value);
            Check(RawFloatReadback.SameColorChannel(value, BitConverter.Int32BitsToSingle(bits + 32)), "precision boundary");
            Check(!RawFloatReadback.SameColorChannel(value, BitConverter.Int32BitsToSingle(bits + 33)), "precision overflow");
        }
        Check(!RawFloatReadback.SameColorChannel(.47f, .187f), "linear/gamma mixup must fail");
        Check(!RawFloatReadback.SameColorChannel(.82f, .821f), "authored tint difference must fail");
        Check(!RawFloatReadback.SameColorChannel(1, .9f), "alpha difference must fail");
        Check(!RawFloatReadback.SameColorChannel(.01f, -.01f), "opposite sign must fail");
        Check(RawFloatReadback.SameColorChannel(0, -0f), "signed zero");
        foreach (float invalid in new[] { float.NaN, float.PositiveInfinity, float.NegativeInfinity })
        {
            Check(!RawFloatReadback.SameColorChannel(invalid, invalid), "nonfinite equality must fail");
            Check(!RawFloatReadback.SameColorChannel(.82f, invalid), "nonfinite readback must fail");
        }
        Console.WriteLine("Material float32 readback regression passed.");
        return 0;
    }
}
