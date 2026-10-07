# Замеры Unity CLI Player

На этой машине сохранённая сцена генератора корабля запускается через подготовленный Player за **8,315 с с RTX 4060** или **4,526 с без графики**. Изменение тела метода игровой assembly, перекомпиляция и повторный запуск до первого кадра занимают **12,664 с**. Это медианы пяти запусков, после одного исключённого прогрева.

Запуск и поддерживаемый `refresh-code` работают без Editor. `refresh-assets` использует Editor для импорта и экспорта AssetBundle, после чего прежний Player загружает новую ревизию при перезапуске. Состояние прошлого запуска не сохраняется.

## Условия

Замеры выполнены 2026-10-05 в `/mnt/data/projects/Dungeon-wt/unity-cli-play`, Unity `6000.6.3f1`, Linux Development Player, Mono.

- CPU AMD Ryzen 7 5700X3D, 16 логических процессоров.
- GPU NVIDIA GeForce RTX 4060, драйвер `580.178.04`.
- Графический запуск через существующий ускоренный `DISPLAY=:0`, OpenGL Core, окно 960×540, целевые 60 FPS, VSync отключён. Xvfb не используется.
- Сцена [ShipGenBenchmark.unity](../../Assets/Dungeon/CLIPlay/Benchmarks/ShipGenBenchmark.unity) с `shipSeed=4242`, `scenarioSeed=1717`, `archetype=0`. Сцена загружалась из одной сохранённой ревизии AssetBundle.
- В startup/resource/code сериях один прогрев исключён, затем пять измеренных запусков; в asset-серии три измеренные итерации. Startup/resource-серии исполняют по 600 кадров, code-серия по 120 кадров.
- Кэши диска, импорта и Bee уже прогреты. Холодный старт машины или первый импорт проекта здесь не измерен.
- В другом ворктри был открыт Unity Editor. Его память не включена в показатели Player, но конкуренция за CPU и GPU могла влиять на время. Сравнения с Editor Play Mode нет.

Характеристики машины сохранены в [benchmark-hardware.json](../../Logs/UnityPlay/benchmark-hardware.json). Старые одиночные запуски через llvmpipe исключены из этих серий и с ними не сравниваются.

## Запуск сцены

Готовность означает первый реально завершённый кадр выбранной активной загруженной сцены. Observer записывает `<report>.ready.json`, launcher считает время от создания процесса до этой отметки. Это не время завершения процесса и не гарантия завершения всех возможных асинхронных действий игрового кода.

| Метрика | RTX 4060, медиана | Минимум / максимум | Headless, медиана | Минимум / максимум |
| --- | ---: | ---: | ---: | ---: |
| Создание процесса → первый кадр сцены | 8,315 с | 8,174 / 8,324 с | 4,526 с | 4,020 / 4,883 с |
| Процесс целиком, включая 600 кадров и выход | 18,888 с | 18,728 / 18,898 с | 14,883 с | 14,304 / 15,181 с |
| CLI целиком | 19,125 с | 18,958 / 19,139 с | 14,941 с | 14,364 / 15,253 с |
| Фактические FPS после готовности | 59,76 | 59,64 / 59,88 | 60,00 | 59,98 / 60,03 |

Полное время процесса содержит примерно десять секунд заданного исполнения кадров. Для оценки скорости старта нужна первая строка таблицы.

Сводки: [GPU](../../Logs/UnityPlay/benchmarks/gpu/summary.json), [headless](../../Logs/UnityPlay/benchmarks/headless/summary.json). Пример первого измеренного GPU-запуска: [ready](../../Logs/UnityPlay/benchmarks/gpu/run-01/runtime.json.ready.json), [runtime](../../Logs/UnityPlay/benchmarks/gpu/run-01/runtime.json), [measurement](../../Logs/UnityPlay/benchmarks/gpu/run-01/measurement.json), [Player log](../../Logs/UnityPlay/benchmarks/gpu/run-01/player.log). Соответствующий headless-запуск: [runtime](../../Logs/UnityPlay/benchmarks/headless/run-01/runtime.json), [measurement](../../Logs/UnityPlay/benchmarks/headless/run-01/measurement.json).

## Ресурсы одного инстанса

С интервалом около 100 мс измеряется один PID Player. Launcher, Editor и остальные приложения не суммируются с ним. `Steady` охватывает работу после первого кадра до окончания бюджета кадров; сохранение PNG и shutdown исключены. Каждая ячейка со средним значением ниже содержит медиану пяти средних по отдельным запускам.

| Метрика | RTX 4060, медиана | Минимум / максимум | Headless, медиана | Минимум / максимум |
| --- | ---: | ---: | ---: | ---: |
| Средняя RAM, RSS | 658,08 MiB | 656,79 / 661,08 | 346,25 MiB | 345,21 / 351,78 |
| Средняя RAM, PSS | 651,65 MiB | 650,33 / 654,67 | 341,06 MiB | 340,04 / 346,55 |
| Средняя private RAM | 650,33 MiB | 649,02 / 653,37 | 340,45 MiB | 339,44 / 345,94 |
| Пиковая RSS за весь процесс | 664,3 MiB | 661,7 / 665,5 | 353,5 MiB | 351,1 / 359,2 |
| Средняя CPU | 202,03% | 201,20 / 203,09% | 14,78% | 14,32 / 17,64% |
| Средняя VRAM процесса | 142,06 MiB | 141,45 / 142,52 | Не измерялась | Недоступно |
| Пиковая steady VRAM процесса | 167,19 MiB | 163,19 / 167,19 | Не измерялась | Недоступно |

В CPU `100%` означает одно полностью занятое логическое ядро. Графический инстанс в этом опыте в среднем использует около 2,02 ядра, headless около 0,15 ядра. Это не процент от всей машины.

RSS включает резидентные общие страницы, PSS делит их между процессами, private RAM учитывает частные страницы процесса. Они описывают разные стороны расхода RAM и не складываются друг с другом. VRAM получена через NVML по PID Player, а не из `SystemInfo.graphicsMemorySize`, который сообщает ёмкость видеокарты. Рабочий стол и открытый Editor в цифры VRAM Player не входят. Headless запускается без graphics device, поле измерения VRAM остаётся недоступным, а не подменяется нулём.

Это расход одной конкретной сцены при 60 FPS и разрешении 960×540. Масштабирование на несколько инстансов, другие разрешения и другие сцены не измерено.

## Итерация C#

В каждом запуске менялось тело существующего `ShipBootstrap.Start` в `Dungeon.ShipGen`, добавлялась уникальная строка лога. `refresh-code` проверял baseline, вызывал сохранённый Roslyn compiler, заменял DLL остановленного Player; затем выполнялся новый графический запуск. Метка проверялась в самом Player log, поэтому измерение подтверждает исполнение изменённого игрового кода. Исходные байты и рабочая DLL восстановлены после серии.

| Метрика | Медиана, n=5 | Минимум / максимум |
| --- | ---: | ---: |
| `refresh-code` целиком | 4,492 с | 3,794 / 4,648 с |
| Новый Player → первый кадр | 8,100 с | 7,191 / 8,422 с |
| Правка файла → первый кадр нового запуска | 12,664 с | 11,152 / 13,043 с |
| Правка файла → выход после 120 кадров | 15,236 с | 13,621 / 15,560 с |

Здесь нет live reload в уже работающий процесс. При перезапуске повторяются загрузка сцены и генерация корабля. Поддерживаются подходящие assembly без требуемого Unity postprocessing и совместимые изменения тела метода. Новые типы, public API, сериализованные поля, runtime initialization metadata, asmdef, native/Burst-код и изменения stripping требуют обычной сборки. Инструмент не доказывает автоматически, что произвольная правка сохраняет эти контракты.

Сводка: [code/summary.json](../../Logs/UnityPlay/benchmarks/code/summary.json). Первая измеренная итерация: [compiler measurement](../../Logs/UnityPlay/benchmarks/code/run-01/compile/measurement.json), [runtime](../../Logs/UnityPlay/benchmarks/code/run-01/runtime.json), [Player log](../../Logs/UnityPlay/benchmarks/code/run-01/player.log). Восстановление: [restore measurement](../../Logs/UnityPlay/benchmarks/code/restore/measurement.json).

## Итерация ассетов

На отдельной небольшой [AssetIterationBenchmark.unity](../../Assets/Dungeon/CLIPlay/Benchmarks/AssetIterationBenchmark.unity) выполнен один исключённый прогрев и три измеренные итерации. Каждый запуск менял сохранённую сцену, материал, PNG-текстуру, prefab и ScriptableObject. Цвета текстуры/материала и фон камеры чередовались; масштаб prefab и revision SO менялись каждый раз. Player подтвердил все пять новых значений по реально загруженным объектам. Хеши executable, UnityPlayer.so и всех файлов Player_Data остались неизменными во всех измеренных итерациях. Исходные байты ассетов восстановлены, исходный контент снова экспортирован.

| Метрика | Медиана, n=3 | Минимум / максимум |
| --- | ---: | ---: |
| `refresh-assets` целиком, включая старт Editor и публикацию | 38,271 с | 37,815 / 38,432 с |
| Внутренний Editor exporter | 15,110 с | 14,944 / 15,177 с |
| Новый Player → первый кадр небольшой asset-сцены | 4,659 с | 4,650 / 4,710 с |
| Вызов команды экспорта → первый кадр изменённой сцены | **43,116 с** | 42,665 / 43,330 с |
| Правка внутри уже запущенного Editor → первый кадр | 31,819 с | 31,797 / 31,948 с |

Для скорости полного CLI-цикла нужна строка **43,116 с**: она включает запуск Editor, импорт/экспорт, проверки, публикацию и перезапуск Player. Интервал 31,819 с начинается при правке тестовых ассетов уже внутри Editor и исключает его начальный запуск. `editorExportSeconds` включает выполнение всей C# команды, включая правку, сохранение ассетов, сборку bundles и создание manifest; это не чистое время `BuildAssetBundles`.

Эта серия проверяет малую asset-сцену, поэтому её запуск 4,659 с нельзя подменять временем загрузки корабля 8,315 с. Для тяжёлых моделей, больших текстур, новых шейдеров и другого набора зависимостей импорт/экспорт нужно измерять отдельно. Ассеты обновляются через штатный Unity Editor pipeline; обход импорта и live reload с сохранением состояния не реализованы.

Сводка: [assets-verified/summary.json](../../Logs/UnityPlay/benchmarks/assets-verified/summary.json). Первая измеренная итерация: [export](../../Logs/UnityPlay/benchmarks/assets-verified/run-01/export/measurement.json), [Editor log](../../Logs/UnityPlay/benchmarks/assets-verified/run-01/export/editor.log), [runtime](../../Logs/UnityPlay/benchmarks/assets-verified/run-01/runtime.json), [Player log с asset-probe](../../Logs/UnityPlay/benchmarks/assets-verified/run-01/player.log), [кадр](../../Logs/UnityPlay/benchmarks/assets-verified/run-01/frame.png). [Хеши Player до серии](../../Logs/UnityPlay/benchmarks/assets-verified/player-hashes-before.json), [экспорт восстановленного контента](../../Logs/UnityPlay/benchmarks/assets-verified/restore/measurement.json).

До этой успешной серии сохранены диагностические неуспешные попытки в `benchmarks/assets`; они не включены в распределения. Ошибка определения runtime assemblies исправлена: Editor scripts из shader/importer dependencies исключаются по публичному Player compilation graph.

## Разовая подготовка

Перед повторными измерениями один раз собран пустой Player shell с сохранением типов для внешнего контента. Эти два отдельных образца не являются распределениями и не характеризуют первый запуск на полностью холодной машине.

| Этап | Наблюдение |
| --- | ---: |
| Процесс начальной сборки shell | 71,656 с |
| Внутри `BuildPipeline` | 43,255 с |
| Пик RSS Editor при сборке shell | 1552,2 MiB |
| Размер результата shell | 218 121 606 байт |
| Начальная подготовка AssetBundle, весь процесс Editor | 50,477 с |
| Внутренний `editorExportSeconds` этой подготовки | 28,055 с |

`editorExportSeconds` включает работу Editor exporter; это не отдельный таймер только `BuildAssetBundles`. Результаты: [shell-build-summary.json](../../Logs/UnityPlay/shell-build-summary.json), [content-prepare-summary.json](../../Logs/UnityPlay/content-prepare-summary.json).

В предыдущем диагностическом запуске ожидание UPM заняло 136,634 с при проблеме соединения с `packages.unity.com`. Попытка отключить UPM через `-noUpm` сломала package graph и компиляцию зависимостей. Это наблюдение не включено в startup/code-серии; экспорт ассетов по-прежнему может зависеть от запуска Editor, состояния импорта и сети. В финальной проверке EditMode весь процесс Editor также занял 160,810 с, из которых 138,048 с пришлись на `ProcessInitializeOnLoadMethodAttributes`; сами 17 тестов прошли за 0,066 с. Эта задержка не меняет опубликованные серии. Диагностика сохранена в [hang-observation.json](../../Logs/UnityPlay/debug-tools/hang-observation.json) и [editor-offline-diagnostic.log](../../Logs/UnityPlay/editor-offline-diagnostic.log).

## Как повторить

Команды ниже используют уже подготовленные Player и immutable content manifest исходных серий. Указанная ревизия должна существовать локально. Если её нет, сначала выполните подготовку shell и экспорт контента по [README.md](../../README.md). Целевые каталоги `reproduce-*` должны отсутствовать; скрипт сохраняет каждый command, ready/runtime report, resource measurement и Player log.

```bash
cd /mnt/data/projects/Dungeon-wt/unity-cli-play

UNITYPLAY_CONTENT="$PWD/Builds/CLIPlay/content/revisions/08e7e1c428961e3d2d0b677519a5e04162d0145c75ef1e71d179e5eb0d53a6a9/manifest.json"

DISPLAY=:0 python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py startup \
  --content "$UNITYPLAY_CONTENT" --runs 5 --warmup 1 --frames 600 \
  --fps 60 --width 960 --height 540 \
  --output Logs/UnityPlay/benchmarks/reproduce-gpu

python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py startup \
  --content "$UNITYPLAY_CONTENT" --headless --runs 5 --warmup 1 \
  --frames 600 --fps 60 \
  --output Logs/UnityPlay/benchmarks/reproduce-headless

DISPLAY=:0 python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py code \
  --content "$UNITYPLAY_CONTENT" --runs 5 --warmup 1 --frames 120 \
  --fps 60 --width 960 --height 540 \
  --output Logs/UnityPlay/benchmarks/reproduce-code

DISPLAY=:0 python3 $UNITY_MICRO_EDITOR_ROOT/benchmark.py assets \
  --runs 3 --warmup 1 --frames 120 --fps 60 --width 960 --height 540 \
  --output Logs/UnityPlay/benchmarks/reproduce-assets
```

`benchmark.py` всегда отключает VSync. `code` временно правит игровой C# файл и восстанавливает его в `finally`; Player должен быть остановлен. `assets` временно правит только специально созданные benchmark assets и также восстанавливает исходные байты. Для экспорта проект этого ворктри должен быть закрыт в Editor.

## Проверки результата

- Python suite: **70 тестов прошли** — [лог](../../Logs/UnityPlay/final-python-tests.log).
- Unity EditMode: **17 test cases прошли**, failed/skipped=0 — [XML](../../Logs/UnityPlay/final-editmode/results.xml), [Editor log](../../Logs/UnityPlay/final-editmode/editor.log).
- После восстановления ассетов опубликован общий `content/current.json` с кораблём и asset-сценой — [export](../../Logs/UnityPlay/final-content-summary.json).
- Повторный GPU-запуск восстановленного корабля: 120 кадров, 0 ошибок/исключений — [runtime](../../Logs/UnityPlay/final-gameplay/runtime.json), [кадр](../../Logs/UnityPlay/final-gameplay/frame.png).
- Исходный игровой код и ProjectSettings не изменены. Замеры и скриншоты находятся в игнорируемой `Logs/UnityPlay`, инструменты и benchmark assets остаются в ворктри для ревью.
