# Multiplayer proof helper

Фактический прогон 6 октября 2026: два Linux Mono Player запущены через Go runner, комната `ZSFVW`, host PID `1919523`, client PID `1924645`. Editor не запускался; использован подготовленный native cache и описанные ниже shader snapshots. Исходные `Boot.unity`, `ShipGenDemo.unity`, `NetShipPlayer.prefab`, FBX и UI-ассеты не изменялись.

Оба peers независимо построили корабль по ship seed `20823`, scenario seed `23385`: 33 комнаты, 17 зон, 1334 ячейки. Layout SHA-256 `e8071149d5497d815d660d7bd89d557bff82ca63268b4a21936777f7425cae0a`, scenario SHA-256 `577be57b7703ad0e26efa53e44508e5ad221d5106977327909710cfcf8312b72` совпали. На каждом — 11235 активных mesh renderers с поддержанными шейдерами, 5499 solid colliders, два живых сетевых аватара; raycast обоих игроков попадает в настоящий пол корабля. Захвачено 0 Unity errors/exceptions.

`exercise` подтвердил переключение и восстановление питания сектора, настоящий клиентский `UseRpc` для двери №8 и открытие её панели на обоих peers (>0.88), движение клиента по горизонтали (0.632 м) и его наблюдение хостом (0.453 м). После проверки получено подтверждение остановки бота. `verify` повторно прошёл после всех команд. Lobby, ShipHUD и PlayerHUD получили исходные USS и шрифты; все элементы с классом `.hidden` имеют computed `Display.None`. Скриншоты подтверждают видимую геометрию корабля, второго персонажа и исходный HUD.

Замеры при двух одновременно работающих Player, 960×540, лимит 30 FPS, NVIDIA RTX 4060 / OpenGLCore:

| Метрика | Host | Client |
|---|---:|---:|
| Go launch → readiness | 8.334 с | 7.717 с |
| Player RAM, PSS | 1070.1 МиБ | 1091.5 МиБ |
| Go launcher RAM, PSS | 35.7 МиБ | 37.0 МиБ |
| Player + launcher, PSS | 1105.8 МиБ | 1128.5 МиБ |
| GPU framebuffer, NVIDIA pmon | 333 МБ | 333 МБ |
| Установившийся FPS, окно ≥1 с | 30.01 | 30.00 |

Корабль клиента готов на отметке 21.788 с его Unity runtime clock. Отметка host 63.272 с включает ожидание ручного запуска второго Player и не является временем генератора. Эти числа относятся к готовому host/cache, не к первому импорту произвольного проекта. Последняя подготовка с перекомпиляцией runtime bridge заняла 21.857 с, без native worker; предыдущая повторная подготовка без изменений кода — 14.787 с. Additive scene/content edits пока требуют перезапуска Player и сетевой сессии.

Отчёты и изображения лежат в игнорируемом `Temp/UnityPlay/multiplayer`: `proof-final.json`, `exercise-proof-final.json`, `final-metrics/metrics.json`, `host-final.png`, `client-final.png`. GPU измерен через `nvidia-smi pmon`, PSS — через `/proc/<pid>/smaps_rollup`; FPS получен из frameCount/time в реальном InShip, а не из заданного лимита. Результаты воспроизводятся командами ниже.

Опциональная тестовая DLL и Go driver для уже запущенных Player. Здесь нет запуска Editor, импорта или изменения исходного контента. Observer использует публичные `NetSession`, `SessionNetwork`, `ShipRuntime`, `ShipDoor`, `NetworkPlayer`; компонентов ищет через корневые объекты загруженных сцен. Частные члены через рефлексию не читаются.

```bash
$UNITY_MICRO_EDITOR_ROOT/proofs/multiplayer/compile.sh \
  "$PWD/Temp/UnityPlay/multiplayer-proof/helper" \
  /absolute/fresh/Player/UnityPlayHost_Data/Managed
```

Скрипт сначала инструментирует исходники настоящим SDK `LifecycleCompiler`, затем собирает DLL; proof и преобразованные исходники сохраняются рядом. Go-команда `cmd/prepare` добавляет DLL и SHA в обе сцены, подключает `RawContentProvider` к `NetLifetimeScope` и записывает initializer:

```json
{"assembly":"UnityPlay.MultiplayerProof","typeName":"UnityPlay.MultiplayerProof.MultiplayerObserver","methodName":"__UnityPlay_RuntimeInitialize_MultiplayerProof","phase":3}
```

Для каждого Player задать свои абсолютные пути:

```text
UNITY_PLAY_MULTIPLAYER_REPORT=/absolute/Temp/host.json
UNITY_PLAY_MULTIPLAYER_COMMAND=/absolute/Temp/host-command.json
UNITY_PLAY_MULTIPLAYER_ROLE=host
```

Без `UNITY_PLAY_MULTIPLAYER_REPORT` initializer ничего не создаёт. Snapshot обновляется каждые 100 мс. Он содержит сетевое состояние, членов комнаты, параметры sortie, канонический SHA-256 формы корабля и сценария, аватары с ownership/object ID и публичной диагностикой полученных samples, sector power и состояния дверей. `shipGeometry` и `players[].geometry` подтверждают активные Renderer с непустыми vertex/index buffers, материалами с поддержанными шейдерами и активные Collider с непустыми bounds. `players[].shipFloorSupported` подтверждает raycast вниз к настоящему collider внутри иерархии корабля. `ready` требует этих объектов и ровно двух живых игроков без захваченных Unity errors/exceptions; driver проверяет также согласованность двух peers. Эти проверки не доказывают видимость меша в конкретной камере, для неё нужен screenshot. Неподдержанные материалы сохраняются отдельным счётчиком; shader passes и скрытые URP debug shaders здесь не проверяются.

`uiDocuments` раз в секунду читает активные `UIDocument` во всех загруженных сценах, включая `DontDestroyOnLoad`: ссылки VTA/USS, порядок стилей на элементах, классы, вычисленный `display`, inline `display`, layout и подключение к panel. Уникальный VTA/USS сериализуется через публичный `JsonUtility` один раз в `<имя отчёта>-ui-assets/`; `jsonFile` и `jsonSHA256` указывают сохранённый native asset snapshot. Диагностика не меняет UI. `phaseFirstObservedInShipAtSeconds` отмечает первое наблюдение состояния `InShip` после синхронной загрузки сцены, поэтому не измеряет начало этой загрузки; `steadyFramesPerSecond` рассчитывается по разнице `Time.frameCount` и реального времени в окне не короче секунды после двух секунд прогрева.

Observer сканирует обычные сцены и публичную persistent scene своего `gameObject`: NGO переносит общий root Session через `DontDestroyOnLoad`, а `SceneManager.sceneCount` эту сцену не перечисляет. В `observedScenes` сохраняются корни, типы MonoBehaviour, имя сборки и MVID; список позволяет отличить отсутствие компонента от несовпадения типов DLL. Пока session не найдена, диагностика обновляется раз в секунду; у каждого корня записываются первые 256 MonoBehaviour с отметкой усечения.

`RawContentProvider` — отдельный opt-in adapter публичного `ContentRuntimeProvider`: network prefab и ship scene по GUID загружаются через `Dungeon.CLIPlay.Raw.RawProjectContent`. Сцену создаёт наш raw loader, новый Avatar/scene AssetBundle не нужен. Остальные native ассеты продолжают использовать подготовленный heavy content cache. Provider добавляется в Boot manifest и передаётся сериализованной ссылкой `NetLifetimeScope.contentProvider`.

Для полного корабля нужны Unity `6000.6.3f1` Linux SDK и уже подготовленный `Temp/UnityPlay/hybrid-sample/current.json`: Mono host, compiler plan/configurations и native catalog этого же проекта. UI/fonts и пока неподдержанные cold compression profiles используют проверенные source/meta-совпадающие артефакты из существующей `Library`. Для `Dungeon/Screen` текущая проверка использует retained compiled Linux shader в ship native index; это не доказательство самостоятельной компиляции текущего HLSL. Исходный Linux archive и извлечённые shader members также должны оставаться доступными. Эти команды не готовят чистый произвольный Unity-проект; границы подготовки описаны в [GO_RUNNER.md](../../docs/GO_RUNNER.md) и [worker README](../../docs/WORKER_IMPORT.md).

Повторяемая подготовка настоящих `Boot`, `ShipGenDemo` и `NetShipPlayer` из корня проекта. Каталог `proof_base` переиспользуется для compiler/native caches; отдельный `proof_dir` хранит отчёты конкретного прогона:

```bash
proof_project="$PWD"
proof_base="$proof_project/Temp/UnityPlay/multiplayer-base"
proof_ship=a4c2d5e87cc894d488ffbfd8c52ed16a
proof_builtin="$proof_project/Temp/UnityPlay/builtin-shaders/panoramic-v1/runtime.bin.snapshot.json"
proof_dir="$proof_project/Temp/UnityPlay/multiplayer-proof/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$proof_dir"
```

`ShipGenDemo` ссылается на builtin shader `Skybox/Panoramic` (`fileID: 108`), который отсутствует в Sample host. Один раз создайте его snapshot из уже собранного Linux builtin archive и сохранённого SerializedFile member. Команда `cmd/builtin-shader` проверяет SHA archive/member, извлечённые bytes против настоящего UnityFS member и имя builtin108 против `unity_builtin_extra` установленного движка. Она сохраняет один Shader и compiled GPU payload; исходные ассеты не меняются, Unity процессы не запускаются:

```bash
proof_editor=$(jq -er '.compiler.unity' "$proof_project/Temp/UnityPlay/hybrid-sample/current.json")
proof_builtin_bundle="$proof_project/ServerData/StandaloneLinux64/83b116fcd6c107ce42c4e91787af67e0_unitybuiltinassets_9354b72a9bcafe03d438c548e5f47bc9.bundle"
proof_builtin_member="$proof_project/Temp/UnityPlay/shader-search/extracted/44b3be058069541c9603e0ab7800a6e9ffc25723c64d4cce8a1cd2648b64e187/CAB-8fd08f3d449cb05affd5d18578396b44"

if [ ! -f "$proof_builtin" ]; then
  (cd "$UNITY_MICRO_EDITOR_ROOT" && go run ./cmd/builtin-shader \
    --project "$proof_project" \
    --input "$proof_builtin_member" \
    --input-sha256 e0830e780513f8c3ef74142cfab25093a88ab81d51991d9bef9765ea87e9a9cd \
    --bundle "$proof_builtin_bundle" \
    --bundle-sha256 44b3be058069541c9603e0ab7800a6e9ffc25723c64d4cce8a1cd2648b64e187 \
    --member-name CAB-8fd08f3d449cb05affd5d18578396b44 \
    --builtin-source "$(dirname "$proof_editor")/Data/Resources/unity_builtin_extra" \
    --output "${proof_builtin%.snapshot.json}")
fi
```

Это пути и SHA существующего Linux build в данном ворктри. Для другого build нужны его настоящий archive/member и соответствующие SHA; один лишь файл `unity_builtin_extra` не даёт compiled Linux GPU programs. Готовый snapshot переиспользуется, а runner при каждой подготовке заново проверяет его происхождение и payload hashes. Сохраняйте также archive/member, на которые он ссылается.

```bash

GOMEMLIMIT=1GiB $UNITY_MICRO_EDITOR_ROOT/unity-micro-editor run \
  --project "$proof_project" \
  --baseline "$proof_project/Temp/UnityPlay/hybrid-sample/current.json" \
  --output "$proof_base" \
  --scene Assets/Dungeon/Net/Scenes/Boot.unity \
  --content-scene "$proof_ship=Assets/Dungeon/ShipGen/Scenes/ShipGenDemo.unity" \
  --content-prefab Assets/Dungeon/Net/Prefabs/NetShipPlayer.prefab \
  --builtin-shader "$proof_builtin" \
  --prepare-only > "$proof_dir/prepared.jsonl"

proof_manifest=$(jq -er 'select(.event == "prepared") | .manifest' "$proof_dir/prepared.jsonl")
proof_managed=$(dirname "$(jq -er '.assemblies[0]' "$proof_manifest")")
$UNITY_MICRO_EDITOR_ROOT/proofs/multiplayer/compile.sh "$proof_dir/helper" "$proof_managed"

(cd $UNITY_MICRO_EDITOR_ROOT && go run ./proofs/multiplayer/cmd/prepare \
  --input "$proof_dir/prepared.jsonl" \
  --helper "$proof_dir/helper/UnityPlay.MultiplayerProof.dll" \
  --lifecycle-proof "$proof_dir/helper/lifecycle-proof.json" \
  --metadata-helper "$proof_dir/helper/LifecycleCompiler/payload/LifecycleCompiler.dll" \
  --output-launch "$proof_dir/launch.json")
```

`--prepare-only` не запускает Player. Поддержанные heavy ассеты при cache miss импортирует native worker; сцены и префабы читает Go loader. Helper компилируется после подготовки против свежих DLL из Boot manifest. `cmd/prepare` создаёт отдельный overlay и проверяет инструментирование, GUID сцены, ссылки и хеши. Исходные сцены и подготовленный host не изменяются.

Импортированные meshes для MeshCollider получают временные CPU-retention flags в отдельном runtime projection с побайтовым proof. Loader готовит точную native collision geometry через `Physics.BakeMesh` с authored convex/cooking options и возвращает исходную unreadability до script callbacks. Геометрия, FBX/meta и сцены сохраняются; повторный native import для этого шага не нужен. Подробности и границы — в [worker README](../../docs/WORKER_IMPORT.md).

Для повторов сохраняйте `proof_base` вместе с referenced runtime/import files. Новый timestamp в `proof_dir` не сбрасывает этот cache. Новый `--output` заново импортирует недостающие модели и не наследует ship native index с retained Screen shader. Существующие успешные native batch reports можно принять без повторного worker в тот же ship cache:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor native-adopt --project "$proof_project" \
  --baseline "$proof_project/Temp/UnityPlay/hybrid-sample/current.json" \
  --output "$proof_base/content/$proof_ship" \
  --report "$proof_project/Temp/UnityPlay/multiplayer-native-batch/result.json"

$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor native-adopt --project "$proof_project" \
  --baseline "$proof_project/Temp/UnityPlay/hybrid-sample/current.json" \
  --output "$proof_base/content/$proof_ship" \
  --report "$proof_project/Temp/UnityPlay/multiplayer-native-batch-rest/result.json"
```

Это необязательный шаг для сохранённых отчётов данного ворктри: один пакет содержит 176 FBX, другой — 363. `native-adopt` требует их исходные contexts/UDS artifacts и заново проверяет текущие source/meta/dependencies и среду импорта. Он не заменяет baseline, warm UI/texture artifacts или retained shaders. Новые импорты runner собирает в batches; фиксированного обещания cold startup time для полного проекта здесь нет.

Для локальной сессии запустить relay из `Server/relay` в отдельном терминале:

```bash
cd "$proof_project/Server/relay"
go run . -addr 127.0.0.1:18091
```

В терминале запуска Player проверить readiness:

```bash
curl --fail --silent --show-error http://127.0.0.1:18091/healthz
```

Ответ должен быть `ok`. Оба Player ниже используют этот relay через явный `-relay`. После проверки остановить relay через Ctrl+C в его терминале.

Запуск host в отдельном терминале, из корня проекта и с теми же `proof_project` и `proof_dir`:

```bash
UNITY_PLAY_MULTIPLAYER_REPORT="$proof_dir/host.json" \
UNITY_PLAY_MULTIPLAYER_COMMAND="$proof_dir/host-command.json" \
UNITY_PLAY_MULTIPLAYER_ROLE=host \
$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor run --project "$proof_project" \
  --prepared "$proof_dir/launch.json" --output "$proof_dir/host-run" \
  --interactive --fps 30 --width 640 --height 360 -- \
  -relay ws://127.0.0.1:18091/ws \
  -host -name Host -code-file "$proof_dir/room.txt" -autostart 2
```

После появления `room.txt` запустить client в другом терминале:

```bash
proof_room=$(cat "$proof_dir/room.txt")
UNITY_PLAY_MULTIPLAYER_REPORT="$proof_dir/client.json" \
UNITY_PLAY_MULTIPLAYER_COMMAND="$proof_dir/client-command.json" \
UNITY_PLAY_MULTIPLAYER_ROLE=client \
$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor run --project "$proof_project" \
  --prepared "$proof_dir/launch.json" --output "$proof_dir/client-run" \
  --interactive --fps 30 --width 640 --height 360 -- \
  -relay ws://127.0.0.1:18091/ws -join "$proof_room" -name Client
```

Оба процесса используют один immutable overlay. Host создаёт комнату через штатный `NetAutomation`, client входит по её коду. `-autostart 2` запускает sortie после подключения двух участников. Для ограниченного прогона заменить `--interactive` на `--frames 3600`; при 30 FPS это около двух минут. Ctrl+C в каждом терминале останавливает соответствующий Player.

```bash
cd $UNITY_MICRO_EDITOR_ROOT
go run ./proofs/multiplayer verify \
  --host /absolute/Temp/host.json --client /absolute/Temp/client.json \
  --output /absolute/Temp/proof.json --timeout 90s
```

Для активной проверки добавить `exercise` и файлы команд обоих peers:

```bash
go run ./proofs/multiplayer exercise \
  --host /absolute/Temp/host.json --client /absolute/Temp/client.json \
  --host-command /absolute/Temp/host-command.json \
  --client-command /absolute/Temp/client-command.json \
  --output /absolute/Temp/exercise-proof.json --timeout 90s
```

Driver меняет и восстанавливает питание сектора 0 на host, подтверждает получение client через `StateApplied`; выбирает закрытую дверь и вызывает use-door на client через `ship.Commands.Use` (настоящий RPC и host gameplay path), проверяя state/визуальное открытие с обеих сторон. Для powered door и hatch проверяется публичное расстояние до ближайшего участника экипажа на обоих peers: оно должно быть не больше авторского `autoCloseDistance`, иначе штатное автозакрытие сразу отменит запрос. Unpowered sliding door остаётся открытой после crank. Затем driver запускает bot client, подтверждает горизонтальное движение на host и ждёт подтверждения остановки bot. Это проверка публичного gameplay API и сетевого транспорта; клавиатурный ввод и выдерживание hold timer она отдельно не проверяет. Snapshot старше 3 с не принимается. Для проверки возвращаемого пути DLL без нового content API можно компилировать с `MULTIPLAYER_OBSERVER_ONLY=1`.
