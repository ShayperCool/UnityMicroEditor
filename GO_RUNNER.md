# Dungeon через Go runner

`unity-play-go` запускает настоящую Dungeon SampleScene и следит за исходниками. Go готовит сценовый граф, кэш и запуск Player; C# собирает SDK Roslyn. Во время запуска и проверенных итераций Python и основной Unity Editor не вызываются. Рендеринг и физику выполняет Unity Mono Player.

Проверено на Linux с Unity `6000.6.3f1`, RTX 4060 и OpenGL Core. По умолчанию runner запрашивает OpenGL Core, окно 960×540, ограничение 60 FPS и выключенный VSync. Ready check отклоняет отсутствующий/Null renderer и известные software renderer, включая llvmpipe, softpipe, lavapipe и SwiftShader. Это проверка известных имён, а не универсальное доказательство аппаратного исполнения для любого vendor. В проверенных ready reports устройство указано как `NVIDIA GeForce RTX 4060/PCIe/SSE2`.

## Запуск

Из корня этого ворктри:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play-go run --project . \
  --output Temp/UnityPlay/go-project/native-live --watch
```

`--watch` держит Player открытым и обновляет его после изменений в проекте. Ctrl+C останавливает runner и его Player. Для конечной проверки:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play-go run --project . \
  --output Temp/UnityPlay/go-project/check --frames 120
```

Wrapper собирает статический Go executable через стандартный build cache и запускает его напрямую. Команду можно повторять с тем же output для переиспользования compiler/native caches. Output должен находиться внутри `Temp` проекта. `--width`, `--height` и `--fps` меняют параметры окна; `--interactive` оставляет окно открытым без watcher.

Один output принадлежит одной запущенной сессии. Повторный запуск с занятым output отклоняется до старта ещё одного Player; после остановки или аварийного выхода runner блокировка освобождается автоматически.

## Что уже должно быть подготовлено

Runner использует существующий `Temp/UnityPlay/hybrid-sample/current.json`. `--baseline /absolute/current.json` выбирает другую проверенную подготовку. Baseline должен принадлежать этому же проекту и содержать пути к raw manifest, native catalog и готовому универсальному Mono host. Нужны также сохранённый compiler plan, assembly configurations и соответствующая установка Unity SDK.

Запуск произвольного чистого проекта без baseline пока не готов. Историческая подготовка host и исходного тяжёлого контента описана в [RAW_PROJECTS.md](RAW_PROJECTS.md). Новая команда использует её результат; она не превращает любую папку с `Assets` в готовый Player.

Неизменённые модели, анимации, UI, URP и остальные тяжёлые зависимости SampleScene продолжают загружаться из существующего native bundle. Он сохраняется при проверенных обновлениях WAV и сцены. Исходные `.unity` и авторские `.prefab` читает наш loader, они не сериализуются в AssetBundle.

## Как применяются изменения

| Изменение | Что происходит |
| --- | --- |
| Имя, active, layer, tag GameObject; положение, вращение, масштаб Transform | Runner публикует небольшой scene patch. Применяются изменённые исходные поля; остальные runtime значения сохраняются |
| Новый пустой GameObject с Transform, дочерний или корневой | Patch добавляет объект и обновляет hierarchy/SceneRoots в том же Player |
| Другие поддержанные изменения сцен, префабов и raw-данных | Loader пересоздаёт сцену в том же Player. Игровое состояние сцены сбрасывается; загруженные сборки и native cache остаются |
| Поддержанный WAV/FBX с существующим GUID | Штатный AssetImportWorker импортирует файл в собственном контексте. Go адаптирует native SerializedFile и загружает его в тот же Player через ContentLoadInterface. Затем сцена пересоздаётся |
| Тело C# метода | SDK перекомпилирует изменённую сборку, затем runner перезапускает Player. Состояние процесса и сцены сбрасывается |
| Изменение C# API | Дополнительно могут пересобраться зависимые сборки; Player также перезапускается |

Go хранит подготовленные JSON revisions и DLL отдельно от исходников. Watcher начинается до подготовки Player, поэтому изменения во время старта попадают в проверку. Компилятор сверяет SDK, source hashes и конфигурацию сборок. Native replacement сверяет source/meta hashes, GUID, точные local IDs и типы. Несогласованная версия или неподдержанное изменение получает `update-rejected`; прежняя подтверждённая версия остаётся рабочей, если runtime ack не требует перезапуска.

Native overlay поддерживает существующие GUID из catalog. Для WAV проверены PCM/DecompressOnLoad и preloaded Vorbis/CompressedInMemory с синхронной загрузкой. Streaming, background load, новые native GUID и неподдержанные import profiles отклоняются. FBX допускается при неизменных object IDs, hierarchy, component/reference order и authored scalar values. Геометрия Mesh может меняться. Непроверенные skinned/Animator layouts и внешние зависимости, которым нет проверенного ContentFile descriptor, не принимаются как замена модели.

Настройки проекта, `.asmdef`/package configuration, GUID/type changes и несовместимые изменения графа требуют отдельной поддержки или новой подготовки baseline. Runner проверяет эти ограничения до публикации. Полная совместимость всех Unity-проектов и полный Editor Play Mode не заявлены.

## Замеры итерации

Замеры сделаны на этой машине и SampleScene 2026-10-06. Время до ACK включает watcher, подготовку, публикацию и первый подтверждённый кадр. Время компиляции отдельно от полной итерации.

| Операция | Наблюдавшееся время |
| --- | --- |
| Тёплый запуск до готового Player | Около 7-11 с; первый старт после обновления managed bridge включает его SDK compile |
| Небольшая правка сцены или пустой GameObject до ACK | Около 0.6-1.3 с |
| Тёплая подготовка обычной revision | Около 0.2-0.35 с; первая затронутая правка может стоить больше |
| Свежий импорт реального `door_crank_loop.wav` штатным worker | Около 3.1 с |
| Этот WAV от записи файла до ACK в SampleScene | Около 6.0 с |
| Возврат к уже импортированной версии WAV из кэша до ACK | Около 2.7 с |
| SDK compile после правки тела `AmbientSound` | 2.1-3.9 с |
| Та же правка кода до готовой сцены после перезапуска | 10.6-13.6 с |

Правка C# сейчас требует перезапуска, это основная оставшаяся задержка такой итерации. Scene patch сохраняет Player и его runtime state; более широкий scene reload и native update пока пересоздают игровой граф.

Финальный повтор на той же версии CLI: scene rename/restore — 1.04/1.06 с до ACK при preparation 0.31 с; применение в Unity — 13/7 мс. Body-only edit/restore с проверкой свежего IL и сохранением исходных байтов — SDK compile 2.45/2.44 с, полный перезапуск 11.81/10.96 с. В обоих случаях пересобрана только `Dungeon.Runtime`, runtime errors/exceptions — 0.

Один запущенный Player с этой SampleScene занимал около 815-875 МиБ RSS. Go runner с кэшами проекта занимал около 260-365 МиБ RSS. Wrapper по умолчанию задаёт `GOMEMLIMIT=384MiB`; это мягкий лимит памяти Go runtime, а не предел RSS всех процессов. Во время свежего native импорта worker дополнительно занимал около 405 МиБ. Процессы SDK компиляции также учитываются отдельно.

Снимки `nvidia-smi pmon -c 1 -s m` показали для отдельных Player `fb=85 MB` и `fb=263 MB`; второй замер сделан после итоговых обновлений кода и сцены. Это framebuffer memory по учёту драйвера для процесса; снимки не описывают все общие GPU allocations. Общее использование видеокарты в прежних снимках было около 549 и 716 МиБ вместе с рабочим столом.

## Проверки и отчёты

Runner печатает JSON events `ready`, `updated`, `code-updated`, `native-imported`, `native-cache-hit` и `update-rejected`. В `output/runs/<id>/` сохраняются Player log, ready/final reports и reload request/ack. В ready report записаны фактический GPU и PID. ACK содержит revision, native load count и ошибки; успешный ACK относится к завершённому кадру.

Проверенные результаты этого ворктри находятся в `Temp`:

- `Temp/UnityPlay/go-project-final.jsonl` содержит итерации кода, cache WAV и сцены.
- `Temp/UnityPlay/go-project-release.jsonl`, `scene-watch-release-result.jsonl` и `code-benchmark-release/result.json` содержат итоговый запуск и повтор замеров.
- `Temp/UnityPlay/go-project-running.jsonl` содержит последующий тёплый запуск за 10.27 с; scene preparation — 0.89 с.
- `Temp/UnityPlay/go-project-release/rejection-proof.json` подтверждает, что invalid C# и следующая scene edit отклоняются без остановки прежнего Player; восстановление исходников затем применяет сцену с тем же PID.
- `Temp/UnityPlay/go-project-native-live-fixed.jsonl` содержит свежий native WAV import и успешные runtime ACK без errors/exceptions.
- `Temp/UnityPlay/native-content-production-proof/proof.json` проверяет native Mesh и PCM против Editor fingerprint.
- `Temp/UnityPlay/native-content-production-proof/real-audio/proof.json` проверяет настоящий Vorbis clip и `AudioSource.isPlaying`.
- `Temp/UnityPlay/fast-patch-production-proof/proof.json` проверяет сохранение EntityId, runtime position, layer, tag и active state после rename, child addition и root addition.

Прямой worker import и границы адаптации описаны в [WorkerProbe/README.md](WorkerProbe/README.md) и [RuntimeDeliveryProbe/README.md](WorkerProbe/RuntimeDeliveryProbe/README.md). Команда `$UNITY_MICRO_EDITOR_ROOT/unity-play` остаётся отдельным прежним Python CLI; её инструкции и старые замеры не описывают скорость нового Go runner.
