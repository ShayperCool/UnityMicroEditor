# Unity Player через CLI без Editor во время игры

Исследование от 2026-10-04. Проверена установленная Unity `6000.6.3f1`, revision `45d8eee7de74`, и первоисточники ниже. Факты о локальных файлах получены чтением установки и кэша Dungeon. Предлагаемые эксперименты отдельно отмечены как эксперименты: сам отчёт не доказывает, что каждый из них уже работает.

Практический путь есть: подготовить Linux Mono Player, затем запускать его и обновлять подходящие managed DLL через CLI. Во время игры Editor не нужен. Для импорта новых ассетов и подготовки первого набора данных Editor пока остаётся частью инструментов. Простое подключение `UnityEngine.dll` к `dotnet` не создаёт Unity runtime.

## Что делают моды

Doorstop запускает managed assembly внутри существующего Unity Player раньше игрового кода. В Mono он использует CLR, который уже поставляется с игрой. В IL2CPP managed assembly запускается в отдельном CoreCLR, затем обращается к игре через native IL2CPP API. Doorstop отдельно предупреждает: ранняя точка входа ещё не означает, что все подсистемы Unity инициализированы. [README Doorstop](https://github.com/NeighTools/UnityDoorstop).

На Linux Doorstop через PLT перехватывает `dlsym` у `UnityPlayer` и подменяет разрешение `mono_jit_init_version`, `mono_image_open_from_data_with_name` либо `il2cpp_init`. То есть моды заходят в штатный процесс игры, а не воспроизводят запуск движка снаружи. [Linux entrypoint Doorstop](https://github.com/NeighTools/UnityDoorstop/blob/master/src/nix/entrypoint.c).

В Mono bootstrap вызывает оригинальный `mono_jit_init_version`, загружает указанную DLL и вызывает `Doorstop.Entrypoint.Start`. Перехват загрузки assembly умеет выбирать другую DLL по имени из каталога override. Это даёт полезный образец для замены игрового кода до его загрузки. В IL2CPP bootstrap сначала вызывает оригинальный `il2cpp_init`, потом загружает CoreCLR. [Bootstrap Doorstop](https://github.com/NeighTools/UnityDoorstop/blob/master/src/bootstrap.c).

BepInEx поверх этого строит preloader, патчинг и загрузку плагинов. Preload patches через Mono.Cecil могут менять и заменять assemblies до загрузки; Harmony применяется к уже загруженным методам. Нельзя заранее загрузить DLL, которую ещё требуется переписать. [Документация BepInEx о preloader patches](https://docs.bepinex.dev/master/articles/dev_guide/preloader_patchers.html).

Конкретный способ дождаться более поздней стадии виден в исходниках BepInEx: `EntrypointPatcher` вставляет вызов chainloader в static constructor `Application` из `UnityEngine.CoreModule.dll`. Это реализация самого загрузчика, а не API Unity для внешнего Play Mode. [EntrypointPatcher](https://github.com/BepInEx/BepInEx/blob/master/Runtimes/Unity/BepInEx.Unity.Mono.Preloader/EntrypointPatcher.cs).

IL2CPP усложняет быстрые итерации. Il2CppInterop генерирует managed wrappers и связывает объекты IL2CPP с CoreCLR. Это подход для моддинга готового native приложения; подмена его оригинальной `Assembly-CSharp.dll` не заменяет уже скомпилированный C++ код игры. Для первого CLI прототипа выбираем Mono. [Il2CppInterop](https://github.com/BepInEx/Il2CppInterop), [Unity scripting backends](https://docs.unity3d.com/6000.0/Documentation/Manual/scripting-backends.html).

## Где начинается настоящий движок

В установленном Linux development Mono template:

```text
Editor/Data/PlaybackEngines/LinuxStandaloneSupport/
  Variations/linux64_player_development_mono/
    LinuxPlayer                       около 4.7 KiB
    UnityPlayer.so                    около 95 MiB
    Data/
      MonoBleedingEdge/                около 13 MiB вместе с Resources
      Resources/unity default resources
  Variations/mono/Managed/
    UnityEngine.CoreModule.dll
    UnityEngine.PhysicsModule.dll
    UnityEngine.AnimationModule.dll
    UnityEngine.AudioModule.dll
    UnityEngine.AssetBundleModule.dll
    ...
```

`nm -D LinuxPlayer` показывает зависимость от `_Z10PlayerMainiPPc`. `nm -D --defined-only UnityPlayer.so` в этой установке показывает именно этот единственный экспорт. Маленький executable вызывает `PlayerMain(argc, argv)` внутри большой native библиотеки. Это локальное наблюдение о данной сборке Unity; внешний ABI не следует считать стабильным.

Ожидаемый состав подготовленного Mono Player:

```text
DungeonPlay.x86_64
UnityPlayer.so
MonoBleedingEdge/
DungeonPlay_Data/
  boot.config
  globalgamemanagers
  level0, level1, ...
  resources.assets, sharedassets*, *.resS
  ScriptingAssemblies.json
  Managed/
    Dungeon.Runtime.dll
    Dungeon.ShipGen.dll
    Dungeon.Content.dll
    Dungeon.Net.dll
    UnityEngine.*Module.dll
    Unity.InputSystem.dll, VContainer.dll, ...
  Plugins/, StreamingAssets/, Resources/, ...
```

Точный состав зависит от проекта и build options. Имена в этом примере описывают layout, а не обещают наличие всех файлов в каждом build.

Упрощённая последовательность запуска:

```text
CLI
  -> Linux executable
  -> native PlayerMain в UnityPlayer.so
  -> boot config, настройки Player, runtime и native subsystems
  -> Mono domain, UnityEngine bindings, игровые assemblies
  -> сериализованные данные и сцена
  -> runtime initialization, Awake/OnEnable/Start
  -> native PlayerLoop: scripts, physics, animation, rendering
```

Порядок отдельных native стадий здесь намеренно не объявлен контрактом. Его закрытая реализация зависит от версии. Doorstop показывает точку старта CLR, а UnityCsReference показывает границу между C# и native кодом.

`new GameObject()` в открытом C# коде Unity вызывает `extern Internal_CreateGameObject` с binding `GameObjectBindings::Internal_CreateGameObject`. Это не managed реализация сцены. Даже удачная загрузка DLL обычным CLR не создаст необходимый native объект и состояние движка. [GameObject bindings](https://github.com/Unity-Technologies/UnityCsReference/blob/master/Runtime/Export/Scripting/GameObject.bindings.cs).

В `PlayerLoop` видно деление на `Initialization`, `EarlyUpdate`, `FixedUpdate`, `Update`, `PreLateUpdate`, `PostLateUpdate`. В них есть стадии scripts, physics, input, animation и rendering. `GetDefaultPlayerLoop` и `SetPlayerLoop` в итоге вызывают native `extern` методы. Можно менять loop уже работающего Player, но этот API не инициализирует движок вне Player. [PlayerLoop bindings](https://github.com/Unity-Technologies/UnityCsReference/blob/master/Runtime/Export/PlayerLoop/PlayerLoop.bindings.cs).

DLL с именем `UnityEngine.PhysicsModule` поэтому не является автономным physics engine, который можно поднять отдельным `dlopen`. В данной Linux сборке native реализации находятся внутри `UnityPlayer.so`. Официальный Unity as a Library описывает Android, iOS, Windows и UWP; Linux в списке нет. [Unity as a Library](https://docs.unity3d.com/6000.0/Documentation/Manual/UnityasaLibrary.html).

## Почему нельзя сразу скормить Player папку Assets

Player работает с подготовленными данными сцены, ссылками на объекты, платформенными текстурами, шейдерами и assemblies. AssetBundle может содержать сцены, prefabs и другие ассеты, но не C# assemblies. Он платформоспецифичен. Сериализованные script instances сопоставляются с классами по имени assembly, namespace и class. Это позволяет разделить доставку контента и DLL, если соблюдать совместимость. [AssetBundle introduction, sections Structure and Script support](https://docs.unity3d.com/6000.0/Documentation/Manual/AssetBundlesIntro.html).

В момент исследования у Dungeon были WebGL `data.unity3d` и Addressables bundles. Их наличие не означает готовность Linux Player: WebGL-контент собран под другую платформу. Распаковка архива не преобразует shader variants, texture formats, platform flags и script backend. Это вывод из локального состава кэша и ограничения платформенных bundles, а не проверенный способ переноса.

Прямое чтение YAML `.unity` и `.prefab` потребовало бы заново реализовать разрешение GUID/fileID, импорт, сериализацию, variant overrides и native создание объектов. Для небольшого искусственного набора компонентов это отдельный эксперимент. Для Dungeon с URP, Addressables и сторонними пакетами он пока не заменяет штатный pipeline.

## Предлагаемый CLI

`doctor` проверяет версию Unity, наличие Linux Mono support, Editor process и готового Player. `modules` показывает физические файлы runtime и managed bindings. `build` подготавливает Player через batchmode, сохраняя `Library` и инкрементальные кэши. `run` стартует готовый Player без Editor. `install` подключает узкий runtime bridge к выбранному проекту.

Runtime bridge в `Assets/Dungeon/CLIPlay` может разбирать CLI arguments, выбирать уже собранную сцену, ограничивать число кадров или время, писать машинный отчёт и завершать процесс с exit code. Для проекта, которым мы управляем, такой bridge проще сопровождать, чем внедрение через Doorstop. Переиспользуем идею модов о separate runtime и assemblies; не требуется тащить весь mod loader для собственного build.

Для headless запуска Unity поддерживает `-batchmode -nographics`; второй флаг отключает инициализацию graphics device. `-logFile -` задаёт stdout. Автоматизация должна отдельно проверять наличие своего отчёта, потому что логирование зависит от режима и версии. Для визуального запуска убираются headless flags. [Unity Player CLI reference](https://docs.unity3d.com/6000.0/Documentation/Manual/PlayerCommandLineArguments.html).

## Быстрая итерация через Bee response files

В `Library/Bee/artifacts` уже есть Roslyn `.rsp` для конкретных assemblies. Они содержат исходники, defines, references, analyzers, language options и output path. Можно повторно вызвать поставляемый Unity compiler с исправленным output path, затем заменить соответствующую DLL в копии Mono Player и перезапустить его. Compiler process остаётся; запуск Editor для такой компиляции не требуется.

Это экспериментальное повторение промежуточного шага Bee. Полный Unity pipeline включает дополнительные стадии. [Исходники BeeScriptCompilation](https://github.com/Unity-Technologies/UnityCsReference/blob/master/Editor/Mono/Scripting/ScriptCompilation/BeeDriver/BeeScriptCompilation.cs).

Локальная проверка обнаружила важную ловушку: имеющиеся `2000b0aP.dag` и `2000b0aPDev.dag` относятся к WebGL и имеют `ENABLE_IL2CPP`; `2000b0aE.dag` содержит `UNITY_EDITOR`. Имя каталога, суффикс `P` и новый timestamp не доказывают, что `.rsp` подходит Linux Mono. Первый Linux Mono build должен создать корректный набор. Выбор делается по содержимому defines и references: нужна Standalone Linux конфигурация с Mono, без `UNITY_EDITOR`, WebGL и IL2CPP.

Для первого варианта compiler tool нужны следующие ограничения:

- Использовать compiler и targeting packs той же Unity installation. Сохранять исходные references, defines и analyzers, в том числе source generators. Рабочий каталог должен разрешать относительные source paths.
- Писать DLL, PDB, diagnostics и новые response files в отдельный `Temp` каталог. Не изменять Bee outputs и не заменять DLL в Player при compile error. Замену делать при остановленном Player.
- Проверять исчезнувшие source files. Добавление, удаление или перемещение `.cs`, изменение `.asmdef`, package version, define constraints или platform symbols требует новой подготовки графа. Повтор старого `.rsp` не обнаруживает новые файлы сам.
- При изменении публичного API перекомпилировать зависимые assemblies в правильном порядке либо возвращаться к Unity build. Замена одной DLL не обновляет обращения её потребителей.
- Сначала поддержать изменение тела методов существующих типов. Новый script type, `[RuntimeInitializeOnLoadMethod]`, переименование assembly/class и изменение runtime initialization metadata могут потребовать обновления `ScriptingAssemblies.json`, metadata и Player data.
- Не выдавать raw Roslyn output за postprocessed assembly. ILPostProcessors могут генерировать registrations и переписывать IL; например packages для networking, jobs и Burst. Наличие обычных Roslyn analyzers не означает, что эти стадии выполнены. Нужна отдельная проверка применяемых processors; неизвестные случаи должны требовать обычный build.
- Burst Player code компилируется AOT в native output. Перекомпиляция managed DLL не обновляет этот output. Изменённые Burst jobs могут продолжить исполнять старый native код либо перестать совпадать с его registrations. Для таких изменений нужен Burst build; безопасный эксперимент возможен со специально подготовленным Player без Burst. [Burst AOT settings](https://docs.unity3d.com/Packages/com.unity.burst@1.8/manual/building-aot-settings.html).
- Изменение сериализованных полей может скомпилироваться и всё равно конфликтовать со старыми данными. Добавленные поля не получат новые значения из Inspector без обновления ассетов; переименование, изменение типа или удаление поля может повредить загрузку. Offline compile не обновляет сцены, ScriptableObject, prefab variants, materials, input assets, UXML, shader code и Addressables catalog.

Официальный `BuildOptions.BuildScriptsOnly` уже поддерживает итерацию scripts после первого полного build и пропускает Player data. Он остаётся частью Editor build API. Это хороший контрольный вариант для сравнения с offline compiler, а не доказательство полноты `.rsp` replay. [BuildScriptsOnly](https://docs.unity3d.com/6000.0/Documentation/ScriptReference/BuildOptions.BuildScriptsOnly.html).

## Эксперименты с измеримым результатом

1. Подготовить Linux Mono Player и выполнить headless запуск до заданного числа кадров. Проверить загрузку сцены, исполнение bridge, exit code, отчёт и отсутствие Editor процесса во время `run`. Измерить cold startup и peak RSS отдельно от build.
2. В существующем обычном методе изменить константу или видимый текст. Повторить Linux Mono `.rsp`, заменить DLL и запустить Player. Отчёт или скриншот должен содержать новое значение. Сравнить время с `BuildScriptsOnly`. Отдельный отрицательный тест должен доказать, что compile error не затирает рабочий Player.
3. После работающего DLL цикла вынести подготовленную сцену в Linux AssetBundle и загружать её reusable Player. Для нового контента Editor строит bundle, а запуск и доставка используют CLI. Это проверит, насколько часто требуется пересборка Player, и где мешают script metadata, native plugins и URP shader inclusion.

Самая полезная первая цель здесь: обычная правка C# метода, несколько секунд компиляции и перезапуск Player без Editor. Обещание запускать любой сырой Unity project без подготовки не подтверждается ни устройством Player, ни тем, как устроены моды.
