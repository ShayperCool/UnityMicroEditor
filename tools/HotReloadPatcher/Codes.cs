// Every reason and note code of the result document, each with its one-line meaning. The codes are
// part of the interface (the runner forwards them, the documentation lists them): add here, never
// rename. `dotnet HotReloadPatcher.dll --codes` prints this table.

internal static class Codes
{
    // Reasons an edit cannot be applied to the running Player ("unsupported").
    public const string NetcodeAssembly = "netcode_assembly";
    public const string NetcodeRpcChanged = "netcode_rpc_changed";
    public const string NetcodeVariableChanged = "netcode_variable_changed";
    public const string NetcodeSerializationChanged = "netcode_serialization_changed";
    public const string NetcodeGeneratedCodeChanged = "netcode_generated_code_changed";
    public const string AssemblyIdentityChanged = "assembly_identity_changed";
    public const string CustomAttributesChanged = "custom_attributes_changed";
    public const string TypeAdded = "type_added";
    public const string TypeRemoved = "type_removed";
    public const string TypeFlagsChanged = "type_flags_changed";
    public const string BaseTypeChanged = "base_type_changed";
    public const string InterfacesChanged = "interfaces_changed";
    public const string TypeLayoutChanged = "type_layout_changed";
    public const string StructLayoutChanged = "struct_layout_changed";
    public const string EnumChanged = "enum_changed";
    public const string InterfaceChanged = "interface_changed";
    public const string GenericType = "generic_type";
    public const string GenericMethod = "generic_method";
    public const string VirtualMethodAdded = "virtual_method_added";
    public const string VirtualMethodRemoved = "virtual_method_removed";
    public const string MethodFlagsChanged = "method_flags_changed";
    public const string MethodOverridesChanged = "method_overrides_changed";
    public const string StaticConstructorChanged = "static_constructor_changed";
    public const string ConstructorAdded = "constructor_added";
    public const string ExternMethodAdded = "extern_method_added";
    public const string ExternMethodChanged = "extern_method_changed";
    public const string UnityMessageAdded = "unity_message_added";
    public const string UnityMessageRemoved = "unity_message_removed";
    public const string FieldFlagsChanged = "field_flags_changed";
    public const string StaticReadonlyRemoved = "static_readonly_removed";
    public const string SpecialFieldAdded = "special_field_added";
    public const string SerializationChanged = "serialization_changed";
    public const string CrossAssemblyAddition = "cross_assembly_addition";
    public const string GeneratedTypeNotLoaded = "generated_type_not_loaded";
    public const string GenericGeneratedMember = "generic_generated_member";
    public const string UnownedGeneratedCode = "unowned_generated_code";
    public const string UnsupportedInstruction = "unsupported_instruction";
    public const string EmitNotSupported = "emit_not_supported";
    public const string EmitVerificationFailed = "emit_verification_failed";
    public const string InternalError = "internal_error";

    // Notes: the edit is applied, and the caller is told what the running Player does differently.
    public const string ExistingInstancesKeepDefault = "existing_instances_keep_default";
    public const string SerializedFieldNotRestored = "serialized_field_not_restored";
    public const string FieldValueReset = "field_value_reset";
    public const string ConstructorAppliesToNewObjects = "constructor_applies_to_new_objects";
    public const string StartedObjectsKeepState = "started_objects_keep_state";
    public const string SpawnedObjectsKeepState = "spawned_objects_keep_state";
    public const string RunningCoroutinesKeepOldCode = "running_coroutines_keep_old_code";
    public const string LambdaNotRemapped = "lambda_not_remapped";
    public const string EarlierPatchDelegatesKeepOldCode = "earlier_patch_delegates_keep_old_code";
    public const string AddedMethodDelegateIdentity = "added_method_delegate_identity";
    public const string AddedMemberAttributesIgnored = "added_member_attributes_ignored";
    public const string AddedMethodInvisibleByName = "added_method_invisible_by_name";
    public const string AddedMethodNamedByString = "added_method_named_by_string";
    public const string AddedMethodComputedName = "added_method_computed_name";
    public const string AccessibilityChanged = "accessibility_changed";
    public const string RuntimeInitializerAdded = "runtime_initializer_added";
    public const string RuntimeInitializerRemoved = "runtime_initializer_removed";
    public const string RuntimeInitializerChanged = "runtime_initializer_changed";

    public static readonly (string Code, string Meaning)[] Unsupported =
    {
        (NetcodeAssembly, "The assembly uses Netcode for GameObjects and its post-processed image is not of the form the patch builder knows (another version of the package, an image that was not post-processed, the Netcode runtime itself), so nothing says what Netcode registered when the Player started; no edit of it is patched."),
        (NetcodeRpcChanged, "An RPC was added, removed or renamed, or its parameters, its attribute or the code that sends or receives it changed; Netcode registers the RPCs of a class by an id of their signature when the first object of the class is initialized, and both ends of a connection must agree on them. An edit of the body of an RPC is patched."),
        (NetcodeVariableChanged, "A NetworkVariable (or another NetworkVariableBase field) of a NetworkBehaviour was added, removed, renamed, retyped or reordered; every object lists its network variables once, when it is initialized, and both ends of a connection replicate them by their position in that list."),
        (NetcodeSerializationChanged, "The value types that Netcode registers serializers for changed (the type of a network variable, [GenerateSerializationForType]); they are registered once, when the Player starts."),
        (NetcodeGeneratedCodeChanged, "Code that the Netcode post-processor generates and that runs when the Player starts or an object is initialized changed for another reason (the name of a NetworkBehaviour, a class that became one or stopped being one, a generated member the patch builder does not know)."),
        (AssemblyIdentityChanged, "The name, version, culture or public key of the assembly changed."),
        (CustomAttributesChanged, "Custom attributes of an existing assembly, type, member or parameter changed; they are read through reflection when the game starts."),
        (TypeAdded, "A type (class, struct, enum, interface or delegate) was added or renamed."),
        (TypeRemoved, "A type was removed or renamed; the loaded type stays registered until a restart."),
        (TypeFlagsChanged, "Flags of an existing type changed (visibility, abstract, sealed, serializable, layout kind)."),
        (BaseTypeChanged, "The base type of a loaded type is fixed."),
        (InterfacesChanged, "The interface list of a loaded type is fixed."),
        (TypeLayoutChanged, "Explicit layout of a type changed (packing, size or a field offset)."),
        (StructLayoutChanged, "Instance fields of a struct or of a class with sequential/explicit layout were added, removed, retyped or reordered."),
        (EnumChanged, "Members or the underlying type of an enum changed; reflection and serialization keep the loaded members."),
        (InterfaceChanged, "A member of an interface was added, removed or edited."),
        (GenericType, "The edit touches a generic type; its code is shared between instantiations and cannot be redirected."),
        (GenericMethod, "The edit touches a generic method; its code is shared between instantiations and cannot be redirected."),
        (VirtualMethodAdded, "A virtual, abstract, override or interface-implementing method was added; the vtable of a loaded class is fixed."),
        (VirtualMethodRemoved, "A virtual, override or interface-implementing method was removed; the loaded class keeps dispatching to it."),
        (MethodFlagsChanged, "Flags of an existing method changed (virtual, abstract, sealed, special name, synchronized, ...)."),
        (MethodOverridesChanged, "The explicit overrides (interface implementations) of a type changed."),
        (StaticConstructorChanged, "A static constructor or static field initializer was added, changed or removed; it runs once when the type is first used."),
        (ConstructorAdded, "A constructor was added or its signature changed; `new` needs a real constructor on the loaded type."),
        (ExternMethodAdded, "A method without IL body (extern, P/Invoke, internal call) was added."),
        (ExternMethodChanged, "The import, the marshalling of a parameter or of the return value, or the presence of a body of a method without IL body changed; the runtime keeps the stub of the loaded method."),
        (UnityMessageAdded, "A Unity message or lifecycle callback (Update, OnEnable, OnCollisionEnter, ...) was added to an existing class; the engine binds messages once per class."),
        (UnityMessageRemoved, "A removed Unity message cannot be switched off safely (it returns a value, a base class declares or may declare the same message, the class is generic, or the engine works differently while the message exists: OnAnimatorMove, OnRenderImage, OnAudioFilterRead, OnGUI)."),
        (FieldFlagsChanged, "Flags of an existing field changed (not-serialized, marshalling, special name, ...)."),
        (StaticReadonlyRemoved, "`readonly` was removed from a static field; the runtime may have compiled its value into loaded and patched code as a constant, so an assignment would not be seen."),
        (SpecialFieldAdded, "The added field cannot live in the side table (thread-static, volatile, pointer or by-ref type, marshalled, mapped data)."),
        (SerializationChanged, "The accessibility or `readonly` of an existing field changed so that Unity starts or stops serializing it; the Player reads scenes, prefabs and assets with the loaded fields until a restart. A field of a type Unity never serializes (a delegate, a dictionary without [SerializeField], an interface or a class without [Serializable], a 64-bit enum, an engine type such as Coroutine, Ray or NativeArray) is not refused."),
        (CrossAssemblyAddition, "Patched code uses a type or member that this same change added to another game assembly."),
        (GeneratedTypeNotLoaded, "Patched code needs an anonymous type or a generic compiler-generated type that the loaded assembly does not contain."),
        (GenericGeneratedMember, "Patched code contains a generic lambda or local function."),
        (UnownedGeneratedCode, "Compiler-generated code (a lambda, a local function or a state machine) that some body of the assembly names differs between the two compilations and no method is known to own it, so the edit cannot be compared. Generated code that nothing names (behind a `return`, in a branch a constant switches off, a local function without a call) cannot run and is left out."),
        (UnsupportedInstruction, "Patched code uses an added member in a way the patch cannot express (reflection handle, delegate over an added struct method, volatile access, ...)."),
        (EmitNotSupported, "The patch builder cannot prove a correct patch for this member (a synchronized added method, a variable argument list or a call of one, ...)."),
        (EmitVerificationFailed, "The emitted patch assembly did not pass its verification; nothing was published."),
        (InternalError, "The patch builder failed on this assembly, which is a defect of the tool; the message carries the exception and nothing was published."),
    };

    public static readonly (string Code, string Meaning)[] Notes =
    {
        (ExistingInstancesKeepDefault, "Objects that already exist read the default value of an added field that a constructor or an initializer sets; that runs only in objects constructed after the patch. An added field without one has its default everywhere and gets no note, unless a field of the same type was removed with it: a renamed field does not take over the value of the old name."),
        (SerializedFieldNotRestored, "An added field that Unity would serialize is not restored from scenes, prefabs or assets until a restart."),
        (FieldValueReset, "A field whose type or static modifier changed is stored anew; its previous value is lost."),
        (ConstructorAppliesToNewObjects, "A changed constructor or instance field initializer affects only objects constructed after the patch."),
        (StartedObjectsKeepState, "A changed method that the engine calls once for an object does not run again for the objects that already ran it: the parameterless Start of a MonoBehaviour, Awake of a MonoBehaviour or ScriptableObject, OnEnable (again only when the object is disabled and enabled; what the old body subscribed stays), OnEnable of a ScriptableObject (when the asset loads) and OnAfterDeserialize; also a method that only such methods call. Its new body runs for objects that start after the patch."),
        (SpawnedObjectsKeepState, "A changed OnNetworkSpawn, OnNetworkPreSpawn, OnNetworkPostSpawn, OnNetworkSessionSynchronized or OnInSceneObjectsSpawned of a NetworkBehaviour, or a method that only they call, does not run again for the objects that are spawned; it runs for objects spawned after the patch. A changed OnIsServerAuthoritative of a NetworkTransform is read at spawn and kept, so the objects that are spawned mix the old and the new answer."),
        (RunningCoroutinesKeepOldCode, "Coroutines, iterators and async methods that are already running finish on the old code, also when the method was removed or renamed; new calls run the new code."),
        (LambdaNotRemapped, "A lambda or local function was edited, moved or removed and its old version is not redirected: which lambda it became is only known while the code that creates it is unchanged, and a redirect needs the same captures, none of them taken by reference. Delegates and calls made by code from before the patch keep the old body."),
        (EarlierPatchDelegatesKeepOldCode, "An added method, lambda, local function or state machine that an earlier patch brought was changed or dropped; delegates, coroutines and async calls created while that patch was applied keep running its code."),
        (AddedMethodDelegateIdentity, "Patched code makes a delegate of an added method that the earlier patch also had; each patch has its own copy, so `-=` does not remove a handler subscribed while the earlier patch was applied, until a full recompile."),
        (AddedMemberAttributesIgnored, "Attributes on an added member are invisible to reflection until a full recompile."),
        (AddedMethodInvisibleByName, "An added instance method of an engine object cannot be found by name until a full recompile: SendMessage, Invoke, StartCoroutine(string), UnityEvent, animation events, input messages and reflection look in the loaded class. Calls in code reach it."),
        (AddedMethodNamedByString, "A string of the game equals the name of an added method: Invoke, SendMessage, StartCoroutine(string) and reflection look in the loaded class and do not find it until a full recompile, so a call by that name does nothing. The string may stand in another assembly of the game (another entry of the request, or an assembly next to the loaded one that references it, that it references, or Assembly-CSharp). The note names the types whose code holds the string and is new again when another place names the method."),
        (AddedMethodComputedName, "An instance method was added to an engine object, and code of the game calls SendMessage, BroadcastMessage, Invoke, InvokeRepeating or StartCoroutine with a name it computes: if such a call means the added method, it does nothing until a full recompile. The note names the types whose code computes the name."),
        (AccessibilityChanged, "The accessibility of an existing member changed; reflection keeps the loaded accessibility until a full recompile. A field that Unity would serialize differently is refused instead (serialization_changed)."),
        (RuntimeInitializerAdded, "A [RuntimeInitializeOnLoadMethod] method was added; it runs at the next start."),
        (RuntimeInitializerRemoved, "A [RuntimeInitializeOnLoadMethod] method was removed or renamed; the change is effective at the next start."),
        (RuntimeInitializerChanged, "The body of a [RuntimeInitializeOnLoadMethod] method, or of a method only such methods call, changed; it ran when the Player started and does not run again, so the new body is not in effect before the next start."),
    };
}

/// What the caller does with one change.
internal static class Actions
{
    public const string Patch = "patch", None = "none", Note = "note", Unsupported = "unsupported";
}

/// A construct the patch cannot carry. Caught per change, reported with its code.
internal sealed class Unsupported : Exception
{
    public readonly string Code;
    public Unsupported(string code, string message) : base(message) => Code = code;
}
