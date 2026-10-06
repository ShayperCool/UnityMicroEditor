"""Exercise semantic instrumentation through real bundled Roslyn compilations."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import raw_compile as raw

UNITY = Path(os.environ.get("UNITY_EDITOR", "/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity"))


@unittest.skipUnless(UNITY.is_file(), "installed Unity SDK required for semantic instrumentation")
class LifecycleCompileTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.shared = tempfile.TemporaryDirectory()
        cls.host = Path(cls.shared.name) / "Protocol_Data/Managed"
        cls.host.mkdir(parents=True)
        data = raw._data(UNITY)
        cls.sdk = data / "DotNetSdk"
        cls.compiler = next(cls.sdk.glob("sdk/*/Roslyn/bincore/csc.dll"))
        bcl = data / "MonoBleedingEdge/lib/mono/unityaot-linux"
        refs = {p.name: p for p in bcl.glob("*.dll")}
        for p in (bcl / "Facades").glob("*.dll"):
            refs.setdefault(p.name, p)
        refs["UnityEngine.CoreModule.dll"] = data / "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed/UnityEngine.CoreModule.dll"
        source = cls.host.parent / "Protocol.cs"
        source.write_text('''using System;
using UnityEngine;
public class ForeignBase : MonoBehaviour { protected void Awake() {} }
public class VirtualForeignBase : MonoBehaviour { protected virtual void Awake() {} protected virtual void OnDestroy() {} }
public class PrivateForeignBase : MonoBehaviour { private void Awake() {} }
namespace Dungeon.CLIPlay.Raw {
    [AttributeUsage(AttributeTargets.Assembly)]
    public sealed class RawLifecycleInstrumentedAttribute : Attribute {
        public RawLifecycleInstrumentedAttribute(int version) {}
    }
    [AttributeUsage(AttributeTargets.Assembly)]
    public sealed class RawScriptableObjectInstrumentedAttribute : Attribute {
        public RawScriptableObjectInstrumentedAttribute(int version) {}
    }
    public static class RawLifecycle {
        public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
        public static bool ShouldCollect(MonoBehaviour target, Phase phase) { return false; }
        public static void Enqueue(MonoBehaviour target, Phase phase, Action body, int order) {}
        public static bool ShouldCollect(ScriptableObject target, Phase phase) { return false; }
        public static void Enqueue(ScriptableObject target, Phase phase, Action body, int order) {}
        public static bool IsActiveAndEnabled(Behaviour target) { return target.isActiveAndEnabled; }
        public static void SetActive(GameObject target, bool active) { target.SetActive(active); }
    }
    public static class RawPrefabTransforms {
        public static Transform Root(Transform target) { return target.root; }
        public static Component GetComponentInChildren(Component target, Type type) { return target.GetComponentInChildren(type); }
        public static Component GetComponentInChildren(Component target, Type type, bool includeInactive) { return target.GetComponentInChildren(type, includeInactive); }
        public static Component GetComponentInChildren(GameObject target, Type type) { return target.GetComponentInChildren(type); }
        public static Component GetComponentInChildren(GameObject target, Type type, bool includeInactive) { return target.GetComponentInChildren(type, includeInactive); }
        public static T GetComponentInChildren<T>(Component target) { return target.GetComponentInChildren<T>(); }
        public static T GetComponentInChildren<T>(Component target, bool includeInactive) { return target.GetComponentInChildren<T>(includeInactive); }
        public static T GetComponentInChildren<T>(GameObject target) { return target.GetComponentInChildren<T>(); }
        public static T GetComponentInChildren<T>(GameObject target, bool includeInactive) { return target.GetComponentInChildren<T>(includeInactive); }
    }
}
''')
        arguments = ["-nologo", "-target:library", "-nostdlib+", "-langversion:9.0",
                     "-out:" + str(cls.host / "Dungeon.CLIPlay.dll")]
        arguments.extend("-reference:" + str(p) for p in refs.values())
        arguments.append(str(source))
        run = subprocess.run([str(cls.sdk / "dotnet"), str(cls.compiler), *arguments],
                             capture_output=True, text=True, timeout=60)
        if run.returncode:
            raise AssertionError(run.stdout + run.stderr)

    @classmethod
    def tearDownClass(cls):
        cls.shared.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.project = self.root / "raw project"
        (self.project / "Assets").mkdir(parents=True)
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")

    def tearDown(self):
        self.temp.cleanup()

    def write(self, relative, content):
        path = self.project / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(content) if isinstance(content, dict) else content)
        return path

    def compile(self, revision="output", cache=False):
        return raw.compile_project(self.project, UNITY, self.root / revision, self.host,
                                   self.root / "code-cache" if cache else None)

    def compile_with_transformation(self):
        original_run = subprocess.run
        transformations = []

        def capture(arguments, *args, **kwargs):
            result = original_run(arguments, *args, **kwargs)
            if arguments and str(arguments[-1]).endswith("request.json") and not result.returncode:
                request = json.loads(Path(arguments[-1]).read_text())
                transformations.append((json.loads(result.stdout), "\n".join(
                    Path(source["OutputPath"]).read_text() for source in request["Sources"])))
            return result

        with patch.object(raw.subprocess, "run", side_effect=capture):
            result = self.compile()
        self.assertEqual(len(transformations), 1)
        return result, *transformations[0]

    def test_all_four_phases_inheritance_base_call_and_helper_name_collision_compile(self):
        source = self.write("Assets/Probe.cs", '''using UnityEngine;
public class Base : MonoBehaviour {
    int __UnityPlay_Awake;
    protected void Awake() { __UnityPlay_Awake++; }
    private void OnEnable() {}
    private void OnDisable() {}
    private void OnDestroy() {}
}
public class Derived : Base { protected new void Awake() { base.Awake(); } }
public class Other { public int Awake(int value) { return value; } }
''')
        before = hashlib.sha256(source.read_bytes()).hexdigest()
        result = self.compile()
        proof = result["assemblies"][0]["lifecycleBarrier"]
        self.assertTrue(result["capabilities"]["lifecycleBarrier"])
        self.assertEqual(proof["callbacks"], 5)
        self.assertEqual({m["Type"] for m in proof["methods"]}, {"Base", "Derived"})
        self.assertEqual(proof["phases"], ["Awake", "OnEnable", "OnDisable", "OnDestroy"])
        self.assertEqual(before, hashlib.sha256(source.read_bytes()).hexdigest())
        self.assertFalse((self.project / "Library").exists())
        self.assertIn(b"RawLifecycleInstrumentedAttribute", (self.root / "output/Assembly-CSharp.dll").read_bytes())

    def test_getter_rewrite_matches_exact_unity_property_and_preserves_nameof(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public class Probe : MonoBehaviour {
    void Awake() { bool self = isActiveAndEnabled; bool explicitSelf = this.isActiveAndEnabled; }
}
public class Other { public bool isActiveAndEnabled { get { return true; } } }
public static class Observer {
    public static bool Read(Behaviour value, Probe probe, Other other) {
        string name = nameof(value.isActiveAndEnabled);
        return value.isActiveAndEnabled && ((Behaviour)probe).isActiveAndEnabled && other.isActiveAndEnabled;
    }
}
''')
        result = self.compile()
        proof = result["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual(proof["activeAndEnabledReads"], 4)
        self.assertEqual(proof["callbacks"], 1)

    def test_nullable_property_read_fails_without_publishing_dll(self):
        self.write("Assets/Probe.cs", "using UnityEngine; public class Observer { public bool? Read(Behaviour b) => b?.isActiveAndEnabled; }")
        with self.assertRaisesRegex(raw.RawCompileError, "Null-conditional Behaviour.isActiveAndEnabled"):
            self.compile()
        self.assertEqual(list((self.root / "output").glob("*.dll")), [])

    def test_transform_root_bridge_preserves_nameof_and_foreign_property(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public class Probe : MonoBehaviour { Transform Read() => transform.root; }
public class Other { public Other root { get { return this; } } }
public static class Observer {
    public static Transform Read(Transform value, Object obj, Other other) {
        string name = nameof(value.root);
        Other foreign = other.root;
        return value.root == ((Transform)obj).root ? value : null;
    }
}
''')
        result, proof, rewritten = self.compile_with_transformation()
        self.assertEqual(proof["TransformRootReads"], 3)
        self.assertEqual(rewritten.count("RawPrefabTransforms.Root("), 3)
        self.assertIn("nameof(value.root)", rewritten)
        self.assertIn("other.root", rewritten)
        self.assertTrue((self.root / "output/Assembly-CSharp.dll").is_file())

    def test_nullable_transform_root_bridges_preserve_single_evaluation_and_lifted_types(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public static class Observer {
    static int calls;
    static Transform Next() { calls++; return null; }
    public static Transform Direct() => Next()?.root;
    public static Vector3? Value(Transform value) { var p = value?.root.position; return p; }
    public static Transform Child(Transform value) => value?.GetComponent<Transform>().root;
    public static Transform Nested(Transform value) => value?.root?.root;
    public static Transform Collision(Transform value) {
        int __UnityPlay_RootReceiver_0 = 1;
        return __UnityPlay_RootReceiver_0 != 0 ? value?.root : null;
    }
}
''')
        _, proof, rewritten = self.compile_with_transformation()
        self.assertEqual(proof["TransformRootReads"], 6)
        self.assertEqual(rewritten.count("Next()"), 2)  # declaration plus one receiver evaluation
        self.assertIn(" is { } __UnityPlay_RootReceiver_1", rewritten)
        self.assertIn("(global::UnityEngine.Vector3?)null", rewritten)
        self.assertNotIn("?.root", rewritten)

    def test_void_nullable_transform_root_chain_fails_without_publishing_dll(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public static class Observer { public static void Call(Transform value) => value?.root.SendMessage("x"); }
''')
        with self.assertRaisesRegex(raw.RawCompileError, "Void null-conditional chains containing Transform.root"):
            self.compile()
        self.assertEqual(list((self.root / "output").glob("*.dll")), [])

    def test_children_bridge_matches_native_overloads_generic_named_and_foreign_methods(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public class Probe : MonoBehaviour {
    Component Read() => GetComponentInChildren(t: typeof(Transform));
    Transform BaseRead() => base.GetComponentInChildren<Transform>(includeInactive: true);
}
public class Other { public Other GetComponentInChildren() { return this; } }
public static class Observer {
    public static Component Read(Component value, GameObject obj, Other other) {
        string name = nameof(value.GetComponentInChildren);
        Other foreign = other.GetComponentInChildren();
        var a = value.GetComponentInChildren(typeof(Transform));
        var b = value.GetComponentInChildren(includeInactive: true, t: typeof(Transform));
        var c = obj.GetComponentInChildren(typeof(Transform));
        var d = obj.GetComponentInChildren(typeof(Transform), includeInactive: true);
        var e = value.GetComponentInChildren<Transform>();
        var f = value.GetComponentInChildren<Transform>(true);
        var g = obj.GetComponentInChildren<Transform>();
        var h = obj.GetComponentInChildren<Transform>(includeInactive: true);
        return a;
    }
}
''')
        _, proof, rewritten = self.compile_with_transformation()
        self.assertEqual(proof["GetComponentInChildrenCalls"], 10)
        self.assertEqual(rewritten.count("RawPrefabTransforms.GetComponentInChildren"), 10)
        self.assertIn("nameof(value.GetComponentInChildren)", rewritten)
        self.assertIn("other.GetComponentInChildren()", rewritten)
        self.assertRegex(rewritten, r"value,\s*includeInactive: true,\s*type: typeof\(Transform\)")

    def test_nullable_children_and_root_chains_compile_without_repeated_receiver(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public static class Observer {
    static GameObject Next() { return null; }
    public static Transform Direct() => Next()?.GetComponentInChildren<Transform>();
    public static Component Type(GameObject value) => value?.GetComponentInChildren(typeof(Transform), true);
    public static Transform Chain(Transform value) => value?.root.GetComponentInChildren<Transform>();
    public static Transform Nested(Transform value) => value?.root?.GetComponentInChildren<Transform>();
    public static Vector3? Position(Transform value) => value?.GetComponentInChildren<Transform>().position;
}
''')
        _, proof, rewritten = self.compile_with_transformation()
        self.assertEqual(proof["GetComponentInChildrenCalls"], 5)
        self.assertEqual(proof["TransformRootReads"], 2)
        self.assertEqual(rewritten.count("Next()"), 2)
        self.assertNotIn("?.GetComponentInChildren", rewritten)
        self.assertIn("(global::UnityEngine.Vector3?)null", rewritten)

    def test_base_property_receiver_and_named_set_active_arguments_compile(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public class Probe : MonoBehaviour {
    void Awake() { bool active = base.isActiveAndEnabled; gameObject.SetActive(value: true); }
}
''')
        proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual(proof["activeAndEnabledReads"], 1)
        self.assertEqual(proof["setActiveCalls"], 1)

    def test_missing_awake_gets_synthetic_callback_without_shadowing_authored_base(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public class Empty : MonoBehaviour { void Start() {} }
public class Parent : MonoBehaviour { protected void Awake() {} }
public class Derived : Parent { void Start() {} }
''')
        proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual(proof["callbacks"], 2)
        self.assertEqual([(m["Type"], m["Synthetic"]) for m in proof["methods"]],
                         [("Empty", True), ("Parent", False)])

    def test_direct_lifecycle_call_and_set_active_method_group_are_rejected(self):
        self.write("Assets/Probe.cs", "using UnityEngine; public class Probe:MonoBehaviour { public void Awake() {} void Start() { this.Awake(); } }")
        with self.assertRaisesRegex(raw.RawCompileError, "Direct calls/method groups"):
            self.compile()
        self.write("Assets/Probe.cs", "using System; using UnityEngine; public class Probe:MonoBehaviour { void Start() { Action<bool> action = gameObject.SetActive; } }")
        with self.assertRaisesRegex(raw.RawCompileError, "SetActive method groups"):
            self.compile()

    def test_meta_execution_order_overrides_authored_default_attribute(self):
        self.write("Assets/Probe.cs", "using UnityEngine; [DefaultExecutionOrder(123)] public class Probe:MonoBehaviour {}")
        self.write("Assets/Probe.cs.meta", "guid: " + "a" * 32 + "\nMonoImporter:\n  executionOrder: -50\n")
        proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual(proof["methods"][0]["Order"], -50)

    def test_virtual_override_abstract_lifecycle_and_explicit_base_calls_compile(self):
        self.write("Assets/Probe.cs", '''using UnityEngine;
public abstract class Parent : MonoBehaviour {
    protected virtual void Awake() {}
    protected abstract void OnEnable();
}
public class Probe : Parent {
    protected override void Awake() { base.Awake(); }
    protected override void OnEnable() {}
}
''')
        proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual([(method["Type"], method["Phase"]) for method in proof["methods"]],
                         [("Parent", "Awake"), ("Probe", "Awake"), ("Probe", "OnEnable")])

    def test_generic_lifecycle_fails_without_publication(self):
        self.write("Assets/Probe.cs", "using UnityEngine; public class Probe<T>:MonoBehaviour { protected virtual void Awake() {} }")
        with self.assertRaisesRegex(raw.RawCompileError, "generic engine object"):
            self.compile()
        self.assertFalse((self.root / "output/compile.json").exists())

    def test_cross_assembly_source_base_marker_is_verified(self):
        self.write("Assets/Base/Base.asmdef", {"name": "Base", "autoReferenced": False})
        self.write("Assets/Base/Parent.cs", "using UnityEngine; public class Parent:MonoBehaviour { protected void Awake() {} }")
        self.write("Assets/Child/Child.asmdef", {"name": "Child", "references": ["Base"]})
        self.write("Assets/Child/Child.cs", "public class Child:Parent { protected new void Awake() { base.Awake(); } }")
        result = self.compile()
        self.assertEqual([a["name"] for a in result["assemblies"]], ["Base", "Child"])
        self.assertTrue(all(a["lifecycleBarrier"]["verified"] for a in result["assemblies"]))
        self.assertEqual([a["lifecycleBarrier"]["callbacks"] for a in result["assemblies"]], [1, 1])

    def test_unverified_foreign_base_lifecycle_is_rejected(self):
        self.write("Assets/Probe.cs", "public class Probe:ForeignBase {}")
        with self.assertRaisesRegex(raw.RawCompileError, "unverified external assembly"):
            self.compile()

    def test_explicit_native_base_approval_generates_accessible_inherited_bridges(self):
        self.write("Assets/Probe.cs", "public class Probe:VirtualForeignBase {}")
        original = raw._instrument_lifecycle
        with patch.object(raw, "_instrument_lifecycle", side_effect=lambda *args, **kwargs:
                          original(*args[:6], native_base_assemblies=["Dungeon.CLIPlay"])):
            proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual([(method["Phase"], method["Synthetic"]) for method in proof["methods"]],
                         [("Awake", True), ("OnDestroy", True)])

    def test_approved_native_private_base_still_requires_accessible_bridge(self):
        self.write("Assets/Probe.cs", "public class Probe:PrivateForeignBase {}")
        original = raw._instrument_lifecycle
        with patch.object(raw, "_instrument_lifecycle", side_effect=lambda *args, **kwargs:
                          original(*args[:6], native_base_assemblies=["Dungeon.CLIPlay"])):
            with self.assertRaisesRegex(raw.RawCompileError, "inaccessible"):
                self.compile()

    def test_private_runtime_initializers_emit_public_bridges_and_exact_phases(self):
        self.write("Assets/Init.cs", '''using UnityEngine;
internal static class Init {
    [RuntimeInitializeOnLoadMethod] private static void Start() {}
    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.SubsystemRegistration)] private static void Reset() {}
    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.BeforeSceneLoad)] private static void Before() {}
}
''')
        result = self.compile()
        initializers = result["runtimeInitializers"]
        self.assertEqual([item["phase"] for item in initializers], [0, 4, 1])
        self.assertTrue(all(item["assembly"] == "Assembly-CSharp" and item["typeName"] == "Init"
                            and item["methodName"].startswith("__UnityPlay_RuntimeInitialize_") for item in initializers))
        self.assertEqual(len({item["methodName"] for item in initializers}), 3)

    def test_invalid_runtime_initializer_shapes_fail_closed(self):
        for declaration in ["private static int Start() { return 1; }", "private void Start() {}", "private static void Start(int value) {}"]:
            with self.subTest(declaration=declaration):
                self.write("Assets/Init.cs", "using UnityEngine; class Init { [RuntimeInitializeOnLoadMethod] " + declaration + " }")
                with self.assertRaisesRegex(raw.RawCompileError, "static void parameterless"):
                    self.compile()

    def test_private_nested_initializer_is_rejected_and_public_nested_uses_metadata_name(self):
        self.write("Assets/Init.cs", "using UnityEngine; public class Init { private class Nested { [RuntimeInitializeOnLoadMethod] static void Start() {} } }")
        with self.assertRaisesRegex(raw.RawCompileError, "private nested"):
            self.compile()
        self.write("Assets/Init.cs", "using UnityEngine; public class Init { public class Nested { [RuntimeInitializeOnLoadMethod] static void Start() {} } }")
        self.assertEqual(self.compile()["runtimeInitializers"][0]["typeName"], "Init+Nested")

    def test_scriptable_object_callbacks_inherited_virtual_bases_are_instrumented(self):
        self.write("Assets/Config.cs", '''using UnityEngine;
public class Parent : ScriptableObject { protected virtual void Awake() {} }
public class Config : Parent {
    protected override void Awake() { base.Awake(); }
    private void OnEnable() {}
    private void OnDisable() {}
    private void OnDestroy() {}
}
''')
        result = self.compile()
        proof = result["assemblies"][0]["lifecycleBarrier"]
        self.assertTrue(result["capabilities"]["scriptableObjectBarrier"])
        self.assertTrue(proof["scriptableObjectBarrier"])
        self.assertEqual(proof["callbacks"], 5)
        self.assertIn(b"RawScriptableObjectInstrumentedAttribute", (self.root / "output/Assembly-CSharp.dll").read_bytes())

    def test_on_enable_only_scriptable_object_does_not_invent_awake(self):
        self.write("Assets/Config.cs", "using UnityEngine; public class Config:ScriptableObject { private void OnEnable() {} }")
        proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual([(method["Type"], method["Phase"], method["Synthetic"]) for method in proof["methods"]], [("Config", "OnEnable", False)])

    def test_data_only_no_engine_assembly_is_also_verified(self):
        self.write("Assets/Data/Data.asmdef", {"name": "Data", "noEngineReferences": True})
        self.write("Assets/Data/Data.cs", "public class Data { public int Value; }")
        result = self.compile()
        proof = result["assemblies"][0]["lifecycleBarrier"]
        self.assertTrue(proof["verified"])
        self.assertEqual(proof["callbacks"], 0)

    def test_execution_order_metadata_is_read_semantically(self):
        self.write("Assets/Probe.cs", "using UnityEngine; [DefaultExecutionOrder(-123)] public class Probe:MonoBehaviour { void Awake() {} }")
        proof = self.compile()["assemblies"][0]["lifecycleBarrier"]
        self.assertEqual(proof["methods"][0]["Order"], -123)

    def test_verified_code_cache_skips_rewriter_and_retains_proofs(self):
        self.write("Assets/Probe.cs", "using UnityEngine; public class Probe:MonoBehaviour { void Awake() {} }")
        first = self.compile("first", cache=True)
        with patch.object(raw.subprocess, "run", side_effect=AssertionError("cache hit must not invoke compiler")):
            second = self.compile("second", cache=True)
        self.assertTrue(second["cacheHit"])
        self.assertEqual(first["assemblies"][0]["lifecycleBarrier"], second["assemblies"][0]["lifecycleBarrier"])

    def test_partial_cached_phase_proof_is_recompiled_and_repaired(self):
        self.write("Assets/Probe.cs", "using UnityEngine; public class Probe:MonoBehaviour { void Awake() {} }")
        first = self.compile("first", cache=True)
        descriptor = self.root / "code-cache" / first["compilerInputHash"] / "compile.json"
        damaged = json.loads(descriptor.read_text())
        damaged["assemblies"][0]["lifecycleBarrier"]["phases"].remove("OnDisable")
        descriptor.write_text(json.dumps(damaged))
        second = self.compile("second", cache=True)
        self.assertFalse(second["cacheHit"])
        self.assertEqual(json.loads(descriptor.read_text())["assemblies"][0]["lifecycleBarrier"]["phases"],
                         ["Awake", "OnEnable", "OnDisable", "OnDestroy"])

    def test_per_assembly_cached_callback_source_and_initializer_identity_are_verified(self):
        self.write("Assets/A/A.asmdef", {"name": "A", "autoReferenced": False})
        source = self.write("Assets/A/Probe.cs", "using UnityEngine; public class Probe:MonoBehaviour { void Awake() {} [RuntimeInitializeOnLoadMethod] private static void Init() {} }")
        self.write("Assets/B/B.asmdef", {"name": "B", "autoReferenced": False})
        self.write("Assets/B/Other.cs", "public class Other { public static int Value=>1; }")
        first = self.compile("first", cache=True)
        a = next(assembly for assembly in first["assemblies"] if assembly["name"] == "A")
        descriptor = self.root / "code-cache/assemblies" / a["assemblyInputHash"] / "compile.json"
        self.assertEqual(a["lifecycleBarrier"]["methods"][0]["Source"], str(source))
        for index, corrupted_field in enumerate(("callback-source", "initializer-assembly"), 2):
            with self.subTest(field=corrupted_field):
                damaged = json.loads(descriptor.read_text())
                proof = damaged["assemblies"][0]["lifecycleBarrier"]
                if corrupted_field == "callback-source":
                    proof["methods"][0]["Source"] = str(self.project / "Assets/B/Other.cs")
                else:
                    proof["runtimeInitializers"][0]["assembly"] = "OtherAssembly"
                descriptor.write_text(json.dumps(damaged))
                self.write("Assets/B/Other.cs", f"public class Other {{ public static int Value=>{index}; }}")
                repaired = self.compile(f"revision{index}", cache=True)
                a = next(assembly for assembly in repaired["assemblies"] if assembly["name"] == "A")
                self.assertFalse(a["cacheHit"])
                self.assertEqual(a["lifecycleBarrier"]["methods"][0]["Source"], str(source))
                self.assertEqual(a["lifecycleBarrier"]["runtimeInitializers"][0]["assembly"], "A")
                saved = json.loads(descriptor.read_text())["assemblies"][0]["lifecycleBarrier"]
                self.assertEqual(saved, a["lifecycleBarrier"])

    def test_execution_order_metadata_invalidates_only_own_assembly_proof(self):
        self.write("Assets/A/A.asmdef", {"name": "A", "autoReferenced": False})
        self.write("Assets/A/Probe.cs", "using UnityEngine; public class Probe:MonoBehaviour { void Awake() {} }")
        self.write("Assets/A/Probe.cs.meta", "guid: " + "a" * 32 + "\nMonoImporter:\n  executionOrder: -20\n")
        self.write("Assets/B/B.asmdef", {"name": "B", "autoReferenced": False})
        self.write("Assets/B/Other.cs", "public class Other {}")
        self.compile("first", cache=True)
        self.write("Assets/A/Probe.cs.meta", "guid: " + "a" * 32 + "\nMonoImporter:\n  executionOrder: -80\n")
        second = self.compile("second", cache=True)
        self.assertEqual(second["compiledAssemblyCount"], 1)
        self.assertEqual(second["reusedAssemblyCount"], 1)
        a = next(assembly for assembly in second["assemblies"] if assembly["name"] == "A")
        self.assertEqual(a["lifecycleBarrier"]["methods"][0]["Order"], -80)


if __name__ == "__main__":
    unittest.main()
