This SDK regression uses the compiled, instrumented proof helper and fresh game `SortieState`. It checks the Serializable marker, all six primitive report fields, exact copying of network values and numeric JSON names/bounds. Native JsonUtility output is verified separately by the actual Player/Go driver.

```bash
dotnet run --project $UNITY_MICRO_EDITOR_ROOT/tests/SortieReportTests/SortieReportTests.csproj \
  -p:ProofManaged=/absolute/current/Player_Data/Managed
```
