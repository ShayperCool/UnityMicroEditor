# Unity Player из CLI

Текущий Go runner запускает Dungeon SampleScene и наблюдает за сценой, C# и поддержанными native-ассетами без Python и основного Editor. Команда запуска, необходимые baseline/host, замеры и ограничения находятся в [GO_RUNNER.md](GO_RUNNER.md). `$UNITY_MICRO_EDITOR_ROOT/unity-play-go` использует уже подготовленный тяжёлый контент; запуск чистого произвольного проекта пока не готов.

Ниже описан прежний Python CLI `$UNITY_MICRO_EDITOR_ROOT/unity-play` и его этапы подготовки.

Эксперимент для Unity 6 на Linux. [`raw-run`](RAW_PROJECTS.md) компилирует исходный C# и восстанавливает сохранённые `.unity` и авторские `.prefab` через Unity runtime API. Универсальный Mono host готовится один раз в отдельном проекте инструмента.

Гибридный режим использует штатный Unity importer для моделей, аудио, анимации, материалов, UI и URP. Импорт запускается в отдельном проекте при изменении тяжёлого контента; его результат хранится в проверяемом native-кэше. Сцены и авторские префабы остаются на собственном loader и не входят в этот пакет. Изменение сцены и обычного игрового C# не требует Editor или `Library` входного проекта. Минимальный режим для простых ассетов обходится без native-кэша.

Прежний путь с полноценным Unity import сохранён: после подготовки Player команда `run` запускает настоящий native Unity runtime без Editor. `refresh-code` перекомпилирует подходящую C# assembly через Roslyn, `refresh-assets` экспортирует сцены с зависимостями в AssetBundle для следующего запуска того же Player.

Первый импорт, подготовка Player и экспорт изменённых ассетов в прежнем пути требуют установленный Unity Editor. Запуск сцены и поддерживаемая перекомпиляция кода обходятся без него. В этом прежнем пути файлы `.unity`, `.prefab`, материалы и шейдеры должны пройти штатный asset pipeline. `-nographics` отключает графику, но не убирает физику и native runtime. Это не полный аналог Editor Play Mode, где состояние сцены может ещё не быть сохранено.

Подробности моддинга, порядок запуска движка и первоисточники собраны в [RESEARCH.md](RESEARCH.md). Эксперимент с обычным .NET находится в [ManagedProbe](ManagedProbe/README.md). Возможность работать с сырым контентом без Editor исследована отдельно в [NO_EDITOR_CONTENT.md](NO_EDITOR_CONTENT.md), прямые API пробы — в [ContentProbe](ContentProbe/README.md). `--project /absolute/project install` копирует bridge без Dungeon benchmark-сцен; существующие изменённые файлы не перезаписывает.

Штатные native импортёры, import workers и проверенный независимый FBX SDK разобраны в [NATIVE_IMPORTERS.md](NATIVE_IMPORTERS.md).

[Go-координатор AssetImportWorker](WorkerProbe/README.md) импортирует тяжёлые ассеты без основного Editor через собственные SourceAssetDB/DataStore и штатные FBX/WAV импортёры. Прямую доставку native SerializedFile в Player теперь использует [Go runner](GO_RUNNER.md); для неизменённых зависимостей он сохраняет прежний bundle. Старые microbenchmarks двух ассетов относятся к отдельному worker prototype. `unity-play` остаётся прежним Python CLI, а его `import-assets` использует Editor.

Собственный pipeline развивается через [сравнение с настоящим Editor Play Mode](CompatibilityProbe/README.md). Одинаковые исходные сцены запускаются в изолированном Editor и в raw Player; общий observer сравнивает восстановленные данные и lifecycle. Cooked результаты Editor в raw запуск не передаются.

## Запуск Dungeon

Гибридный путь для исходной SampleScene:

```bash
# Однократная подготовка host с runtime-пакетами проекта.
$UNITY_MICRO_EDITOR_ROOT/unity-play host-build --project-packages \
  --player Builds/CLIPlayHybridHost/UnityPlayHost.x86_64

# Штатный импорт тяжёлого контента в отдельном проекте инструмента.
$UNITY_MICRO_EDITOR_ROOT/unity-play import-assets --scene Assets/Scenes/SampleScene.unity

# Загрузка исходной сцены и наблюдение за изменениями.
$UNITY_MICRO_EDITOR_ROOT/unity-play raw-run --scene Assets/Scenes/SampleScene.unity \
  --player Builds/CLIPlayHybridHost/UnityPlayHost.x86_64 \
  --native-import --watch --fps 60 --no-vsync --width 960 --height 540
```

`--watch` готовит новую revision. Совместимые правки сцены, авторских префабов и raw-ассетов загружаются внутри работающего Player; DLL и импортированные native-ассеты сохраняются. Изменение C# или native-пакета требует перезапуска. Пока сцена пересоздаётся целиком, её игровой граф сбрасывается; статическое состояние процесса сохраняется при обновлении контента. При ошибке компиляции прежний процесс продолжает работать. Отдельные команды и ограничения описаны в [RAW_PROJECTS.md](RAW_PROJECTS.md), замеры — в [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md).

На полной Dungeon SampleScene проверены девять обновлений в одном Player на RTX 4060. Медианы от записи файла до подтверждённого кадра: Transform — 4.637 с, добавление GameObject — 4.935 с, raw ScriptableObject — 4.404 с. Первый тёплый CLI запуск занимает 23.988 с; процесс и тяжёлые ассеты затем сохраняются между этими правками. Это сокращает измеренную итерацию сцены примерно в пять раз относительно прежнего перезапуска за 23.032 с.

В этом ворктри уже подготовлен пустой Player и внешний контент для фиксированного корабля:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play run --interactive \
  --content "$PWD/Builds/CLIPlay/content/current.json" \
  --scene ShipGenBenchmark --fps 60 --no-vsync
```

Для нового набора сцен используйте процесс подготовки внешнего контента ниже. Для сцены, встроенной непосредственно в Player:

Из корня этого ворктри:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play doctor --json
$UNITY_MICRO_EDITOR_ROOT/unity-play modules

# Один раз подготовить Player для выбранных сцен.
# Проект должен быть закрыт в Editor. Library и Bee сохраняются.
$UNITY_MICRO_EDITOR_ROOT/unity-play build \
  --scene Assets/Dungeon/ShipGen/Scenes/ShipGenDemo.unity

# Дальше Editor не запускается.
$UNITY_MICRO_EDITOR_ROOT/unity-play run --headless --frames 120

# В desktop session можно открыть обычное окно игры.
$UNITY_MICRO_EDITOR_ROOT/unity-play run --interactive
```

`build --smoke` собирает пустую временную сцену для проверки запуска движка. Builder создаёт её только при отсутствии и удаляет после сборки. Это проверяет runtime и CLI bridge; генератор корабля проверяется отдельной сборкой `ShipGenDemo`.

По умолчанию Player сохраняется в `Builds/CLIPlay/UnityPlay.x86_64`, build manifest в `Builds/CLIPlay/build.json`. Для отдельного результата задайте `--player` командам `build`, `run` и `refresh-code`.

CLI Player по умолчанию собирается только для OpenGL Core. Это сокращает первую компиляцию шейдеров. Чтобы использовать все графические API проекта, задайте `build --graphics-api project`. Builder восстанавливает исходные настройки API, backend и runInBackground после сборки; подготовленный Player сохраняет выбранную конфигурацию.

Observer отмечает первый завершённый кадр выбранной загруженной сцены, отдельно от последнего кадра и выхода процесса. Sidecar `<report>.ready.json` появляется сразу после готовности. Launcher измеряет время от создания процесса до этой отметки; report содержит GPU, фактический FPS, scene/content revision и ошибки. С интервалом около 100 мс снимаются RSS, PSS, private memory, CPU и NVML VRAM по PID Player. Статистика разделена на startup, исполнение кадров и shutdown. Launcher, Editor и их дочерние процессы не входят в память Player.

Отчёты и логи запуска лежат в `Temp/UnityPlay/runs/<id>/`. Лог сборки в `Logs/unity-cli-build.log`. Exit code равен нулю только если Player завершился успешно, отчёт существует и observer выполнил заданное число кадров без ошибок. Ранний выход игры, отсутствующий bridge и зависание не считаются успешной проверкой. Ошибки, которые произошли до запуска observer, могут остаться только в native Player log.

Параметры запуска:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play run --headless --frames 300 --timeout 60 \
  --report "$PWD/Temp/UnityPlay/check.json"
$UNITY_MICRO_EDITOR_ROOT/unity-play run --scene 0 --frames 120
$UNITY_MICRO_EDITOR_ROOT/unity-play run --player-arg=-force-glcore --frames 120
$UNITY_MICRO_EDITOR_ROOT/unity-play build --scene Assets/Scenes/SampleScene.unity --dry-run
```

`--scene` для `run` выбирает сцену, уже включённую в Player. Можно указать индекс, имя или asset path. Начальная сцена Player успевает выполнить свою инициализацию перед переключением; side effects её запуска сохраняются. Для точного теста собирайте нужную сцену первой.

`run --interactive` работает до закрытия игры. Observer сохраняет ready и итоговый отчёт, без выхода по бюджету кадров; поддерживаются `--content` и `--scene`. Для него нет автоматического timeout, если не задан `--timeout`. В headless интерактивном запуске остановка через Ctrl+C завершает собственный процесс Player.

## Обновление C# без Editor

После успешной сборки CLI сохраняет подходящие player response files и compiler paths. Для изменения тела существующего метода:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play refresh-code --assembly Dungeon.CLIPlay --dry-run
$UNITY_MICRO_EDITOR_ROOT/unity-play refresh-code --assembly Dungeon.CLIPlay
$UNITY_MICRO_EDITOR_ROOT/unity-play run --headless --frames 120
```

Player должен быть остановлен. Это перекомпиляция DLL с последующим перезапуском процесса, состояние предыдущей игры не сохраняется.

После следующей обычной сборки CLI синхронизирует существующие Player DLL с финальными Linux outputs Bee. Это учитывает postprocessing и stripping и возвращает Player к собранным исходникам, даже если инкрементальное копирование Unity сохранило DLL от предыдущего offline refresh. Подменяемые файлы получают backup. Сборки из Editor `Library/ScriptAssemblies` для этого не используются.

Конфигурация привязана к Unity installation, этому проекту и baseline Player. Editor `.rsp`, WebGL/IL2CPP и неподходящие outputs не используются. Raw compiler output, изменённый ILPostProcessor, нельзя выдавать за готовую Player assembly. `refresh-code` работает только для разрешённых baseline assemblies, которые проходят проверки сохранённой конфигурации.

Изменение public API, сериализованных полей, новых script types и runtime initialization attributes требует обычной сборки. Новые C# файлы, asmdef, packages, runtime initialization и native/Burst код требуют сборки Player. Изменённые сцены, материалы, текстуры, prefabs и ScriptableObject можно готовить через `refresh-assets`; импорт, новые/снятые native modules и stripped types всё ещё могут потребовать подготовки нового Player. Addressables — отдельный pipeline. Инструмент не доказывает совместимость произвольного изменения исходников со старой сериализацией. Начинайте с изменения тела метода в assembly без postprocessing.

## Другой Unity-проект

CLI использует Python 3.10+. Для сырого контента дополнительно нужен PyYAML; прежний путь использует стандартную библиотеку. Unity нужна в версии из `ProjectSettings/ProjectVersion.txt`, с Linux Standalone Mono support. Можно указать путь через глобальный `--unity` или переменную `UNITY`.

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play --project /path/to/project doctor --json
$UNITY_MICRO_EDITOR_ROOT/unity-play --project /path/to/project install
$UNITY_MICRO_EDITOR_ROOT/unity-play --project /path/to/project build --scene Assets/Main.unity
$UNITY_MICRO_EDITOR_ROOT/unity-play --project /path/to/project run --headless
```

`install` копирует bridge в `Assets/Dungeon/CLIPlay`. При отличающемся существующем файле он отказывается перезаписывать его до любых записей. Bridge не зависит от VContainer, URP или Addressables и включается только при CLI argument `--unity-cli-report`.

Builder собирает выбранные сцены напрямую и не вызывает генераторы Dungeon. Собственные build processors проекта продолжают выполняться. Если игра требует подготовку Addressables или другого контента, её нужно выполнить отдельно штатной командой проекта. Offline compiler пока использует layout Bee из проверенной Unity 6; поддержка других версий требует отдельной проверки.

## Проверки

Гибридная SampleScene проверена на RTX 4060: 120 кадров, 0 ошибок и 0 исключений. Финальный Python suite прошёл 317 тестов; native HybridProbe проверил тяжёлые ассеты, lifecycle и клонирование префабов. Все семь замеров полной сцены успешны; результаты и условия измерений находятся в [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md).

```bash
python3 -m unittest discover -s $UNITY_MICRO_EDITOR_ROOT/tests -v
```

Unity EditMode tests находятся в `$UNITY_MICRO_EDITOR_ROOT/Packages/com.unitymicroeditor.runtime/Tests/EditMode`, filter `Dungeon.CLIPlay.Tests`. Они проверяют аргументы и вставку callback в дерево PlayerLoop. Проверка живого runtime выполняется `build` и `run`, C# unit tests её не заменяют.

## Первые проверки, 2026-10-04

Актуальные повторные замеры с RTX 4060 и внешними ассетами приведены в [BENCHMARKS.md](BENCHMARKS.md). Ниже сохранены ранние одиночные прогоны для истории эксперимента.

Unity `6000.6.3f1`, Linux Mono, OpenGL Core, 2026-10-04. Все `run` и `refresh-code` ниже выполнены с закрытым Editor этого проекта.

| Проверка | Время | Пик RSS Player | Результат |
| --- | ---: | ---: | --- |
| Пустая сцена, 120 кадров, headless | 1.804 с | 209.7 MiB | Без ошибок и исключений |
| Перекомпиляция `Dungeon.CLIPlay` | 2.844 с | Не измерялся | Player увидел `observerVersion=offline-v1` |
| Ошибка синтаксиса при перекомпиляции | 1.214 с | Не измерялся | Отказ; SHA DLL и compiler manifest не изменились |
| `ShipGenDemo`, 120 кадров, headless | 6.595 с | 348.6 MiB | Сцена загрузилась, без ошибок и исключений |
| `ShipGenDemo`, 120 кадров, OpenGL | 33.369 с | 888.9 MiB | Без ошибок и исключений, renderer llvmpipe |

Графический запуск выполнен под Xvfb с Mesa llvmpipe и разрешением 960×540. Это software rendering, его скорость и память не описывают запуск на обычной видеокарте. Время Player включает создание и завершение процесса. Headless запускает игровую логику и PlayerLoop с отключённым graphics device; он не проверяет изображение.

Полная подготовка `ShipGenDemo` после первого заполнения shader cache заняла 66.342 с. Сам `BuildPipeline` сообщил 26.056 с, остальное приходится на старт Editor и импорт. Первый эксперимент с OpenGL и Vulkan потребовал долгой компиляции новых шейдеров; Vulkan-сборка была остановлена, её кэш сохранён. Подготовка Player по умолчанию теперь использует только OpenGL Core.

Offline proof меняет только строковый initializer JSON-отчёта. Это доказывает загрузку новой DLL после перезапуска, но не совместимость любых игровых изменений. После отрицательного теста исходник восстановлен побайтово и Player снова подтвердил `baseline`.

Подробные результаты этой рабочей копии сохраняются в игнорируемой `Logs/UnityPlay/`: `offline-proof/summary.json`, копии JSON runtime и Player logs для обоих режимов `ShipGenDemo`. Unit test log находится в `Logs/unity-cli-python-tests.log`, Unity test report в `Logs/UnityPlay/editmode.xml`.

Прошли 9 EditMode test cases и Python suite с проверками CLI, compiler replay и остановки дочерних процессов. Все отчёты хранятся в игнорируемых папках.

## Внешние сцены и обновление ассетов

Для переиспользуемого Player сначала готовится контент и manifest типов, затем пустой shell. Встроенная и внешняя сцены должны иметь разные пути, чтобы Unity не выбрала встроенную копию. Сцены и зависимости остаются обычными сохранёнными ассетами проекта.

```bash
# Закрыть Editor именно этого проекта. Кэш Library сохраняется.
$UNITY_MICRO_EDITOR_ROOT/unity-play refresh-assets --prepare --scene Assets/Main.unity
# Взять typePreservationManifest из JSON команды выше.
$UNITY_MICRO_EDITOR_ROOT/unity-play build --smoke --asset-bundle-manifest /absolute/cache/root.manifest
$UNITY_MICRO_EDITOR_ROOT/unity-play refresh-assets --scene Assets/Main.unity
$UNITY_MICRO_EDITOR_ROOT/unity-play run --interactive --content "$PWD/Builds/CLIPlay/content/current.json"

# После сохранённой правки ассетов — экспорт и перезапуск того же Player.
$UNITY_MICRO_EDITOR_ROOT/unity-play refresh-assets --scene Assets/Main.unity
$UNITY_MICRO_EDITOR_ROOT/unity-play run --content "$PWD/Builds/CLIPlay/content/current.json" --frames 120
```

Экспорт использует стабильный `Builds/CLIPlay/.asset-cache/<scene-set>/` и штатный incremental BuildAssetBundles. Успешный snapshot публикуется в immutable `content/revisions/<hash>/`; `current.json` заменяется атомарно после проверки SHA, CRC, платформы, Unity version, исходников, настроек компиляции и хешей известных Player DLL. Для списка необходимых DLL используются assemblies настоящего Player compilation graph, поэтому зависимости shader importers от Editor не требуют редакторские DLL в игре. Неудачный экспорт сохраняет предыдущий current. Пакетные виртуальные пути разрешаются через публичный PackageInfo.resolvedPath. Проверенные изменения cooked Assets обновляют compiler asset checks, поэтому после них можно вновь использовать `refresh-code`; изменённые meta/asmdef/settings и неконвертированные ассеты продолжают блокировать compiler replay.

Подхват требует **перезапуска сцены в новом процессе**; живой reload с сохранением состояния не реализован. AssetBundle не переносит C# код. Новые serialized types/поля, удалённые linker-ом типы/модули, shader variants и Addressables нужно проверять отдельно. `--asset-bundle-manifest` передаёт Unity перечень типов внешнего контента для сохранения в shell. Подробнее: [BuildAssetBundles](https://docs.unity3d.com/6000.6/Documentation/ScriptReference/BuildPipeline.BuildAssetBundles.html), [зависимости](https://docs.unity3d.com/6000.6/Documentation/Manual/AssetBundles-Dependencies.html), [stripping](https://docs.unity3d.com/6000.6/Documentation/Manual/ManagedCodeStripping.html).

## GPU и замеры

Графический `run` использует существующий ускоренный DISPLAY, OpenGL Core и обычное окно. Перед запуском `glxinfo -B` проверяет renderer; llvmpipe/softpipe/software отвергаются, кроме явного `--allow-software`. Выбранный GPU повторно проверяется по SystemInfo в самом Player. Xvfb не создаётся. Для headless не создаётся graphics device. NVML сообщает память процесса, отдельно от рабочего стола и других приложений; если измерение недоступно, report указывает это вместо выдуманного нуля.

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play run --content "$PWD/Builds/CLIPlay/content/current.json" \
  --scene Main --frames 600 --fps 60 --no-vsync --width 960 --height 540
python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py startup --content /absolute/current.json --scene Main
# Dungeon fixtures: фиксированный корабль и проверка реальных serialized asset values.
python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py code --content /absolute/current.json --frames 120
python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py assets --runs 3 --frames 120
```

`benchmark.py code/assets` временно меняет только предназначенные для эксперимента данные, сохраняет сырые результаты в Logs и восстанавливает исходные байты в finally. Code benchmark проверяет исполнившуюся метку в ShipBootstrap.Start; asset benchmark чередует текстуру, материал и background сцены, меняет prefab/SO, проверяет значения в самом Player и неизменность всех Player binaries. Подробные актуальные цифры: [BENCHMARKS.md](BENCHMARKS.md).
