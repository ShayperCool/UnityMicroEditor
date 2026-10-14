// What the engine and the lifecycle rewrite add to plain C#: methods the engine calls by name, and
// the start-up bridges the lifecycle helper generates.
using System.Reflection;
using System.Reflection.Metadata;

internal enum UnityKind
{
    /// The base chain provably ends at System.Object: not an engine object, its methods are plain methods.
    No,
    /// Derives from UnityEngine.Object.
    Yes,
    /// The chain leaves the assemblies that can be read: treated as an engine object.
    Unknown,
}

internal readonly struct UnityType
{
    public readonly UnityKind Kind;
    /// Every base up to the engine's own classes was read, so `BaseMethods` is complete.
    public readonly bool ChainRead;
    /// Instance method names declared by the bases between the type and the engine's classes.
    public readonly HashSet<string> BaseMethods;

    /// The engine class the chain ends at, as `UnityEngine.MonoBehaviour`; empty unless `Kind` is `Yes`.
    public readonly string Root;

    public UnityType(UnityKind kind, bool chainRead, HashSet<string> baseMethods, string root = "") { Kind = kind; ChainRead = chainRead; BaseMethods = baseMethods; Root = root; }

    /// Whether the engine calls a parameterless `Start` of the type once per object: a
    /// MonoBehaviour, or a type whose chain cannot be read to its end.
    public bool Starts => Kind == UnityKind.Unknown || Root == "UnityEngine.MonoBehaviour";
    /// The same for `Awake`, which a ScriptableObject has as well.
    public bool Awakes => Kind == UnityKind.Unknown || Root is "UnityEngine.MonoBehaviour" or "UnityEngine.ScriptableObject" or "UnityEngine.StateMachineBehaviour";
}

internal static class UnityTypes
{
    /// Start-up bridges of the lifecycle rewrite. They are numbered across the assembly and only
    /// called by name at a start, so they are never compared as methods.
    public const string RuntimeInitializerPrefix = "__UnityPlay_RuntimeInitialize_";

    // The engine looks these up by name, once per class, on MonoBehaviour and ScriptableObject
    // types. The signature does not matter to it (OnCollisionEnter with or without a parameter,
    // Start returning void or IEnumerator), so the name alone decides.
    public static readonly HashSet<string> Messages = new(StringComparer.Ordinal)
    {
        "Awake", "Start", "Update", "FixedUpdate", "LateUpdate", "OnEnable", "OnDisable", "OnDestroy", "OnGUI", "Reset", "OnValidate",
        "OnAnimatorIK", "OnAnimatorMove", "OnApplicationFocus", "OnApplicationPause", "OnApplicationQuit", "OnAudioFilterRead",
        "OnBecameInvisible", "OnBecameVisible", "OnBeforeTransformParentChanged", "OnCanvasGroupChanged", "OnCanvasHierarchyChanged",
        "OnCollisionEnter", "OnCollisionEnter2D", "OnCollisionExit", "OnCollisionExit2D", "OnCollisionStay", "OnCollisionStay2D",
        "OnConnectedToServer", "OnControllerColliderHit", "OnDidApplyAnimationProperties", "OnDisconnectedFromServer",
        "OnDrawGizmos", "OnDrawGizmosSelected", "OnFailedToConnect", "OnFailedToConnectToMasterServer",
        "OnJointBreak", "OnJointBreak2D", "OnMasterServerEvent", "OnMouseDown", "OnMouseDrag", "OnMouseEnter", "OnMouseExit",
        "OnMouseOver", "OnMouseUp", "OnMouseUpAsButton", "OnNetworkInstantiate", "OnParticleCollision", "OnParticleSystemStopped",
        "OnParticleTrigger", "OnParticleUpdateJobScheduled", "OnPlayerConnected", "OnPlayerDisconnected", "OnPostRender",
        "OnPreCull", "OnPreRender", "OnRectTransformDimensionsChange", "OnRectTransformRemoved", "OnRenderImage", "OnRenderObject",
        "OnSerializeNetworkView", "OnServerInitialized", "OnTransformChildrenChanged", "OnTransformParentChanged",
        "OnTriggerEnter", "OnTriggerEnter2D", "OnTriggerExit", "OnTriggerExit2D", "OnTriggerStay", "OnTriggerStay2D", "OnWillRenderObject",
    };

    /// Messages whose mere existence changes what the engine does: root motion is left to the script
    /// (OnAnimatorMove), the camera renders through an intermediate image (OnRenderImage), a filter
    /// joins the audio chain (OnAudioFilterRead), the object takes part in the GUI pass (OnGUI). An
    /// empty method is not the same as none.
    public static readonly HashSet<string> PresenceMatters = new(StringComparer.Ordinal) { "OnAnimatorMove", "OnRenderImage", "OnAudioFilterRead", "OnGUI" };

    // The engine's own classes at the top of every script's chain. None of them declares a message.
    static readonly HashSet<string> Roots = new(StringComparer.Ordinal)
    {
        "UnityEngine.Object", "UnityEngine.Component", "UnityEngine.Behaviour", "UnityEngine.MonoBehaviour",
        "UnityEngine.ScriptableObject", "UnityEngine.StateMachineBehaviour",
    };

    // Class libraries cannot derive from the engine: a chain that enters one of them ends at System.Object.
    public static bool IsClassLibrary(string assembly) =>
        assembly is "mscorlib" or "netstandard" or "System" || assembly.StartsWith("System.", StringComparison.Ordinal) || assembly.StartsWith("Mono.", StringComparison.Ordinal);

    /// The engine's own assemblies. It serializes its types by rules of its own, whatever their flags.
    public static bool IsEngine(string assembly) =>
        assembly is "UnityEngine" or "UnityEditor" || assembly.StartsWith("UnityEngine.", StringComparison.Ordinal) || assembly.StartsWith("Unity.", StringComparison.Ordinal);

    /// Value types of the class library that are no enum, for when the library cannot be read:
    /// Unity serializes an enum of any assembly and none of these.
    public static readonly HashSet<string> LibraryStructs = new(StringComparer.Ordinal)
    {
        "System.DateTime", "System.DateTimeOffset", "System.TimeSpan", "System.Guid", "System.Decimal", "System.IntPtr", "System.UIntPtr",
        "System.Threading.CancellationToken", "System.Numerics.BigInteger", "System.Numerics.Complex", "System.Numerics.Vector2",
        "System.Numerics.Vector3", "System.Numerics.Vector4", "System.Numerics.Quaternion", "System.Numerics.Matrix4x4",
        "System.Runtime.InteropServices.GCHandle", "System.RuntimeTypeHandle", "System.RuntimeFieldHandle", "System.RuntimeMethodHandle",
        "System.Collections.DictionaryEntry", "System.Threading.Tasks.ValueTask", "System.Range", "System.Index", "System.Half",
    };

    /// Walks the base chain of a type through every assembly that can be read.
    public static UnityType Classify(Workspace workspace, Asm assembly, TypeDefinitionHandle type)
    {
        var names = new HashSet<string>(StringComparer.Ordinal);
        Asm a = assembly;
        var t = type;
        for (int depth = 0; depth < 64; depth++)
        {
            EntityHandle parent = a.MD.GetTypeDefinition(t).BaseType;
            if (parent.IsNil) return new UnityType(UnityKind.No, true, names); // System.Object itself, or an interface
            if (parent.Kind == HandleKind.TypeSpecification)
            {
                // class Enemy : Pooled<Enemy>: the chain continues at the generic type definition.
                var blob = a.MD.GetBlobReader(a.MD.GetTypeSpecification((TypeSpecificationHandle)parent).Signature);
                if (blob.ReadByte() != 0x15) return new UnityType(UnityKind.Unknown, false, names);
                blob.ReadByte();
                parent = blob.ReadTypeHandle();
            }
            if (parent.Kind == HandleKind.TypeReference)
            {
                var reference = (TypeReferenceHandle)parent;
                string name = a.TypeRefName(reference, bare: true), home = a.TypeRefAssembly(reference);
                if (Roots.Contains(name)) return new UnityType(UnityKind.Yes, true, names, name);
                if (IsClassLibrary(home)) return new UnityType(UnityKind.No, true, names);
                if (home.Length == 0 || workspace.Resolve(home) is not { } next || !next.Types.TryGetValue(name, out var found))
                    return new UnityType(UnityKind.Unknown, false, names);
                a = next;
                t = found;
            }
            else if (parent.Kind == HandleKind.TypeDefinition)
            {
                t = (TypeDefinitionHandle)parent;
                if (Roots.Contains(a.TypeName(t))) return new UnityType(UnityKind.Yes, true, names, a.TypeName(t));
            }
            else return new UnityType(UnityKind.Unknown, false, names);
            foreach (var m in a.MD.GetTypeDefinition(t).GetMethods())
            {
                var d = a.MD.GetMethodDefinition(m);
                if ((d.Attributes & MethodAttributes.Static) == 0) names.Add(a.MD.GetString(d.Name));
            }
        }
        return new UnityType(UnityKind.Unknown, false, names);
    }
}
