# Standalone content API probes

Исследовательский инструмент для Linux Unity 6000.6.3f1. Использует bundled .NET 8 SDK; NuGet feeds отключены. Editor и Player этот runner не запускает. Исходная сцена и ранее собранный bundle только читаются, архивы и логи создаются в `--out`.

```bash
python3 $UNITY_MICRO_EDITOR_ROOT/ContentProbe/run_probes.py \
  --editor /home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity \
  --scene Assets/Scenes/SampleScene.unity \
  --bundle Builds/CLIPlay/content/revisions/<revision>/cli-play-scenes \
  --out Logs/UnityPlay/no-editor-research/probes
```

Runner отдельно проверяет публичный Editor binding из .NET, невозможность обычного `dlopen` установленного Editor PIE, автономное создание/чтение UnityFS и чтение объектов cooked bundle. Negative cases ожидаются; общий exit 0 означает совпадение конкретных исследованных результатов, а не поддержку raw-content build.

Последняя отдельная проба пытается открыть сырой YAML как binary SerializedFile. На установленной версии parse вернул код 6, а cleanup после этого в первоначальном эксперименте упал с SIGSEGV. Поэтому negative input проверяется только в отдельном процессе, native cleanup после неудачного parse пропускается; память освобождает завершение процесса. Coredumps отключены, каждая native проба ограничена 20 секундами. Эту ветку нельзя использовать как production loader неподдерживаемых данных.

Сигнатуры C ABI сверены с [официальным wrapper UnityDataTools](https://github.com/Unity-Technologies/UnityDataTools/blob/b73e33c1847ff81d43bc1da3726145a9a4c97c3a/UnityFileSystem/DllWrapper.cs). Полный разбор и отдельно проведённый template bootstrap эксперимент — [NO_EDITOR_CONTENT.md](../NO_EDITOR_CONTENT.md).

Для воспроизведения bootstrap boundary есть отдельный скрипт. Он запускает Player template без графики и без готовых project data; `--out` должен быть новой папкой. Ожидаемый результат — отказ запуска из-за отсутствующего `globalgamemanagers`, зафиксированный в JSON и `player.log`:

```bash
python3 $UNITY_MICRO_EDITOR_ROOT/ContentProbe/probe_template.py \
  --editor /home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity \
  --assets Assets \
  --out Logs/UnityPlay/no-editor-research/raw-template-reproduce
```

Отдельная bounded-проба проверяет Autodesk FBX SDK, поставляемый архивом пакета `com.autodesk.fbx` в установке Unity:

```bash
python3 $UNITY_MICRO_EDITOR_ROOT/ContentProbe/probe_fbx.py \
  --unity /home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity \
  --source Assets/Dungeon/ShipGen/Art/Custom/Shell/R_Floor.fbx \
  --output Logs/UnityPlay/native-import-research/fbx-probe.json
```

Скрипт извлекает только `UnityFbxSdkNative.so` и читает опубликованные P/Invoke-сигнатуры `NativeMethods.cs`. FBX копируется во временную папку; native API вызывается отдельным процессом с лимитом 10 секунд, coredumps отключены. JSON фиксирует SHA-256 пакета, библиотеки, wrapper и исходника, ABI, число узлов/meshes/control points/polygons и время декодирования. На `R_Floor.fbx` получены 3 узла, 1 mesh, 8 control points и 6 polygons; Editor не запускался.

Это самостоятельный Autodesk decoder, а не Unity `ModelImporter`. Не проверено, использует ли встроенный importer эту библиотеку. Проба не применяет Unity `.meta`, rig/avatar/animation-настройки, не сохраняет Unity subasset IDs и не создаёт cooked runtime-ассеты. [Документация пакета Autodesk FBX SDK](https://docs.unity3d.com/Packages/com.autodesk.fbx@5.1/manual/index.html).
