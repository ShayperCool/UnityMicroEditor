# Штатные импортёры без основного Editor

## Выбранный гибридный путь

По решению пользователя тяжёлые ассеты импортирует сама Unity. Native asset stage запускает batchmode Editor в отдельном проекте, сохраняет его инкрементальную `Library` и экспортирует нативные модели, анимации, аудио, материалы, текстуры и URP assets. Основной проект не открывается и его ассеты не перезаписываются.

Сцены и authored text prefabs собирает наш [raw loader](RAW_PROJECTS.md). Нативный pack содержит carrier с точными GUID/local fileID ссылками и тяжёлые объекты. `.unity` и authored `.prefab` не входят в него. ScriptableObject с анимационными кривыми и preloaded settings клонируются в служебном проекте; ссылки на authored prefabs восстанавливаются затем через собственные templates до разрешения пользовательских callbacks.

Render pipeline global settings получают собственные именованные roots в pack. Их bundle address совпадает с исходным logical asset path даже при native clone. Наш loader регистрирует settings для типа pipeline и устанавливает их текущими через публичный `GraphicsSettings.TrySetCurrentRenderPipelineGlobalSettings` до загрузки carrier. URP renderer features обращаются к ним в native `OnEnable`; одной регистрации недостаточно.

Поиск тяжёлых зависимостей проходит и через preloaded assets. Например, `VContainerSettings` ссылается на root prefab, тот на наш raw `DungeonSoundBank`, а банк на WAV и mixer. Наш loader создаёт prefab и банк, Unity импортирует их тяжёлые зависимости. Экспорт не останавливает обход на первом prefab и не переносит этот prefab в native pack.

Registry и catalog исключают объекты с публичным native флагом `HideFlags.DontSaveInBuild`. После native build экспорт также читает настоящий Linux pack публичным `AssetBundle.LoadFromFile` и проверяет каждую registry ссылку по снимку исходных ID и типов, сделанному до build. Нулевой cooked объект исключается только с доказательством exact ID, SHA/CRC прочитанного pack и исходного доступного класса. В том же процессе выполняется второй инкрементальный build; его readback должен подтвердить отсутствие null. Экспорт сохраняет точные ID и флаги исключённых объектов и public metadata всех FBX animation clips. Имя `__preview__...` само по себе не даёт основания удалить clip. Настоящая ссылка на исключённый ID остаётся ошибкой валидации.

Все deferred prefab bindings снимаются с исходных ScriptableObject до создания и изменения любых clones. Это сохраняет ссылки дочерних modifiers, которые `OnValidate` родительского профиля может обнулить при работе с cloned graph. Наш runtime восстанавливает исходные ссылки перед пользовательскими callbacks.

Изменения позиций в сцене и обычного игрового кода переиспользуют готовый native cache и проходят через собственный loader/compiler. Новые тяжёлые данные и их import settings требуют Unity import stage. Нужный Editor установлен для этой стадии, однако при валидном готовом cache процесс Editor не запускается.

Подготовка сохраняет исходный inventory и точный request до запуска Editor. Если CLI прервалась после успешного экспорта, `unity-play import-recover` проверяет сохранённый snapshot, nonce, checksum запроса и native files и публикует тот же результат. Unity cache с доказанными package versions и Git commits можно скопировать в служебные immutable snapshots и подключить к import project через `file:`. Исходные `Packages/manifest.json` и `packages-lock.json` остаются неизменными.

Методы обычных MonoBehaviour компилируются без реимпорта. Для классов сериализованных ScriptableObject пока проверяется полный хеш `.cs`, включая методы. Их изменение вызывает повторный native export: ещё нет проверки совместимости схемы полей, defaults и serialization/import callbacks, позволяющей безопасно ослабить это условие.

Финальная проверка `SampleScene` с preloads и их тяжёлыми зависимостями на Unity `6000.6.3f1` сохранила 13 460 native object records, 110 model hierarchies и 82 deferred prefab bindings. Все bindings совпали с исходными GUID/fileID/property path; 90 Editor preview-объектов исключены по точному cooked readback proof. Pack размером около 113,3 MiB прошёл проверку отсутствия scene/prefab dependencies. Native metadata подтвердила ноль lightmaps и ноль baked renderers. Сцена завершила 120 кадров без ошибок на RTX 4060. Измеренный повторный импорт изменённого WAV занял 181,067 с с запуском отдельного Editor; полная итерация до готовой сцены — 206,004 с. Условия измерений и остальные итерации приведены в [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md).

## Предыдущее исследование автономных импортёров

Ниже сохранён разбор штатных импортёров установленной Unity `6000.6.3f1` на Linux, 2026-10-05. Автономно вызывать TextureImporter/ModelImporter из обычного .NET host не удалось. Выбранная гибридная стадия использует штатный batchmode Editor и не зависит от недоказанного worker handshake или внутренних адресов функций.

## Managed API и native реализация

`UnityEditor.AssetDatabase.ImportAsset` объявлен как `extern`. TextureImporter и AssetImporter имеют bindings к `Editor/Src/AssetPipeline`, а запись scene/asset data в ContentBuildInterface заканчивается native вызовом. Это видно в открытых исходниках Unity: [AssetDatabase](https://github.com/Unity-Technologies/UnityCsReference/blob/def4ed521d3613a67619d5ac5111e3fd8c17a605/Modules/AssetDatabase/Editor/ScriptBindings/AssetDatabase.bindings.cs), [TextureImporter](https://github.com/Unity-Technologies/UnityCsReference/blob/master/Editor/Mono/AssetPipeline/TextureImporter.bindings.cs), [ContentBuildInterface](https://github.com/Unity-Technologies/UnityCsReference/blob/def4ed521d3613a67619d5ac5111e3fd8c17a605/Modules/ContentBuild/Editor/Managed/ContentBuildInterface.bindings.cs).

Изолированная проба ContentProbe подключила установленный `UnityEditor.CoreModule.dll` из обычного .NET. Публичный `AssetPathToGUID` завершился `SecurityException: ECall methods must be packaged into a system module`. Эта ошибка доказывает недостаточность обычного .NET host, но не невозможность специального native host.

## Реальные точки вызова установленной версии

`Editor/Unity` представляет собой ELF PIE. Его dynamic exports содержат только `__bss_start`, `_edata`, `_end`. Обычный `dlopen` отказывается загружать этот executable. При этом соседний `Unity_s.debug` размером около 26 MiB содержит static symbol table с адресами внутренних функций. DWARF type layouts в нём нет.

Разбор debug symbols и disassembly установленного executable дал цепочку:

```text
UnityEditor.AssetDatabase.ImportAsset
  -> AssetDatabase_CUSTOM_ImportAsset
  -> AssetDatabaseBindings::ImportAsset
  -> AssetDatabase::ImportAtPath
```

Native entries этой конкретной revision:

| Функция | Relative virtual address |
| --- | --- |
| AssetDatabase_CUSTOM_ImportAsset | `0x40663f0` |
| AssetDatabaseBindings::ImportAsset | `0x2f6ee10` |
| AssetDatabase::ImportAtPath | `0x2f39a00` |
| ImportAssetV2 | `0x2f9b510` |
| ProcessImportedImage | `0x374abf0` |
| AssetImportWorker::Import | `0x2ffbbb0` |

Эти addresses не являются экспортированным стабильным API. Прямому вызову нужны загруженный executable, его relocations, native allocators, object/type managers, scripting bindings, AssetDatabase и import context. Какой минимальный набор реально необходим, ещё не установлено.

Сравнение static symbols Editor и Linux Mono Player в этой установке нашло 98 `TextureImporter::` и 188 `ModelImporter::` symbols в Editor и ни одного в Player. `AssetImportWorker::Import` и `AssetDatabaseBindings::ImportAsset` также найдены только в Editor. Это подтверждает размещение исследованных реализаций; names из Editor нельзя просто разрешить через UnityPlayer.so.

## Import workers

Unity уже выполняет часть import pipeline без UI. [Документация Unity 6.6](https://docs.unity3d.com/6000.6/Documentation/Manual/ParallelImport.html) описывает import workers как дополнительные headless instances Editor. Поддержаны перечисленные там texture/model/audio/video importers; прочие типы импортируются при main AssetDatabase refresh.

Локальные worker logs этой версии подтверждают запуск того же executable:

```text
Unity -adb2 -batchMode -noUpm -name AssetImportWorkerHW0
      -projectPath <project> -srvPort <port> -parentPid <pid> -nographics
```

Это только относящаяся к import части команда. Реальный worker также получает сведения о лицензии и настройках процесса от coordinator. Самостоятельного `AssetImportWorker.so` или отдельного worker executable в установке не найдено.

В debug symbols обнаружены native coordinator и message handlers:

| Сообщение | Coordinator SendAsync | Worker HandleMessage |
| --- | --- | --- |
| InitializeMessage | `0x3146130` | `0x2fa9970` |
| PrepareMessage | `0x31463a0` | `0x2fa9f60` |
| ImportRequestMessage | `0x31464f0` | `0x2faa550` |

`AssetImportWorkerClient::OnMessageReceived` находится по `0x2fa98a0`, dispatch к handlers подтверждён disassembly. Из этих символов нельзя восстановить все message fields: type layouts и готового описания protocol framing пока нет.

## Независимый FBX decoder

В установленном SDK есть `Data/Resources/PackageManager/Editor/com.autodesk.fbx-5.1.3.tgz`. В нём поставляются native `UnityFbxSdkNative.so` и public managed wrappers Autodesk.Fbx. Это самостоятельная библиотека FBX SDK, не полный native Unity ModelImporter. Не установлено, что встроенный ModelImporter использует именно эту библиотеку пакета: она предоставлена через отдельный Autodesk.Fbx API.

В отдельном ограниченном по времени процессе библиотека напрямую выполнила `FbxManager_Create`, `FbxScene_Create`, `FbxImporter_Create`, `FbxIOBase_Initialize`, `FbxImporter_Import` и чтение root node. Настоящий `Assets/Dungeon/ShipGen/Art/Custom/Shell/R_Floor.fbx` успешно прочитан без Editor и project Library. Первая проба измерила импорт около 0.011 с. Это время только native Import, без подготовки Python/распаковки SDK и загрузки сцены.

Этот decoder можно переиспользовать вместо написания FBX parser. Чтобы получить эквивалент Unity asset, ещё нужны mesh/animation conversion, интерпретация ModelImporter.meta, rig/avatar, стабильные Unity subasset fileIDs и передача результата runtime. Ни одна из этих стадий не доказана фактом чтения FBX. Воспроизводимая проба находится в ContentProbe.

## Альтернативный эксперимент с import workers

Выбранный гибридный путь использует batchmode Editor для тяжёлых ассетов и собственный исходный граф сцены. Альтернативный эксперимент с более компактным coordinator import workers состоит в замене штатного coordinator. Нужно снять handshake и Initialize/Prepare/ImportRequest для одного PNG и FBX в отдельном тестовом проекте, повторить эти jobs из CLI и сравнить artifacts/subasset IDs с штатным импортом. Замерить отдельно startup, idle RAM, импорт изменённого файла и число необходимых workers.

Если такой worker сможет работать без основного Editor, штатные TextureImporter и ModelImporter можно переиспользовать без их переписывания. Это гипотеза, независимый импорт не доказан. Процесс всё ещё будет исполнять native Editor executable в worker mode. Чтобы исключить и этот executable, придётся отдельно исследовать загрузку и инициализацию его native функций в собственном host.

Публичный `UnityFileSystemApi.so` уже проверен без Editor: он создаёт UnityFS archives и читает готовые SerializedFiles. Импорт YAML/PNG/FBX и создание native object graph этот API не предоставил. Конкретные результаты сохранены в [NO_EDITOR_CONTENT.md](NO_EDITOR_CONTENT.md).

Материалы текущего разбора лежат в игнорируемом `Logs/UnityPlay/native-import-research`: static symbol lists для Editor/Player, список entry points и два disassembly фрагмента. Editor process для этого разбора не запускался.
