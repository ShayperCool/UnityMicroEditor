# Проверка сырого проекта

Этот проект запускается собственным importer через универсальный Unity Mono host. Его `Assets` содержат текстовую сцену, prefab, material, ScriptableObject, PNG и два C# класса. `Library`, импортированные ассеты, собственный Player и bundles не нужны.

Из корня ворктри после `$UNITY_MICRO_EDITOR_ROOT/unity-play host-build`:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play --project $UNITY_MICRO_EDITOR_ROOT/Samples/RawProject \
  raw-run --scene Assets/Scenes/RawSample.unity --watch --fps 60 --no-vsync
```

Сцена показывает вращающийся куб. `RawIterationProbe` проверяет private ссылки и сериализованные bool/enum/array до Awake, исходного родителя и active flags связанных корней. В Start он пишет `[unity-raw-proof]` с данными реально загруженных ассетов и версией кода.

Для проверки итераций меняйте `CodeRevision` в скрипте, `revision` в Tuning.asset, цвет Checker.mat или override префаба в RawSample.unity. Watcher перезапускает сцену; ошибка подготовки оставляет старую игру. Более подробные условия и ограничения в [RAW_PROJECTS.md](../../RAW_PROJECTS.md).

Это отдельная проверочная сцена. Она не заменяет настоящую Dungeon SampleScene и не доказывает поддержку URP/FBX/Animation.
