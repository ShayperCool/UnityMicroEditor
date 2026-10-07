# Go prefab authoring and native readback

Executed on 2026-10-07 with Unity `6000.6.3f1`. Prefab changes ran through our
Go MCP at `127.0.0.1:8095`, started with `--no-editor`. Every prefab/component
mutation reported `backend: raw_source` and `editorStarted: false`. No Player
ran. Native defaults were captured once in a separate owned Editor project;
ordinary schema/add requests read the exact versioned cache for all 22 types.

The final authored target was `Temp/OwnAuthoring/final-project`, a fresh copy of
the three source roots from `Samples/RawProject`. Independent batch Editors
opened separate final-source copies and read them through the public APIs in
[SourceAuthoringReference.cs.txt](SourceAuthoringReference.cs.txt). They did not
save the scene or write to the authored target. The fixture's empty package
manifest lacks `com.unity.modules.jsonserialize`, although its probe and the
readback helper use `JsonUtility`. Each reference copy added that built-in
dependency only for oracle compilation. The authored target manifest remained
unchanged.

| Checkpoint | Objects | Components | Assets | Prefab bindings | Represented properties | Unrepresented properties | Differences |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Base/nested/variant matrix | 13 | 26 | 2 | 13 | 2,706 | 777 | 0 |
| Added components and partial unpack | 13 | 27 | 2 | 13 | 2,772 | 798 | 0 |
| Complete unpack after partial unpack | 13 | 27 | 2 | 8 | 2,778 | 792 | 0 |

The first checkpoint repeats the earlier source-authoring operation matrix:
BoxCollider trigger and size, ParticleSystem with ParticleSystemRenderer,
Canvas with RectTransform replacing the original Transform ID, base prefab,
nested linked prefab, variant, nested BoxCollider override/apply, second override
and revert, complete unpack and a separate linked variant. It also changes the
existing material and ScriptableObject fields while retaining their GUIDs.
Current public captures and source representation produce a different property
count from the historical 2,807/676 run. Unrepresented properties are counted
explicitly and are not treated as equal.

The second checkpoint adds Light to the nested child of a linked variant and
applies it to the variant. Its existing scene component ID remains a stripped
source alias. It adds Rigidbody with mass 7 and reverts that addition, then
unpacks the outer variant layer. The Light remains at intensity 3, Rigidbody is
absent, and the immediate nested/base links remain. The third checkpoint fully
unpacks this remaining instance. All represented values, hierarchy, component
ownership/order, ordinary fileIDs and public source bindings match Unity.

Generated evidence remains outside Git beneath `Temp/OwnAuthoring/final-receipts`:

- `source-parity.json`, `source-partial-parity.json`, `source-complete-parity.json`.
- Corresponding `editor-*-readback.json`, hierarchy data and `asset-sources.json`.
- `variant-apply-replay.json`, `complete-unpack-replay.json` and
  `variant-override-conflict.json`: after MCP restart, identical requests returned
  the confirmed results; changed arguments with the same ID returned
  `request_id_conflict`.
- `source-hashes-{before,after}-replay.txt`: the complete three source roots were
  byte-identical across replay/conflict requests.
- `original-metadata-audit.json`: all seven metadata files inherited from
  `Samples/RawProject` remained byte-identical after the complete matrix.

The run found two real defects that unit parsing alone did not expose. A
nonempty flow modification array on a separate line was valid to `yaml.v3` but
rejected by Unity's narrower parser. Prefab modification writes now emit block
arrays, retaining unknown sibling fields. Copying arbitrary scene stripped
aliases into a new prefab caused two source-binding differences through a
variant. New prefabs now retain ordinary IDs and remap inherited aliases to the
canonical prefab-local IDs. Failed projects, reports and logs remain under
`Temp/OwnAuthoring/receipts` and `verified-receipts`; the successful run used a
fresh fixture. Both defects have focused serialization/identity regressions.

Source-only support deliberately fails for unresolved/native-model prefabs,
unproven references outside the selected source graph, inherited-parent
instantiation, unsupported removed/added GameObject structural operations and
changing an inherited Transform to RectTransform. Array overrides preserve the
existing element count. These failures do not start an Editor. The successful
fixture proves the supported authoring and identity paths; it does not establish
arbitrary-project support or ParticleSystem/Canvas rendering behavior.

The implementation uses the existing project lock, dependency/source/meta CAS,
multi-file journal and publication undo. Immutable source plans support replay
after process restart. Native template/context guards are reattached immediately
before a new publication; confirmed receipt replay performs no source write.
Relevant authoring, MCP/coordinator and all three Go modules passed race/vet
checks after the source changes.
