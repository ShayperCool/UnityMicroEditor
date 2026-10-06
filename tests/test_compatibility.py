"""Regression checks for comparison boundaries, not expected scene contents."""
import importlib.util
import json
from pathlib import Path
import unittest
import tempfile

spec = importlib.util.spec_from_file_location("comparison", Path(__file__).resolve().parents[1] / "CompatibilityProbe/run_comparison.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


class ComparisonTests(unittest.TestCase):
    def test_appended_probe_preserves_explicit_scene_root_order(self):
        with tempfile.TemporaryDirectory() as temporary:
            scene = Path(temporary) / "Example.unity"
            scene.write_text("--- !u!1660057539 &99\nSceneRoots:\n  m_ObjectHideFlags: 0\n  m_Roots:\n  - {fileID: 21}\n")
            comparison.append_probe(scene, 800, "Probe", "a" * 32)
            text = scene.read_text()
            self.assertIn("  - {fileID: 21}\n  - {fileID: 801}\n--- !u!1 &800", text)

    def test_nested_mismatch_points_to_field(self):
        errors = comparison.differences({"objects": [{"enabled": True}]}, {"objects": [{"enabled": False}]})
        self.assertEqual(errors[0]["path"], "$.objects[0].enabled")

    def test_bool_does_not_equal_number(self):
        self.assertTrue(comparison.differences(True, 1))

    def test_missing_field_is_distinct_from_null(self):
        errors = comparison.differences({"reference": None}, {})
        self.assertTrue(errors[0]["missingKey"])

    def test_numeric_roundoff_is_tolerated_but_content_change_is_not(self):
        self.assertFalse(comparison.differences(0.5, 0.5000001))
        self.assertTrue(comparison.differences(0.5, 0.501))
        self.assertTrue(comparison.differences(float("nan"), float("nan")))

    def test_serializer_json_uses_same_numeric_tolerance(self):
        self.assertFalse(comparison.differences({"serializedJson": '{"speed": 0.5}'}, {"serializedJson": '{"speed": 0.5000001}'}))
        errors = comparison.differences({"serializedJson": '{"target": "a"}'}, {"serializedJson": '{"target": "b"}'})
        self.assertEqual(errors[0]["path"], "$.serializedJson{json}.target")

    def test_root_interleaving_is_not_lifecycle_order(self):
        a = {"objectPath": "a", "phase": "Awake", "frameCount": 0}
        b = {"objectPath": "b", "phase": "Awake", "frameCount": 8}
        start = {"objectPath": "a", "phase": "Start", "frameCount": 9}
        self.assertEqual(comparison.lifecycle_groups([a, b, start]), comparison.lifecycle_groups([b, a, start]))
        self.assertNotEqual(comparison.lifecycle_groups([a, start]), comparison.lifecycle_groups([start, a]))

    def test_lifecycle_observations_are_not_discarded(self):
        a = {"objectPath": "a", "phase": "Awake", "peerRootActiveInHierarchy": True}
        b = {**a, "peerRootActiveInHierarchy": False}
        self.assertTrue(comparison.differences(comparison.lifecycle_groups([a]), comparison.lifecycle_groups([b])))

    def test_native_active_and_enabled_observations_are_compared(self):
        event = {"objectPath": "a", "phase": "Awake", "isActiveAndEnabled": False,
                 "peerBehaviourIsActiveAndEnabled": False}
        for field in ("isActiveAndEnabled", "peerBehaviourIsActiveAndEnabled"):
            with self.subTest(field=field):
                self.assertTrue(comparison.differences(comparison.lifecycle_groups([event]),
                                                      comparison.lifecycle_groups([{**event, field: True}])))

    def test_execution_order_checks_cross_object_order(self):
        early = {"objectPath": "Compatibility active", "phase": "Awake"}
        late = {"objectPath": "Compatibility peer", "phase": "Awake"}
        self.assertEqual(comparison.lifecycle_groups([early, late]), comparison.lifecycle_groups([late, early]))
        self.assertTrue(comparison.execution_order_errors([late, early], "disable-peer", "raw"))
        self.assertFalse(comparison.execution_order_errors([early, late], "disable-peer", "raw"))
        self.assertFalse(comparison.execution_order_errors([late, early], "cyclic-roots", "raw"))

    def test_execution_order_requires_complete_non_mutating_trace(self):
        events = [{"objectPath": owner, "phase": phase}
                  for phase in ("Awake", "OnEnable", "Start")
                  for owner in ("Compatibility active", "Compatibility peer")]
        self.assertFalse(comparison.execution_order_errors(events, "execution-order", "raw"))
        self.assertTrue(comparison.execution_order_errors(events[:-1], "execution-order", "raw"))
        self.assertFalse(comparison.execution_order_errors([], "destroy-peer-immediate", "raw"))

    def test_dynamic_creation_requires_callbacks_before_addcomponent_returns(self):
        component = {"typeName": "UnityPlay.Compatibility.CompatibilityDynamicCreationProbe",
                     "serializedJson": '{"childAwakeRanBeforeAddReturned": true, "childEnableRanBeforeAddReturned": true}'}
        scene = {"objects": [{"components": [component]}]}
        self.assertFalse(comparison.dynamic_creation_errors(scene, "raw"))
        component["serializedJson"] = '{"childAwakeRanBeforeAddReturned": false, "childEnableRanBeforeAddReturned": true}'
        errors = comparison.dynamic_creation_errors(scene, "raw")
        self.assertEqual(errors[0]["path"], "$.dynamicCreation.raw.childAwakeRanBeforeAddReturned")
        self.assertTrue(comparison.dynamic_creation_errors({"objects": []}, "editor"))

    def test_reentrant_activation_requires_nested_peer_callbacks_before_setactive_returns(self):
        before = {"objectPath": "Compatibility active", "phase": "Awake"}
        peer_awake = {"objectPath": "Compatibility peer", "phase": "Awake"}
        peer_enable = {"objectPath": "Compatibility peer", "phase": "OnEnable"}
        after = {"objectPath": "Compatibility active", "phase": "Awake.AfterActivatePeerRoot"}
        self.assertFalse(comparison.reentrant_activation_errors([before, peer_awake, peer_enable, after], "raw"))
        self.assertTrue(comparison.reentrant_activation_errors([before, after, peer_awake, peer_enable], "raw"))
        self.assertTrue(comparison.reentrant_activation_errors([before, peer_awake, after], "editor"))
        self.assertFalse(comparison.execution_order_errors([peer_enable, before], "reentrant-activation", "raw"))

    def test_fixture_scenes_resolve_real_inherited_scripts_and_peer_components(self):
        import raw_scene
        operations = {"disable-peer": 1, "deactivate-peer": 2, "destroy-peer-immediate": 4,
                      "late-reactivate": 2, "reentrant-activation": 5}
        with tempfile.TemporaryDirectory() as temporary:
            for case in comparison.CASES:
                with self.subTest(case=case):
                    output = Path(temporary) / case
                    project = comparison.fixture(output, case)
                    before = comparison.digest_tree(project)
                    manifest = raw_scene.validate_scene(project, "RawSample")
                    self.assertTrue(manifest["supported"], manifest["diagnostics"])
                    self.assertEqual(comparison.digest_tree(project), before)
                    self.assertFalse((project / "Library").exists())
                    components = {item["fileId"]: item for item in manifest["objects"] if item["classId"] == 114}
                    active = components["802"]
                    active_data = json.loads(active["dataJson"])
                    if case in comparison.ORDERED_CASES or case in ("cyclic-roots", "reentrant-activation"):
                        self.assertEqual(active_data["peerRoot"]["fileID"], "850")
                        self.assertEqual(active_data["peerBehaviour"]["fileID"], "852")
                        peer_data = json.loads(components["852"]["dataJson"])
                        if case != "enable-only-peer":
                            self.assertEqual(peer_data["peerRoot"]["fileID"], "800")
                            self.assertEqual(peer_data["peerBehaviour"]["fileID"], "802")
                    if case in comparison.ORDERED_CASES or case == "reentrant-activation":
                        self.assertEqual(active["typeName"], "UnityPlay.Compatibility.CompatibilityEarlyLifecycleProbe")
                        peer_type = "CompatibilityEnableOnlyProbe" if case == "enable-only-peer" else "CompatibilityLateLifecycleProbe"
                        self.assertEqual(components["852"]["typeName"], "UnityPlay.Compatibility." + peer_type)
                    if case in operations:
                        self.assertEqual(active_data["awakeOperation"], operations[case])
                    if case == "inherited-lifecycle":
                        self.assertEqual(active["typeName"], "UnityPlay.Compatibility.CompatibilityInheritedLifecycleProbe")
                        self.assertEqual(active_data["derivedRevision"], 29)
                    if case == "late-reactivate":
                        self.assertEqual(active_data["reactivatePeerOnStart"], 1)
                    if case == "dynamic-add-component":
                        self.assertEqual(active["typeName"], "UnityPlay.Compatibility.CompatibilityDynamicCreationProbe")
                        self.assertEqual(active_data["created"]["fileID"], "0")
                        self.assertEqual(active_data["childAwakeRanBeforeAddReturned"], 0)
                        self.assertEqual(active_data["childEnableRanBeforeAddReturned"], 0)
                    if case == "reentrant-activation":
                        peer_root = next(item for item in manifest["objects"] if item["fileId"] == "850")
                        self.assertEqual(json.loads(peer_root["dataJson"])["m_IsActive"], 0)


if __name__ == "__main__":
    unittest.main()
