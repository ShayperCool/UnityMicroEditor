# Сырой Unity-проект без Editor: что потребуется

Исследование 2026-10-05, установленная Linux Unity `6000.6.3f1`. Работа выполнена в `unity-cli-play`. Editor для экспериментов не запускался. Ни исходные ассеты, ни `Library`, ни работающий интерактивный SampleScene не менялись.

Собственный CLI для сборки и загрузки сырого контента технически возможен. Доступные библиотеки уже решают запуск native runtime и часть работы с бинарными файлами. Автономный импорт всего Unity-проекта ими пока не получен. Для полной замены Editor нужно либо самостоятельно поддерживать content pipeline, либо отдельно исследовать автономную инициализацию штатных native импортёров. Для строгого запуска только из библиотек нужны также данные инициализации Player. Работающий компилятор произвольного сырого Dungeon в этом исследовании не получен.

После исследования реализован [первый raw prototype](RAW_PROJECTS.md), проверенный на малой сцене без Library и cooked контента входного проекта. Дополнительный разбор штатных Editor функций и протокола import workers находится в [NATIVE_IMPORTERS.md](NATIVE_IMPORTERS.md).

## Проверенные вызовы

В ContentProbe сохранены воспроизводимые пробы. Результаты этой машины — `Logs/UnityPlay/no-editor-research/probes/results.json`; снимок экспортов — `native-symbols.json` рядом. Эти файлы игнорируются git.

| Эксперимент | Фактический результат | Что это устанавливает |
| --- | --- | --- |
| Публичный `AssetDatabase.AssetPathToGUID` из .NET 8 с установленной `UnityEditor.CoreModule.dll` | `SecurityException: ECall methods must be packaged into a system module` | DLL компилируется/загружается, но её binding требует Unity native host. Проба относится к обычному .NET, а не ко всем возможным собственным Mono hosts. |
| Загрузка `Editor/Unity` через `NativeLibrary.Load` / `dlopen` | `cannot dynamically load position-independent executable` | Этот установленный executable не является обычной импортируемой `.so`. |
| `UFS_Init`, `UFS_CreateArchive`, mount/read из `UnityFileSystemApi.so` | Создан LZ4 UnityFS archive с сырым `SampleScene.unity`; прочитаны исходные 101536 байт, SHA-256 совпадает | Native библиотеку действительно можно вызывать без процесса Editor. Она упаковывает байты; импорт сцены при этом не происходит. |
| Чтение готового Linux scene bundle той же библиотекой | Четыре SerializedFile, 18 + 14 + 9578 + 16 = **9626 объектов** | Доступна независимая инспекция подготовленного контента. Этот вход был ранее собран Editor. |
| Попытка открыть YAML внутри нового архива как бинарный SerializedFile | Возврат `6` (`HigherSerializedFileVersion`), файла объектов не получено | Текст не преобразуется в runtime serialization. Первоначальная проба дополнительно получила SIGSEGV при cleanup после неудачного parse; финальная изолированная проба завершает процесс без cleanup на этой ветке. |
| Установленный Linux Player template + UnityPlayer + Mono/BCL + built-in Resources + сырые Assets, без данных готового проекта | Mono, Input и часть native initialization стартуют; затем `No GlobalGameManagers file was found ... quitting player!`, exit 1 | Одних установленных runtime библиотек недостаточно для завершения запуска: необходимы файлы инициализации. Это не доказывает невозможность их самостоятельно написать. |

Последняя проба шла с `-batchmode -nographics -quit`, без DISPLAY/Wayland и без рендера. Её staging и лог: `Logs/UnityPlay/no-editor-research/raw-template/`. Для проверки bootstrap использованы только файлы установки Unity, не `globalgamemanagers` или сцены готового Player. Отсутствие `RuntimeInitializeOnLoads.json` и `ScriptingAssemblies.json` тоже видно в логе; остановка произошла на отсутствующем `globalgamemanagers`. Это проверка запуска runtime, а не проверка визуальной совместимости.

## Какие части установки можно использовать

Локальный Linux `UnityPlayer.so` размером 98697864 байт экспортирует один символ — `PlayerMain(int, char**)`. LinuxPlayer template — маленький launcher этого native ядра. Здесь находятся runtime подсистемы; managed `UnityEngine.*Module.dll` предоставляют bindings и часть C# реализации.

`Editor/Unity` размером 119618552 байта — ELF PIE; в динамической таблице определены только `__bss_start`, `_edata`, `_end`. Экспортированного C ABI для AssetDatabase / ModelImporter / штатного сериализатора в нём не найдено. Это наблюдение о конкретной установленной Linux версии, а не универсальный запрет на reverse engineering.

В `Data/Tools` есть отдельные `UnityFileSystemApi.so`, `UnityShaderCompiler`, FSBTool, ASTC encoders, FreeImage, DXCompiler, GeometryToolbox/Unwrap, lightmapping workers. В установке также присутствуют Roslyn, Bee, ILPP и Burst инструменты. Их наличие позволяет использовать отдельные стадии. Оно не устанавливает, что каждый worker можно вызвать автономно с исходным `.shadergraph` или `.fbx`: нужны его входной протокол и остальные стадии импорта. Автономный shader/model cooking в этом исследовании не проверен.

Границу видно и в исходниках: открытый Scriptable Build Pipeline вызывает `ContentBuildInterface.WriteSerializedFile` / `WriteSceneSerializedFile`. Эти C# методы переходят в `extern` native bindings, а не реализуют сериализацию полностью в managed коде. В установленном пакете точки вызова лежат в `Editor/WriteTypes/AssetBundleWriteOperation.cs` и `SceneBundleWriteOperation.cs`. [ContentBuildInterface bindings, закреплённый commit](https://github.com/Unity-Technologies/UnityCsReference/blob/def4ed521d3613a67619d5ac5111e3fd8c17a605/Modules/ContentBuild/Editor/Managed/ContentBuildInterface.bindings.cs).

AssetDatabase и TextureImporter также обращаются к Editor-native реализации. Подключить их managed DLL и зарегистрировать несколько runtime InternalCall недостаточно: необходимы соответствующая native реализация и её состояние. [AssetDatabase bindings](https://github.com/Unity-Technologies/UnityCsReference/blob/def4ed521d3613a67619d5ac5111e3fd8c17a605/Modules/AssetDatabase/Editor/ScriptBindings/AssetDatabase.bindings.cs), [TextureImporter bindings](https://github.com/Unity-Technologies/UnityCsReference/blob/master/Editor/Mono/AssetPipeline/TextureImporter.bindings.cs).

Извлечение внутренних функций по адресам с собственным ELF loader и регистрацией bindings — возможное направление исследования, но пока нет доказательства, что их можно инициализировать независимо от остального Editor. Оно потребовало бы воспроизводить AssetDatabase, object manager, package/import environment и зависело бы от revision движка. Называть этот путь готовым лёгким SDK оснований нет.

## Что уже делают открытые инструменты

- **UnityDataTools / UnityFileSystemApi:** автономная работа с UnityFS и бинарными SerializedFiles. `UFS_CreateArchive` даёт упаковку; доступные `UFS_*` экспорты этой установки не содержат API импорта исходных PNG/FBX/YAML или записи дерева объектов в новый SerializedFile. Нужный wrapper опубликован Unity. [Описание библиотеки](https://github.com/Unity-Technologies/UnityDataTools), [C ABI wrapper, закреплённый commit](https://github.com/Unity-Technologies/UnityDataTools/blob/b73e33c1847ff81d43bc1da3726145a9a4c97c3a/UnityFileSystem/DllWrapper.cs).
- **AssetsTools.NET:** независимое чтение и запись Unity assets/bundles, добавление объектов и полей по схемам. Это полезная основа для собственного writer, а не только reader. Проверку записи файлов версии `6000.6.3f1` и их загрузки нашим Player ещё нужно провести; готовый импорт Dungeon из возможностей библиотеки не следует. [Репозиторий](https://github.com/nesrak1/AssetsTools.NET), [Примеры записи](https://github.com/nesrak1/AssetsTools.NET/wiki/Getting-Started:-Assets-file-writing).
- **AssetRipper:** восстанавливает проект из подготовленных игровых данных. Это обратное направление. Его exporter не является заменой Unity importer/build pipeline. [Репозиторий](https://github.com/AssetRipper/AssetRipper).
- **Unity glTFast:** загружает сырой glTF в работающем Player, создаёт meshes/materials и поддерживает анимационный контент. Документация требует заранее включённых shaders/variants: это реальный обход model import для поддерживаемого формата, но shader cooking остаётся отдельной задачей. [Runtime import](https://github.com/atteneder/glTFast/blob/main/Documentation~/ImportRuntime.md), [Shader setup](https://github.com/atteneder/glTFast/blob/main/Documentation~/ProjectSetup.md).
- **Content Directories, Unity 6.6:** ещё один способ доставки подготовленных SerializedFiles и ресурсов. Он может улучшить гранулярность обновления, но смена формата доставки сама по себе не устраняет импорт. [Описание формата Unity](https://github.com/Unity-Technologies/UnityDataTools/blob/main/Documentation/contentdirectory-format.md).

## Два архитектурных пути

Это варианты реализации, а не уже работающие режимы `unity-play`.

### 1. Универсальный host + собственные runtime importers

Один version/platform-specific Player поставляется как часть инструмента. В нём заранее есть bootstrap, нужные native modules, package runtime DLL и фиксированный набор shaders. Входной проект может быть сырым: ему не нужны собственные `Library`, готовый Player или AssetBundles. CLI разрешает packages/asmdef, компилирует код и читает исходные ассеты. Runtime loader создаёт объекты через публичные Unity API. Движок продолжает выполнять rendering, physics, animation и input.

```text
Assets + .meta + Packages + ProjectSettings
  -> наш CLI: зависимости, компиляция C#, чтение/конвертация контента
  -> наш loader внутри универсального Mono Player
  -> native Unity PlayerLoop и GPU
```

Для первых итераций реализуемые кандидаты: YAML Transform/GameObject/простые компоненты, ссылки GUID/fileID, материалы на известных shaders, PNG/JPEG, примитивы/meshes, ограниченные ScriptableObject. Texture2D может загружать PNG/JPEG через публичный `ImageConversion.LoadImage`, без штатного TextureImporter. Но importer settings, mipmaps, color space, texture compression и sprite slicing должен воспроизвести наш код. [LoadImage API](https://docs.unity3d.com/6000.0/Documentation/ScriptReference/ImageConversion.LoadImage.html).

Сложные Unity типы нельзя обещать автоматически получить из всех YAML полей через reflection: у них есть native состояние и import-only representations. Нужны явные адаптеры и порядок создания объектов, назначения ссылок, DI и lifecycle. Unsupported classID/importer/settings должен останавливать загрузку с указанием файла, а не молча пропадать. Чтение контента не должно перезаписывать исходные ассеты или `.meta`.

Этот путь устраняет Editor для итерации поддерживаемого содержимого и для подготовки конкретного входного проекта. Однако универсальный host и его shaders изначально подготовлены; это не строгий вариант «ни разу никакой заранее собранной data».

### 2. Собственный content compiler + bootstrap writer

Чтобы обойтись только установленными native/managed библиотеками даже при первом запуске, CLI должен также создавать корректные Player data: `globalgamemanagers`, scripting/runtime-initialization metadata, стартовую сцену либо другой проверенный механизм входа, platform-specific shader/resource data. Шаблон launcher и Mono runtime уже можно брать из установки. После этого Player может читать самостоятельно записанные SerializedFiles или использовать runtime importers пути 1.

AssetsTools.NET и описание форматов делают задачу исследуемой: это не неизвестный зашифрованный контейнер. Но необходимы схемы точной версии, native class layouts, корректные object references и runtime/editor различия. Для `.unity/.prefab` дополнительно нужны instance/variant overrides, subasset IDs, зависимости и преобразования данных. Для полноценного writer потребуется доказать на независимых файлах загрузку нашим Player, а не только успешную запись и повторное чтение своим же parser.

Shader Graph/HLSL — отдельный блок. Нужны generation, variants, platform compilation, reflection/binding metadata и сериализованный Unity Shader, а не только результат DXCompiler. Наличие `UnityShaderCompiler` ещё не решает этот интерфейс. Публичный API создания shader из исходника находится в `UnityEditor.ShaderUtil`; runtime GPU compilation подготовленной программы не равна импорту нового Shader Graph. [CreateShaderAsset](https://docs.unity.com/en-us/engine/6000.3/script-reference/unityeditor/shaderutil/createshaderasset).

Возможен промежуточный результат: самостоятельно записывать простые объекты/текстуры в новые bundles, сохраняя заранее подготовленный shader/host baseline. Он снизит зависимость от Editor, но результат нужно явно отличать от полного compiler сырого проекта.

## Что требуется именно Dungeon

Инвентаризация `Assets` этого worktree: **705 FBX, 537 prefabs, 360 .asset, 257 materials, 111 PNG, 92 WAV, 76 .anim, 5 Animator Controllers, 8 override controllers, 4 Shader Graph**. Числа для всего `Assets`, включая сторонние пакеты; это не dependency closure одной сцены. В `Assets/Dungeon` отдельно — 558 FBX и 522 prefabs. Полная ведомость — `Logs/UnityPlay/no-editor-research/inventory.json`.

SampleScene содержит 34 GameObject, 24 MeshRenderer/MeshFilter пары, 18 BoxCollider, 6 CapsuleCollider, 7 MonoBehaviour, 2 PrefabInstance. Прямые ссылки ведут на FirstPersonPlayer, CAS prefab, материалы и Volume Profile. Package GUID относятся к VContainer LifetimeScope, URP camera/light data и Volume. Поэтому первый importer пустой сцены или нескольких кубов не доказывает поддержку того SampleScene, который сейчас работает.

| Задача | Что потребуется без Editor |
| --- | --- |
| Полная первая компиляция | UPM lock/package resolution, asmdef/asmref graph, player defines, response files и generated sources без зависимости от прежнего Bee capture; нужные ILPP/Burst stages. Нынешний `refresh-code` использует подготовленный контракт и не является таким cold compiler. UnityEngine references всё равно нужны коду, который обращается к Unity API. |
| Scenes / prefabs / variants | GUID/fileID resolver, built-in и package references, nested instances, overrides, component adapters, serialized refs, lifecycle и DI. |
| ScriptableObject / .asset | Схемы конкретных runtime типов, создание instances и назначение ссылок; отдельные обработчики engine assets, которые не являются обычными SO. |
| Textures / audio | Source decoding плюс интерпретация `.meta`, sampler/color-space/mips/compression; PCM creation или независимое кодирование audio. |
| FBX / CAS / humanoid | Geometry, bones, skinning, subasset IDs, rig/avatar settings, clips/retargeting; выбор независимого FBX importer или явная конвертация в glTF. |
| Animator Controllers | Создание runtime representation бинарным writer либо собственное исполнение графа через Playables; совместимость поведений CAS надо доказывать отдельно. |
| URP / Shader Graph | Package runtime, renderer settings/assets и prepared shader variants; самостоятельный shader pipeline для новых исходников. |
| Остальной контент | Addressables catalog/provider compatibility, input-actions data/codegen, AudioMixer, baked lighting/cubemap, UI Toolkit/font assets по фактическим зависимостям. |

Глобальную совместимость произвольных пользовательских ScriptedImporter и Editor build callbacks нельзя получить одним набором стандартных adapters. Их семантику тоже придётся поддерживать либо явно ограничивать.

## План первых экспериментов и критерии готовности

Этот план сохраняет варианты первоначального исследования. Текущая реализация по выбору пользователя восстанавливает сцены напрямую, без SerializedFile/AssetBundle writer. Очередь lifecycle проверена в CompatibilityProbe; рабочий путь описан в [RAW_PROJECTS.md](RAW_PROJECTS.md).

1. **Отдельно доказать bootstrap.** Из SDK template и самостоятельно созданных data получить готовый native Player без копирования `globalgamemanagers`/сцен из сборки. Проверить scripting initialization и первый кадр. Если сначала выбираем универсальный prepared host, пометить эту зависимость явно.
2. **Raw-content proof на малой сцене.** Читать исходные `.unity`, `.prefab`, `.mat`, PNG и `.meta`, создавать эквивалентную сцену через runtime adapters. Запускать из отдельной папки с `Assets/Packages/ProjectSettings`, без доступа к `Library` исходного проекта и без scene bundle. Host baseline допускается только для пути 1.
3. **Проверить write/load цикл.** Независимо создать/изменить небольшой SerializedFile с AssetsTools.NET, упаковать UFS, загрузить настоящим Player; проверить references, размеры/цвета и типы. Это отдельное доказательство для content compiler.
4. **Измерить raw-итерацию.** Изменить Transform, prefab override, texture bytes и C# method; фиксировать время от сохранения до первого кадра с изменением, CPU/RSS/VRAM. Для code reload явно разделить restart и state-preserving hot reload.
5. **Расширять по dependency closure SampleScene.** Сначала validate-only выдаёт все неподдерживаемые classID/importers, затем добавляем URP/Volume/SO, FBX и анимацию. До этого не объявлять SampleScene поддержанным.

Сейчас оправдано начинать с универсального host и raw loader, одновременно исследуя независимую запись bootstrap/data. Полностью совместимая замена Unity import/build — существенно больший проект. Имеющиеся результаты показывают конкретные границы и пригодные кирпичи, но не дают оснований обещать полную совместимость Dungeon только после подключения DLL.
