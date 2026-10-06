import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import raw_compile as raw

UNITY = Path(os.environ.get("UNITY_EDITOR", "/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity"))


class RawGraphTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.project = self.root / "project with spaces"
        (self.project / "Assets").mkdir(parents=True)
        self.data = self.root / "installation/Data"
        self.data.mkdir(parents=True)

    def tearDown(self):
        self.temp.cleanup()

    def write(self, relative, value):
        path = self.project / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value) if isinstance(value, dict) else value)
        return path

    def graph(self):
        return raw._graph(self.project, [self.project / "Assets"], {"UNITY_STANDALONE_LINUX"}, {})

    def test_guid_reference_and_nested_asmref_assign_sources(self):
        self.write("Assets/Shared/Shared.asmdef", {"name": "Shared", "autoReferenced": False})
        self.write("Assets/Shared/Shared.asmdef.meta", "guid: " + "a" * 32)
        self.write("Assets/Shared/Foo.cs", "public class Foo {}")
        self.write("Assets/Game/Game.asmdef", {"name": "Game", "references": ["GUID:" + "a" * 32]})
        extra = self.write("Assets/Extra/Extra.asmref", {"reference": "Game"})
        source = self.write("Assets/Extra/Bar.cs", "class Bar : Foo {}")
        graph = self.graph()
        self.assertEqual(graph["Game"].dependencies, ["Shared"])
        self.assertEqual(graph["Game"].sources, [source])
        self.assertEqual([item.name for item in raw._order(graph, set())], ["Shared", "Game"])

    def test_editor_default_sources_are_excluded_but_editor_folder_in_runtime_asmdef_is_not(self):
        self.write("Assets/Editor/Excluded.cs", "compile error")
        source = self.write("Assets/Runtime/Editor/Included.cs", "public class Included {}")
        self.write("Assets/Runtime/Runtime.asmdef", {"name": "Runtime"})
        graph = self.graph()
        self.assertEqual(graph["Assembly-CSharp"].sources, [])
        self.assertEqual(graph["Runtime"].sources, [source])

    def test_runtime_reference_to_editor_only_assembly_fails(self):
        self.write("Assets/Editor/Tools.asmdef", {"name": "Tools", "includePlatforms": ["Editor"]})
        self.write("Assets/Editor/Tool.cs", "class Tool {}")
        self.write("Assets/Game/Game.asmdef", {"name": "Game", "references": ["Tools"]})
        self.write("Assets/Game/Game.cs", "class Game {}")
        with self.assertRaisesRegex(raw.RawCompileError, "excluded assembly Tools"):
            raw._order(self.graph(), set())

    def test_excluded_editor_assembly_can_reference_unavailable_editor_apis(self):
        self.write("Assets/Editor/Tools.asmdef", {"name": "Tools", "includePlatforms": ["Editor"], "references": ["Unity.Some.Editor.Package"]})
        self.write("Assets/Editor/Tool.cs", "class Tool {}")
        self.assertFalse(self.graph()["Tools"].active)

    def test_unity_ignored_package_samples_and_hidden_directories_are_not_compiled(self):
        self.write("Assets/Hidden~/Duplicate.asmdef", {"name": "Game"})
        self.write("Assets/.hidden/Broken.cs", "not valid C#")
        self.write("Assets/Samples~/Broken.cs", "not valid C#")
        self.write("Assets/Game/Game.asmdef", {"name": "Game"})
        good = self.write("Assets/Game/Game.cs", "class Game {}")
        graph = self.graph()
        self.assertEqual(graph["Game"].sources, [good])
        self.assertEqual(graph["Assembly-CSharp"].sources, [])

    def test_missing_reference_and_dependency_cycle_fail(self):
        self.write("Assets/A/A.asmdef", {"name": "A", "references": ["Missing"]})
        with self.assertRaisesRegex(raw.RawCompileError, "missing assembly"):
            self.graph()
        self.write("Assets/A/A.asmdef", {"name": "A", "references": ["B"]})
        self.write("Assets/B/B.asmdef", {"name": "B", "references": ["A"]})
        self.write("Assets/A/A.cs", "class A {}")
        with self.assertRaisesRegex(raw.RawCompileError, "Cyclic assembly"):
            raw._order(self.graph(), set())

    def test_define_constraints_or_and_excluded_tests(self):
        self.write("Assets/A/A.asmdef", {"name": "A", "defineConstraints": ["MISSING || UNITY_STANDALONE_LINUX", "!UNITY_EDITOR"]})
        self.write("Assets/B/B.asmdef", {"name": "B", "defineConstraints": ["UNITY_EDITOR"]})
        self.write("Assets/Test/Test.asmdef", {"name": "Test", "optionalUnityReferences": ["TestAssemblies"]})
        graph = self.graph()
        self.assertTrue(graph["A"].active)
        self.assertFalse(graph["B"].active)
        self.assertFalse(graph["Test"].active)

    def test_embedded_local_transitive_packages_and_version_validation(self):
        self.write("Packages/manifest.json", {"dependencies": {"example.embedded": "1.0.0"}})
        self.write("Packages/example.embedded/package.json", {"name": "example.embedded", "version": "1.0.0", "dependencies": {"example.local": "file:../../local"}})
        local = self.root / "local"
        local.mkdir()
        (local / "package.json").write_text(json.dumps({"name": "example.local", "version": "2.0.0"}))
        roots, versions = raw._package_roots(self.project, self.data)
        self.assertIn(local.resolve(), roots)
        self.assertEqual(versions["example.local"], "2.0.0")
        self.write("Packages/manifest.json", {"dependencies": {"example.embedded": "9.0.0"}})
        with self.assertRaisesRegex(raw.RawCompileError, "version mismatch"):
            raw._package_roots(self.project, self.data)

    def test_registry_cache_is_explicit_and_project_library_is_not_used(self):
        self.write("Packages/manifest.json", {"dependencies": {"example.registry": "1.0.0"}})
        cache = self.write("Library/PackageCache/example.registry@hash/package.json", {"name": "example.registry", "version": "1.0.0"}).parent
        with patch.dict(os.environ, {"UNITY_PLAY_PACKAGE_ROOT": ""}):
            with self.assertRaisesRegex(raw.RawCompileError, "has no raw sources"):
                raw._package_roots(self.project, self.data)
        with patch.dict(os.environ, {"UNITY_PLAY_PACKAGE_ROOT": str(cache)}):
            with self.assertRaisesRegex(raw.RawCompileError, "cannot point"):
                raw._package_roots(self.project, self.data)
        external = self.root / "external-cache/example.registry@hash"
        external.mkdir(parents=True)
        (external / "package.json").write_text((cache / "package.json").read_text())
        with patch.dict(os.environ, {"UNITY_PLAY_PACKAGE_ROOT": str(external.parent)}):
            roots, _ = raw._package_roots(self.project, self.data)
        self.assertEqual(roots, [external])

    def test_file_package_cannot_resolve_into_project_library(self):
        self.write("Packages/manifest.json", {"dependencies": {"example.package": "file:../Library/PackageCache/example.package"}})
        self.write("Library/PackageCache/example.package/package.json", {"name": "example.package", "version": "1.0.0"})
        with self.assertRaisesRegex(raw.RawCompileError, "cannot use.*Library"):
            raw._package_roots(self.project, self.data)

    def test_embedded_package_symlink_cannot_resolve_into_project_library(self):
        self.write("Packages/manifest.json", {"dependencies": {"example.package": "1.0.0"}})
        directory = self.write("Library/PackageCache/example.package/package.json", {"name": "example.package", "version": "1.0.0"}).parent
        (self.project / "Packages/example.package").symlink_to(directory, target_is_directory=True)
        with self.assertRaisesRegex(raw.RawCompileError, "cannot use.*Library"):
            raw._package_roots(self.project, self.data)

    def test_asset_source_symlink_cannot_read_project_library(self):
        source = self.write("Library/GeneratedCode/Foo.cs", "public class Foo {}")
        (self.project / "Assets/Foo.cs").symlink_to(source)
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")
        with self.assertRaisesRegex(raw.RawCompileError, "cannot use.*Library"):
            raw._source_inputs(self.project, [self.project / "Assets"])

    def test_library_symlink_alias_is_rejected_even_when_target_is_external(self):
        target = self.root / "external-library"
        target.mkdir()
        (target / "Foo.cs").write_text("public class Foo {}")
        (self.project / "Library").symlink_to(target, target_is_directory=True)
        (self.project / "Assets/Foo.cs").symlink_to(target / "Foo.cs")
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")
        with self.assertRaisesRegex(raw.RawCompileError, "cannot use.*Library"):
            raw._source_inputs(self.project, [self.project / "Assets"])

    def test_library_name_sibling_is_not_treated_as_library_child(self):
        source = self.write("LibrarySibling/Foo.cs", "public class Foo {}")
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")
        inputs = raw._source_inputs(self.project, [source.parent])
        self.assertIn(str(source.resolve()), inputs)
        self.assertFalse(raw._within(source, self.project / "Library"))

    def test_source_inventory_matches_glob_filtering_without_traversing_directory_symlinks(self):
        self.write("Assets/Runtime/Good.cs", "class Good {}")
        self.write("Assets/.hidden/Bad.cs", "broken")
        self.write("Assets/Samples~/Bad.asmdef", "broken")
        self.write("Assets/.Hidden.cs", "broken")
        self.write("Assets/Docs/Notes.txt", "notes")
        external = self.root / "foreign package"
        external.mkdir()
        (external / "Bad.cs").write_text("broken")
        (self.project / "Assets/LinkedDirectory").symlink_to(external, target_is_directory=True)
        inventory = raw._SourceInventory()
        for pattern in ("*.cs", "*.asmdef", "*.asmref", "*.dll", "*.rsp"):
            self.assertEqual(raw._files(self.project / "Assets", pattern, inventory), raw._files(self.project / "Assets", pattern))

    def test_source_inventory_still_rejects_file_symlink_into_library(self):
        source = self.write("Library/Foo.cs", "public class Foo {}")
        (self.project / "Assets/Foo.cs").symlink_to(source)
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")
        with self.assertRaisesRegex(raw.RawCompileError, "cannot use.*Library"):
            raw._source_inputs(self.project, [self.project / "Assets"], raw._SourceInventory())

    def test_final_sdk_hash_and_overlapping_source_hash_are_checked(self):
        import hashlib
        source = self.write("Assets/Foo.cs", "public class Foo {}")
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")
        inventory = raw._SourceInventory()
        inputs = raw._source_inputs(self.project, [self.project / "Assets"], inventory)
        sdk_file = self.root / "compiler-sdk.dll"
        sdk_file.write_bytes(b"old SDK")
        sdk_hash = hashlib.sha256(sdk_file.read_bytes()).hexdigest()
        sdk_file.write_bytes(b"changed SDK")
        with self.assertRaisesRegex(raw.RawCompileError, "compiler input changed"):
            raw._check_inputs(self.project, [self.project / "Assets"], inputs, {str(sdk_file): sdk_hash}, inventory)
        with self.assertRaisesRegex(raw.RawCompileError, "compiler input changed"):
            raw._check_inputs(self.project, [self.project / "Assets"], inputs, {str(source): "f" * 64}, inventory)

    def test_version_ranges_and_prerelease(self):
        self.assertTrue(raw._matches_version("2.1.3", "[2.0,3.0)"))
        self.assertFalse(raw._matches_version("3.0.0", "[2.0,3.0)"))
        self.assertFalse(raw._matches_version("2.0.0-exp.1", "2.0"))
        self.assertTrue(raw._matches_version("2.0.0-exp.2", "[2.0.0-exp.1,2.0)"))
        self.assertFalse(raw._matches_version("1.0", "[2.0]"))
        with self.assertRaises(raw.RawCompileError):
            raw._matches_version("2.0", "invalid")


@unittest.skipUnless(UNITY.is_file(), "installed Unity SDK required for Roslyn integration")
class RawCompileIntegrationTests(RawGraphTests):
    def setUp(self):
        super().setUp()
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.6.3f1\n")
        self.output = self.root / "compiled output"

    def compile(self):
        return raw.compile_project(self.project, UNITY, self.output)

    def test_cold_monobehaviour_compiles_from_no_library_with_script_guid(self):
        self.write("Assets/Mover.cs", "using UnityEngine; public class Mover: MonoBehaviour { public float Speed=2; void Update() { transform.position += Vector3.right * Speed * Time.deltaTime; } }")
        self.write("Assets/Mover.cs.meta", "guid: " + "b" * 32)
        self.write("Assets/Editor/Broken.cs", "this must not compile")
        result = self.compile()
        self.assertFalse((self.project / "Library").exists())
        self.assertFalse(result["editorStarted"])
        self.assertFalse(result["projectLibraryUsed"])
        self.assertEqual(result["scripts"][0]["guid"], "b" * 32)
        self.assertTrue((self.output / "Assembly-CSharp.dll").read_bytes().startswith(b"MZ"))

    def test_cross_assembly_references_compile_in_dependency_order(self):
        self.write("Assets/A/A.asmdef", {"name": "A", "autoReferenced": False, "noEngineReferences": True})
        self.write("Assets/A/Foo.cs", "public class Foo { public static int Value => 42; }")
        self.write("Assets/B/B.asmdef", {"name": "B", "references": ["A"]})
        self.write("Assets/B/Bar.cs", "using UnityEngine; public class Bar:MonoBehaviour { public int Value=>Foo.Value; }")
        result = self.compile()
        self.assertEqual([item["name"] for item in result["assemblies"]], ["A", "B"])

    def test_native_uxml_generator_emits_serialized_data_type_and_attribute_fields(self):
        self.write("Assets/CornerBrackets.cs", """using UnityEngine.UIElements;
namespace Example {
    [UxmlElement] public partial class CornerBrackets:VisualElement {
        [UxmlAttribute] public int segments { get; set; }
    }
    public static class SerializationProbe {
        public static UxmlSerializedData Create() => new CornerBrackets.UxmlSerializedData();
    }
}""")
        captured = []
        original_run = raw.subprocess.run
        def capture_generated_sources(*args, **kwargs):
            result = original_run(*args, **kwargs)
            for argument in args[0]:
                if str(argument).startswith("@"):
                    root = Path(str(argument)[1:]).parent / "Generated"
                    captured.extend(path.read_text() for path in root.rglob("*.cs"))
            return result
        with patch.object(raw.subprocess, "run", side_effect=capture_generated_sources):
            first = self.cached_compile("first")
        self.assertTrue(first["capabilities"]["unitySourceGenerators"])
        generated = first["assemblies"][0]["generatedSources"]
        self.assertTrue(any("CornerBrackets" in source["path"] for source in generated), generated)
        serialized_source = next(source for source in captured if "class UxmlSerializedData" in source)
        self.assertIn("[global::UnityEngine.SerializeField] int segments;", serialized_source)
        self.assertIn("segments_UxmlAttributeFlags", serialized_source)
        self.assertIn("e.segments = this.segments;", serialized_source)
        self.assertTrue(any(name.endswith("Unity.UIToolkit.SourceGenerator.dll") for name in first["compilerInputs"]))
        with patch.object(raw.subprocess, "run", side_effect=AssertionError("cached generated ABI must not invoke Roslyn")):
            second = self.cached_compile("second")
        self.assertTrue(second["cacheHit"])
        self.assertEqual(second["assemblies"][0]["generatedSources"], generated)

    def test_compile_error_does_not_publish_successful_dependencies(self):
        self.write("Assets/A/A.asmdef", {"name": "A"})
        self.write("Assets/A/Foo.cs", "public class Foo {}")
        self.write("Assets/Broken.cs", "this must not compile")
        with self.assertRaisesRegex(raw.RawCompileError, "Cold compilation failed"):
            self.compile()
        self.assertEqual(list(self.output.glob("*.dll")), [])
        self.assertFalse((self.output / "compile.json").exists())

    def test_implicit_runtime_initializer_requires_explicit_supported_lifecycle(self):
        self.write("Assets/Init.cs", "using UnityEngine; class Init { [RuntimeInitializeOnLoadMethod] static void Start() {} }")
        with self.assertRaisesRegex(raw.RawCompileError, "registration is not supported"):
            self.compile()

    def test_burst_annotation_fails_before_roslyn(self):
        self.write("Assets/Job.cs", "[Unity.Burst.BurstCompile] class Job {}")
        with self.assertRaisesRegex(raw.RawCompileError, "Burst native compilation"):
            self.compile()

    def test_host_engine_references_do_not_duplicate_sdk_assemblies(self):
        import shutil
        host = self.root / "host"
        host.mkdir()
        core = raw._data(UNITY) / "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed/UnityEngine.CoreModule.dll"
        shutil.copy2(core, host / core.name)
        self.write("Assets/Mover.cs", "using UnityEngine; public class Mover:MonoBehaviour {}")
        result = raw.compile_project(self.project, UNITY, self.output, host)
        self.assertEqual(result["assemblies"][0]["name"], "Assembly-CSharp")

    def test_project_sdk_version_mismatch_fails(self):
        self.write("ProjectSettings/ProjectVersion.txt", "m_EditorVersion: 6000.0.0f1\n")
        with self.assertRaisesRegex(raw.RawCompileError, "SDK is"):
            self.compile()

    def test_custom_standalone_defines_are_available(self):
        self.write("ProjectSettings/ProjectSettings.asset", "  scriptingDefineSymbols:\n    Standalone: CUSTOM_FLAG\n    Android: ANDROID_ONLY\n")
        self.write("Assets/Flag.cs", "#if !CUSTOM_FLAG || ANDROID_ONLY\n#error wrong defines\n#endif\npublic class Flag {}")
        result = self.compile()
        self.assertIn("CUSTOM_FLAG", result["defines"])
        self.assertNotIn("ANDROID_ONLY", result["defines"])

    def test_input_fingerprint_ignores_texture_changes_and_tracks_code_changes(self):
        self.write("Assets/Foo.cs", "public class Foo { public int Value=1; }")
        self.write("Assets/Image.png", "arbitrary bytes")
        original = self.compile()
        self.write("Assets/Image.png", "changed image bytes")
        asset_changed = self.compile()
        self.assertEqual(original["compilerInputHash"], asset_changed["compilerInputHash"])
        self.write("Assets/Foo.cs", "public class Foo { public int Value=2; }")
        code_changed = self.compile()
        self.assertNotEqual(original["compilerInputHash"], code_changed["compilerInputHash"])

    def test_source_mutation_during_roslyn_does_not_publish_dlls(self):
        source = self.write("Assets/Foo.cs", "public class Foo { public int Value=1; }")
        original = raw.subprocess.run
        def edit_after_compile(*args, **kwargs):
            result = original(*args, **kwargs)
            source.write_text("public class Foo { public int Value=2; }")
            return result
        with patch.object(raw.subprocess, "run", side_effect=edit_after_compile):
            with self.assertRaisesRegex(raw.RawCompileError, "changed during compilation"):
                self.compile()
        self.assertEqual(list(self.output.glob("*.dll")), [])

    def test_user_assembly_with_host_bridge_name_is_not_silently_skipped(self):
        import shutil
        host = self.root / "Host_Data/Managed"
        host.mkdir(parents=True)
        core = raw._data(UNITY) / "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed/UnityEngine.CoreModule.dll"
        shutil.copy2(core, host / "Dungeon.CLIPlay.dll")
        self.write("Assets/User/Game.asmdef", {"name": "Dungeon.CLIPlay"})
        self.write("Assets/User/UserScript.cs", "public class UserScript {}")
        (self.root / "host.json").write_text(json.dumps({"sourceFiles": []}))
        with self.assertRaisesRegex(raw.RawCompileError, "differs from bundled host bridge"):
            raw.compile_project(self.project, UNITY, self.output, host)

    def cached_compile(self, output_name):
        return raw.compile_project(self.project, UNITY, self.root / output_name,
                                   cache_dir=self.root / "compiler-cache")

    def test_asset_only_edit_reuses_verified_code_cache_without_roslyn(self):
        self.write("Assets/Foo.cs", "public class Foo { public int Value=1; }")
        self.write("Assets/Image.png", "first image")
        original = self.cached_compile("revision1")
        self.assertFalse(original["cacheHit"])
        self.write("Assets/Image.png", "updated image")
        with patch.object(raw.subprocess, "run", side_effect=AssertionError("Roslyn must not run on cache hit")):
            changed = self.cached_compile("revision2")
        self.assertTrue(changed["cacheHit"])
        self.assertEqual(original["compilerInputHash"], changed["compilerInputHash"])
        self.assertEqual(original["assemblies"][0]["sha256"], changed["assemblies"][0]["sha256"])
        self.assertEqual(changed["assemblies"][0]["path"], str(self.root / "revision2/Assembly-CSharp.dll"))

    def test_code_edit_invalidates_cache_and_builds_new_dll(self):
        self.write("Assets/Foo.cs", "public class Foo { public int Value=1; }")
        original = self.cached_compile("revision1")
        self.write("Assets/Foo.cs", "public class Foo { public int Value=2; }")
        changed = self.cached_compile("revision2")
        self.assertFalse(changed["cacheHit"])
        self.assertNotEqual(original["compilerInputHash"], changed["compilerInputHash"])
        self.assertNotEqual(original["assemblies"][0]["sha256"], changed["assemblies"][0]["sha256"])

    def test_corrupt_cached_dll_recompiles_and_repairs_cache(self):
        self.write("Assets/Foo.cs", "public class Foo {}")
        original = self.cached_compile("revision1")
        entry = self.root / "compiler-cache" / original["compilerInputHash"]
        (entry / "Assembly-CSharp.dll").write_bytes(b"corrupt cached DLL")
        repaired = self.cached_compile("revision2")
        self.assertFalse(repaired["cacheHit"])
        self.assertTrue((entry / "Assembly-CSharp.dll").read_bytes().startswith(b"MZ"))
        with patch.object(raw.subprocess, "run", side_effect=AssertionError("repaired cache should hit")):
            cached = self.cached_compile("revision3")
        self.assertTrue(cached["cacheHit"])

    def test_source_mutation_during_cache_hit_does_not_publish(self):
        source = self.write("Assets/Foo.cs", "public class Foo {}")
        self.cached_compile("revision1")
        original = raw._read_cache
        def mutate_after_cache_read(*args):
            value = original(*args)
            source.write_text("public class Foo { public int Changed; }")
            return value
        with patch.object(raw, "_read_cache", side_effect=mutate_after_cache_read):
            with self.assertRaisesRegex(raw.RawCompileError, "changed during compilation"):
                self.cached_compile("revision2")
        self.assertEqual(list((self.root / "revision2").glob("*.dll")), [])

    def test_new_source_during_cache_hit_is_detected_by_fresh_final_inventory(self):
        self.write("Assets/Foo.cs", "public class Foo {}")
        self.cached_compile("revision1")
        original = raw._read_cache
        def add_after_cache_read(*args):
            value = original(*args)
            self.write("Assets/Added.cs", "public class Added {}")
            return value
        with patch.object(raw, "_read_cache", side_effect=add_after_cache_read):
            with self.assertRaisesRegex(raw.RawCompileError, "changed during compilation"):
                self.cached_compile("revision2")
        self.assertEqual(list((self.root / "revision2").glob("*.dll")), [])

    def test_deleted_source_during_cache_hit_is_detected_by_fresh_final_inventory(self):
        self.write("Assets/Foo.cs", "public class Foo {}")
        extra = self.write("Assets/Removed.cs", "public class Removed {}")
        self.cached_compile("revision1")
        original = raw._read_cache
        def remove_after_cache_read(*args):
            value = original(*args)
            extra.unlink()
            return value
        with patch.object(raw, "_read_cache", side_effect=remove_after_cache_read):
            with self.assertRaisesRegex(raw.RawCompileError, "changed during compilation"):
                self.cached_compile("revision2")
        self.assertEqual(list((self.root / "revision2").glob("*.dll")), [])

    def test_malformed_cache_metadata_is_a_miss(self):
        self.write("Assets/Foo.cs", "public class Foo {}")
        original = self.cached_compile("revision1")
        entry = self.root / "compiler-cache" / original["compilerInputHash"]
        (entry / "compile.json").write_text("not JSON")
        repaired = self.cached_compile("revision2")
        self.assertFalse(repaired["cacheHit"])
        self.assertEqual(json.loads((entry / "compile.json").read_text())["cacheSchema"], 1)

    def test_code_cache_cannot_write_into_project_assets(self):
        self.write("Assets/Foo.cs", "public class Foo {}")
        with self.assertRaisesRegex(raw.RawCompileError, "cache cannot be placed"):
            raw.compile_project(self.project, UNITY, self.output, cache_dir=self.project / "Assets/Cache")

    def incremental_fixture(self):
        for name, dependencies, source in (
                ("A", [], "public class A { public static int Value => 1; }"),
                ("B", ["A"], "public class B { public static int Value => A.Value; }"),
                ("C", [], "public class C { public static int Value => 1; }"),
                ("D", [], "public class D { public static int Value => 1; }")):
            self.write(f"Assets/{name}/{name}.asmdef", {"name": name, "references": dependencies, "autoReferenced": False})
            self.write(f"Assets/{name}/{name}.cs", source)

    def test_assembly_cache_rebuilds_changed_dependency_and_reuses_independent_assemblies(self):
        self.incremental_fixture()
        first = self.cached_compile("first")
        self.write("Assets/A/A.cs", "public class A { public static int Value => 2; }")
        original_run = raw.subprocess.run
        with patch.object(raw.subprocess, "run", wraps=original_run) as compiler:
            second = self.cached_compile("second")
        self.assertEqual(compiler.call_count, 2)
        self.assertFalse(second["cacheHit"])
        self.assertEqual(second["compiledAssemblyCount"], 2)
        self.assertEqual(second["reusedAssemblyCount"], 2)
        hits = {assembly["name"]: assembly["cacheHit"] for assembly in second["assemblies"]}
        self.assertEqual(hits, {"A": False, "B": False, "C": True, "D": True})
        originals = {assembly["name"]: assembly for assembly in first["assemblies"]}
        for assembly in second["assemblies"]:
            self.assertEqual(assembly["sources"], originals[assembly["name"]]["sources"])
            if assembly["cacheHit"]:
                self.assertEqual(assembly["sha256"], originals[assembly["name"]]["sha256"])
        with patch.object(raw.subprocess, "run", side_effect=AssertionError("warm full cache must not compile")):
            third = self.cached_compile("third")
        self.assertTrue(third["cacheHit"])
        self.assertEqual(third["compiledAssemblyCount"], 0)
        self.assertEqual(third["reusedAssemblyCount"], 4)

    def test_corrupt_per_assembly_dll_and_pdb_are_recompiled_before_reuse(self):
        self.incremental_fixture()
        result = self.cached_compile("first")
        c = next(assembly for assembly in result["assemblies"] if assembly["name"] == "C")
        entry = self.root / "compiler-cache/assemblies" / c["assemblyInputHash"]
        for index, suffix in enumerate((".dll", ".pdb"), 2):
            with self.subTest(suffix=suffix):
                (entry / ("C" + suffix)).write_bytes(b"tampered assembly payload")
                self.write("Assets/D/D.cs", f"public class D {{ public static int Value => {index}; }}")
                changed = self.cached_compile(f"revision{index}")
                hits = {assembly["name"]: assembly["cacheHit"] for assembly in changed["assemblies"]}
                self.assertEqual(hits, {"A": True, "B": True, "C": False, "D": False})
                self.assertNotEqual((entry / ("C" + suffix)).read_bytes(), b"tampered assembly payload")

    def test_source_metadata_guid_edit_rebuilds_only_its_assembly(self):
        self.incremental_fixture()
        self.write("Assets/C/C.cs.meta", "guid: " + "c" * 32)
        first = self.cached_compile("first")
        self.write("Assets/C/C.cs.meta", "guid: " + "d" * 32)
        second = self.cached_compile("second")
        self.assertEqual(second["compiledAssemblyCount"], 1)
        self.assertEqual(second["reusedAssemblyCount"], 3)
        self.assertEqual(next(script["guid"] for script in first["scripts"] if script["assembly"] == "C"), "c" * 32)
        self.assertEqual(next(script["guid"] for script in second["scripts"] if script["assembly"] == "C"), "d" * 32)


if __name__ == "__main__":
    unittest.main()
