# Cache retention

`unity-micro-editor cache-prune --project /absolute/game/project` prints a JSON deletion plan. Add `--apply` to remove the listed directories. The project must contain `Assets` and `ProjectSettings`; there is no project discovery.

The command keeps the newest two successful preparations for each scene, Editor, backend and configured engine/host/catalog combination. It follows references in retained JSON receipts, including JSON object keys and referenced nested metadata, to protect dependent preparations and import contexts. Two recent contexts of each known native import worker layout stay warm. Incomplete worker contexts also retain the age window. Unknown import layouts stay. Host builds, engine profiles, package snapshots, baked native resources, source assets and `.meta` files are outside the deletion set.

Incomplete preparations and old session `runs` expire after 14 days. Each session keeps its newest two runs. Change the age with `--max-age-days N`, with a minimum of seven days. Unknown preparation directories without `request.json` stay. Malformed or oversized retained receipts cause the command to fail before deletion.

Create `.keep` inside a preparation, import context or run to pin it. Alternatively, put a JSON array of absolute paths in `Library/UnityMicroEditor/retention-pins.json`. A path within a preparation or run pins that directory. Keep independent benchmark/oracle reports outside expired run directories, or pin their directory.

Deletion uses at most four workers for disjoint generated directories. The project lock stays held until every worker finishes.

Pruning holds the same `project-actions.lock` as agent preparation and publication. It refuses a busy lock or a detected Editor, helper or Player using the project. Run maintenance while the project is idle. A process launched outside the coordinator can still race with the maintenance check; close it before an explicit manual cleanup. Symlink cache roots and symlink collection directories are refused. Symlink entries are not traversed.

A user systemd timer can call an installed copy of the binary with explicit project paths. For example:

```ini
# ~/.config/systemd/user/ume-cache-retention.service
[Unit]
Description=Prune idle Unity Micro Editor caches

[Service]
Type=oneshot
ExecStart=/absolute/unity-micro-editor cache-prune --project /absolute/game/project --apply
Nice=15
IOSchedulingClass=idle
```

```ini
# ~/.config/systemd/user/ume-cache-retention.timer
[Unit]
Description=Daily Unity Micro Editor cache retention

[Timer]
OnCalendar=*-*-* 03:40:00
Persistent=true
RandomizedDelaySec=15m

[Install]
WantedBy=timers.target
```

Then run `systemctl --user daemon-reload` and `systemctl --user enable --now ume-cache-retention.timer`. Inspect the next run with `systemctl --user list-timers ume-cache-retention.timer` and the cleanup log with `journalctl --user -u ume-cache-retention.service`. Keep the installed binary separate from a Cargo build directory that you may clean.

This complements the runner's existing revision/assembly/Player pruning. It does not clean arbitrary `Temp` trees, Cargo targets, old host builds or unreferenced baked native data. Those need a separate review because experimental project copies can contain authored changes and pinned runtime evidence.
