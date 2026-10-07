# Live package and GUID assembly update without Editor

Executed on 2026-10-07 against the explicitly owned
`Temp/AgentExpansion/dungeon-project` copy. MCP at `127.0.0.1:8094` ran with
`--no-editor`, the Go source baseline `dungeon-go-source-02`, an accepted Unity
`6000.6.3f1` engine profile, an existing development host and the previously
verified `dungeon-auto-05` native catalog. This sequence did not start an Editor,
build a host or recook native assets. The accepted profile, host and catalog had
been prepared earlier with public Unity APIs.

The tested CLI was `Temp/OwnAuthoring/unity-micro-editor-runtime-verified`, SHA256
`21621eb96849de5c141985019b4a5cb7e6950126c9bdc57ee37ce2f0bd6ad19f`.
The first live Player passed all 15 checks in
[check-dungeon-runtime](cmd/check-dungeon-runtime/main.go): actual GPU, generated
ship, public layout/scenario/player state, ready VContainer, supported materials,
no missing scripts and no captured runtime errors/exceptions. It loaded 12,508
native objects.

The validation fixture added exactly nine files under two new folders:

- `Packages/com.unitymicroeditor.validation.live`: `package.json`,
  `Runtime/AgentParity.LivePackage.asmdef`, `Runtime/LivePackageProbe.cs` and
  the two corresponding metadata files.
- `Assets/AgentParityLivePackage`: `AgentParity.LiveAssets.asmdef`,
  `LiveAssetsProbe.cs` and their metadata files.

The embedded package declares its public runtime package dependency. Both new
asmdefs use `autoReferenced: false`; the Assets assembly references the package
assembly through `GUID:d6314636a05c29f4cee86582ef2b2535`. The fixture methods are
public static `[AgentTool]` methods with a public DTO whose integer argument is
bounded to 0–10. A validation-only Go helper published these new folders and used
exact-byte CAS for the later script edits. This verifies source configuration
recovery; it does not claim a package-authoring MCP endpoint.

| Checkpoint | Confirmed update sequence | Player PID | Package method | GUID-linked Assets method |
| --- | ---: | ---: | --- | --- |
| New package and asmdefs | 5 | 2571880 | `22`, `embedded-package-v1` | `22`, caller `AgentParity.LiveAssets` |
| Package method body changed | 7 | 2585548 | `34`, `embedded-package-v2` | `34`, caller `AgentParity.LiveAssets` |
| Original body restored | 9 | 2599367 | `22`, `embedded-package-v1` | `22`, caller `AgentParity.LiveAssets` |

Adding the package/asmdefs first produced an explicit graph rejection while
preserving the old paused Player, PID 2499578. Its public readback still
answered. `wait_update` automatically queued
`config-594ca46979f0af2cc9b6461de3745b1163c4113f`; repeated polls retained the same
configuration identity. The completed preparation reported
`go_authored_source`, `go_source_native_resource`, `go_package_resolver` and
`dotnet_sdk`. Its graph resolved the exact GUID dependency. The replacement
Player refreshed the MCP catalog from two to four tools without descriptor
errors.

Changing only the package method body compiled exactly
`AgentParity.LivePackage`, in 3.95 seconds, and confirmed its new Player in
25.23 seconds. Restoring the exact original script bytes compiled the same
assembly in 5.76 seconds and confirmed readiness in 34.11 seconds. The consumer
assembly observed both changes without its own compilation. Imported native
identity digest and object count remained unchanged. The three full Dungeon
checkpoints each passed 15/15; the combined configuration/tool/update/source
checks passed 25/25.

All 5,711 preexisting files in the owned copy's `Assets`, `Packages` and
`ProjectSettings` remained byte-identical. The final 5,720-file inventory contains
exactly the nine planned additions, including all four metadata files, with
their initial hashes. The package method body is restored to v1. These named
validation helpers remain in the owned copy; the original Dungeon project was
not a write target. This is a preservation audit of the owned copy, not proof of
the original project's earlier history.

Evidence is retained outside Git in `Temp/OwnAuthoring/dungeon-live-package`:

- `live-package-assertions.json`, `check-live-package.jq` and the three
  `*-invariants.json` reports.
- `install-fixture.json`, `hot-fixture.json`, `restore-fixture.json`,
  `source-preservation.json`, complete before/after source hash inventories and
  the validation-only CAS helper.
- `package-recovery-01.json` through `05.json`,
  `configuration-preparation.json`, `new-assemblies.json`, tool catalogs and
  public tool/readback/log receipts for every checkpoint.
- `package-hot-update-02.json`, `package-restored-update.json` and
  `package-restored-pause.json` confirm the later ACKs and paused handoff.

The earlier `package-hot-update-01.json` used a wrongly nested `afterSequence`
and returned the previous ACK. It is retained and excluded from the checks;
subsequent requests use the top-level field and require a newer sequence.
The earlier startup closure-bound failure is also retained. Deduplicating the
imported dependency queue fixed it before this successful sequence.

At handoff, PID 2599367 was deliberately paused through the gated gameplay loop,
with audio paused and `timeScale: 0`. Later runtime/host validation can replace
that process. This run establishes the supported embedded-package/GUID asmdef
and method-body update paths in this project, with retained prepared native
content. It does not establish new native import/build behavior or generic
Editor extension execution.

## Final host06/profile06 repeat

Repeated the complete method-body cycle after the runtime material/builtin
shader fixes, using the final CLI SHA256
`7f463667868750464ba3b7a70195782c646a85f82cc1c2ffddc72a32f84be501`.
MCP still ran with `--no-editor`; the prepared source baseline was
`dungeon-go-source-04` and the accepted engine profile was `source-profile-06`.
This profile already contains the native `AgentParity.LivePackage` assembly,
and the Go source preparation supplies the package and GUID consumer through
the SDK. The repeat therefore covers a package already captured in the host
profile as well as the earlier newly introduced package.

| Checkpoint | Confirmed sequence | Player PID | Both method results | Package-only compile / source-to-ready |
| --- | ---: | ---: | --- | --- |
| Final baseline | 2 | 2853790 | `22`, v1 | Initial readiness |
| Changed body | 4 | 2863017 | `34`, v2 | 3.08 s / 23.28 s |
| Exact restored body | 6 | 2872667 | `22`, v1 | 4.25 s / 23.73 s |

Both the initial and restored Dungeon checkpoints passed 15/15. The final
configuration, public tool, SDK ACK, native identity, source preservation and
pause checks passed 22/22. All 5,711 original files in the owned copy remain
unchanged, and the same nine fixture files retain their initial hashes. The
final Player was left paused at revision `622326accf9ecc930bf9511f9a71cc5a`,
with 12,508 native objects and zero readiness errors/exceptions.

Final evidence is separate from the earlier proof, under
`Temp/OwnAuthoring/dungeon-live-package-final06`: `final06-assertions.json`,
`accepted-profile-summary.json`, `accepted-baseline-summary.json`, initial/final
Dungeon invariant reports, public tool receipts, `hot-update.json`,
`restored-update.json`, `restored-pause.json` and `source-preservation.json`.

The source-only MCP at `127.0.0.1:8095` was also restarted on this exact CLI.
PID 2865684 exposes 20 tools, reports `mainEditorAllowed: false`, source prefab
apply/revert/unpack and the versioned component cache. Its actual initialize
reply describes source prefab authoring. A cached BoxCollider schema read
reported `editorStarted: false`; runtime remained stopped at sequence zero.
Its authored source hashes stayed unchanged. The eight checks and raw replies
are in `Temp/OwnAuthoring/source-server-final06-receipts`. Both local MCP servers
were left running for handoff.
