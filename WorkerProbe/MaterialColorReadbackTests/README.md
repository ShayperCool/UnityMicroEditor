# Material color readback regression

```bash
/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Data/DotNetSdk/dotnet run \
  --project $UNITY_MICRO_EDITOR_ROOT/WorkerProbe/MaterialColorReadbackTests/MaterialColorReadbackTests.csproj
```

Этот SDK executable проверяет production `RawFloatReadback` без Player или Editor. Значения взяты из finite GPU probe Unity 6000.6.3f1: `Dungeon/PixelLit`, `_BaseColor`, `MainColor`, Linear space. При SetColor `(0.82, 0.47, 0.16, 1)` GetColor и GetVector вернули `(0.82, 0.4699999, 0.159999967, 1)`.

В последующем запуске `Decor_SignFace` цвет `(0.05, 0.05, 0.06, 1)` вернулся как `(0.0499999821, 0.0499999821, 0.0599999651, 1)`, с отклонением до 9 ULP.

Допуск ограничен 32 ULP на канал float32. Проверки отвергают 33 ULP, изменение tint/alpha, смену знака, NaN, Infinity и крупное отличие после перепутанного linear/gamma преобразования. Исходные значения материала loader передаёт в SetColor без изменения. Vector свойства, которые Unity тоже хранит в `m_Colors`, восстанавливаются отдельно через SetVector с точным GetVector readback; это сохраняет Infinity sentinel у `_CameraFadeParams`. Отчёт finite probe сохраняется в игнорируемом `Temp/UnityPlay/material-color-proof/diagnostic-readback.json`.
