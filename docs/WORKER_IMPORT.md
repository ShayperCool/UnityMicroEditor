# Собственный координатор Unity AssetImportWorker

Основной Go executable также запускает MCP-сервер для агентов: `unity-micro-editor mcp --project /absolute/project`. Настройка stdio/Streamable HTTP, source/runtime-инструменты и проверки ревизий описаны в [AGENT_MCP.md](AGENT_MCP.md). Автоподготовка разрешает пакеты и строит Player-граф из `.asmdef`/`.asmref` нашим Go-кодом, затем компилирует C# через SDK. Совместимый нативный Mono host и его профиль готовятся один раз; неподдержанный контент всё ещё может требовать Editor. Префабы редактируются на нашей стороне. `cache-component-defaults` один раз сохраняет native defaults, после чего `component_schema` и `component_add` работают без Editor. Сравнение с настоящим Editor и Coplay MCP находится в [Integrations/EditorParity](../Integrations/EditorParity/README.md).

Сохраняйте agent compiler/baseline, native staging и runtime-сессии в `Library/UnityMicroEditor` явно выбранного проекта. Native import, extraction и adoption принимают этот собственный cache-подкаталог либо прежние пути под `Temp`; существующий native catalog автоматически не переносится. Перезапуск Unity Editor удаляет проектный `Temp`, что уже воспроизведено при сравнении. Произвольные каталоги `Library`, соседние проекты и symlink-пути не разрешены. Графы собственного backend и независимого публичного Editor oracle имеют разное происхождение; их нельзя переименовывать друг в друга. `--no-editor` запрещает запуск основного Editor и явно отклоняет отсутствующий native bootstrap. **Сборка проекта через `project_build` остаётся прокси в настоящий Editor** и явно отмечается в ответе.

Координатор используется в [Go runner для Dungeon](GO_RUNNER.md), включая cache и прямую доставку поддержанных native FBX/WAV/PNG в Mono Player. [Project import](#импорт-файлов-проекта) принимает выбранные исходные файлы вместо фиксированных fixtures. Runtime readback и ограничения описаны в [RuntimeDeliveryProbe/README.md](../proofs/runtime-delivery/README.md).

Исходный режим `--mode import` ниже проверяет маленький FBX из Dungeon и детерминированный PCM WAV. Его замеры и строгие validators относятся к этим двум fixtures. Основной процесс Editor, GUI, AssetBundle и заранее импортированная `Library` для worker запуска не нужны.

Поддерживается конкретная установка **Unity 6000.6.3f1, Linux x64**. Координатор собирается в самостоятельный Go executable с `CGO_ENABLED=0`; для импорта он запускает `Unity` в режиме AssetImportWorker и отдельный `Unity.DataStore` из установленного движка. Импортёры пока остаются внутри штатного executable Unity. Их код и native-библиотеки не вынесены в независимый SDK.

## Импорт файлов проекта

Из корня ворктри можно импортировать настоящий WAV в новый собственный context:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor project-import --project . \
  --output "$PWD/Temp/UnityPlay/project-import-manual-01" \
  --asset Assets/Dungeon/Audio/Generated/door_crank_loop.wav \
  --timeout 45s
```

`--asset` можно повторять; `--output` должен быть новым каталогом под `Temp` проекта. Координатор читает реальные source/meta bytes и зависимости, создаёт свою SourceAssetDB и извлекает ответ worker из собственного DataStore. Он проверяет native content hashes, SHA-256 и неизменность source inputs. Это импорт выбранных файлов, а не bootstrap произвольного проекта и не полная проверка качества всех import profiles.

Для запуска сцены и автоматического применения поддержанных импортов используйте [Go runner](GO_RUNNER.md). Он требует существующий baseline/native catalog и host; результат `project-import` сам по себе не создаёт эти данные.

Перед expansion runner обходит GUID-ссылки выбранной сцены, runtime prefabs и их authored зависимостей. Новые или изменённые FBX/WAV и PNG, требующие нативного resize/compression, импортируются пакетами по 32 ассета в одном worker. Неизменённый baseline/cache и PNG в поддержанном raw profile пропускаются. Этим же способом обновляется прежний index с изменившейся средой импорта. Существующий успешный `project-import` можно принять в runtime cache без повторного запуска worker:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor native-adopt --project . \
  --output "$PWD/Temp/UnityPlay/go-project" \
  --report "$PWD/Temp/UnityPlay/project-import-manual-01/result.json"
```

`native-adopt` заново проверяет source/meta/GUID, все скопированные зависимости, текущую среду и native XXH3-128/SHA256 каждого фактического UDS-артефакта. После адаптации он сохраняет object IDs, model graph, topology hash и resource aliases. Legacy-отчёт без полного v2 environment отвергается.

Для MeshCollider raw loader дополнительно готовит физику из точных native meshes. Editor artifact с `m_IsReadable=0` может содержать vertex/index bytes, но не содержать cooked Player collision data. Go создаёт отдельный immutable runtime файл, временно включая только `m_IsReadable`, `m_KeepVertices` и `m_KeepIndices` у исходно unreadable Mesh; FBX, `.meta`, scene и collider geometry не меняются. Projection proof сохраняет оригинальные bools и offsets, Mesh/type-tree hashes и доказывает побайтовое совпадение всего файла после маскирования этих трёх флагов. Кэш проверяет вывод заново против исходного импортированного файла.

Descriptor `meshCpuRetention` передаёт original readability. Loader вызывает публичный `Physics.BakeMesh` для точного Mesh, authored `MeshCollider.convex` и `cookingOptions`, затем `UploadMeshData(true)` восстанавливает исходную unreadability до пользовательских callbacks. Изменённый cooking profile требует повторной загрузки immutable native файла и bake. Это подготовка настоящей формы коллайдера; bounding-box замены не используются. Прежние native cache entries перепроецируются без нового worker: проверка 545 реальных моделей/1408 meshes с replay и geometry-tamper rejection заняла 0.419 с в одном Go-only прогоне (`Temp/UnityPlay/mesh-cpu-proof/18dc0585279e6977-1a02f0/result.json`). Эта цифра не включает runtime physics cooking или запуск Player.

## Запуск

Нужны Go 1.25 или новее, указанная установка Unity и исходный Dungeon с `R_Floor.fbx`. MCP использует официальный Go SDK v1.8.0. Из корня ворктри:

```bash
mkdir -p Temp/UnityPlay/worker-probe/bin
CGO_ENABLED=0 go -C $UNITY_MICRO_EDITOR_ROOT build -trimpath \
  -ldflags='-s -w' \
  -o "$PWD/Temp/UnityPlay/worker-probe/bin/unity-worker-probe" ./cmd/unity-micro-editor

Temp/UnityPlay/worker-probe/bin/unity-worker-probe \
  --project "$PWD" \
  --output "$PWD/Temp/UnityPlay/worker-probe/first-import" \
  --mode import --timeout 45s
```

`--output` должен быть новым каталогом внутри `Temp` исходного проекта. Повторный запуск требует другого имени каталога. Исходные ассеты и `Library` Dungeon не изменяются; эксперимент создаёт собственный контекст в `<output>/project`.

Параметры:

| Параметр | Значение |
| --- | --- |
| `--editor` | Путь к executable поддержанной версии Unity; по умолчанию установленный `6000.6.3f1` в `/home/shaypercool/Unity/Hub/Editor` |
| `--project` | Корень исходного Dungeon, доступ только для чтения |
| `--output` | Новый каталог под `<project>/Temp` |
| `--mode import` | Инициализация, два импорта и проверка содержимого; режим по умолчанию |
| `--mode initialize` | Только проверка handshake/инициализации, без доказательства работы импортёров |
| `--timeout` | Общий deadline эксперимента, по умолчанию `45s`, максимум `5m` |
| `--reference` | Необязательный успешный JSON-отчёт Editor oracle для сравнения Mesh и PCM |

Отчёт должен показывать `mainEditorStarted=false`, `sourceDBSeeded=false`, `success=true`, `stage=content-validated` и штатное завершение worker. Нулевой `ErrorCode` ответа сам по себе не считается успешным импортом.

## Что происходит

`CreateFixture` создаёт пустые `Assets`, `ProjectSettings`, `Packages`, `Temp`, `Library` и `Logs` собственного контекста. Он копирует **точные байты** `Assets/Dungeon/ShipGen/Art/Custom/Shell/R_Floor.fbx` вместе с `.meta`, а также создаёт стерео WAV: PCM16, 4096 кадров, 22050 Гц. Его `.meta` задаёт PCM, `DecompressOnLoad` и preload. Это две фиксированные проверки, а не сканирование произвольного проекта.

`WriteSourceRegistry` записывает минимальный `SourceAssetDB` непосредственно из Go. Данные о GUID, путях, importer settings и XXH3-128 хешах файлов берутся из созданных исходников. База не копируется из Editor и не требует предварительного импорта.

Координатор поднимает DataStore с уникальным namespace, принимает локальное TCP-соединение worker, выполняет handshake и посылает `Initialize`, `Prepare` и `ImportRequest`. Оба запроса проходят в одном worker. Используется **primary importer ID `-1`**: native pipeline выбирает импортёр и читает настройки из `.meta`. Конкретные ID `1041`/`1020` в этом протоколе запускают extra importer с настройками по умолчанию; такой запрос ранее давал успешный код ответа, но неправильный AudioClip и отсутствующий ресурс, поэтому этот путь отвергается проверкой содержимого.

В `project-import` параметры worker берутся из точной копии `ProjectSettings.asset`: color space, shader precision и Linux graphics API override. Профиль ограничен Unity `6000.6.3f1`, Linux standalone target `24`, subtarget `2`; проверенный автоматический набор API — OpenGLCore `17` и Vulkan `21`. Координатор передаёт эти значения и native custom dependencies `graphics/shader-precision-model` / `graphics/metal-needs-renderpass-fbfetch-fallback` в обеих фазах инициализации. Неподдержанные или неоднозначные настройки отвергаются. SHA256 исходных настроек и применённые значения записываются в `environment` отчёта.

TextureImporter получает `graphics/texture-import-overrides` с явным отсутствием пользовательских overrides и `graphics/default-texture-compression-format`, прочитанным из Linux/Standalone source settings. Проверен DXTC default; остальные настройки требуют отдельной проверки. Холодный импорт `Poster_D.png` занял 3.589 с, сам импорт — 21.2 мс. Native результат и runtime projection побайтово совпали с точным Unity warm artifact: исходный NPOT 96×128 преобразован в RGB24 128×128, восемь mip levels. Отчёт: `Temp/UnityPlay/native-png-production-proof/result.json`.

Холодная доставка PNG ограничена обычными uncompressed textures. Compressed PNG используют точный warm artifact, а изменённый source/meta без такого артефакта отвергается. Проверенный `Starfield.png` сохраняет native BC7 4096×2048, 13 mip levels и 11 184 848 байт. Для BC7 дополнительно сверяется записанная в ArtifactDB custom dependency `graphics/bc7-compressor` с source `EditorSettings.asset`; pinned Default(0) даёт ISPC(2). Его SHA записывается отдельно в `configSourceSha256` и в ключ compressed cache. Изменение compressor settings отвергает прежний cache и исторический warm artifact, сохраняя валидность остальных FBX/WAV entries.

Go runner записывает `environmentSha256` в каждый новый native cache entry. Fingerprint включает ревизию pinned protocol, путь Unity, точный SHA256 `ProjectSettings.asset`, разобранные настройки и wire environment. Старый index без fingerprint или с другой средой пропускается и требует повторного импорта; неизменяемые runtime файлы сохраняются. Изменение этой среды внутри запущенного runner требует его перезапуска, чтобы прежние native objects не выдавались за новый импорт. Текущий fingerprint можно прочитать без Unity процессов:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor native-environment --project .
```

В warm-artifact proof `settingsSourceSha256` фиксирует текущие настройки при извлечении. Его SourceAssetDB/ArtifactDB проверка доказывает совпадение source/meta и native importer revision. Сам этот guard не доказывает совпадение всех глобальных настроек с исторической средой, в которой Editor создал warm artifact.

Этот профиль позволяет вызвать настоящий `ShaderImporter` для `Dungeon/Screen` без основного Editor: проверенный прогон занял 2.815 с, включая запуск worker; сам импорт — 64.4 мс. Его SerializedFile содержит разобранные HLSL snippets, но массивы platform GPU programs пусты. Это результат импорта, требующий отдельного GPU bake перед доставкой Player. Переключение target у такого файла отвергается runtime adapter. Отчёт и дамп находятся в `Temp/UnityPlay/native-screen-shader-production-probe/`.

После ответов координатор завершает worker через протокол, затем DataStore. Go-reader читает только остановленный собственный store. Связь с результатом устанавливается через `ImportResponse.ArtifactBlobs[1]` → `ArtifactMetaInfo.Files[].Hash` → UDS; поле `ReturnedPaths` не является перечнем файлов импортированного ассета.

## Проверка результатов

Для каждого связанного файла проверяются длина, границы хранения и native XXH3-128 content key. Дополнительно записывается SHA256. SerializedFile инспектируется штатным отдельным `Data/Tools/binary2text`; этот процесс не запускает Editor.

Проверяются реальные данные:

- FBX: позиции, нормали, тангенты, UV, bindposes и индексы Mesh. Для текущей модели это 24 вершины и 36 индексов.
- WAV: AudioClip имеет ожидаемые GUID/local file ID, имя, формат, preload и ссылку на ресурс правильного размера. PCM16 из FSB5 декодируется собственным Go-кодом и сверяется с исходным WAV.
- При `--reference`: все шесть Mesh-отпечатков и PCM float hash сравниваются с результатом публичных Unity API в Editor.
- Хеши исходного FBX и `.meta` повторно проверяются после импорта.

Файлы проверки сохраняются в `<output>/artifacts`, wire payloads — в `<output>/frame-*.bin`, логи процессов — в `<output>/logs`. Итог — `<output>/result.json`. Успех выставляется только после извлечения и проверки содержимого.

## Editor oracle

[`Editor/WorkerImportReference.cs`](../tools/WorkerImportReference/WorkerImportReference.cs) — отдельный эталон для измерений. Он запускается только в disposable reference project с теми же входными FBX/WAV. Это не часть собственного импортера и не источник его `SourceAssetDB`.

Публичная точка входа: `WorkerImportReference.Capture`; путь отчёта передаётся как `--worker-reference-report <absolute-json-path>`. Helper выполняет in-process import, читает объекты, иерархию и stable local file IDs через `AssetDatabase`, Mesh arrays через Unity API, PCM через `AudioClip.GetData`. Он также записывает точные пути от `GetArtifactPaths`.

Пути `VirtualArtifacts/Primary/<GUID>` и `VirtualArtifacts/Primary/<GUID>.resource` разрешаются внутри Unity в `uds:/...`. Через обычный `System.IO` они недоступны. Oracle сохраняет исходное значение API с `materialized=false`, `bytes=-1` и пустым SHA256. `success=true` в таком отчёте означает успешное измерение объектов/Mesh/PCM; `artifactFilesMaterialized` отдельно описывает доступность файлов. Собственный UDS-reader проверяет байты независимо.

## Замеры

Пакет из 176 настоящих FBX корабля прошёл через один новый worker за **10.298 с**, peak RSS **396.16 МиБ** (405 668 KiB). Последующие UDS/source проверки, адаптация и запись всех 176 runtime cache entries через `native-adopt` заняли **3.780 с** без Unity процессов. Другой пакет из 363 FBX занял **16.022 с**, его принятие в cache — **4.489 с**. Это по одному измерению каждого пакета; отчёты — `Temp/UnityPlay/multiplayer-native-batch/result.json` и `adopt.jsonl`, `multiplayer-native-batch-rest/result.json` и `adopt.jsonl`.

Три последовательных прогона 2026-10-06 на этой машине, Go 1.26.0, Unity 6000.6.3f1. Каждый создавал новую собственную SourceAssetDB и новый DataStore; worker запускался заново. Кэш файлов ОС и установка Unity уже были прогреты. Во всех трёх прогонах `success=true`, `referenceMatched=true`, `sourceUnchanged=true`, worker завершился с `exit 0`.

| Метрика | Медиана | Диапазон трёх прогонов |
| --- | ---: | ---: |
| Новый worker готов, включая создание контекста | 2.094 с | 2.073–2.173 с |
| FBX, 16 316 байт | 27.3 мс | 26.8–27.7 мс |
| WAV, 16 428 байт | 41.3 мс | 40.8–42.0 мс |
| Оба импорта, остановка процессов и проверка содержимого | 2.877 с | 2.734–3.067 с |
| RSS Go-координатора после проверки | 7.97 МиБ | 7.86–8.08 МиБ |
| RSS worker перед остановкой | 381.3 МиБ | 380.0–382.7 МиБ |
| Peak RSS worker | 405.8 МиБ | 404.1–407.2 МиБ |
| RSS отдельного DataStore перед остановкой | 4.09 МиБ | 4.02–4.12 МиБ |

Stripped `CGO_ENABLED=0` executable занимает 3 428 514 байт, 3.27 МиБ. `file` подтверждает static ELF; Python и .NET для координатора не нужны. Native worker всё ещё загружает библиотеки Unity. Маленькая обвязка не устраняет его расход памяти.

Исходные отчёты лежат в игнорируемых `Temp/UnityPlay/worker-probe/go-measured-{1,2,3}/result.json`, эталон в `Temp/UnityPlay/worker-probe/reference/report.json`. Эти файлы создаются экспериментом и не входят в git.

`stageSeconds` разделяет готовность DataStore, handshake и инициализацию worker; `workerReadyTotal` измеряет весь путь до готового worker. `imports[].elapsedSeconds` включает подготовку следующего запроса и ожидание его ответа. `nativeSeconds` включает остановку native процессов; `elapsedSeconds` также включает последующую offline-проверку. `content.elapsedSeconds` отдельно измеряет извлечение, `binary2text` и проверку. Память coordinator, worker и DataStore учитывается раздельно в KiB; peak RSS worker берётся из Linux process accounting. Время импорта ассета не следует выдавать за время запуска новой сцены.

## Код и тесты

| Файл/API | Ответственность |
| --- | --- |
| `main.go` | CLI, процессы, deadlines, отчёт и штатное завершение |
| `fixtures.go`: `CreateFixture` | Входные файлы и изолированный scratch context |
| `source_database.go`: `WriteSourceRegistry` | Собственная LMDB-база SourceAssetDB и native source hashes |
| `protocol.go` | Наблюдаемый wire format, requests/responses и ArtifactMetaInfo |
| `datastore.go`: `DataStoreService` | Собственный namespace, readiness и остановка native DataStore |
| `lmdb.go`: `ReadLMDBTable` | Чтение остановленной LMDB без native DLL |
| `artifact.go`: `OpenUDSArtifactStore` | Связанное извлечение UDS-файлов и проверка content keys |
| `FingerprintFixtureMesh`, `FingerprintFixturePCM`, `ValidateFixtureAudioClipDump` | Строгие проверки двух текущих fixtures |
| `validation.go` | Условия успеха и сравнение с Editor oracle |

```bash
go -C $UNITY_MICRO_EDITOR_ROOT test ./...
```

Обычные тесты проверяют границы протокола/баз, повреждённое содержимое и неверные настройки; Unity не запускается. Дополнительные integration tests читают только остановленные store snapshots. Пути передаются через `UNITY_WORKER_ARTIFACT_REFERENCE`, `UNITY_WORKER_ARTIFACT_OUTPUT`, `UNITY_WORKER_ARTIFACT_EDITOR`, `UNITY_WORKER_ARTIFACT_RESPONSE`, `UNITY_WORKER_REFERENCE_REPORT` и `UNITY_WORKER_AUDIO_CLIP_DUMP`. Они запускают только standalone `binary2text`, если нужна инспекция SerializedFile.

## Границы прототипа

Wire protocol, LMDB schema и UDS layout получены для одной версии Unity и не являются стабильным публичным API. Другая версия требует отдельной проверки; несовпадение версии worker вызывает отказ.

Fixed fixture validators проверяют указанный FBX и PCM WAV. Дополнительно Go runner прошёл native Player readback настоящего preloaded Vorbis `door_crank_loop.wav` и его обновления в SampleScene, а PNG проверен через точное совпадение native output с Unity artifact. Нет доказанной общей поддержки анимации, skinned meshes, сложных model settings, произвольных texture settings, остальных audio codecs, platform overrides, пользовательских AssetPostprocessor и всех зависимостей произвольного проекта. Mesh/FSB5 validators намеренно принимают только наблюдаемые fixture layouts. UDS-reader поддерживает inline и concatenated storage, ограничивает один файл 64 МиБ и отвергает остальные варианты; LMDB-reader рассчитан на наблюдаемый Linux x64 формат.

Доставка поддержанных native SerializedFile, GUID/local-ID binding и проверяемый cache реализованы в [Go runner](GO_RUNNER.md). Неизменённые тяжёлые зависимости продолжают загружаться из подготовленного bundle. Автоподготовка поддержанного профиля доступна через `auto-prepare`; покрытие произвольных import profiles остаётся неполным. Координатор запускает worker на требуемый импорт, постоянная native служба пока не используется. `.unity`/`.prefab` остаются на собственном loader и не собираются в AssetBundle.
