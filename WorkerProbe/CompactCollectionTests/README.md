SDK tests link the production compact collection normalizer and JSON parser. They cover the actual MissionCatalog rewards, Grammar lists and ShipArchetype sizes, nested rules, inherited and SerializeField metadata, UInt8/Int32 bounds, Int32 little-endian byte order, unchanged string/PPtr fields, empty collections, malformed hex, unverified uint/float/signed-byte encodings and failed readback.

```bash
/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Data/DotNetSdk/dotnet run \
  --project $UNITY_MICRO_EDITOR_ROOT/WorkerProbe/CompactCollectionTests/CompactCollectionTests.csproj
```

No Unity process is started. Native JsonUtility restoration is verified by public JSON readback in the Player loader.
