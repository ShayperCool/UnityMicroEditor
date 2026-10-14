# The Player side of hot reload

These sources are the applier: the code that redirects methods inside a running Player. The runner compiles the directory into `UnityMicroEditor.HotReload.dll` for a session, with the bridge's session flags and against the staged Player's `Managed` directory. The Player bridge (`Runtime/CodeReload.cs`) loads the assembly from bytes on the first code reload request and calls `UnityMicroEditor.HotReload.Entry.Handle(string requestPath)` on the main thread, at the end of `EarlyUpdate`. The applier is not part of the bridge and is not pinned to a host build.

Verified for Unity `6000.6.3f1`, Mono `6.13.0`, Linux x86-64. Other platforms, architectures and backends are refused with a message.

| File | Responsibility |
| --- | --- |
| `Entry.cs` | The entry point; one request from the descriptor to the ack |
| `Detour.cs` | Entries of compiled methods and every store into them: the first byte, the second instruction, the gated jump |
| `Island.cs` | The code such a store leads to, islands and relays, and the pages it is mapped in |
| `Canary.cs` | Whether the JIT of this process inlines |
| `FieldStore.cs` | Storage of fields a patch added to loaded types; the only public API besides `Entry` |
| `Callbacks.cs`, `Bodies.cs` | `[InvokeOnHotReload]` and `[InvokeOnHotReloadLocal]`; whether a body changed with a generation |
| `Engine.cs` | Everything that needs the engine or the bridge assembly |
| `Json.cs` | Reader and writer of the three documents |

Every `.cs` file of the directory belongs to the assembly; nothing else does.

## What one request does

The request names a descriptor. The descriptor is cumulative: it lists every method that differs from the assemblies the Player loaded at its start and replaces the generation before it entirely.

1. Read the request (at most 64 KiB) and the descriptor (at most 16 MiB, SHA-256 checked). The generation must be newer than the applied one.
2. Read every patch assembly (at most 64 MiB, SHA-256 checked) and its symbol file.
3. Check the JIT (see "Inlining"). Only when the descriptor lists a detour.
4. Find each game assembly among the loaded ones by simple name and MVID. Resolve every original by its metadata token and check that it is a closed method with an IL body.
5. Load the patch assemblies with `Assembly.Load(raw[, pdb])`. An assembly that was loaded before, by SHA-256, is used again. Resolve every replacement by token and compare the native signatures.
6. Compile every replacement and every original.
7. Decide how each method is written (see "The redirect") and map the islands and relays that are needed. This is where the process map is read, after everything listed is compiled.
8. Compile every other method of the patch assemblies.
9. Write. For every method that is redirected for the first time: its first byte, or its second instruction, or a whole jump behind the gate with one drain of 1 ms for the batch. For the methods that are already redirected: their pointer slot or their jump. For the methods the descriptor no longer lists: their saved bytes.
10. Run the callbacks.

Steps 1 to 8 write nothing into the game's code. If any of them reports a failure, the request ends there and the Player keeps running the generation it had. The ack then has `success: false`, the unchanged `appliedGeneration` and one entry per failure:

| `stage` | Reported for |
| --- | --- |
| `resolve` | request, descriptor, hashes, generation order, assembly not loaded or of another MVID, tokens, methods that are generic or have no body, signatures that do not match, a patch assembly that does not load |
| `inlining` | a JIT that inlines |
| `compile` | a method the JIT refuses, a type of the patch that does not load, a static initializer that fails |
| `write` | a method for which there is no way to redirect it (see "When there is no way"); an entry that is not 8-byte aligned or not in a writable and executable mapping; one method listed twice; a body whose first bytes something else changed; a check of the JIT that could not run; an exception during step 9 |

`error` holds the first message, with the count when there are more; at most 32 failures are listed. A request id that was answered before returns the same ack again and runs nothing.

`restartRequired` is true in two cases. An exception interrupted step 9: some methods may be redirected and others not, or a method may be left closed by its gate. Or a static initializer of the game failed while the request compiled (see "Static initializers"): nothing was written, but the game has lost a type.

### The ack

```json
{"schemaVersion":1,"requestId":"request-41","pid":4084366,"success":true,"generation":7,"appliedGeneration":7,
 "restartRequired":false,"error":"","failures":[],
 "detoursApplied":2,"detoursRestored":0,"islandWrites":2,"gatedWrites":0,
 "callbacksInvoked":8,"callbackErrors":3,
 "inliningDisabled":true,"inliningChecked":true,"initializersRun":true,
 "elapsedSeconds":0.0021,"completedUtcUnixMilliseconds":1791879000000}
```

A failure is `{"assembly":"Game","member":"Game.Player::Health","stage":"write","message":"..."}`. Beyond the members of the design contract:

| Member | Meaning |
| --- | --- |
| `islandWrites` | first redirects of this request that go through an island or a relay with no drain: the first byte, or the second instruction, of the body was replaced |
| `gatedWrites` | first redirects of this request written behind the gate, with one 1 ms drain for the whole batch: a whole jump over a seven-byte first instruction, or bytes 1 to 3 of a `sub rsp, imm8` body to reach an island when its fixed address was taken |
| `inliningChecked` | false while the check of the JIT has not given an answer: no request with a detour came yet, or the check could not run. `inliningDisabled` is false then too, and says nothing |
| `initializersRun` | the request made the runtime compile methods the applier had not asked for before. Static initializers that were pending for their types have run, also when `success` is false |

A retarget and a restore count in `detoursApplied` and `detoursRestored` only.

## What a patch assembly must be

- A simple name no loaded assembly has. A reference by name binds to the first assembly loaded under that name, so a second assembly of a used name is refused: `An assembly named X is already loaded`.
- The declaration `SecurityPermission(SecurityAction.RequestMinimum, SkipVerification = true)`. Without it this Mono refuses access to non-public fields when it compiles, but it compiles a call of a non-public method and throws `MethodAccessException` only when the call runs. The applier therefore looks for the declaration in the file and refuses an assembly without it before loading it.
- Replacements of these forms, which keep the native signature of the original:

| Original | Replacement |
| --- | --- |
| static method | static method, same parameters and return type |
| instance method or constructor of a class | static method whose first parameter is the instance (the class or a base of it), then the same parameters; or an instance method of any class |
| instance method or constructor of a struct | instance method of a struct in the patch (a stand-in; `this` is the game's struct), same parameters |

A struct method replaced by a static method that takes the struct by reference is refused: with a result returned through a hidden buffer the two forms pass `this` and the buffer in swapped registers. Generic methods, methods of generic types, static constructors, abstract and extern methods are refused on either side. The applier never asks the runtime for the entry of a generic definition; that aborts the process.

Every method of a patch assembly is compiled before anything is written, not only the replacements.

## Static initializers

This Mono runs the static initializers that are still pending for the types a method involves when it compiles the method: the method's own type, and types whose static fields it reads. A request compiles the replacements, the originals and the rest of each patch assembly, so it runs initializers of the game at patch time, earlier than the game would have, and it has run them when it is refused afterwards. No public API tells whether an initializer is pending, so this cannot be avoided; the ack reports it:

- `initializersRun` is true for every request that compiled a method the applier had not compiled before. A request that is refused before step 6 has it false.
- A request that is refused in step 6 or 7 does not go on to step 8, so it runs no more initializers than its listed methods need.
- A static initializer of the game that throws is final: the runtime fails every later use of the type, in a process that might have initialized it without error later. The failure has stage `compile`, names the type, and the ack has `restartRequired: true`. A failing initializer of a type of the patch itself only refuses the patch.

## The redirect

Both addresses of a redirect come from `RuntimeMethodHandle.GetFunctionPointer()`. The Player thread that applies a patch has no game method on its stack; other threads are not stopped, and one of them may be about to execute any instruction of a body, for as long as the scheduler keeps it waiting. So a first redirect replaces bytes of **one** instruction and never the first byte of the next one. This JIT starts a body in three ways, and each has its own:

| Body starts with | Share | First redirect |
| --- | --- | --- |
| `55 48 8B EC 48` (`push rbp`: a `try` block, `stackalloc`, an argument passed or received on the stack) | 9.1 % | the first byte, to an island |
| `48 83 EC ib` (`sub rsp`, 8-bit) | 89.4 % | the first byte, to an island. Where that island would lie below address zero, which is the case when the second instruction is `mov r32, imm32` (`B8` to `BF`: the method first loads a constant; 1.1 to 3.3 % of all methods, depending on where the JIT's code lies), that second instruction, to a relay. Where the fixed island cannot be mapped (another mapping or the island of another method holds the address), bytes 1 to 3, which lie inside the four-byte first instruction, behind the gate, to an island anywhere in the 16 MiB window whose top displacement byte is byte 4 |
| `48 81 EC id` (`sub rsp`, 32-bit: more than 127 bytes of locals) | 1.5 % | the first byte, to an island. Where the island address is taken (104 of the 864), a whole jump behind the gate |

Shares are of 57,213 methods of `mscorlib`, `System`, `System.Core` and `System.Xml`, compiled by the Player runtime with `-O=-float32,-inline` (2026-10-13). No other first instruction occurred.

### The first byte and an island

Only byte 0 of the body changes, to `E9`. Bytes 1 to 4 are not written: they stay the rest of the old prologue, and after byte 0 they are read as the distance of the jump, which therefore lands at `entry + 5 + int32(bytes 1..4)`. The applier maps an island at exactly that address before it writes the byte:

- `mmap` of the page (two, when the island crosses a page end) with `MAP_FIXED_NOREPLACE | MAP_ANONYMOUS | MAP_PRIVATE`, readable, writable and executable. A returned address other than the requested one is unmapped again and counts as a failure.
- At the address: `FF 25 disp32`, a jump through a pointer slot. The slot is the 8-byte aligned word after those six bytes, or the one before them when the island of a body 16 bytes further on is in the way. The rest of a new page is `int3`.
- The slot holds the entry of the replacement.

One byte is stored atomically and no thread can be inside it. A thread that has not reached the body takes the jump; a thread that is past byte 0 runs the old body, of which no other byte changed. A **retarget** is one aligned 8-byte store into the slot. A **restore** stores the saved byte 0.

The island of a `push rbp` body lies 1.22 GiB above it, that of a `sub rsp, imm8` body between 1.1 GiB below and 1.6 GiB above, that of a `sub rsp, imm32` body 9 to 16 MiB above, inside the JIT's own area. In a running Player, game code is compiled after Unity has mapped its heaps, and the fixed island address of a game method can lie in one of them. Measured over 87,222 methods compiled in a running Player (sample game and engine assemblies, 2026-10-14): the fixed island was taken for 95 of 73,554 `sub rsp, imm8` bodies (0.13 %, all in the managed heap `rw-p [heap]`), for 0 of 8,031 `push rbp` bodies, for 6 of 1,126 `sub rsp, imm32` bodies, and for 20 of 30 bodies of an unrecognised first instruction. The taken addresses are never the applier's own island pages (those are `rwxp`, mapped `MAP_FIXED_NOREPLACE` where no other mapping is, and never overlap). The stage 6 review (`served6-1`) found the first redirect of one `sub rsp, imm8` static method of the sample game refused in about one fresh Player in eight (3 of 25, 5 of about 45 overall) for this reason.

A `sub rsp, imm8` body that does not load a constant is now reached through the gate when its fixed island cannot be mapped (see "A sub rsp, imm8 island through the gate"); that is 73,554 of the 87,222 methods of the running-Player sample, and one that loads a constant takes the relay. Two forms stay refused when their fixed island is taken, with `apply_failed`, stage `write`, and `fullRecompileRequired`: a `push rbp` body, whose second instruction starts at byte 1 so none of bytes 1 to 4 may change; and a body of an unrecognised first instruction, whose length the applier does not know. The address follows from the method's compiled body, so such a method keeps being refused for the life of that Player process; a Player started again compiles it at another address. In the running-Player sample these are the 20 of 30 "other" bodies and none of the `push rbp` bodies, together under 0.03 % of methods. Two islands coincide when two bodies with different prologues lie at the matching distance: 482 of the 56,473 islands of an earlier class-library sample, had all of them been mapped at once.

### The second instruction and a relay

`sub rsp, imm8; mov r32, imm32` puts the island of the first byte 1.0 to 1.15 GiB below the body, and the JIT's code starts at 1 GiB. The `mov` is five bytes, as long as a jump, and a thread is before it or after it. It is replaced whole: `EB FE` on its first two bytes, so that a thread arriving there spins; its last three bytes; then `E9` and the first displacement byte. Both ends are single aligned 2-byte stores and nothing has to be waited for, because no thread can be inside the instruction. The jump leads to a relay, an island at an address of the applier's choice (a page 1.75 GiB above the body, shared by relays while it has room): `add rsp, imm8`, which gives back what the first instruction took, then the jump through a pointer slot. Retarget stores the slot; restore puts the five bytes back the same way.

### A whole jump behind the gate

Only a first instruction of at least five bytes contains a whole `E9 rel32`, and of the three only `sub rsp, imm32` is one. For such a body without an island the jump is written as the design contract describes, each step one aligned 8-byte store that keeps bytes 5 to 7: `EB FE` on bytes 0 and 1; one sleep of 1 ms for the whole batch; bytes 2 to 4; then `E9` and the first displacement byte. Retarget and restore are one store each. The next first redirect of the method tries the island again.

A whole jump over a shorter first instruction ends in the second one. A thread that is about to execute that instruction then continues in the middle of the jump, whenever it is scheduled again: the 1 ms only make that unlikely. This is why a whole jump over bytes 0 to 4 is written only for `sub rsp, imm32`, whose first instruction is seven bytes. Measured on `Hot.Work` (`48 83 EC 08 48 89 3C 24`), three threads calling nothing else, Player runtime: one SIGSEGV in about 11,000 first redirects while other builds ran on the machine, and one in the first run of 500 with 16 busy loops beside it, `at Game.Hot:Work <0x00004>`, the second instruction.

### A `sub rsp, imm8` island through the gate

The first instruction of a `sub rsp, imm8` body is four bytes (`48 83 EC ib`), so bytes 1 to 3 lie inside it and byte 4 is the first byte of the second instruction. When only byte 0 becomes `E9` the island must lie at the one address `entry + 5 + int32(bytes 1..4)`; when that address cannot be mapped (another mapping or the island of another method holds it), bytes 1 to 3 are rewritten instead, behind the same gate the whole jump uses. No thread can be inside bytes 1 to 3 once `EB FE` holds byte 0, and byte 4 stays, so the island may be any free address in the 16 MiB window whose top displacement byte is byte 4:

- the applier scans that window page by page for a free address and maps the island (`FF 25 disp32` and its pointer slot) there;
- the gate is written as for the whole jump: `EB FE` on bytes 0 and 1; one sleep of 1 ms for the whole batch; bytes 2 and 3 (the middle of the displacement, keeping byte 4); then `E9` and the first displacement byte in one aligned store;
- a **restore** puts bytes 0 to 3 back in one aligned store, keeping bytes 4 to 7: the four-byte first instruction flips whole, so no gate is needed, as for the whole jump;
- a **retarget** stores the pointer slot, like any island; a later first redirect of a restored method finds the same window island, which is never unmapped.

This counts in `gatedWrites`. The window is 16 MiB wide whatever byte 4 is, so a free address is found unless the whole window is taken. Of 73,554 `sub rsp, imm8` bodies in a running Player, 95 had a taken fixed island and all were reached this way. On real Players (island6, the e13 driver, 2026-10-14) one method of the sample game, whose window lies about 0.9 GiB below its body, found its whole window taken in 1 of 80 fresh Players, and in 2 of 40 with its fixed island forced taken. On probe game code of 23 prologue shapes (islandplayers6), a request that edits all of them was refused in 3 of 40 fresh Players of the sample and in 3 of 32 Netcode host and client Players, against 5 of 40 of the sample before the window island. Every refusal was a `sub rsp, imm8` body whose second instruction stores to the stack (byte 4 `C7`, `F2` or `F3`), whose island lands in the managed heap, and whose whole window was taken; in a survey of 60 such Players (24,000 probe methods) no `push rbp`, loads-constant or other `sub rsp, imm8` body had a taken island. These are probe shapes, not a measure of typical games.

### When there is no way

A method fails with stage `write`, before anything is written, when none of the ways applies: the island address of its first byte is taken, lies outside the address space or overlaps the island of another method, and the body is a `push rbp` body (byte 1 begins the second instruction) or a body whose first instruction the applier does not recognise (so its length is unknown); or the body is `sub rsp, imm8` and no free address was found in its whole 16 MiB window; or no relay page can be mapped for a body that loads a constant; or the replacement is out of reach of the 32-bit jump of the gated write. The request is refused as a whole and the runner takes its fallback. In a running Player this is the `push rbp` and unrecognised-first-instruction bodies whose fixed island is taken (20 of 30 such "other" bodies and none of the `push rbp` bodies in the sample, together under 0.03 % of methods); a `sub rsp, imm8` body whose whole window is taken (see above); and, of the 482 coinciding islands of an earlier sample, a pair whose two methods are both patched in one Player when the second to be mapped is a `push rbp` body or one of an unrecognised first instruction (the other forms take a relay, a window island or the whole jump).

### What is left for other threads

Nothing known for the three stores themselves. Measured on 2026-10-13 (AMD Ryzen 7 5700X3D, 16 hardware threads) with three threads that do nothing but call the method while the main thread redirects it for the first time, retargets and restores it, without a fault and without a result other than those of the three bodies:

| Where | Method and way | Runs × first redirects | Calls of the three threads |
| --- | --- | --- | --- |
| Player runtime (`libmonobdwgc-2.0.so` in the embedding host) | `Hot.Guarded`, `push rbp`: first byte and island | 10 × 200 | 0.87 billion |
| | `Hot.Flat`, loads a constant: second instruction and relay | 10 × 200 | 1.31 billion |
| | `Hot.Large`, `sub rsp, imm32`, island address taken: whole jump behind the gate | 10 × 200 | 0.87 billion |
| | `Hot.Work`, `sub rsp, imm8`, fixed island address taken: bytes 1 to 3 behind the gate, window island | 10 × 200 | comparable |
| the same, with 16 busy loops on the 16 hardware threads | the four above | 5 × 500 each | 1.70, 2.00, 1.39 billion (and comparable) |
| the same, the reviewer's reproduction of the stage 1 crash (cycles of first redirect and restore) | `Shapes.CallsSeven`, `push rbp`: first byte and island | 10 × 3000 | 131 billion |
| standalone Mono | the four above | 10 × 200 each | 1.22, 1.38, 1.57 billion (and comparable) |
| the stock `LinuxPlayer` process, without game data | the three above | 1 × 500 each | 0.19, 0.23 and 0.18 billion |

Every run of the first four rows also retargets and restores the method once per first redirect. The stage 1 write crashed in each of three runs of the reviewer's reproduction, before 250 first redirects. `Hot.Work` is the `sub rsp, imm8` body the stage 1 whole jump crashed on; the gated island avoids that by keeping byte 4.

What a redirect does not do:

- A frame that is on a stack while its method is redirected finishes the old body. A running coroutine or `async` method keeps the body it started with.
- A thread inside an island or a relay is at an address the runtime has no method for. A stack walk of that thread from outside (a sampling profiler) loses frames for those one or two instructions.
- An island is never unmapped, because a thread may be inside it at any time; a later first redirect of the same method uses it again. Pages are unmapped in one case only: an attempt that failed returns the pages it mapped itself, to which no jump was ever written.

Mean time inside the applier for a request with one detour, under the three calling threads on the Player runtime: 0.76 ms for a first redirect through an island or a relay, 1.84 ms behind the gate (1 ms of it the drain), 0.57 ms for a retarget, 0.23 ms for a restore. Nearly all of it is the request itself: reading and hashing the descriptor and the patch assembly, the callbacks. A request with 590 first redirects of methods that were never compiled took 26 ms in the stock Player process, compiling included.

## Inlining

A JIT that inlines copies small methods (IL under 20 bytes) and `AggressiveInlining` methods into their callers, also into callers it compiles later. A redirect does not reach those copies. With the first request that lists a detour the applier redirects three probe methods of its own and lets the JIT compile their callers: when any of them still runs its original body, every descriptor with a detour is refused with stage `inlining`. The runner starts the Player with `MONO_ARGUMENTS=-O=-float32,-inline`; `MONO_INLINELIMIT=0` is not enough, forced inlining remains.

The ack reports the answer as `inliningDisabled`, and with `inliningChecked` whether there is one. When the probes cannot be redirected (their code is not in a writable and executable mapping, the process map cannot be read, the platform is not supported), nothing is known about the JIT: the request is refused with stage `write` and the message `The inlining check could not run`, `inliningChecked` stays false, and the next request with a detour checks again. Only an answer is kept for the life of the process.

## Added fields

```csharp
namespace UnityMicroEditor.HotReload
{
    public static class FieldStore
    {
        public static ref T Instance<T>(object owner, string id);
        public static ref T Static<T>(string id);
    }
}
```

`id` is `<assembly>|<type id>|<field>`, a string literal of the patch: literals are interned, and the lookup compares references. An id built at run time names the same slot through a slower path. A new owner, and a static field, start with `default(T)`. Owners are weak keys: an object and the values of its added fields are collected together, also when a value refers back to the object. The same id used with another `T` is another slot, so a field that changed its type starts from the default and code of an older patch keeps its own value. Values outlive generations; a field that is removed and added again has its old value.

Reads take no lock. Measured on 2026-10-12 (AMD Ryzen 7 5700X3D, this Mono): 60 to 78 ns per access of an instance field, almost all of it the weak table.

## Callbacks

The loaded game assemblies named by `gameAssemblies` (and the patched ones) are scanned once each for methods with an attribute whose simple name is `InvokeOnHotReload`, `OnHotReload` or `InvokeOnHotReloadLocal`, with or without `Attribute`, from any namespace. Measured on the class library: about 3.6 µs per method, 87 ms for 23,896 methods, once per assembly at the first applied patch.

- `InvokeOnHotReload` and `OnHotReload`: after every applied request that changed a body the Player runs: a first redirect, a replacement that differs from the one before, a restore, a removed message. A request whose replacements are all the same code as those of the generation before (a comment or a blank line was saved) runs no callback. A static method runs once; an instance method runs on every object `Object.FindObjectsByType(type, FindObjectsInactive.Include)` returns. The method takes no parameter or one `List<UnityMicroEditor.MethodPatch>`.
- `InvokeOnHotReloadLocal`: the parameterless method itself is invoked, through the original and therefore with its new body, when its body changed with this generation or was restored. On `Awake`, `OnEnable`, `OnDisable` and `OnDestroy` of an engine object the attribute stands for the helper as well: the lifecycle rewrite of the compiler keeps the attributes on the entry the engine calls and moves the body into `__UnityPlay_<name>`, which is what a patch replaces. When the helper changed, the entry is invoked, as the engine invokes it: an object whose class overrides the callback runs its own, like any virtual method with the attribute. `MethodPatch.originalMethod` names the helper. Only those four callbacks of an engine object have a helper: a method that is merely named like one is no callback, and the helper is the name with the most trailing underscores, which the rewrite appends until the name is free.
- The list holds one entry per method whose body changed with this generation: first redirects (`previousMethod` is the original), methods whose replacement differs from the one of the generation before, and restored methods (`newMethod` is the original again). A method that an earlier generation replaced with the same body is not listed, and a removed message redirected to an empty method never is. "The same body" is decided on the IL: the same instructions naming the same members. Members of the two patch assemblies match by name; their types may differ in one segment of the full name, the same one throughout, because the builder names the types of a patch after its generation. What only the patches define is part of the body that names it and is compared by its own code too, through every level: a lambda or local function, a method the patch added, and every method of a state machine (an iterator or `async` method names only the constructor and the fields of its state machine, never `MoveNext`). So a method counts as changed when its lambda, its added helper or its coroutine body was edited, although its own instructions are the same; a method of the game it calls does not make it changed, since that one is redirected and listed on its own. Anything undecidable counts as changed, which includes generated types whose ordinal moved because a member was inserted before them. A method of the patch that many redirected methods name is compared once for a request: what one comparison found the same is not compared again for the next (300 redirected methods that reach 100 added ones took 4.2 s before and 0.05 s since, 2026-10-13).
- An instance callback is not invoked on an object that an earlier callback of the same request destroyed at once (`DestroyImmediate`). An object passed to `Destroy` exists until the end of the frame and is still called.
- An exception is passed to `Debug.LogException` and counted in `callbackErrors`. So is a method that cannot be called (another parameter, a generic method, an instance method of a type that is not a `UnityEngine.Object`), with one warning `callback-skipped` per method and Player. Neither fails the patch.

## Limits

- An attribute on a method that a patch added is not seen: only the loaded assemblies are scanned. Methods a patch added are not in the list of patches.
- Callbacks that take the `MethodPatch` type of another tool are skipped.
- One applier per Player process. The bridge answers a request that names another applier hash with `restartRequired: true` and one failure of stage `applier`; only a new Player takes another applier.
- The check for the `SkipVerification` declaration finds its encoded named argument in the file; it does not read the declarative security table.
- The harness runs on the standalone Mono of the Unity installation, on the Player's `libmonobdwgc-2.0.so` through an embedding host, and inside the stock `LinuxPlayer` process without game data (a preloaded shim runs it there once the Player has started Mono), always with stand-ins for the engine.
- In a Player with a game the applier ran in watched sessions on copies of `Samples/RawProject` with added scripts (2026-10-13): patches of the patch builder for four bodies written for the kinds above in one request (two that return a constant, one with `try`/`finally`, one with 160 bytes of locals; the ack had `islandWrites: 4` and `gatedWrites: 0`, their entry bytes were not read), their restore and a second first redirect; struct, static and instance methods, accessors, constructors, lambdas and coroutines; added fields through `FieldStore`; static, instance and `Local` callbacks on live objects, one that throws and one that takes another tool's `MethodPatch`; seven first redirects and six restores of a method that a thread called in a loop; 54 generations in one process; two patched assemblies; and the refusal with stage `inlining` in a Player started with `MONO_ARGUMENTS=-O=-float32`. A `sub rsp, imm8` method whose fixed island address a running Player had taken was reached through the gate (island6, the e13 driver: with the fixed island forced occupied in every Player, 38 of 40 fresh Players took the edit through a window island where the unfixed applier refused all 40; naturally 79 of 80 fresh Players took it, the one refusal a full window). Not exercised there: a Player with the debugger agent.

## Checks

```bash
# the bridge both ways and this directory, against a prepared host
tests/HotReloadRuntimeTests/check-compile.sh <host>/<Name>_Data/Managed

# the applier and the bridge's CodeReload.cs on Unity's Mono, and on the Player's runtime
tests/HotReloadRuntimeTests/run.sh
tests/HotReloadRuntimeTests/run.sh --host Temp/hotreload/mono-probe/out/host --stress 10
```
