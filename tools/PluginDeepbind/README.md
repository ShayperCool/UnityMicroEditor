# Native plugins that keep their own symbols

A Player of Play mode code ([docs/UNITY_RELEASES.md](../../docs/UNITY_RELEASES.md#play-mode-code)) runs the paths a game keeps for the Editor, and they may load native libraries for the desktop. Firebase's `FirebaseCppApp` carries a whole gRPC (with Abseil and BoringSSL), and so does `libgrpc_csharp_ext` of a game's own gRPC services, each of another version. Opened as the engine opens plugins, the second one runs on the first one's symbols and crashes. zavod's Editor images preload a library for the same reason.

`plugin-deepbind.c` is preloaded into such a Player (`LD_PRELOAD`). It opens every library below the directories `UME_DEEPBIND_PREFIXES` names (the Player's `<Player>_Data/Plugins/`, separated by `:`) with `RTLD_DEEPBIND`, so that it binds to its own definitions before those already loaded. `UME_DEEPBIND_LOG=1` prints each open with its flags and the library's base address on standard error, which is how a crash's frames are told apart by library.

`RTLD_DEEPBIND` does not separate `STB_GNU_UNIQUE` symbols: the loader keeps one of each name for the whole process. gRPC's metadata tables are such symbols (static locals of templates), so the runner also rewrites their binding to `STB_WEAK` in the session's copy of each plugin (`player::isolate_plugin_symbols`); the project's files are not changed.

The runner builds the library with the system's C compiler (`cc -shared -fPIC -O2 ... -ldl`) the first time a session needs it, into `Library/UnityMicroEditor/compiler-store/plugin-deepbind/<hash of the source>/`. Without a C compiler the session says so and the plugins share their symbols, as in a Player without it.
