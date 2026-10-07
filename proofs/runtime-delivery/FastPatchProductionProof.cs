using System;
using System.IO;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace UnityPlay.WorkerProbe
{
    public sealed class FastPatchProductionProof : MonoBehaviour
    {
        public GameObject targetObject;
        public Transform targetTransform;
        private string targetInstanceID;
        private string reportPath, phasePath, lastReport;
        private double nextPoll;
        private int baselineSceneRootCount;

        private void Start()
        {
            reportPath = RequiredPath("UNITY_PLAY_FAST_PATCH_REPORT");
            phasePath = RequiredPath("UNITY_PLAY_FAST_PATCH_PHASE_FILE");
            if (targetObject == null || targetTransform == null || targetTransform.gameObject != targetObject)
                throw new InvalidDataException("Fast patch observer references were not restored.");
            targetInstanceID = targetObject.GetEntityId().ToString();
            targetTransform.localPosition = new Vector3(100, 2, 3);
            targetObject.layer = 7;
            targetObject.tag = "Player";
            targetObject.SetActive(false);
            baselineSceneRootCount = gameObject.scene.rootCount;
            Publish();
        }

        private void Update()
        {
            double now = Time.realtimeSinceStartupAsDouble;
            if (now < nextPoll) return;
            nextPoll = now + .025;
            Publish();
        }

        private void Publish()
        {
            if (string.IsNullOrEmpty(reportPath)) return;
            int phase = 0;
            if (File.Exists(phasePath)) int.TryParse(File.ReadAllText(phasePath).Trim(), out phase);
            var report = new PatchReport { phase = phase, observerActive = gameObject.activeInHierarchy, baselineSceneRootCount = baselineSceneRootCount };
            if (targetObject == null || targetTransform == null) report.error = "Existing target was destroyed or replaced.";
            else
            {
                report.targetInstanceID = targetObject.GetEntityId().ToString();
                report.sameTargetInstance = report.targetInstanceID == targetInstanceID;
                report.targetName = targetObject.name;
                report.localPosition = targetTransform.localPosition;
                report.layer = targetObject.layer;
                report.tag = targetObject.tag;
                report.activeSelf = targetObject.activeSelf;
                report.activeInHierarchy = targetObject.activeInHierarchy;
                report.childCount = targetTransform.childCount;
                if (report.childCount != 0)
                {
                    Transform child = targetTransform.GetChild(0);
                    report.childName = child.name;
                    report.childLocalPosition = child.localPosition;
                    report.childWorldPosition = child.position;
                    report.childParentRestored = child.parent == targetTransform;
                }
                Scene scene = gameObject.scene;
                GameObject[] roots = scene.GetRootGameObjects();
                report.sceneRootCount = roots.Length;
                report.sceneRootNames = new string[roots.Length];
                for (int index = 0; index < roots.Length; index++)
                {
                    report.sceneRootNames[index] = roots[index].name;
                    if (roots[index].name == "RootAdded") report.rootAdded = true;
                    if (roots[index] == targetObject || roots[index] == gameObject || roots[index].name == "RootAdded") report.authoredSceneRootCount++;
                }
                Array.Sort(report.sceneRootNames, StringComparer.Ordinal);
                bool runtimeState = report.sameTargetInstance && report.observerActive
                    && report.localPosition == new Vector3(100, 2, 3) && report.layer == 7
                    && report.tag == "Player" && !report.activeSelf && !report.activeInHierarchy;
                bool name = report.targetName == (phase == 0 ? "OriginalTarget" : "RenamedTarget");
                bool childExpected = phase < 2 ? report.childCount == 0 : report.childCount == 1 && report.childName == "ChildAdded"
                    && report.childParentRestored && report.childLocalPosition == new Vector3(1, 0, 0)
                    && report.childWorldPosition == new Vector3(101, 2, 3);
                bool root = phase < 3 ? report.sceneRootCount == baselineSceneRootCount && report.authoredSceneRootCount == 2 && !report.rootAdded
                    : report.sceneRootCount == baselineSceneRootCount + 1 && report.authoredSceneRootCount == 3 && report.rootAdded;
                report.success = runtimeState && name && childExpected && root;
                if (!report.success) report.error = "Waiting for expected authored phase while checking preserved runtime state.";
            }
            string json = JsonUtility.ToJson(report, true);
            if (json == lastReport) return;
            string temporary = reportPath + ".tmp";
            File.WriteAllText(temporary, json);
            if (File.Exists(reportPath)) File.Replace(temporary, reportPath, null);
            else File.Move(temporary, reportPath);
            lastReport = json;
        }

        private static string RequiredPath(string name)
        {
            string path = Environment.GetEnvironmentVariable(name);
            if (string.IsNullOrEmpty(path) || !Path.IsPathRooted(path)) throw new InvalidDataException(name + " must be absolute.");
            return path;
        }

        [Serializable]
        private sealed class PatchReport
        {
            public int phase;
            public bool success, observerActive, sameTargetInstance;
            public string targetInstanceID;
            public string targetName, tag, error;
            public Vector3 localPosition;
            public int layer;
            public bool activeSelf, activeInHierarchy;
            public int childCount;
            public string childName;
            public Vector3 childLocalPosition, childWorldPosition;
            public bool childParentRestored;
            public int sceneRootCount;
            public int baselineSceneRootCount, authoredSceneRootCount;
            public string[] sceneRootNames;
            public bool rootAdded;
        }
    }
}
