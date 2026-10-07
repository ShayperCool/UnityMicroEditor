# Сырой Unity проект через универсальный Player

Новый [Go runner для Dungeon](../../GO_RUNNER.md) использует `$UNITY_MICRO_EDITOR_ROOT/unity-micro-editor`: scene patch, SDK compile и прямую доставку поддержанных native worker artifacts. Он требует существующий baseline, native catalog/compiler configuration и готовый Mono host. Ниже описан прежний Python CLI `$UNITY_MICRO_EDITOR_ROOT/unity-play`, включая его минимальный режим и подготовку гибрида; его ограничения и замеры отличаются от нового runner.

Минимальный режим `raw-run` компилирует исходный C# и загружает сохранённые `.unity`, `.prefab`, `.mat`, `.asset` и PNG/JPEG собственным загрузчиком. Входному проекту не нужны `Library`, готовый Player или AssetBundle. Рендеринг, физика, MonoBehaviour и PlayerLoop исполняются настоящим Unity runtime.

Универсальный host собирается один раз в отдельном проекте инструмента. Эта подготовка пока требует Editor. Последующие сборки и запуски поддержанного сырого проекта Editor не вызывают. Ниже отдельно описаны минимальный режим и гибрид со штатным импортом тяжёлых ассетов.

## Гибридный режим для Dungeon

Для тяжёлого контента используется штатный Unity importer в отдельном проекте инструмента. Сцены и текстовые префабы по-прежнему читает наш loader; они не входят в native asset pack. Подробности кэша и границы совместимости: ImportedAssets/README.md.

```bash
# Подготовка Mono host с точными runtime-пакетами исходного проекта.
$UNITY_MICRO_EDITOR_ROOT/unity-play host-build --project-packages \
  --player Builds/CLIPlayHybridHost/UnityPlayHost.x86_64

# Импорт моделей, аудио, анимации, материалов, UI и URP-зависимостей.
$UNITY_MICRO_EDITOR_ROOT/unity-play import-assets --scene Assets/Scenes/SampleScene.unity

# Собственный scene loader; native importer вызывается только при промахе его кэша.
$UNITY_MICRO_EDITOR_ROOT/unity-play raw-run --scene Assets/Scenes/SampleScene.unity \
  --player Builds/CLIPlayHybridHost/UnityPlayHost.x86_64 \
  --native-import --watch --fps 60 --no-vsync --width 960 --height 540

# Восстановление завершённого импорта после прерывания CLI, без повторного Editor.
$UNITY_MICRO_EDITOR_ROOT/unity-play import-recover
```

`--native-catalog <catalog.json>` использует конкретный проверенный кэш и не запускает Editor. Для source DLL создаётся отдельная копия Player через reflink, DLL регистрируются в его `ScriptingAssemblies.json` до старта процесса. Общий host остаётся неизменным. Это необходимо для native десериализации custom ScriptableObject внутри импортированных ассетов; одной поздней managed загрузки DLL недостаточно.

Roslyn использует штатные Unity source generators, включая UI Toolkit. Кэш отдельных сборок переиспользует неизменённые DLL и пересобирает изменённую сборку с её зависимостями. Он проверяет DLL/PDB, исходники, SDK, генераторы и lifecycle proofs. Настройки `preloadedAssets`, исходные URP global settings и pipeline читаются из проекта. Сцена загружается в первом EarlyUpdate до первого рендера.

Global settings сначала регистрируются для типа pipeline, затем становятся текущими через публичный `GraphicsSettings.TrySetCurrentRenderPipelineGlobalSettings`. Это отдельные операции: регистрация типа сама по себе не обеспечивает доступ SSAO к ресурсам. Для builtin Default-Skybox host хранит точную копию материала установленной Unity; loader восстанавливает исходную ссылку и обновляет окружение через `DynamicGI.UpdateEnvironment`.

Авторские prefab templates неактивны под служебными контейнерами. Компилятор направляет обращения `Transform.root` и `GetComponentInChildren` через публичный bridge, который использует авторскую иерархию для этих шаблонов. Обычные объекты сцены и native `Instantiate` clones сохраняют штатные запросы Unity. Отдельный runtime probe проверяет циклические ссылки до Awake, корни, активные/неактивные дочерние компоненты и оба варианта `Instantiate`.

Изменение обычного игрового C# или Transform сцены не требует native импорта. Для кода сериализованных SO, входящих в тяжёлый кэш, пока действует консервативная проверка всего исходного файла: такое изменение требует повторного импорта. Изменения тяжёлого файла, его `.meta`, настроек импорта и структуры префаба, использованной в native compatibility observations, также обновляют кэш.

## Запуск

Проверенная конфигурация: Unity `6000.6.3f1`, Linux Standalone Mono, Python 3.10+ и PyYAML. Для C# используется Roslyn и библиотеки из установленного Unity SDK. Host и исходный проект должны иметь одинаковую версию Unity.

Из корня ворктри:

```bash
# Одна подготовка host. Входные игровые ассеты в него не попадают.
$UNITY_MICRO_EDITOR_ROOT/unity-play host-build

# Проверка исходного контента до компиляции и запуска.
$UNITY_MICRO_EDITOR_ROOT/unity-play --project $UNITY_MICRO_EDITOR_ROOT/Samples/RawProject \
  raw-validate --scene Assets/Scenes/RawSample.unity

# Конечный запуск с аппаратным GPU и проверкой 120 кадров.
$UNITY_MICRO_EDITOR_ROOT/unity-play --project $UNITY_MICRO_EDITOR_ROOT/Samples/RawProject \
  raw-run --scene Assets/Scenes/RawSample.unity --frames 120 --fps 60 --no-vsync

# Окно игры и наблюдение за изменениями исходников.
$UNITY_MICRO_EDITOR_ROOT/unity-play --project $UNITY_MICRO_EDITOR_ROOT/Samples/RawProject \
  raw-run --scene Assets/Scenes/RawSample.unity --watch --fps 60 --no-vsync
```

`--interactive` держит окно открытым без наблюдения за исходниками. `--headless` отключает графику, но raw сцена с Standard materials пока не запускается с Null graphics device: shader недоступен. Проверенный путь здесь графический. Он проверяет renderer и отказывается использовать llvmpipe и другие известные программные renderer без явного `--allow-software`.

`--player` выбирает другой подготовленный host. Глобальный `--unity` или переменная `UNITY` указывают установку SDK. `raw-build` только готовит immutable revision; `raw-run` готовит её и запускает. `raw-validate --report Temp/raw-validation.json` сохраняет граф и диагностику. Эта проверка относится к исходному контенту; ошибки компиляции, совместимости CLR и часть ограничений lifecycle проверяются позднее.

## Обновления

Каждая подготовка создаёт отдельную revision с DLL, JSON графом и копиями исходных текстур в `Temp/UnityPlay/raw`. Host DLL и cooked data остаются неизменными. C# собирается по исходникам, `.asmdef` и `.asmref`, без response files Bee. Изменения полей, новых обычных классов и новых `.cs` могут компилироваться в новую revision.

Кэш компиляции учитывает исходники, настройки сборок, package sources, компилятор, Unity references и версию host. При изменении только сцены или ассетов готовая DLL берётся из кэша. В минимальном режиме JSON и текстуры обновляются из исходных файлов без Unity import и сборки bundles. В гибриде изменение тяжёлого ассета вызывает штатный импорт; изменение самой сцены его не требует.

`--watch` опрашивает исходники примерно каждые 300 мс. Новую revision он готовит, пока старая сцена работает. Первая подготовка полностью проверяет исходники, SDK, DLL и native-кэш. Сессия затем сохраняет индекс GUID, разобранный YAML, данные файлов и их SHA. Повторное использование требует совпадения device/inode/size/mtime/ctime; добавления, удаления, новые `.meta` и изменения симлинков также проверяются. В конце подготовки весь inventory проверяется повторно, чтобы не публиковать смесь версий.

Совместимая правка `.unity`, авторского `.prefab` или raw-ассета пропускает компилятор и повторную проверку native-каталога. Player сохраняет загруженные DLL, GPU и тяжёлые ассеты, создаёт новый raw-граф, пересоздаёт root scope и выгружает старую сцену. Подтверждение приходит после первого кадра и завершения выгрузки. `RuntimeInitializeOnLoadMethod` исполняется один раз на процесс. Сейчас это пересоздание всей сцены, поэтому её игровой граф сбрасывается; статическое состояние и изменяемое состояние сохранённых native-ассетов остаются в процессе.

Изменение кода, SDK, конфигурации или native-пакета вызывает полную подготовку и перезапуск Player. Тяжёлые ассеты требуют штатного импорта. Для ручного `--native-catalog` ссылка на отсутствующий native-ассет остаётся ошибкой. При ошибке подготовки или runtime-проверки до активации прежняя сцена сохраняется. Ошибка после начала активации и отсутствие подтверждения требуют перезапуска с подготовленной revision; автоматического возврата к прежним игровым данным нет.

По умолчанию watcher создаёт собственный файл запросов внутри raw output. `--reload-request /absolute/request.json` задаёт известный путь для автоматизации. Запрос содержит revision, абсолютный manifest path и его SHA; ack проверяется по request ID, revision и PID Player. В соседнем `.events.jsonl` сохраняются время подготовки и подтверждения обновлений.

Input `Assets` не изменяется. Перемены исходников во время подготовки приводят к отказу и повторной попытке, чтобы не публиковать смесь версий. Неудачная подготовка сохраняет прежний `current.json`. Исправление следующего исходного файла вызывает новую попытку watcher.

## Контент минимального режима

| Источник | Поддержка |
| --- | --- |
| Сцены и обычные текстовые префабы | Иерархия, active flags, слои, базовые теги, вложенные префабы, scalar/reference overrides |
| Transform и Camera | Положение, вращение, масштаб, perspective/orthographic параметры, clear color, culling mask |
| MeshFilter и MeshRenderer | Встроенные primitive meshes, материалы, базовые настройки renderer |
| Физика | Rigidbody, BoxCollider, SphereCollider, CapsuleCollider; ограничения настроек проверяются |
| Свет | Realtime directional, point и spot, базовые RenderSettings |
| Материалы | Проверенный путь Standard и ограниченный набор свойств; host также содержит Unlit shaders |
| Текстуры | PNG/JPEG, sRGB, mipmaps, filter/wrap, readable flag; несжатые RGB24/RGBA32 с учётом реально используемой alpha |
| MonoBehaviour | Обычные скомпилированные типы, public/private сериализованные поля, bool/enum, вложенные значения, массивы и ссылки |
| ScriptableObject | Прямой data-only наследник, сериализованные поля и ссылки, без Awake/OnEnable |
| C# packages | Embedded/local/builtin sources; registry/git sources нужно предоставить отдельно |

Скрипты загружаются через публичный `UnityEngine.Assemblies.CurrentAssemblies.LoadFromPath`, поля восстанавливаются через `JsonUtility.FromJsonOverwrite`. Native реализацию API исполняет подготовленный Mono Player. Пользовательские ассеты загрузчик создаёт через публичные Unity API.

Минимальный режим не импортирует FBX, skinning, AnimationClip/Animator, URP, произвольные ShaderGraph, UI/TMP и Addressables; для моделей, анимации, шейдеров и UI используется гибридный режим. Lightmap baking и полноценный перенос всех ProjectSettings не реализованы. Минимальный host использует Built-in pipeline. Для некоторых настроек renderer, текстур и света доступны только явно проверенные значения. Действующие texture compression, Crunch и явный platform format требуют собственного импортёра и вызывают отказ. Явный uncompressed Automatic profile поддерживается; отсутствующие настройки сжатия дают предупреждение о неопределённом формате.

Импорт prefab root учитывает имя файла, как проверено на настоящем Editor; после него применяются instance overrides. Порядок корней из `SceneRoots` и `m_RootOrder` сохраняется. Сцены без этих данных не входят в проверку точного порядка корней. Неединичные quaternion отвергаются: публичный Transform setter нормализует их, тогда как Editor может вернуть исходный serialized literal.

Вложенные Unity value types и массивы ссылок проверены. Произвольные пользовательские `[Serializable]` графы требуют отдельного сравнения. Нативный `JsonUtility` в проверенном Player пропустил вложенные DTO из динамически загруженной DLL; для отчётов observer использует собственный сериализатор и проверяет полноту графа.

Структурные prefab overrides, собственные prefab templates для `Instantiate`, `RequireComponent`, MonoImporter.executionOrder, virtual/override lifecycle и `RuntimeInitializeOnLoadMethod` поддерживаются собственным loader/compiler. Гибрид дополнительно включает ScriptableObject barrier и штатные source generators. Собственная компиляция IL postprocessing/Burst остаётся ограничением; готовые package DLL могут содержать результат штатной Unity обработки. Compiler и runtime отказываются от известных неподдержанных случаев. Неизвестные требования игрового кода остаются предметом проверки на реальном проекте.

Package discovery минимального режима не использует `Library/PackageCache`. Исходники registry/git packages можно предоставить через `UNITY_PLAY_PACKAGE_ROOT`, список путей разделяется `:` на Linux. Источники пакета должны соответствовать manifest/lock версии; минимальный режим не загружает их автоматически. Гибрид использует проверенные точные package snapshots из собственного import project.

## Lifecycle

Загрузчик создаёт неактивные объекты, восстанавливает иерархию, компоненты, ассеты и сериализованные ссылки, затем устанавливает авторские active flags. Пользовательские Awake/OnEnable начинаются после восстановления всего графа. Циклические ссылки между активными корнями поддерживаются этой прямой загрузкой, без бинарной сцены и AssetBundle.

Roslyn семантически находит MonoBehaviour callbacks Awake, OnEnable, OnDisable и OnDestroy. Их точки входа остаются методами Unity; тела переезжают в методы того же класса. Во время построения сцены точки входа собирают callbacks в очередь. После активации графа очередь исполняет их с учётом DefaultExecutionOrder и исходной последовательности native вызовов. Явный base.Awake в наследнике выполняется непосредственно внутри текущего Awake. Для компонентов без собственного или унаследованного Awake компилятор добавляет пустой callback, отмечающий начало их инициализации.

Нативные компоненты Player активируются раньше исполнения отложенного пользовательского кода. Поэтому компилятор также направляет чтения публичного Behaviour.isActiveAndEnabled через compatibility accessor. До начала Awake ожидающего компонента accessor возвращает false; затем использует реальное native состояние. В обычном исполнении вне очереди возвращается native значение. Это преобразование исходного кода: вызов из непереписанной DLL или внутреннего кода движка не получает такую подмену.

Вызовы GameObject.SetActive из компилируемых исходников проходят через публичный adapter инструмента. Он сохраняет ожидающий Awake при деактивации объекта и выпускает его при поздней активации, включая выключенный компонент без OnEnable. OnDisable отменяет устаревший ожидающий OnEnable; удалённый объект не получает ещё не начавшиеся callbacks. Активация внутри Awake исполняет новые callbacks до возврата из SetActive. Новые объекты из AddComponent/Instantiate не входят в исходный граф очереди и проходят обычный синхронный native lifecycle; AddComponent покрыт отдельным сравнением. Исключение в пользовательском callback журналируется через Unity и не отменяет остальные callbacks.

Каждая DLL получает compiler proof и assembly marker. Loader сверяет весь список DLL и markers до создания сцены. Старый вход без подтверждённого barrier сохраняет прежнюю активацию по зависимостям и отказ от циклов. Кэш учитывает компилятор, его преобразования и host DLL, поэтому прежние DLL не принимаются как результат нового преобразования.

Порядок задаётся DefaultExecutionOrder и MonoImporter.executionOrder из .cs.meta. Поддерживаются virtual/override callbacks и явно доступные callbacks базовых runtime-пакетов. Отклоняются static, async, generic и partial lifecycle methods, прямые вызовы или method groups callbacks, кроме явного base в том же callback. Null-conditional чтения isActiveAndEnabled и вызовы SetActive, а также method groups SetActive требуют отдельного преобразования и не принимаются.

Эта реализация воспроизводит проверенные наблюдения исходного кода, а не весь внутренний lifecycle Unity. Пакетная активация и изменение активности через переподчинение объектов требуют отдельных проверок. Native sceneLoaded, Editor-only hooks и полная совместимость сериализационных callbacks не воспроизведены. В минимальном режиме ScriptableObject остаётся data-only. В гибриде wrappers задерживают callbacks собственных SO до восстановления полей, prefab templates и native deferred bindings; package SO с приватными callbacks сохраняют штатные native вызовы и требуют проверки их ранней инициализации.

## Dungeon SampleScene

2026-10-06 настоящая `Assets/Scenes/SampleScene.unity` прошла 120 кадров при 960×540/60 FPS на RTX 4060/OpenGL Core: 0 ошибок и 0 исключений. На кадре видны двор, мишени, лестница, оружие и HUD. Геометрия и camera settings проверены относительно ранее собранного SampleScene Player; идентичность пикселей не проверялась, поскольку живая desktop session подаёт ввод мыши.

Frontend восстанавливает 857 scene records с native моделями, исходными префабами, UI, URP и project preloads. Native-кэш содержит 13 460 объектов и 82 deferred prefab binding; каждый binding совпал с исходным GUID/fileID/property path. 90 Editor preview-объектов отсутствуют после штатной сборки и исключены по точному cooked readback proof. Сцена и авторские префабы в пакет не входят.

Старые HDRP components, отсутствующие и в установленном Editor, имеют точную native observation и пропускаются с диагностикой. Native `GetComponents` null slots для отсутствующих скриптов публичным API не воспроизводятся. В native логе также есть сообщения о неподдержанных APV debug shaders и stripped DepthOfField/Panini shaders; эти эффекты не проверены. Запуск SampleScene не доказывает поддержку всех Unity-проектов, произвольных PostProcessing/Addressables или переходов через `SceneManager.LoadScene`. Замеры старта, итераций и ресурсов — в [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md).

Работающая `RawSample.unity` в комплекте представляет небольшой проект без сторонних пакетов. Она проверяет реальный запуск исходной сцены, prefab override, private references до Awake, Material, PNG и ScriptableObject. Это другая сцена, её результаты нельзя переносить на полный Dungeon.

## Проверки и замеры

Первоначальная Python-проверка 2026-10-06 прошла 317 тестов. Native HybridProbe подтвердил сохранение общих Mesh, AudioClip и AnimatorController, циклические ссылки до Awake, корни и дочерние компоненты исходных префабов и их клонов. Он также сверил пять AnimationCurve wrap observations со штатным импортом. Полная SampleScene и все семь итерационных прогонов завершились без ошибок и исключений.

После добавления кэша и persistent Player полная SampleScene приняла девять обновлений без смены PID, повторной загрузки native-каталога и ошибок. Восстановлены новые позиции, ссылки на добавленные GameObject и значения raw ScriptableObject; четыре конкретных EntityId и digest всех 13 460 native объектов сохранены. Медианы запись файла → кадр: 4.637 / 4.935 / 4.404 с соответственно. Source project и host остались неизменными, во входной копии нет Library/Builds. Условия и подробные этапы приведены в [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md).

Финальная Python-проверка после этих изменений прошла 367 тестов. Offline C# suite прошёл 78 assertions; runtime собран против установленного Unity SDK с warnings-as-errors. Независимое review проверило гонки кэша, recovery при потере ack, освобождение старой сцены и автоматическое расширение native dependency closure.

```bash
python3 -m unittest discover -s $UNITY_MICRO_EDITOR_ROOT/tests -v
/path/to/Unity/Editor/Data/DotNetSdk/dotnet run \
  --project $UNITY_MICRO_EDITOR_ROOT/RawRuntimeTests/RawRuntimeTests.csproj
python3 $UNITY_MICRO_EDITOR_ROOT/benchmark_raw.py
```

Benchmark копирует sample в отдельный игнорируемый каталог, измеряет cold/warm start, изменение C# и изменение сцены, prefab, material, ScriptableObject и PNG. Значения проверяются логом реально загруженного скрипта. Он также сравнивает SHA host до и после, проверяет отсутствие `Library` и `Builds` входного проекта. Отчёты остаются в `Logs/UnityPlay/raw-benchmark`; параметры измерения фиксированы: 960×540, 60 FPS, 120 кадров, аппаратный GPU.

Измеренные результаты приведены в [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md).

Исследование повторного использования штатных native импортёров и import workers находится в [NATIVE_IMPORTERS.md](NATIVE_IMPORTERS.md).

Для проверки поведения на настоящем Editor добавлен CompatibilityProbe. Он запускает эталон только в отдельной копии fixture и сравнивает его с независимым raw запуском.
