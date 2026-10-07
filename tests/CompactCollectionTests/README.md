SDK tests link the production compact collection normalizer and JSON parser. They cover byte and Int32 lists of several sizes, nested rules, inherited and SerializeField metadata, UInt8/Int32 bounds, Int32 little-endian byte order, unchanged string/PPtr fields, empty collections, malformed hex, unverified uint/float/signed-byte encodings and failed readback.

```bash
"$UNITY_DATA/DotNetSdk/dotnet" run \
  --project tests/CompactCollectionTests/CompactCollectionTests.csproj
```

No Unity process is started. Native JsonUtility restoration is verified by public JSON readback in the Player loader.
