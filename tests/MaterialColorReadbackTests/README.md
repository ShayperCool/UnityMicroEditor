# Material color readback regression

```bash
"$UNITY_DATA/DotNetSdk/dotnet" run \
  --project tests/MaterialColorReadbackTests/MaterialColorReadbackTests.csproj
```

`UNITY_DATA` is the `Editor/Data` directory of the Unity installation.

This SDK executable checks the production `RawFloatReadback` without a Player or Editor. Its values come from a GPU probe on Unity 6000.6.3f1 in Linear color space: after `SetColor(0.82, 0.47, 0.16, 1)`, `GetColor` and `GetVector` returned `(0.82, 0.4699999, 0.159999967, 1)`. A second sample, `(0.05, 0.05, 0.06, 1)`, came back as `(0.0499999821, 0.0499999821, 0.0599999651, 1)`, up to 9 ULP away.

The tolerance is 32 ULP per float32 channel. The checks reject 33 ULP, a changed tint or alpha, a sign change, NaN, Infinity, and the large difference produced by a mixed-up linear/gamma conversion. The loader passes authored material values to `SetColor` unchanged. Vector properties, which Unity also stores in `m_Colors`, are restored separately through `SetVector` with an exact `GetVector` readback, which preserves the Infinity sentinel in `_CameraFadeParams`.
