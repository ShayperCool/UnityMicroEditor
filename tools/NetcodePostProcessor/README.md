# NGO после компиляции через SDK

Go compiler запускает этот небольшой C# executable после Roslyn для сборок, чей подготовленный граф ссылается на `Unity.Netcode.Runtime`. Используются неизменённые исходники `Editor/CodeGen` из закреплённого NGO package: `INetworkSerializableILPP`, `INetworkMessageILPP` и `NetworkBehaviourILPP`. Каждый процессор вызывается через публичный `ILPostProcessor.Process(ICompiledAssembly)`; Unity Editor не запускается.

Helper собирается SDK .NET 8 из установленной Unity. Он ссылается на `Unity.CompilationPipeline.Common`, пакет Mono.Cecil, runtime Unity modules и unstripped standalone NGO reference из `Library/Bee/artifacts/StandaloneLinux64_CodeGen`. Последний нужен потому, что linker готового Player удаляет friend-access metadata, необходимую самому package CodeGen. При обработке игровых DLL resolver получает настоящие Player references.

В package source упоминание Editor initialization attribute находится внутри обычного `if`, хотя `using UnityEditor` условный. В helper есть недостижимый placeholder этого типа. Запросы с `UNITY_EDITOR` отклоняются; сборки UnityEditor не загружаются. Исходники стороннего package не меняются.

После ILPP проверяются RPC handlers, send/registration paths и serializer calls для payloads. Публичные runtime bridges из исходного lifecycle compiler должны сохраниться. NGO initialization для `NetworkVariable` оборачивается в публичный `__UnityPlay_RuntimeInitialize_Netcode`; его descriptor передаётся raw loader для вызова после создания сцены и до `Start`.

Roslyn убирает неиспользованные assembly references. Если emitted DLL фактически не использует NGO, helper возвращает её исходные байты с подтверждённым `required=false`. Для сетевой сборки `Dungeon.Net` проверены все 11 RPC и сохранение 15 lifecycle callbacks.

Кэш учитывает helper source, package CodeGen source, SDK/compiler binaries, все helper dependencies и хеши собранного helper. Обработанные DLL/PDB проверяются при повторном использовании. Старый raw baseline без RPC generation пересобирается один раз; дальнейшая компиляция тела метода по-прежнему переиспользует зависимости, когда объявления не изменились.

SDK-only проверка без Player и Editor:

```bash
cd $UNITY_MICRO_EDITOR_ROOT
UNITY_GO_NETCODE_SDK=/absolute/Unity/Editor/Unity \
  go test ./cmd/unity-micro-editor -run TestGoNetcodePostprocessorSDK -v
```
