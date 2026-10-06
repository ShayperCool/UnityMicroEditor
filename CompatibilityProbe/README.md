# Проверка совместимости собственного Play Mode с Unity

Цель — воспроизводить поведение Unity `6000.6.3f1` на конкретных исходных проектах и проверять каждое расширение через один и тот же наблюдаемый результат. Совпадение картинки или конечных полей не доказывает совпадение импорта, идентификаторов, сериализации и lifecycle. Неподдержанный сценарий должен давать отдельный результат `unsupported`, а не становиться успешным после удаления части объектов.

## Запуск сравнения

После подготовки universal host:

```bash
unity_editor=/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity
python3 $UNITY_MICRO_EDITOR_ROOT/CompatibilityProbe/run_comparison.py --unity "$unity_editor"
python3 $UNITY_MICRO_EDITOR_ROOT/CompatibilityProbe/run_comparison.py --unity "$unity_editor" --case transparent-texture
python3 $UNITY_MICRO_EDITOR_ROOT/CompatibilityProbe/run_comparison.py --unity "$unity_editor" --case cyclic-roots
```

Каждый запуск создаёт новый каталог в `Temp/UnityPlay/compatibility/`, два отдельных исходных проекта и `comparison.json`. Editor вызывается только для `reference-project`; самостоятельный runner получает `raw-project`. Исходные Assets обоих входов содержат одинаковые игровые данные и общий observer. Editor instrumentation добавлен отдельно. Рабочий Dungeon не открывается и не перезаписывается. Для своего каталога используйте `--output`; существующий fixture не перезаписывается.

Базовый профиль использует Built-in rendering, обычный текстовый prefab, unit quaternions, explicit SceneRoots, data-only ScriptableObject, PNG с явными FromInput и uncompressed Standalone settings. Observer сравнивает порядок и состояние восьми объектов, выбранные public native properties, serialized script fields, ссылки, material properties, texture format/texels и callbacks обычного/disabled/inactive компонентов. Это покрытие конкретного fixture, а не доказательство совпадения всех Unity APIs.

Граф объектов идентифицируется hierarchy path и component index. Для ассетов общий observer использует тип и имя, сохраняя отдельные дубликаты; GUID/local IDs дополнительно снимаются на стороне Editor. Полная проверка стабильной subasset identity требует отдельного importer case из таблицы ниже.

Числа сравниваются с absolute/relative tolerance `1e-5`, включая JSON script fields. Журнал сравнивается по последовательности событий каждого объекта и наблюдаемым полям. Для классов с DefaultExecutionOrder отдельно проверяется заданный порядок Awake/OnEnable/Start. Вложенная активация и AddComponent имеют отдельные проверки синхронного выполнения. Общий порядок равноприоритетных callbacks на разных объектах и process-relative frameCount не являются критериями. Неактивный объект остаётся в snapshot без callbacks; disabled компонент на active объекте проверяется отдельно.

DTO отчёта записывает общий managed serializer. На реальном raw Player `JsonUtility.ToJson` молча исключил вложенные DTO из загруженной DLL, поэтому отчёт через него не давал полного наблюдения. Serialized игровые поля продолжает читать публичный Unity serializer; неподдержанные пользовательские графы должны обнаруживаться отдельным сравнением.

## Прямой loader с очередью, 2026-10-05

Все 12 сценариев на текущем host прошли настоящее сравнение Editor Play Mode и raw Player. Scene data, serialized references, наблюдаемые флаги внутри callbacks и проверяемые отношения порядка совпали, различий нет.

| Сценарий | Результат | Объекты | Ассеты | Lifecycle события |
| --- | --- | ---: | ---: | ---: |
| baseline | equivalent | 8 | 4 | 4 |
| transparent-texture | equivalent | 8 | 4 | 4 |
| cyclic-roots | equivalent | 9 | 4 | 7 |
| inherited-lifecycle | equivalent | 8 | 4 | 6 |
| execution-order | equivalent | 9 | 4 | 7 |
| disable-peer | equivalent | 9 | 4 | 6 |
| deactivate-peer | equivalent | 9 | 4 | 5 |
| destroy-peer-immediate | equivalent | 8 | 4 | 5 |
| late-reactivate | equivalent | 9 | 4 | 9 |
| enable-only-peer | equivalent | 9 | 4 | 4 |
| reentrant-activation | equivalent | 9 | 4 | 8 |
| dynamic-add-component | equivalent | 8 | 4 | 1 |

Во всех случаях исходные SHA сохранились, raw-проект остался без Library, runtime использовал RTX 4060. Обе перезагрузки Editor явно включены. Отчёты находятся в `Temp/UnityPlay/compatibility/<case>-direct-results/`: `comparison.json`, `editor-observation.json`, `raw-observation.json`, `raw-runtime.json`. Это покрытие 12 небольших fixtures; полноценная Dungeon SampleScene сюда не входит.

205 Python tests и 61 pure C# assertion прошли. Компилятор дополнительно проверен настоящим bundled Roslyn: 14 новых integration cases входят в Python suite. Новые [замеры итерации](../RAW_BENCHMARKS.md) выполняются отдельно, без observer из CompatibilityProbe.

## Предыдущая проверка без очереди callbacks, 2026-10-05

Unity `6000.6.3f1`, Linux Mono host, NVIDIA RTX 4060, raw окно 960×540, 60 FPS, 30 кадров. Настоящий reference Editor входит в Play Mode с domain и scene reload. Его `Library` остаётся только в изолированном reference-проекте.

| Сценарий | Результат | Наблюдение |
| --- | --- | --- |
| baseline | equivalent | 8 объектов, 4 ассета и 4 lifecycle события; 0 различий по scene data и lifecycle. Непрозрачный RGBA PNG импортируется в RGB24. |
| transparent-texture | equivalent | То же сравнение с alpha 0/128/255 и цветными texels; 0 различий, texture format RGBA32. |
| cyclic-roots | unsupported | Editor загрузил 9 объектов и записал 7 событий. Оба peer активны уже в Awake; raw loader отверг цикл до создания сцены. |

В обоих raw запусках отсутствовала `Library`, SHA исходного проекта не изменились, runtime подтвердил аппаратный RTX 4060. Отчёты находятся в `Temp/UnityPlay/compatibility/baseline-final/` и `transparent-final/`: `comparison.json`, `editor-observation.json`, `raw-observation.json`, `raw-runtime.json`. Это не новый benchmark скорости итераций: observer и startup instrumentation добавляют работу.

Отрицательный результат находится в `cyclic-final/`. Его `comparison.json` имеет `success:false`, `outcome:unsupported` и конкретную причину из native runtime. Неуспешный raw запуск не превращается в совпадение из-за отсутствующего snapshot.

По наблюдениям исправлены типы внешних ссылок, имя импортированного prefab root, texture storage и default innerSpotAngle. У fixture исправлены неполные Unity YAML версии/metadata, root-order metadata и ненормализованные quaternion. Unsupported texture compression, Crunch, explicit platform formats и nonunit rotations теперь диагностируются, а не принимаются как эквивалентные.

Проверены 183 Python tests и 28 pure C# runtime assertions. Общий observer/Editor instrumentation компилируются установленным SDK; оба положительных результата дополнительно проверены настоящими Editor и Player процессами.

## Контракт независимого pipeline

Собственный редактор и CLI работают с общим представлением проекта. Источником остаются `Assets`, доступные исходники packages, `.meta` и `ProjectSettings`; UI изменяет эти файлы через транзакции с сохранением существующих GUID/fileID и prefab overrides. Импортированные результаты принадлежат отдельному кэшу инструмента. `Library` эталонного проекта и его ранее собранные сцены не являются входом независимого pipeline.

| Стадия | Обязательный результат |
| --- | --- |
| Обнаружение и зависимости | Реестр source GUID, локальных IDs и package-происхождения; граф зависимостей; явная диагностика отсутствующих и неоднозначных ссылок. |
| Компиляция и схема типов | Runtime assembly graph, player defines, сериализуемые поля и типы, требуемые генераторы/ILPP. Изменение схемы типа инвалидирует зависимый контент. |
| Импорт | Отдельный backend для каждого поддержанного importer с учётом `.meta`, версии, platform и влияющих settings. Результат содержит subassets и их идентификаторы. |
| Сериализация и prefab composition | Полный граф объектов, сохранённые ссылки, variant/instance overrides, политика неизвестных полей и типов. Обход графа не определяет произвольно lifecycle. |
| Запуск | Явно выбранная политика reload, обработка script execution order, создание native объектов и проверяемый момент начала callbacks. Первый корректный кадр — критерий готовности. |

Unity refresh обрабатывает изменения файлов, затем code-related импорт/компиляцию, загрузку кода, postprocessing и остальные ассеты; импорт может ставить новые работы и перезапускать refresh. Для собственного pipeline практическое требование — довести граф до стабильного состояния и публиковать revision целиком после проверки исходных хешей. Простого списка файлов, отсортированного по имени, недостаточно для совместимости importer callbacks. [Asset Database refresh](https://docs.unity3d.com/6000.6/Documentation/Manual/AssetDatabaseRefreshing.html).

Привязка внешней ссылки использует пару GUID/local file ID. Local IDs нужно хранить как 64-битные значения или строки, без преобразования в JSON float; imported subassets нельзя идентифицировать только именем или позицией в массиве. В эталоне используется публичный `AssetDatabase.TryGetGUIDAndLocalFileIdentifier`. [GUID и local file ID](https://docs.unity3d.com/6000.6/Documentation/ScriptReference/AssetDatabase.TryGetGUIDAndLocalFileIdentifier.html).

## Эталон и сравнение

Базовый `Samples/RawProject` задаёт `textureCompression: 0` в исходном `Probe.png.meta`: сравнение проверяет несжатый 2D PNG, включая native format, texels и sampling. Это часть контракта fixture, а не удаление проверки формата. Platform compression и её defaults проверяются отдельными cases; текущий raw decoder их не воспроизводит.

Ручной source fixture также содержит актуальный `SceneRoots` с авторским порядком roots и нормализованные rotation quaternions камеры и света. Нормализация сохраняет направление поворота; она исправляет входной профиль, а не ослабляет сравнение полей. Editor способен сохранить ненормализованные YAML значения, тогда как публичный runtime setter их нормализует. Для таких исходников независимому loader нужна явная диагностика неподдержанного случая; игнорировать различие компонентов quaternion при сравнении нельзя.

Оба запуска получают один неизменный input snapshot с теми же исходниками, `.meta`, Unity version и settings. Настоящий Editor импортирует отдельную рабочую копию и входит в настоящий Play Mode; независимый runner читает другую копию без Editor и без imported `Library`. Данные, экспортированные эталоном, служат только ожидаемым результатом сравнения. Передача его готовой сцены второму запуску проверяет загрузку Unity data, но не независимый импорт.

Профили компиляции тоже фиксируются: настоящий Editor может исполнять ветки `UNITY_EDITOR`, `OnValidate`, `ExecuteAlways` и Editor initialization hooks, которых нет в холодном Mono Player. В первом fixture эти зависимости исключаются явно. При расширении совместимости их нужно воспроизводить или отмечать как неподдержанные; одинаковый snapshot обычного MonoBehaviour не доказывает выполнение Editor-side hooks.

Режим reload фиксируется в отчёте. В Unity 6.6 default — **Reload Scene only**, с выключенным domain reload. Для первого сравнения с холодным процессом нужно явно включить **Reload Domain and Scene**. Сценарий без domain reload проверяется отдельно: перезапуск процесса сбрасывает static state и не эквивалентен этому режиму. [Настройки входа в Play Mode](https://docs.unity3d.com/6000.6/Documentation/Manual/configurable-enter-play-mode.html).

Сравниваются стабильные идентификаторы, component types, значения полей, object-reference identity, hierarchy/active state, результаты importer и отдельный журнал callbacks. Process PID, instance/entity IDs, абсолютные пути и время не сравниваются как семантика. Float tolerances задаются для конкретных измерений; кадр и fixed-step фиксируются явно. Вызовы одного типа на разных объектах Unity не обещает упорядочивать произвольным стабильным образом, поэтому сравниваются гарантированные отношения порядка и наблюдаемые состояния, а не полный текстовый trace. [Execution order](https://docs.unity3d.com/6000.6/Documentation/Manual/execution-order.html).

## Шесть проверок с высокой ценностью

| Проверка | Минимальный вход и наблюдение | Какой дефект обнаруживает |
| --- | --- | --- |
| Идентичность и subassets | Один FBX с двумя meshes, material и animation clip; scene references на разные subassets. В эталоне перечислить `LoadAllAssetsAtPath` и GUID/local ID/type/name. Затем переместить FBX вместе с `.meta` и повторить импорт. | Потерянные local IDs, неверная привязка meshes/clips, зависимость ссылок от пути. Обычная геометрия из FBX SDK сама эту проверку не проходит. |
| Prefab composition | Base prefab → variant → nested instance. Переопределить scalar и object reference; отдельно добавить/удалить компонент и проверить reparenting. Снимать hierarchy, component multiplicity, source identity и итоговые ссылки. | Неполные structural overrides, неверные instance-local IDs и потерянные связи между variant и исходником. Пока structural operations не поддержаны, результат — `unsupported`. |
| Lifecycle и активные ссылки | Два активных root GameObject, A и B, с циклическими serialized references. В `Awake` каждый пишет состояние полей и `partner.activeInHierarchy`; отдельно проверить disabled behaviour на active GO и behaviour на inactive GO. | Процедурная последовательность `SetActive` может показать частично активную сцену. Legacy activation без instrumentation такие cycles отклоняет. Прямой loader с проверенными wrappers восстанавливает весь граф до начала пользовательских callbacks; состояние ожидающего компонента дополнительно проверяется через isActiveAndEnabled. |
| Сериализация и ScriptableObject | SO с авторским serialized значением, `OnEnable`, `ISerializationCallbackReceiver`, ссылкой из MB и повторным использованием той же SO instance. Журналировать значения в callbacks и `Awake`. Затем отдельными вариантами проверить missing field/default, field rename и managed-reference aliasing. | `CreateInstance` + overwrite может вызвать `OnEnable` до авторских полей; итоговый snapshot этого не заметит. Alias identity и callback ordering требуют самостоятельного контракта. |
| Texture importer settings | Маленький PNG с известными texels: variants `.meta` для sRGB/linear, mipmaps, wrap/filter, readability и max-size. Сравнить dimensions, format, mip count, texels на выбранных mip levels и фиксированный render sample. | Совпадение исходных PNG bytes скрывает неверный native texture, пропущенный resize, mip generation и sampler/color-space differences. Compression и sprites выделяются в следующие независимые cases. |
| Reload и публикация изменений | Два входа в Play Mode с заданными reload flags и static counter; отдельно изменить существующее поле, schema типа, PNG и prefab reference. После compile/import ошибки наблюдать, какая revision осталась активной. | Перезапуск процесса вместо выбранной reload семантики; stale DLL/schema/content cache, частичная публикация revision. Время итерации измеряется отдельно от корректности. |

В callback probe нельзя использовать Unity API из `OnBeforeSerialize`/`OnAfterDeserialize` только ради логирования: serializer может работать на другом потоке. Записывается собственный pure-managed tag и данные объекта. Для managed references Unity гарантирует host `OnBeforeSerialize` до callbacks его references и host `OnAfterDeserialize` после них; порядок между references произвольный. [Serialization callbacks](https://docs.unity3d.com/6000.6/Documentation/ScriptReference/ISerializationCallbackReceiver.html).

`RequireComponent` лучше проверять отдельным вариантом lifecycle case: сериализованный требуемый компонент должен существовать единожды и быть тем же объектом, на который указывают authored references. Автоматическое добавление native компонента во время создания MB нельзя незаметно считать восстановлением сериализованного component record. [RequireComponent](https://docs.unity3d.com/6000.6/Documentation/ScriptReference/RequireComponent.html).

## Прямой lifecycle без AssetBundle

Выбран прямой путь по решению пользователя. Loader не пишет Unity binary scene и не упаковывает AssetBundle. Runtime восстанавливает исходный граф публичными Unity API, а компилятор добавляет точки управления lifecycle. Реализация описана в [RAW_PROJECTS.md](../RAW_PROJECTS.md).

Очередь сохраняет Awake→OnEnable по компонентам с учётом DefaultExecutionOrder. Отложенный Awake переживает деактивацию до своего начала. Вложенные native callbacks при AddComponent или активации из Awake должны завершаться синхронно до возврата в вызывающий код. Чтения Behaviour.isActiveAndEnabled и вызовы GameObject.SetActive в исходниках семантически направляются через adapter; одноимённые пользовательские свойства не изменяются.

Компилятор не изменяет authored source и выдаёт proof для каждой DLL. Непереписанный код и внутренние вызовы движка остаются вне этого протокола. Полная эквивалентность Editor не заявляется: sceneLoaded, Editor-only hooks, ScriptableObject lifecycle и иные способы активации требуют собственных сравнений.

Observer сохраняет снимок через один кадр после Start и публикует одинаковый gate для Editor и raw Player. Reference читает именно сохранённый снимок, а не более позднее состояние EditorUpdate. Вход в Play откладывается на два публичных EditorApplication.delayCall: штатная Search startup-инициализация успевает закончиться в Edit Mode. Error/Exception/Assert продолжают делать сравнение неуспешным; ошибка Search не исключается фильтром.
