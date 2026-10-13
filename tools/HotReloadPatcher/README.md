# Hot reload patch builder

After a C# edit in a watched session the runner recompiles the changed game assemblies as usual and then runs this executable. It compares each assembly the Player loaded with its new compilation, classifies every difference, and emits one patch assembly per changed assembly: `<name>.HotReload.<generation>.dll` with a portable PDB. The applier in the Player loads the patch and redirects the old methods to the new ones.

It is built with the .NET 8 SDK of the Unity installation and uses only `System.Reflection.Metadata` from the shared framework: no package, no reference, no Unity process.

```bash
dotnet HotReloadPatcher.dll <request.json>                       # one JSON document on stdout, exit 0
dotnet HotReloadPatcher.dll --codes                              # every reason and note code with its meaning
dotnet HotReloadPatcher.dll --explain <request.json> <member id> # what the comparison sees of one method
```

A request that cannot be read (bad JSON, a missing or invalid assembly, an unreadable previous result) is a message on stderr and exit code 1. An edit that cannot be patched is a normal result with the verdict `unsupported`. So is a failure of the tool on one assembly: the reason is `internal_error` with the exception, and nothing is written.

A result with the verdict `unchanged` can still have notes: a changed accessibility of a field or method and a removed start-up method leave every body as it was, and the Player keeps the loaded form until it restarts. Whether such a difference is a note or a refusal follows one line: what the Player still runs correctly for the code that was compiled is a note, also when Unity would serialize or find something else after a restart (an added serialized field is a note for the same reason). The runner prints the notes of such a result on its `unchanged` or `updated` event.

The request and the result are described at the top of `Program.cs`. Two of their members serve patches that follow each other in one Player:

- Each assembly of a result lists its `patchMembers`: the methods that only the patch defines (added methods, lambdas, local functions, closure and state machine methods), each with a hash of the code it can run. Replacements of loaded methods are not listed: callers and delegates name the loaded method, and every patch redirects it anew.
- The request of the next patch names that result as `previousResult`. Nothing redirects from one patch into the next, so a delegate, coroutine or async call that the earlier patch created keeps the earlier code. The builder compares the two and says so (`earlier_patch_delegates_keep_old_code`), and it reports a delegate of an added method that both patches have (`added_method_delegate_identity`): `-=` does not find a handler the earlier patch subscribed. An assembly that was patched before and is byte-identical to the loaded one again must still be listed (with `current` equal to `loaded`) to get these notes.

## How it decides

Two compilations of the same source never agree on metadata tokens, and the compiler renumbers its generated members (`<M>b__5_0`, `<>c__DisplayClass5_0`, `<M>d__5`, `<>f__AnonymousType3`) whenever a member is inserted earlier. So a method is compared by what its tokens mean, and the compiler-generated members it owns are compared by structure (`Canon.cs`). A comment, a reordered member or a new file changes nothing in that form.

The policy is in `Diff.cs`. An edit is patched only when the patch and the runtime are known to carry it exactly; everything else is refused with a code from `Codes.cs`. One refused change makes the whole result `unsupported`, and no file is written.

What a patch contains (`Emit.cs`):

- An instance method of a class becomes a static method with the receiver as first parameter. An instance method of a struct stays an instance method, on an empty stand-in struct: that keeps the calling convention of a struct receiver with a hidden return buffer.
- Bodies are copied byte for byte with their tokens rewritten in place. A call to a changed method keeps naming the loaded original, which the Player redirects; a call to an added method names the copy in the patch.
- Closure classes and iterator/async state machines are copied as new top-level types; the shared `<>c` class and `<PrivateImplementationDetails>` are copied in part.
- An anonymous type or a generic compiler-generated type is never copied. The patch refers to the loaded type of the same structure, or the edit is refused.
- A field the loaded type lacks is reached through accessors that call `UnityMicroEditor.HotReload.FieldStore`, with the id `<assembly>|<type>|<field>`.
- An added instance method starts with a check of its receiver, because its copy is static and called with `call`: a null receiver throws at the call, as `callvirt` does.
- A removed `void` Unity message is redirected to an empty method, unless the engine works differently while the message exists.
- An edited lambda or local function is redirected from its old method when the pairing is proven: the code that creates it is identical in both compilations apart from the names of the generated members, so the k-th lambda of one is the k-th lambda of the other. An async lambda or an iterator local function is a stub that starts a state machine; the stub is redirected when the state machine changed. In every other case the result carries `lambda_not_remapped`, except when the old lambda is unchanged at the same position among the lambdas of its method.
- A compiler-named method that is virtual (the `<Clone>$` of a record) is never copied: the patch calls the loaded one, virtually.
- The assembly carries the `SkipVerification` permission set and `[module: UnverifiableCode]`. Its types live in the namespace `HotReload.G<generation>`.

Every emitted patch is read back and verified before it is published (`Verify.cs`): no type name of a loaded assembly, every reference into a game assembly resolves in the loaded one, the permission set is present, every body decodes, every redirect pair has the same shape.

## Limits

- Generic methods, methods of generic types, added or removed types, virtual methods, constructors and Unity messages, changed static constructors, attribute changes, `readonly` removed from a static field, changed marshalling of an extern method or delegate, calls with a variable argument list and assemblies processed by Netcode are refused; `--codes` lists every reason.
- The attributes the compiler writes on an iterator or async method (`*StateMachine`, `DebuggerStepThrough`) are not compared, so a method can become one or stop being one. Reflection keeps seeing the loaded attributes.
- Patch code is not redirected to later patch code. What an earlier patch created keeps running that patch; the result of the next patch names it when it is given `previousResult`.
- A removed method that is not a Unity message stays loaded. Code that reaches it by name (`Invoke`, `SendMessage`, a UnityEvent, an animation event) keeps running it until a restart.
- A class counts as an engine object unless its base chain is read to `System.Object`. Bases in assemblies outside the request are read from the directory of the loaded assembly; when that fails, a method named like a Unity message is treated as one.

Tests: `tests/HotReloadPatcherTests`.
