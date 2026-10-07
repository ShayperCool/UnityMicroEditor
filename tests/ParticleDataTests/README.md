These SDK tests link the production Particle adapter and use Unity's real managed `MinMaxCurve`, `MinMaxGradient` and `Burst` structs. They check the authored Sparks burst (12–26 particles), zero emission versus unused minimum, HDR colors/alpha, unsigned seeds, and rejection of unknown modes. The test context forbids native Curve, Gradient, component and asset creation.

```bash
/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Data/DotNetSdk/dotnet run \
  --project $UNITY_MICRO_EDITOR_ROOT/tests/ParticleDataTests/ParticleDataTests.csproj
```

This command starts neither Editor nor Player. Native modules, curves and gradients require the actual Player: the production adapter compares the restored settings, keyframes, gradient keys, emission bursts and shape transform through public getters and rejects a mismatch.
