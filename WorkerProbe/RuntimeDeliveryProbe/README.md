# Доставка результатов worker в Mono Player

[Go runner](../../GO_RUNNER.md) использует reusable `runtimeartifact` и production loader. Ниже сохранены отдельные fixture experiments `Patch`/`Archive` и их readback; fixture-only `Patch` не является production converter.

Изолированный эксперимент для Unity Linux `6000.6.3f1`. Основной Editor и `BuildPipeline` не используются. Сцена остаётся JSON-графом нашего raw loader; эти инструменты работают только с уже импортированными native asset artifacts.

Публичный [`ContentLoadInterface.LoadContentFileAsync`](https://docs.unity.com/en-us/engine/6000.6/script-reference/unity/loading/contentloadinterface) загружает binary SerializedFile. `ContentFile.GetObject(ulong)` сохраняет доступ по точному local file ID; `GetObjects()` возвращает загруженные объекты. Модуль уже есть в нашем Mono Host.

Прямой файл worker объявляет target `-2` (Editor). Player отказывается читать его. Простая замена target на Linux `24` восстановила модель, но вызвала native errors для `ModelImporter` и служебного `Prefab`. Удаление этих двух записей из object table дало семь runtime objects без native errors: native Mesh, два GameObject, два Transform, MeshFilter и MeshRenderer. Mesh содержит 24 vertices, 36 indices; наблюдавшееся время load — около 7 мс в уже запущенном Player. Это один маленький fixture, а не замер полного проекта.

`Patch` намеренно принимает только SHA-256 двух точных проверенных артефактов fixture. Меняются platform, object count и metadata size; runtime object bytes, type trees, local IDs, references и data offset сохраняются. Editor-only data blobs остаются в файле, но больше не создают объектов. Формат metadata сверялся с [официальным UnityDataTools reader](https://github.com/Unity-Technologies/UnityDataTools/blob/main/UnityBinaryFormat/SerializedFileDetector.cs). Этот patcher нельзя применять к произвольным ассетам.

`Archive` — маленький Go-инструмент, создающий uncompressed UnityFS с копиями SerializedFile и `.resource`. Он не сериализует Unity objects, не содержит AssetBundle registry или сцены. Aliases сохраняют пути `VirtualArtifacts/Primary/<GUID>[.resource]`. [`ArchiveFileInterface.MountAsync`](https://docs.unity.com/en-us/engine/6000.6/script-reference/unity/io/archive/archivefileinterface/mountasync) монтирует этот контейнер в VFS; каждый импортируемый ассет можно хранить отдельно.

Сборка из каталога `$UNITY_MICRO_EDITOR_ROOT/WorkerProbe`:

```bash
CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' \
  -o ../../../Temp/UnityPlay/worker-probe/runtime-delivery/patch-native-fixture \
  ./RuntimeDeliveryProbe/Patch

CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' \
  -o ../../../Temp/UnityPlay/worker-probe/runtime-delivery/native-content-archive \
  ./RuntimeDeliveryProbe/Archive

bash RuntimeDeliveryProbe/compile.sh \
  /absolute/path/to/project/Temp/UnityPlay/worker-delivery
```

`compile.sh` вызывает bundled Roslyn и ссылки Linux Mono Player. C# probe — обычная DLL для существующего Player, не отдельная .NET runtime обвязка. Сборка проходит с warnings as errors.

Patcher вызывается с новым output path:

```bash
patch-native-fixture \
  --input /absolute/native/artifact.bin \
  --strip-editor-objects \
  --output /absolute/Temp/converted.bin

native-content-archive \
  --guid 71705265666978617564696f70726f62 \
  --serialized /absolute/Temp/converted.bin \
  --resource /absolute/native/artifact.resource \
  --output /absolute/Temp/new-content.unityfs
```

Raw manifest для probe содержит пустые `objects` и `assets`, DLL в `assemblies` и initializer:

```json
{
  "assembly": "WorkerRuntimeDeliveryProbe",
  "typeName": "UnityPlay.WorkerProbe.WorkerRuntimeDeliveryProbe",
  "methodName": "__UnityPlay_RuntimeInitialize_WorkerDelivery",
  "phase": 3
}
```

Перед запуском Player задаются:

```text
UNITY_PLAY_WORKER_DELIVERY_FILE=/absolute/converted.bin
UNITY_PLAY_WORKER_DELIVERY_KIND=mesh|audio
UNITY_PLAY_WORKER_DELIVERY_REPORT=/absolute/Temp/report.json
```

Опциональный `UNITY_PLAY_WORKER_DELIVERY_ARCHIVE=/absolute/content.unityfs` включает mount и загрузку члена архива. `UNITY_PLAY_WORKER_DELIVERY_DEFAULT_NAMESPACE=1` выбирает Default namespace. Нужен внешний timeout процесса: native parse/GetData способны упасть; ограниченные WaitForCompletion не защищают от ошибок native code.

Probe проверяет Mesh topology, точные IDs, model hierarchy и ссылку MeshFilter на тот же Mesh. Для WAV проверяются samples/channels/frequency и SHA-256 реального `AudioClip.GetData` против Editor oracle. `success` требует отсутствие native errors. Проверка AudioClip не вызывает GetData после failed resource load: Unity `6000.6.3f1` в таком случае может упасть внутри FMOD.

Простой archive mount в отдельном namespace не разрешил относительный аудиоресурс: FMOD сообщил File not found. Физическое размещение `.resource` восстановило `AudioClip.loadState=Loaded`, `GetData=true` и PCM hash Editor без native errors; наблюдавшееся время загрузки маленького WAV — около 8 мс. В исходном fixture каталог SerializedFile совпадал с CWD Player, поэтому он не различал эти базы относительного пути. Project readback выявил, что физический SerializedFile ищет `VirtualArtifacts/Primary/<GUID>.resource` рядом со своим каталогом. Production staging сохраняет ресурс там и в CWD mirror; runtime проверяет оба пути до native load. Adapter принимает preloaded PCM/DecompressOnLoad и preloaded Vorbis/CompressedInMemory с проверенным inline AudioClip layout; streaming и background load отклоняются.

Публичный [контракт CompressedInMemory](https://docs.unity.com/en-us/engine/6000.5/script-reference/unityengine/audiocliploadtype/compressedinmemory) также требует полного хранения сжатых данных в памяти. Из этого следует, что Loaded/preloaded clip должен продолжить воспроизведение своей версии после публикации нового ресурса по тому же GUID; это вывод из документации, а не отдельный результат проверки live replacement.

Отсутствие ошибок parser или `LoadingStatus.Completed` само по себе не доказывает загрузку: обязательны native object readback и resource readback.

После этих native проверок projection вынесена в reusable Go package [runtimeartifact](../../runtimeartifact/adapt.go): `Adapt(input, "fbx"|"wav")`. Она возвращает adapted bytes, точные object IDs/types, ordered external references и resource paths. Известные Editor-only и DontSaveInBuild records удаляются; неизвестные classes, Shader, script-backed types и SerializeReference отклоняются. `RuntimeDeliveryProbe/Adapt` — отдельная CLI для проверки этого API.

Для FBX metadata также содержит `modelGraph` и `modelTopologySHA256`: точные component lists, hierarchy, mesh/material references и используемые raw loader значения GameObject/Transform/MeshRenderer. Позиции и имена входят в проверку, геометрия Mesh — нет. Поля читаются только при совпадении доказанных inline type tree hashes; непроверенные skinned/Animator layouts возвращают `modelGraphUnsupportedReason`, поэтому их нельзя принимать для замены cached модели. Совпадение local IDs само по себе недостаточно: изменённый native Transform или component graph нельзя маскировать старым raw catalog.

Runtime loader принимает `nativeContentFiles` в raw manifest, читает их через публичный ContentLoadInterface, сохраняет handles в session cache и подменяет registry GUID/localID. Новая версия native asset может загружаться в уже запущенный Player; прежний файл удерживается, пока живёт предыдущая сцена. Обычные native bundle packs остаются источником остальных ассетов. Общая поддержка анимации/Avatar, shader content и произвольных import profiles ещё требует отдельных проверок.

Native golden tests читают только игнорируемые экспериментальные artifacts:

```bash
UNITY_WORKER_NATIVE_FIXTURES=/absolute/Temp/worker-import/artifacts \
  UNITY_WORKER_PROJECT_AUDIO_FIXTURES=/absolute/Temp/real-project-audio/artifacts \
  go test -race ./runtimeartifact
```

`ProductionFixture` создаёт отдельную raw сцену с настоящим Dungeon `R_Floor`, AudioSource и proof MonoBehaviour, использующую production `NativeContentRuntime`. Editor/Player процесс этот генератор не запускает. DLL компилируется обычным `compile.sh`; scene.json публикуется через:

```bash
go run ./RuntimeDeliveryProbe/ProductionFixture \
  --project /absolute/path/to/project \
  --output /absolute/path/to/project/Temp/UnityPlay/native-content-production-proof
```

Player fixture работает с CWD output directory; ресурс лежит также рядом с SerializedFile по тому же virtual alias. В `UNITY_PLAY_NATIVE_CONTENT_PROOF_REPORT` передаётся абсолютный путь JSON-отчёта. Для PCM proof сравнивает с Editor oracle SHA-256 от little-endian длины массива и float samples; для Vorbis проверяет native audio identity/load state и вызывает публичный AudioSource.Play, без GetData. Оба production fixture прошли finite Player readback без errors/exceptions; реальный `door_crank_loop.wav` вернул `isPlaying=true`. В headless fixture MeshRenderer отсутствует: Host URP не содержит Standard shader для builtin material 10303. Проверка Render/URP относится к отдельной SampleScene.

`FastPatchFixture` проверяет сохранение runtime state при быстрых сценовых patch. Его Go generator публикует четыре raw revisions: исходную сцену, rename, добавление child и добавление root со SceneRoots. Observer MonoBehaviour получает target через сериализованные ссылки и меняет его position на `(100,2,3)`, layer на `7`, tag на `Player`, active на `false`. Последующие source patch должны сохранить все эти значения и EntityId объекта.

```bash
go run ./RuntimeDeliveryProbe/FastPatchFixture \
  --project /absolute/path/to/project \
  --output /absolute/Temp/UnityPlay/fast-patch-production-proof \
  --phase 0
```

Базовый manifest лежит в `revisions/phase0/scene.json`. Player запускается отдельно в interactive режиме с `--unity-cli-raw-reload <output>/request.json`. Env `UNITY_PLAY_FAST_PATCH_REPORT=<output>/proof.json` и `UNITY_PLAY_FAST_PATCH_PHASE_FILE=<output>/phase.txt` задают абсолютные пути. Повторные вызовы generator с `--phase 1`, `2`, `3` публикуют соответствующий patch request; после каждого ожидается `proof.phase=N && proof.success=true` и успешный runtime ack. JSON observer и request публикуются атомарной заменой файлов. Добавленный child должен получить local position `(1,0,0)` и world position `(101,2,3)`; последняя фаза должна иметь три authored roots и на один native scene root больше исходной. Runtime resource owner также входит в настоящий `Scene.rootCount` и показывается в отчёте.
