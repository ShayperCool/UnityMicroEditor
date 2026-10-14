# C# hot reload

A watched session applies a saved C# edit to the running Player, without a restart, when that can be done exactly. The game keeps its process, its scene and the state of every object and static field. An edit that cannot be applied exactly is refused with its reasons, and only a restart of the Player puts it into effect.

This guide is for a person who runs a game with `unity-micro-editor run --watch` and for an agent that drives a session through `unity-micro-editor mcp`. The contracts it summarizes are in [RUNNER.md](RUNNER.md#hot-reload-of-c) (the runner's events and commands), [AGENT_MCP.md](AGENT_MCP.md#hot-reload) (the replies of the MCP tools), [tools/HotReloadPatcher](../tools/HotReloadPatcher/README.md) (which edits are patched) and [tools/HotReloadRuntime](../tools/HotReloadRuntime/README.md) (how the Player applies a patch).

## In one screen

Hot reload works in a watched session (`run --watch`, or a session of an `mcp` server) on the verified profile: Unity `6000.6.3f1`, a Mono development Player on Linux x86-64. After every save of a `.cs` file the runner compiles the changed assemblies and either patches the running Player or says why it cannot.

**Applied live:**

- a changed body of a method of a non-generic type that is not itself generic: instance, static and struct methods, virtual and override bodies, property, indexer and event accessors, operators, instance constructors and field initializers, lambdas, local functions, iterators (coroutines) and `async` methods;
- added non-virtual methods, properties, events and indexers; a changed name, signature or `static` of a non-virtual method (a removed and an added method, and its callers are patched with it); removed methods (a removed `void` Unity message such as `Update` stops running);
- added instance fields of classes, added static fields without an initializer, removed fields of classes, changed constants and default parameter values;
- the same in an assembly that uses Netcode for GameObjects, the body of an existing RPC included.

Some live edits leave something out until a restart: a `Start` that already ran does not run again, a running coroutine finishes its old body, objects that existed read the default of an added field. The reply says so with a **note** ([the list](#patched-with-a-note)).

**Refused:** added, removed or renamed types (a new script is a new type), anything in generic code, added virtual or override methods, added Unity messages (`Update`, `OnCollisionEnter`, ...), added constructors, static constructors and static field initializers, attribute changes of existing members, fields added to or removed from a struct, a field whose accessibility makes Unity start or stop serializing it, and what Netcode registers when the Player starts. Every refused member is named with a reason code ([the list](#refused)). Nothing is approximated.

**What a refusal costs.** The Player keeps running the code it ran before the edit. Scene and asset edits keep working on it. Every later C# edit waits with the refused one, because a patch holds every difference from the code the Player loaded: revert the edit to go on patching, or recompile. A recompile restarts the Player process: the scene is loaded again, static fields start over and game state is lost. It took 4.3 to 4.8 s on the sample project ([Measured](#measured)).

**How to get the full recompile:**

| Where | How |
| --- | --- |
| MCP | `hot_reload` with `{"action": "recompile"}`. The error `full_recompile_required` names this call in `details.next`. |
| MCP, without asking | `session_control(start)` or `reset` with `arguments.onUnsupportedEdit: "restart"`: the runner restarts the Player by itself for an edit it cannot patch, and the reply of the wait says so. |
| `run` | `--hot-reload-fallback restart`, the default: the runner restarts the Player by itself (`code-updated` with `reason: "hot_reload_unsupported"`). With `--hot-reload-fallback report --commands-stdin` it keeps the Player and waits for `{"command":"recompile","requestId":"r1"}` on its standard input. |
| No hot reload at all | `run --watch --no-hot-reload`, or `arguments.hotReload: false` in MCP: every C# edit restarts the Player, as before hot reload existed. |

**The promise.** No reply says, or leaves you to believe, that an edit runs in the Player when it does not. Whenever a restart is needed for the saved code to take effect, the reply says so: a refused edit fails the MCP waits with `full_recompile_required` (in the runner: `update-rejected` with `fullRecompileRequired: true`), and an applied edit with a limit starts the MCP reply with `attention` (in the runner: `newNotes` on the event). The other side of the promise: an edit that can be applied exactly is applied, because every refusal costs you a restart and the game state.

## Compared with Hot Reload for Unity and other tools

Hot Reload for Unity ([hotreload.net](https://hotreload.net), version 1.13.25) is the product this feature follows; the comparison below comes from its documentation, changelog and public issue tracker as read on 2026-10-12, not from running it.

| | Hot Reload for Unity | UnityMicroEditor |
| --- | --- | --- |
| Where it runs | In the Unity Editor (Edit and Play Mode) and in Mono development builds connected to an Editor | In a Mono development Player on Linux x86-64, without an Editor |
| What is compiled | Only the changed methods, by a server with its own Roslyn; edits are classified at source level by the compiler's Edit-and-Continue rules | Each changed assembly whole, by the SDK's Roslyn in a compiler server the session keeps; the patch builder compares the compiled assembly with the one the Player loaded |
| Classification | Its changelog carries a long series of edits that were patched and should have been refused | Patched only when the builder proves it; everything else is refused with a code |
| Application | Method by method, without rollback | All or nothing per edit: everything is resolved and compiled by the JIT before the first byte is written |
| Redirect | A native detour per method (MonoMod/Harmony) | One instruction at the method's compiled entry is replaced; other threads keep running ([how](#redirecting-a-method)) |
| Private members | Sets a flag inside Mono's own method structure | A security declaration in the patch assembly; no Mono internals |
| JIT inlining | Detected, then "needs a recompile"; advice to use Debug mode | Switched off for the Player of a session, at a cost in speed ([Inlining](#inlining-is-off)) |
| An unsupported edit | A warning; "Auto recompile unsupported changes" (on by default for new users) recompiles, leaving and re-entering Play Mode | `full_recompile_required` with every reason and the call to make, or a restart by the runner when asked for |
| Added Unity message (`Update`) | Supported, through proxy components | Refused (`unity_message_added`) |
| Generic code | Supported, methods of generic classes since 1.13.0 | Refused |
| Added fields | Side table; initializers run on first access | Side table; objects that existed read the default (a note) |
| New types | Refused | Refused |
| Callbacks | `[InvokeOnHotReload]`, `[InvokeOnHotReloadLocal]`, `MethodPatch` | The same names and members; attributes are matched by simple name in any namespace, so a method marked with the product's attribute runs too |
| Code of IL weavers | Not hot reloaded (Mirror, Fish-Net, Photon); an NGO RPC was reported to stop working after a patch | The post-processed images are compared; for Netcode for GameObjects RPC bodies are patched and what it registers is refused |
| Speed | "Less than 300 milliseconds" claimed for most projects; 0.7 to 1.6 s in users' logs | 0.33 s median from save to answer for a body edit of the sample ([Measured](#measured)) |

Other ways to do it, and why they were not taken:

- **Edit and Continue** (`MetadataUpdater.ApplyUpdate`, as in Rider, Visual Studio and `dotnet watch`): the code is in Unity's Mono, compiled out. Not available.
- **A renamed copy of the changed file plus a detour** (FastScriptReload): static state lives in the copy and is lost, and `this` has the wrong type. A patch here never defines a second copy of a type the game has.
- **A patch of the changed files retargeted to the original assembly plus a detour** (UnityScriptHotReload): the closest design. Here the whole assembly is compiled, so nothing has to be retargeted.
- **A dispatch check woven into every method beforehand** (Unity's experimental Code Reload, InjectFix): immune to inlining, but a check in every call and a rewrite of every assembly at every compile.
- **Swapping assemblies** (a domain reload, a restart): loses the state. It is what the recompile does.

## How it works

1. **Compile.** The watcher waits 35 ms for more changes of a save, then the runner compiles the assemblies whose sources changed, as a session without hot reload does: the lifecycle rewrite, the SDK's Roslyn, and the Netcode post-processor for an assembly that uses Netcode. A compile error leaves the Player as it is.
2. **Compare.** The patch builder (`tools/HotReloadPatcher`) compares each changed assembly with the one the Player **loaded at its start**, not with the previous patch. Methods are compared by what their tokens mean, and the members the compiler generates for lambdas, closures and state machines by their structure, so a comment, a moved member or a renumbered lambda is no change. Every difference is classified: patched, patched with a note, or refused. One refusal refuses the whole edit.
3. **Patch assembly.** For each changed assembly the builder writes `<name>.HotReload.<generation>.dll` with its PDB: the new bodies (an instance method of a class becomes a static method with the instance as first parameter), added methods, accessors of added fields, and a security declaration that lets it reach private members. It reads the assembly back and verifies it before it is used.
4. **Apply.** The runner writes a request file into the run directory of the Player. The bridge in the Player picks it up between two frames, on the main thread, and hands it to the applier (`tools/HotReloadRuntime`, compiled for each session). The applier resolves every method, loads the patch, has the JIT compile every replacement and original, decides how each method is redirected, and only then writes. If anything fails before the write, nothing is written and the Player keeps the code it had. Then it runs the game's callbacks and answers. The runner checks the answer (Player process, generation, number of redirects) and prints `code-patched`.

The next edit is compared with the loaded assemblies again, and its patch replaces the one before it; a method the new patch no longer lists is restored. Reverting an edit therefore restores the loaded code.

### Redirecting a method

A redirect overwrites the start of the method's compiled code. Other threads are not stopped and may be about to run any instruction of it, so a first redirect replaces the bytes of **one** instruction only. Usually that is the first byte: it becomes a jump whose distance is read from the four bytes after it, and the applier maps a small page, an *island*, at exactly the address the jump reaches; the island jumps on through a pointer slot to the new body. When that address cannot be used, a body that loads a constant after its first instruction has its second instruction replaced instead, by a jump to a *relay*, and other bodies have their first instruction rewritten behind a *gate*: two bytes that make an arriving thread spin, one wait of 1 ms for the threads inside, then the rest. For the most common body, which starts with `sub rsp, imm8` (84 % of the methods measured in a running Player), the gate lets the island lie anywhere in a 16 MiB window instead of at one fixed address. Retargeting a redirected method later is a single store, usually into its pointer slot, and restoring it puts the saved bytes back. A method that has no way left is refused before anything is written ([limits](#limits-and-known-problems)). [The applier's README](../tools/HotReloadRuntime/README.md#the-redirect) has the forms, the stores, and the stress tests under three calling threads.

### Added fields

A field the loaded type does not have lives in a side table of the applier (`FieldStore`), keyed weakly by the object, and the patch reaches it through accessors. Objects that existed before the patch read the field's default value, also when the source gives it an initializer (note `existing_instances_keep_default`). A static field starts at its default. An access took 60 to 78 ns ([Added fields](../tools/HotReloadRuntime/README.md#added-fields)).

### Callbacks

After a patch that changed what the Player runs, the applier runs the game's `[InvokeOnHotReload]` and `[InvokeOnHotReloadLocal]` methods on the main thread ([API](#api-for-game-code)). Their exceptions are logged and counted, and do not fail the patch (note `callback_failed`).

### Inlining is off

A JIT that inlines copies small methods into their callers, and a redirect does not reach those copies. The runner therefore starts the Player of a hot reload session with `MONO_ARGUMENTS=-O=-float32,-inline` (a value the variable already has is passed on), and the applier checks with probe methods of its own that the JIT does not inline; when it does, every patch is refused with `apply_failed` and the stage `inlining`. The cost: script code that mostly calls small methods runs about half as fast. 200 million calls of a one-line static method took 365 ms against 179 ms, and 20 million `Vector3` additions and scalings 582 ms against 249 ms (2026-10-13). Engine code is not affected. A session with `--no-hot-reload` or `hotReload: false` keeps inlining on.

### The kept processes and the one-shot fallback

A watched session keeps three .NET processes between edits: the Roslyn compiler server, the lifecycle helper and the patch builder. A request one of them does not answer in time, or answers wrongly, runs once in a process of its own, whose result counts; after three failures in a row the session runs every request that way. A process above 1 GiB of resident memory is replaced before its next request. `UME_COMPILER_COLD=1` in the runner's environment runs a session without them. [RUNNER.md](RUNNER.md#processes-a-watched-session-keeps) has the details.

## Edits and what the reply says

The patch builder decides alone; the tables below follow its code table (`dotnet HotReloadPatcher.dll --codes`), the runner and the controller. A reason or note entry has `code`, `member` and `message`; the message names the member and what happens to it.

### Patched

| Edit | Reply |
| --- | --- |
| Body of a method, accessor, operator or finalizer of a non-generic type | Patched. A frame that is on a stack finishes its old body; the next call runs the new one. |
| Body of an instance constructor, an instance field initializer | Patched, note `constructor_applies_to_new_objects`. |
| An edited lambda or local function | Patched; delegates made before the patch run the new body when the old and the new lambda are proven to be the same one, otherwise note `lambda_not_remapped`. |
| A changed iterator (coroutine) or `async` method, one removed or renamed, or one made a plain method | Patched; note `running_coroutines_keep_old_code` for the coroutines and calls that run. A plain method made an iterator or `async` is patched without it. |
| `Start`, `Awake`, `OnEnable`, `OnAfterDeserialize` (once-run engine callbacks), and a method only they call | Patched, note `started_objects_keep_state`. |
| A `[RuntimeInitializeOnLoadMethod]` method, or a method only it or a static constructor calls; one added or removed | Patched, note `runtime_initializer_changed`, `runtime_initializer_added` or `runtime_initializer_removed`. |
| Added method, property, event or indexer (not virtual) | Patched. An added method whose name the game passes to a call by name has `added_method_named_by_string`; otherwise an added instance method of an engine object has `added_method_computed_name` or `added_method_invisible_by_name`. An attribute on an added member has `added_member_attributes_ignored`. |
| Name, signature or `static` of a non-virtual method changed | Patched as a removed and an added method; every caller in the assembly changed too and is patched. |
| Removed method | No redirect, except a removed `void` Unity message of an existing class, which is redirected to an empty method and stops running. |
| Added instance field of a class, added static field without an initializer | Patched through the side table. Notes `existing_instances_keep_default`, `serialized_field_not_restored` (a field Unity would serialize), `field_value_reset` (a field whose type or `static` changed). |
| Removed field (an instance field of a struct, or of a class with sequential or explicit layout, is refused: `struct_layout_changed`) | Nothing to do; no new code reads it. |
| Constant, default parameter value | Patched: the bodies that use it changed, also in other files and assemblies. |
| Accessibility of a method, or of a field Unity does not serialize differently | Nothing to patch; note `accessibility_changed`. |
| Comment, blank line, moved member | No change. While a patch is applied, a new generation with the same bodies; no callback runs for it. |
| A method body, an RPC body, `OnNetworkSpawn` in an assembly that uses Netcode for GameObjects | Patched as in any assembly; spawn callbacks have `spawned_objects_keep_state` ([Netcode](#netcode-for-gameobjects)). |

### Patched with a note

A note says what of an applied edit does not reach what exists until the Player restarts. The column "attention" is how the MCP reply treats it: **held** notes make the `attention` sentence and offer the recompile, **told** notes make the sentence without the offer (a restart would not undo them), **listed** notes are in `hotReload.newNotes` only.

| Code | What it means | What to do | Attention |
| --- | --- | --- | --- |
| `runtime_initializer_changed` | A `[RuntimeInitializeOnLoadMethod]` method, or one only such methods or a static constructor call, changed; it ran at the start, or when its type was first used, and does not run again. | Recompile when the new body must have run. | held |
| `runtime_initializer_added` | A `[RuntimeInitializeOnLoadMethod]` method was added; it runs at the next start. | Recompile. | held |
| `runtime_initializer_removed` | One was removed or renamed; what it did at this start stays. | Recompile. | held |
| `added_method_named_by_string` | Code of the game passes the name of an added method to a call by name (`Invoke`, `SendMessage`, `StartCoroutine(string)`, `Type.GetMethod`, ...), or a scene, prefab, asset or animation names it in a UnityEvent or animation event. Such a call looks in the loaded class and does nothing. | Recompile. | held |
| `added_method_computed_name` | An instance method with at most one parameter was added to an engine object while the game (or a package that sends messages, such as an input component's `On<Action>`) calls methods by a name it computes. If such a call means the added method, it does nothing. | Recompile if it does. | held |
| `serialized_field_not_restored` | An added field Unity would serialize is not filled from scenes, prefabs or assets; a later scene edit that sets it is applied without the value. A value saved under a `[FormerlySerializedAs]` name stays unread also after a restart: the session's loader does not apply the attribute. | Recompile to load the saved value. | held |
| `running_coroutines_keep_old_code` | Coroutines, iterators and `async` calls that run finish on the old code, also when the method was removed or renamed. | Start the coroutine again, or recompile. | held |
| `started_objects_keep_state` | `Start`, `Awake`, `OnEnable`, `OnAfterDeserialize`, or a method only they call, does not run again for objects that already ran it. What an old `OnEnable` subscribed stays subscribed. | Create the object again, disable and enable it, or recompile. | held |
| `spawned_objects_keep_state` | An `OnNetworkSpawn` or other Netcode spawn callback, or a method only they call, does not run again for the spawned objects. | Spawn again, or recompile (which leaves the network session). | held |
| `lambda_not_remapped` | Delegates made before the patch keep the old body of a lambda or local function that cannot be proven to be the same one. | Create the delegate again (an `[InvokeOnHotReloadLocal]` method is a place for that), or recompile. | held |
| `earlier_patch_delegates_keep_old_code` | A delegate, coroutine or `async` call that an earlier patch created keeps running that patch's code. | Create it again, or recompile. | held |
| `constructor_applies_to_new_objects` | A constructor, a field initializer, or a method only they call changed; objects that exist keep what it gave them. | Create new objects, or recompile. | held |
| `existing_instances_keep_default` | Objects that exist read the default of an added field that an initializer or a constructor sets, or that replaces a removed field of the same type (a rename does not carry the value over). | Set it from a callback, or recompile. | held |
| `added_method_delegate_identity` | Patched code makes a delegate of an added method the earlier patch had too; `-=` does not remove a handler subscribed while the earlier patch was applied. | Recompile if the handler must go. | held |
| `added_member_attributes_ignored` | An attribute on an added member is invisible to reflection: an added `[AgentTool]` method is not in the tool catalog. | Recompile. | held |
| `callback_failed` | An `[InvokeOnHotReload]` method of the game threw or could not be called; the patch is applied. From the runner. | Fix the callback. | told |
| `field_value_reset` | A field whose type or `static` changed is stored anew; its value is lost. A restart does not bring it back. | Set the value again. | told |
| `accessibility_changed` | Reflection sees the loaded accessibility of a member. | Recompile if reflection must see it. | listed |
| `added_method_invisible_by_name` | An added instance method of an engine object cannot be found by name (`SendMessage`, a UnityEvent, an animation event); no code of the game names it. Calls in code reach it. | Recompile before naming it in data. | listed |

The notes of a patch are those of every edit since the Player started, because a patch holds all of them; what a save brought is listed apart (`newNotes`). A held note also says that a process of the same game started on the saved code meanwhile runs all of it, so the two may not agree until this Player restarts too.

### Refused

A refusal names every refused member, at most 16 in the reply with the count and the report file that has all of them. The answer is always the same: recompile, or revert the edit to keep the Player and its state. The table says what each code means.

| Code | What was edited |
| --- | --- |
| `type_added` | A type (class, struct, enum, interface, delegate) was added or renamed. A new script is one; a scene that uses its component is rejected by the running Player until the recompile, which applies both. |
| `type_removed` | A type was removed or renamed; the loaded type stays registered. |
| `type_flags_changed` | Visibility, `abstract`, `sealed`, `[Serializable]` or layout kind of a type. |
| `base_type_changed` | The base type of a type. |
| `interfaces_changed` | The interface list of a type. |
| `interface_changed` | A member of an interface. |
| `type_layout_changed` | Explicit layout of a type (packing, size, field offsets). |
| `struct_layout_changed` | An instance field of a struct, or of a class with sequential or explicit layout, added, removed, retyped or reordered. |
| `enum_changed` | The members or the underlying type of an enum. |
| `generic_type` | Anything in a generic type, also a lambda or local function of one: its code is shared between instantiations. |
| `generic_method` | Anything in a generic method, also in its lambdas and local functions. |
| `generic_generated_member` | Patched code contains a generic lambda or local function. |
| `virtual_method_added` | A virtual, abstract, override or interface-implementing method was added: the method table of a loaded class is fixed. |
| `virtual_method_removed` | Such a method was removed. |
| `method_flags_changed` | `virtual`, `abstract`, `sealed` and the other flags of an existing method. |
| `method_overrides_changed` | The explicit interface implementations of a type. |
| `constructor_added` | A constructor was added, or the signature of one changed. |
| `static_constructor_changed` | A static constructor or a static field initializer was added, changed or removed: it ran once. |
| `static_readonly_removed` | `readonly` was removed from a static field, whose value the JIT may have compiled in as a constant. |
| `unity_message_added` | A Unity message or lifecycle callback (`Update`, `OnEnable`, `OnCollisionEnter`, ...) was added to an existing class: the engine finds a class's messages once. |
| `unity_message_removed` | A removed Unity message that cannot be switched off safely: it returns a value, a base class declares it, the class is generic, or it is `OnAnimatorMove`, `OnRenderImage`, `OnAudioFilterRead` or `OnGUI`. |
| `custom_attributes_changed` | An attribute of an existing assembly, type, member or parameter was added, removed or changed (`[SerializeField]` included); they are read through reflection when the game starts. |
| `serialization_changed` | The accessibility or `readonly` of an existing field changed so that Unity starts or stops serializing it; the Player reads scenes with the loaded fields. A field of a type Unity never serializes is a note instead. |
| `field_flags_changed` | Other flags of an existing field (`[NonSerialized]`, marshalling). |
| `special_field_added` | An added field the side table cannot hold: thread-static, volatile, a pointer or by-ref type, marshalled, mapped data. |
| `extern_method_added` | A method without IL body (`extern`, P/Invoke, internal call) was added. |
| `extern_method_changed` | The import or marshalling of such a method changed. |
| `assembly_identity_changed` | The name, version, culture or key of the assembly. |
| `cross_assembly_addition` | Patched code uses a type or member that the same change added to another game assembly. |
| `generated_type_not_loaded` | Patched code needs an anonymous type, or a generic compiler-generated type, the loaded assembly lacks. |
| `unsupported_instruction` | Patched code uses an added member in a way a patch cannot express (a reflection handle, a delegate of an added struct method, a volatile access). |
| `emit_not_supported` | The builder cannot prove a patch correct for this member (a synchronized added method, a variable argument list). |
| `netcode_rpc_changed` | An RPC added, removed or renamed, or its parameters, attribute, send or receive code. |
| `netcode_variable_changed` | A network variable of a NetworkBehaviour added, removed, renamed, retyped or reordered, or its read permission where it is the argument of its construction. |
| `netcode_serialization_changed` | The value types Netcode registers serializers for. |
| `netcode_generated_code_changed` | Other start-up code the Netcode post-processor generates. |
| `netcode_assembly` | An assembly that uses Netcode in a form the builder does not know (another package version), or the Netcode runtime itself: no edit of it is patched. |
| `unowned_generated_code` | Generated code differs and no method is known to own it, so the edit cannot be compared. A defect of the builder if it happens; no known source produces it. |
| `emit_verification_failed` | The emitted patch failed its own verification. A defect of the builder; report it with the report file. |
| `internal_error` | The builder failed on the assembly. A defect of the builder; report it with the report file. |

The runner adds three reasons of its own:

| Code | What happened | What to do |
| --- | --- | --- |
| `patch_builder_failed` | The builder failed, did not finish within 60 s, or answered unusably. | Recompile; if it repeats, report it with the runner's log. |
| `apply_failed` | The Player refused the patch and changed nothing. The message begins with the applier's stage: `resolve`, `inlining`, `compile` or `write` (or is the bridge's message). | Recompile. For `inlining`, remove a `MONO_ARGUMENTS` from the runner's environment that leaves inlining on. For `write`, see the [limits](#limits-and-known-problems). |
| `assembly_not_loaded` | The compiler graph has an assembly the running Player did not load. | Recompile. |

When the Player says its code is in an unknown state (an exception while writing, a game static initializer that failed while the patch was compiled), exits during a patch or does not answer within 20 s, the runner restarts it on the newest code in either fallback mode: `code-updated` with `reason: "hot_reload_failed"`, and in MCP `hotReload.lastRestart` and an `attention` that says the game state was lost.

A change of an `.asmdef`, `.asmref`, package or project setting, a deleted script file, or a new script in a package of the project is no hot reload either: it needs a new preparation ([below](#when-the-source-graph-changes)).

## API for game code

The runtime package declares three types in the namespace `UnityMicroEditor` ([HotReloadAttributes.cs](../Packages/com.unitymicroeditor.runtime/Runtime/HotReloadAttributes.cs)). The applier matches the attributes by simple name, `InvokeOnHotReload`, `OnHotReload` or `InvokeOnHotReloadLocal`, with or without `Attribute`, in any namespace: a method marked with Hot Reload for Unity's own attribute runs as well. Callbacks run on the main thread after the redirects are written, and only for a patch that changed what the Player runs (not for a comment saved while a patch is applied).

The examples are from the script that the real-Player check runs (each callback ran and logged in the stage 6 sessions of 2026-10-14); `tests/HotReloadRuntimeTests` covers the same cases on Unity's Mono and the Player's runtime.

**`[InvokeOnHotReload]`** runs after every applied patch. A static method runs once; an instance method of a `UnityEngine.Object` type runs on every live object of its type, inactive ones included. The method takes no parameter or a `List<MethodPatch>`.

```csharp
using System.Collections.Generic;
using UnityEngine;
using UnityMicroEditor;

public sealed class HrCallbacks : MonoBehaviour
{
    private static int staticCalls;

    [InvokeOnHotReload]
    private static void OnReloadStatic()
    {
        staticCalls++;
        Debug.Log("[hr-smoke] callback static calls=" + staticCalls);
    }
}
```

**`MethodPatch`** describes one method whose running body the patch changed: `originalMethod` (the method of the loaded assembly), `previousMethod` (what ran before this patch) and `newMethod` (what runs now; the original again when an edit was reverted). The list leaves out methods whose body is the same as in the patch before, and a removed Unity message.

```csharp
    private int instanceCalls;

    [InvokeOnHotReload]
    private void OnReloadInstance(List<MethodPatch> patches)
    {
        instanceCalls++;
        var first = patches.Count > 0 ? patches[0].originalMethod.DeclaringType.Name + "." + patches[0].originalMethod.Name : "";
        Debug.Log("[hr-smoke] callback instance object=" + name + " calls=" + instanceCalls + " patches=" + patches.Count + " first=" + first);
    }
```

**`[InvokeOnHotReloadLocal]`** runs the parameterless method itself, with its new body, when a patch changed it or restored it: its own code, a lambda or local function in it, its state machine when it is an iterator or `async` method, or a method a patch added that it calls. An instance method runs on every live object. It is the place to build anew what a once-run method made, for example delegates whose lambda changed. On `Awake`, `OnEnable`, `OnDisable` and `OnDestroy` it also stands for the body the lifecycle rewrite moved out of them.

```csharp
    [InvokeOnHotReloadLocal]
    private void LocalProbe()
    {
        Debug.Log("[hr-smoke] callback local v0 object=" + name);
    }
```

An exception of a callback is logged with `Debug.LogException` and counted, and the patch stays applied (note `callback_failed`). A callback that blocks for 20 s makes the runner restart the Player. An attribute on a method a patch added is not seen until a restart. [The applier's README](../tools/HotReloadRuntime/README.md#callbacks) has the exact rules.

## Frameworks and packages

Hot reload is framework-agnostic by design: its rules are about C#, the Mono runtime and the Unity engine, not about one library. What follows holds for any framework.

- **Code that ran once.** What a framework set up from code that ran when the Player started, when a type was first used, or when an object was created or spawned keeps what the old code set up. The builder follows the calls from the engine's once-run callbacks, `[RuntimeInitializeOnLoadMethod]`, static constructors and constructors, and the notes name the method that ran (`runtime_initializer_changed`, `started_objects_keep_state`, `constructor_applies_to_new_objects`). A handler subscribed to an event in such a method runs as often as the event fires, which the code does not show: it gets no note.
- **Lookups by reflection and by name.** A framework that finds methods by attribute, by reflection or by name sees the loaded assembly: attribute changes on existing members are refused, attributes on added members are invisible (`added_member_attributes_ignored`), and an added method is not found by name (`added_method_*` notes).
- **Generated and post-processed code.** The builder compares the images the Player loads: after the lifecycle rewrite of the sources, Unity's source generators and, for an assembly that uses Netcode, Netcode's post-processor. Generated code is compared like any other code, and generated registration that runs at the start is code that ran once. The compiler runs no other IL post-processor on the game's assemblies: code that another package's post-processor would rewrite runs as Roslyn compiled it, with or without hot reload.
- **Networked games.** A patch reaches the Player of its own session. Another Player of the same game takes the edit from its own session, and until then the two run different code: data both ends write and read (a struct's serializer, a named message) is misread without an error. A restart restarts one Player, so after a refusal every Player of the game needs the same recompile, and the replies say so in general words. Objects that two Players built with different constructor code stay different for as long as they live, also after both took the edit.

### Netcode for GameObjects

The existing support, verified with `com.unity.netcode.gameobjects` 2.13.2. The builder reads from the post-processed images what Netcode registered when the Player started and refuses a change of it: RPCs (`netcode_rpc_changed`), network variables and their read permission where it is written as the argument of their construction (`netcode_variable_changed`), serializers (`netcode_serialization_changed`), other generated start-up code (`netcode_generated_code_changed`), and every edit of an image in a form it does not know, another package version among them (`netcode_assembly`). Everything else, the body of an existing RPC included, is patched; a spawn callback has `spawned_objects_keep_state`. A refusal or restart for a `netcode_*` reason says that the other Players of the game must be restarted on the new code too. A read permission supplied by a property, helper, factory or subclass, and the write permission, are patched with the note of the code that ran them, not refused. [The builder's README](../tools/HotReloadPatcher/README.md#netcode-for-gameobjects) has the table.

Limits of a session itself, with or without hot reload (a generic MonoBehaviour cannot be prepared; NGO's scene management and in-scene placed NetworkObjects do not work), are listed under [the session's limits](AGENT_MCP.md#limits-of-a-session).

## The agent flow through MCP

By default the `mcp` server starts its runner with `--hot-reload-fallback report --commands-stdin`, so the agent decides when the game loses its state. The flow:

1. Read `session_control(status).lastSequence` before the edits.
2. Save with `script_edit` or `script_create` (or any editor). The watcher sees every save below `Assets`, `Packages` and `ProjectSettings`.
3. Call `session_control(wait_update)` with `afterSequence` set to that sequence. The wait first lets the runner answer every save made before the call (a `sync` command), so one wait after several saves reports the last of them. It is held while the runner compiles or has marked its answer as not the last one (`updatePending`).
4. Read the reply:
   - **Success**, same `pid`, `hotReload.codeGeneration` advanced: the edit runs.
   - **Success that starts with `attention`**: read it first. It names a held or told note of the edits since the mark (a told note without the recompile offer), a restart the runner made by itself, or that nothing was applied or rejected after the mark, with what stands. `hotReload.newNotes` lists every note since the mark.
   - **`full_recompile_required`**: the edit compiled and is not in the Player. `details` has `unsupported` (at most 16), `unsupportedCount`, `report`, `next` (the call), `lastSequence` (the mark for the next wait), and `rejected` (`error`, `sequence`) when a scene saved since was rejected by the running Player. Call `hot_reload` with `{"action": "recompile"}`, or revert the edit and wait again. Both waits answer this for every mark until the code is in the Player; only a compile error saved after it is reported first (`operation_failed`), and the refusal again once the sources compile.
   - **`operation_failed`** with the compiler's text: the sources do not compile; the Player keeps its code. A rejected scene or asset part is reported the same way, with its own text.
5. `hot_reload(recompile)` checks that the whole graph compiles, restarts the Player and returns the session state like `wait_update`, with a new `pid` and `codeGeneration` 0. When the sources do not compile, it fails with the compiler's text (`operation_failed`) and the Player keeps running; when an edit compiled after its restart is refused, it fails with `full_recompile_required` and the next recompile needs a new `requestId`. When the restart puts into effect a refused edit or a held note, its `attention` says so, and that other processes of the same game that took the same edits without a restart need the same restart. A `requestId` names one command: repeating it returns that command's outcome without a second restart.

Other answers:

- **`wait_timeout`**: the wait had `timeoutSeconds` (1 to 86400) and ended first. Nothing was decided; `details` say what the session was doing (`phase`, `lastSequence`, `updatePending`, `runnerAnswered`). Repeat the call. Without `timeoutSeconds` a wait has no limit.
- **`session_output_in_use`**: a `start` or `reset` while another session, of another `mcp` server of the project, holds the session output. Nothing was started; the message names the holding process. Stop that server, or give this one another `--output`.
- **`session_not_ready`**, **`recompile_pending`**: a recompile without a ready Player, or while another one is unanswered.

### When the source graph changes

An added or removed `.asmdef`, a changed package, a deleted script file, or a new script in a package of the project cannot be applied to the running Player at all. The session state then has `preparationRequired: true`, and the next `status`, `wait` or `wait_update` recovers by itself: a preparation the project already has for these sources is used at once, otherwise a new one is queued. The waits answer `result_pending` with a `message` meanwhile, the old Player stays available, and the session restarts on the new preparation. `hot_reload(recompile)` in that state is answered the same way: a recompile on the old graph would not apply the change. [AGENT_MCP.md](AGENT_MCP.md#make-persistent-changes-and-verify-them) has the rules.

### An example exchange

Copied from a session of the stage 6 checks (2026-10-14); the path of the report is shortened. An agent saves a script that adds a class and waits with the sequence it read before:

```json
{"name": "session_control", "arguments": {"action": "wait_update", "afterSequence": 6}}
```

```json
{
  "success": false,
  "error": {
    "code": "full_recompile_required",
    "message": "The C# edit compiled but cannot be applied to the running Player: HrAddedByAgent: Type HrAddedByAgent was added or renamed [type_added]. The Player keeps the code it ran before this edit. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.",
    "details": {
      "lastSequence": 8,
      "next": {"arguments": {"action": "recompile"}, "tool": "hot_reload"},
      "report": "<output>/runs/<run>/hot-reload/3/report.json",
      "unsupported": [{"code": "type_added", "member": "HrAddedByAgent", "message": "Type HrAddedByAgent was added or renamed."}],
      "unsupportedCount": 1
    }
  }
}
```

It makes the call the reply names, which returned after 4.26 s with the session state of the new Player (members left out as `...`):

```json
{"name": "hot_reload", "arguments": {"action": "recompile", "requestId": "hr-c-recompile-1"}}
```

```json
{
  "success": true,
  "data": {
    "attention": "The Player was restarted on the saved code, which puts into effect what it did not run before: HrAddedByAgent: Type HrAddedByAgent was added or renamed [type_added]. Other processes of the same game that took the same edits without a restart (the other Players of a network game, for instance) still run without it until they are restarted on the saved code too.",
    "hotReload": {
      "codeGeneration": 0,
      "enabled": true,
      "fullRecompileRequired": false,
      "lastRestart": {"reason": "requested", "requestId": "hr-c-recompile-1", "sequence": 12, "unsupportedCount": 0},
      "unsupported": [],
      "unsupportedCount": 0
    },
    "lastSequence": 12,
    "phase": "ready",
    "pid": 1196724,
    "...": "..."
  }
}
```

An applied edit with a held note, from another session of the same checks (an added field with an initializer and a changed constructor), answered `wait_update` with:

```json
{
  "success": true,
  "data": {
    "attention": "The edit is applied, with a limit that lasts until the Player restarts: HrCallbacks::.ctor() and the instance field initializers apply to objects constructed after the patch; existing objects keep their values [constructor_applies_to_new_objects] (and 1 more in hotReload.newNotes). If what you are checking depends on it, hot_reload with action \"recompile\" restarts the Player on the saved code and resets game state; otherwise go on. A process of the same game that starts on the saved code meanwhile (another Player of a network game, for instance) runs all of it, so the two may not agree until this Player restarts too.",
    "hotReload": {
      "codeGeneration": 9,
      "newNotes": [
        {"code": "constructor_applies_to_new_objects", "member": "HrCallbacks::.ctor()", "message": "HrCallbacks::.ctor() and the instance field initializers apply to objects constructed after the patch; existing objects keep their values."},
        {"code": "existing_instances_keep_default", "member": "HrCallbacks::lives", "message": "Objects that already exist read the default value of the added field HrCallbacks::lives; its initializer runs only in objects constructed after the patch."}
      ],
      "newNotesCount": 2,
      "newNotesSequence": 20,
      "...": "..."
    },
    "pid": 1245197,
    "...": "..."
  }
}
```

## Measured

On one machine: AMD Ryzen 7 5700X3D (8 cores, 16 threads), 30 GiB, XFS, Unity `6000.6.3f1`, with other agents' builds and Players running beside. Projects: copies of `Samples/RawProject` with added scripts (one game assembly, a Player directory of 177 MB). Dates and sources as given.

| What | Time |
| --- | --- |
| A body edit, from the write of the file to `code-patched` read from the runner (32 edits in sessions alternating with the binary before the stage 6 fixes, load 3.1; 2026-10-14) | median 0.33 s, p90 0.43 s, max 0.45 s (before: 0.44 s, 0.55 s) |
| The same through MCP: the `wait_update` call after the `script_edit` of a patched edit (15 calls in four MCP sessions of the stage 6 checks, 2026-10-14; the session outputs had been used before) | median 0.43 s, 0.36 to 0.92 s |
| A refusal through MCP, the `wait_update` call | 0.13 to 0.30 s for an edit of an existing file, 1.38 to 1.65 s for a new script file |
| The first edit on a new session output, which starts the kept processes (stage 6 review): compiler server 0.80 s, lifecycle helper 0.55 s, the loaded Player hashed once 0.47 s, builder 0.25 s | 2.40 s |
| `hot_reload(recompile)`, the call (four sessions, 2026-10-14) | 4.26, 4.36, 4.63, 4.81 s |
| A restart the runner made by itself for a refused edit (`onUnsupportedEdit: "restart"`), the `wait_update` call | 4.57 and 6.20 s |
| A C# edit with `hotReload: false` (a restart for every edit), the `wait_update` call | 4.89 s (one) |
| `session_control(start)` and `wait` until the Player is ready (seven starts) | 5.3 to 7.4 s |
| The applier in the Player, per request | 2 to 4 ms; 10 to 30 ms for the first one of a Player process |

Where the time of a body edit goes (Roslyn about 0.13 s, the copy of the Player directory, the lifecycle rewrite, the request to the Player 0.02 s) is in [RUNNER.md](RUNNER.md#measured), from `UME_TIMINGS=1`.

**Disk.** Every compile publishes a Player directory under the session output (177 MB in the sample, copied with reflinks where the filesystem has them); the runner removes those nothing uses. An output keeps at most about ten Player directories, the newest eight scene revisions, 64 compiled assemblies and eight run directories besides those in use ([RUNNER.md](RUNNER.md#what-a-session-output-keeps)). The twelve session outputs that the stage 6 checks (2026-10-14) had last used held 4 to 7 compiled Player directories, 8 scene revisions, 8 run directories and up to 64 compiled assemblies each, 1.0 to 1.2 GB as `du` counts them (it counts blocks shared through reflinks once per file). Native imports and preparations are not bounded; a preparation is about 250 MB for the sample.

**Memory.** Measured in the stage 6 review (420 and 500 edits in one session):

| Process | Resident memory |
| --- | --- |
| Compiler server | grew about 4 MB per compile in the sample and 12 MB in a Netcode game with the SDK's server collector; the session now runs it with the workstation collector, under which a private server fed a session's compiles grew 0.99 MB per compile. Replaced above 1 GiB. |
| Lifecycle helper | 220 to 390 MB, flat |
| Patch builder | 102 to 111 MB, flat |
| Runner | 29 to 31 MB |
| Player | 224 MB to 280 MB over 500 patches: a Player keeps every patch assembly it loaded |

**Not measured:** the compiler server's growth in a real session since it runs with the workstation collector; the disk of one output over hundreds of edits since the smaller files are bounded; a game larger than the samples (compile time and the memory of the kept processes grow with it); a Player with a debugger attached; a machine other than this one.

## Limits and known problems

Each with what you see.

- **A method that cannot be redirected in this Player.** A first redirect needs an island at the one address a method's body points at, or, for a `sub rsp, imm8` body, at a free address in its 16 MiB window. A `push rbp` body or a body whose first instruction the applier does not recognise, whose island address is taken, and a `sub rsp, imm8` body whose whole window is taken, cannot be redirected. You see a refusal with `apply_failed`, stage `write`, and `fullRecompileRequired: true`. Every patch that changes that method is refused for the life of that Player, and with it every later C# edit until the method's edit is reverted; a recompile applies it, and a new Player has the method's code at another address. Measured in a running Player (87,222 methods, 2026-10-14): under 0.03 % of methods are of the first two kinds with a taken island. One method whose window lies in a large mapping of the Player was refused in 1 of 80 fresh Players, and in 2 of 40 with its island address forced taken.
- **Slower script code** in a hot reload session, because inlining is off ([above](#inlining-is-off)). Measure performance with `--no-hot-reload`.
- **Running code keeps its body.** A frame on a stack finishes its old body; a running coroutine or `async` method keeps the body it started with (note `running_coroutines_keep_old_code`).
- **The Player keeps what it loaded**: script types, serialized layouts, runtime initializers and reflection data of the assemblies it started with. A new script is a new type and is refused; reflection sees loaded members, attributes and accessibility; a removed method stays callable by name.
- **One refused edit holds every C# edit after it** until it is reverted or recompiled; scene edits keep working. MCP: both waits answer `full_recompile_required` (a compile error saved after it first); runner: the refusal is printed again after every compile.
- **A patch reaches one Player.** Other Players of a network game, each in a session of its own, take the edit from their sessions; data they exchange is misread until all run the same code ([Frameworks](#frameworks-and-packages)).
- **Patch code is not redirected to later patch code.** Delegates and coroutines an earlier patch made keep running it (`earlier_patch_delegates_keep_old_code`), and `-=` does not find a handler of an added method subscribed under an earlier patch (`added_method_delegate_identity`).
- **Stack traces** of patched code name `HotReload.G<generation>.<type>.<method>`, with the file and line of the source.
- **A comment or blank line** saved while a patch is applied is a new generation (`code-patched`, `codeGeneration` advances) and runs no callback.
- **Memory grows** with every patch, because the Player keeps every patch assembly ([Measured](#measured)).
- **Callbacks** run inside the 20 s the runner waits for the Player; one that blocks longer makes the runner restart the Player (`hot_reload_failed`).
- **`[FormerlySerializedAs]`** is not applied by the session's loader: a renamed serialized field loses its scene value, also after a restart, until the data is saved under the new name.
- **Only the verified profile.** The applier refuses other platforms and architectures, and the runner then takes its fallback for every C# change. When the patch builder or the applier cannot be built at the start, `ready` says `hotReload: false`, the runner prints `hot-reload-unavailable`, and every C# change restarts the Player.
- **Not verified:** hot reload with content routes and Addressables, a Player with the Mono debugger attached, another Netcode for GameObjects version, a dedicated server or distributed authority.
