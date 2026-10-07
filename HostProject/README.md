# Shared Mono host

This is the minimal host source project for Unity Micro Editor. Unity `6000.6.3f1`, revision `45d8eee7de74`, is pinned in `ProjectSettings/ProjectVersion.txt`.

The project references the sibling `com.unitymicroeditor.runtime` package and Unity engine modules, SRP Core `17.6.0`, and Collections `6.6.0`. The runtime initializes through `RuntimeInitializeOnLoadMethod`; the bootstrap scene needs no game components. The builder creates the scene and shader retention assets only when they are missing.

Run `../build-host.sh --dry-run` to inspect the command, or `../build-host.sh` to build the Linux x64 Mono Player. Close other Unity Editor projects first. Build output, imports, logs, and generated resources stay local.

This project defines the basic runtime profile. A target using URP, Input System, Netcode, or other packages needs a matching package-aware host and validated compiler/native baseline. Do not treat this minimal Player as a compatible binary for every Unity project or engine version.
