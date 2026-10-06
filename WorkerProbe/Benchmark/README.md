# Проверка обновления C# в работающем Go watch

Драйвер использует уже запущенный watch. Player и Editor сам не запускает. По умолчанию только создаёт резервную копию и план; временная правка требует `--apply`.

Из корня `$UNITY_MICRO_EDITOR_ROOT/WorkerProbe`:

```bash
CGO_ENABLED=0 go build -trimpath -o ../../../Temp/UnityPlay/benchmark-code ./Benchmark
../../../Temp/UnityPlay/benchmark-code \
  --project /mnt/data/projects/Dungeon-wt/unity-cli-play \
  --log /mnt/data/projects/Dungeon-wt/unity-cli-play/Temp/UnityPlay/go-project-watch-proof.jsonl \
  --output /mnt/data/projects/Dungeon-wt/unity-cli-play/Temp/UnityPlay/code-benchmark-first \
  --apply --timeout 90s
```

Output должен быть новым каталогом внутри `Temp`. Резервная копия — `AmbientSound.cs.original`, доказательства — `result.json`.

В `Assets/Dungeon/Scripts/Audio/AmbientSound.cs` временно добавляется public static метод с уникальным integer return. Он не вызывается и не меняет игровое поведение, но добавляет настоящее тело IL и metadata. После `code-updated` сверяются новый PID, SHA256 DLL, присутствие метода в metadata, exact manifest/lifecycle coverage и loaded scene readiness с нулём ошибок/исключений.

Затем восстанавливаются точные исходные байты и ожидается отдельный `code-updated` с отсутствующим методом в DLL. При обычном SIGINT восстановление выполняется через `defer` с отдельным bounded context. Чужая параллельная правка не перезаписывается; исходная резервная копия остаётся в output. Завершение процесса через SIGKILL не выполняет `defer`.

`runtimeMarkerInvoked=false` в доказательстве означает, что метод намеренно не вызывался. Проверка доказывает свежую DLL в manifest новой запущенной сцены, а не исполнение конкретного метода.

Для обычной правки тела существующего метода добавь `--body-only`. Драйвер заменит единственный `Random.Range(0.3f, 1f)` на `Random.Range(0.30000004f, 1f)`. Нижняя граница отличается примерно на 30 ns; declarations остаются прежними. Проверка DLL ищет новый `ldc.r4` и точные байты float. В режиме `method-body` expected результат оптимизации — `compiledAssemblies=["Dungeon.Runtime"]` и пустой `declarationChanged`. Если fingerprint не подтвердил равенство, отчёт сохраняет фактически пересобранные сборки.
