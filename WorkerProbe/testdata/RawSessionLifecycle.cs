using System;
using System.Collections.Generic;
using System.IO;
using System.Text.Json;
using Dungeon.CLIPlay.Raw;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

// The scene backend deliberately moves source roots away from their original
// scene. The production session and facade above must retire those exact roots.
namespace UnityEngine
{
    public class Object
    {
        public bool alive = true;
        public Action onDestroy;
        public static void DestroyImmediate(Object value)
        { if (value.alive) { value.alive = false; value.onDestroy?.Invoke(); } }
        public static bool operator ==(Object a, Object b)
        { return ReferenceEquals(a, b) || ((ReferenceEquals(a, null) || !a.alive) && (ReferenceEquals(b, null) || !b.alive)); }
        public static bool operator !=(Object a, Object b) { return !(a == b); }
        public override bool Equals(object value) { return ReferenceEquals(this, value); }
        public override int GetHashCode() { return base.GetHashCode(); }
        public int GetEntityId() { return GetHashCode(); }
    }
    public sealed class GameObject : Object
    {
        public string name;
        public Scene scene;
        public bool active = true;
        public GameObject(string name) { this.name = name; }
        public void SetActive(bool value) { active = value; }
    }
    public sealed class AssetBundle : Object { public void Unload(bool destroy) { alive = false; } }
    public sealed class AsyncOperation { public bool isDone = true; }
    public static class JsonUtility
    {
        public static T FromJson<T>(string data)
        { return JsonSerializer.Deserialize<T>(data, new JsonSerializerOptions { IncludeFields = true }); }
    }
}
namespace UnityEngine.SceneManagement
{
    public struct Scene : IEquatable<Scene>
    {
        public int handle;
        public bool IsValid() { return handle > 0; }
        public bool isLoaded { get { return SceneManager.loaded.Contains(handle); } }
        public bool Equals(Scene value) { return handle == value.handle; }
        public override bool Equals(object value) { return value is Scene other && Equals(other); }
        public override int GetHashCode() { return handle; }
    }
    public static class SceneManager
    {
        internal static readonly HashSet<int> loaded = new HashSet<int>();
        private static int next = 1;
        public static Scene CreateScene(string name)
        { var value = new Scene { handle = next++ }; loaded.Add(value.handle); return value; }
        public static AsyncOperation UnloadSceneAsync(Scene scene)
        { loaded.Remove(scene.handle); return new AsyncOperation(); }
    }
}
namespace Dungeon.CLIPlay.Raw
{
    [Serializable] public sealed class RawSceneManifest
    {
        public string projectRoot;
        public string[] assemblies;
        public ImportedAssetPack[] importedAssetPacks;
        public bool failBeforeCommit;
        public bool failActivation;
    }
    public sealed class ImportedAssetPack
    { public string bundlePath; public string sha256; public uint crc; public string registryAsset; }
    internal sealed class NativeContentHandle { }
    public static partial class RawSceneLoader
    {
        internal static readonly List<GameObject> created = new List<GameObject>();
        internal static readonly Scene persistent = SceneManager.CreateScene("DontDestroyOnLoad");
        internal static Action<GameObject> activation;
        internal static int releasedScopes;
        internal static Action releasingNative;
        private static RawSceneManifest ReadManifest(string path) { return ParseManifest(File.ReadAllText(path)); }
        private static RawSceneManifest ParseManifest(string data) { return JsonUtility.FromJson<RawSceneManifest>(data); }
        private static string AssetPath(RawSceneManifest manifest, string path) { return Path.Combine(manifest.projectRoot, path); }
        private static Dictionary<string, Type> LoadScriptTypes(RawSceneManifest manifest) { return new Dictionary<string, Type>(); }
        private static void InvokeRuntimeInitializers(RawSceneManifest manifest, int phase) { }
        private static void ReleaseRetiredNativeContent(NativeAssetCache cache) { }
        private static void ReleaseNativeContentCache(NativeAssetCache cache) { releasingNative?.Invoke(); }
        private sealed class ImportContext
        {
            private readonly RawSceneManifest manifest;
            private readonly RawOwnedObjects<GameObject> owned = new RawOwnedObjects<GameObject>();
            private bool released;
            internal ImportContext(RawSceneManifest manifest, Dictionary<string, Type> types, NativeAssetCache cache)
            { this.manifest = manifest; }
            internal Scene Load(bool initializers = true, Action commit = null, bool preloads = true)
            {
                Scene scene = SceneManager.CreateScene("Source scene");
                var source = new GameObject("Session") { scene = scene };
                owned.Add(source); created.Add(source);
                try
                {
                    if (manifest.failBeforeCommit) throw new InvalidOperationException("construction failure");
                    commit?.Invoke();
                    source.scene = persistent;
                    activation?.Invoke(source);
                    if (manifest.failActivation) throw new InvalidOperationException("activation failure");
                    return scene;
                }
                catch
                { ReleaseOwnedSceneObjects(); ReleasePreloadedRootScopes(); SceneManager.UnloadSceneAsync(scene); throw; }
            }
            internal bool TryGetPrefab(string guid, out GameObject prefab) { prefab = null; return false; }
            internal void ReleaseOwnedSceneObjects()
            { owned.Release(value => value != null, value => value.SetActive(false), Object.DestroyImmediate); }
            internal void ReleasePreloadedRootScopes()
            { if (!released) { released = true; releasedScopes++; } }
        }
    }
}
internal static class Program
{
    private const string ShipGUID = "a4c2d5e87cc894d488ffbfd8c52ed16a";
    private static int checks;
    private static void Check(bool value, string message) { if (!value) throw new Exception(message); checks++; }
    private static string Manifest(string root, string revision, bool before = false, bool activation = false)
    {
        string directory = Path.Combine(root, revision); Directory.CreateDirectory(directory);
        string path = Path.Combine(directory, "scene.json");
        File.WriteAllText(path, JsonSerializer.Serialize(new RawSceneManifest { projectRoot = root,
            failBeforeCommit = before, failActivation = activation }, new JsonSerializerOptions { IncludeFields = true }));
        return path;
    }
    private static string Routes(string root, string manifest)
    {
        string path = Path.Combine(root, "routes.json");
        File.WriteAllText(path, JsonSerializer.Serialize(new { schemaVersion = 1, project = root,
            scenes = new[] { new { guid = ShipGUID, manifest, sha256 = RawReloadIdentity.HashFile(manifest) } } }));
        return path;
    }
    private static void Main(string[] args)
    {
        string root = args[0];
        var observer = new GameObject("Unrelated persistent observer") { scene = RawSceneLoader.persistent };
        string initial = Manifest(root, "initial"), next = Manifest(root, "next"), ship = Manifest(root, "ship");
        var session = RawSceneLoader.OpenSession(initial, Routes(root, ship));
        GameObject oldRoot = RawSceneLoader.created[0];
        Check(oldRoot.scene.Equals(RawSceneLoader.persistent), "NGO root did not leave Boot scene in fixture");
        RawSceneLoader.activation = value =>
        { Check(!oldRoot.alive, "Previous DDOL Session still alive during replacement Awake"); Check(observer.alive, "Unrelated persistent observer was destroyed"); };
        session.Reload(next, RawReloadIdentity.HashFile(next));
        RawSceneLoader.activation = null;
        GameObject currentRoot = RawSceneLoader.created[1];
        Check(!oldRoot.alive && currentRoot.alive, "Full reload did not replace source-owned DDOL roots");
        Check(!session.LastReloadRestartRequired && session.ReloadCount == 1, "Successful commit kept restart-required flag");

        string rejected = Manifest(root, "rejected", true);
        try { session.Reload(rejected, RawReloadIdentity.HashFile(rejected)); throw new Exception("Construction failure accepted"); }
        catch (InvalidOperationException) { }
        Check(currentRoot.alive && !RawSceneLoader.created[2].alive, "Precommit rollback destroyed the live scene or leaked staging objects");
        Check(!session.LastReloadRestartRequired, "Precommit rejection invalidated current revision");

        Check(RawProjectContent.TryLoadScene(ShipGUID, out Scene loaded), "Registered raw additive scene did not load");
        GameObject child = RawSceneLoader.created[3];
        child.onDestroy = () => RawProjectContent.UnloadScene(loaded);
        RawProjectContent.UnloadScene(loaded);
        Check(!child.alive && currentRoot.alive && observer.alive, "Additive unload leaked DDOL objects or destroyed the root scene");
        Check(!loaded.isLoaded, "Additive scene native unload was not queued");

        string failedChild = Manifest(root, "failed-child", false, true);
        try { session.LoadAdditive(failedChild, RawReloadIdentity.HashFile(failedChild)); throw new Exception("Failed child activation accepted"); }
        catch (InvalidOperationException) { }
        Check(!RawSceneLoader.created[4].alive && currentRoot.alive && observer.alive,
            "Failed additive activation leaked a DDOL root or destroyed unrelated objects");

        Check(RawProjectContent.TryLoadScene(ShipGUID, out Scene childA), "Child could not load again");
        Scene childB = session.LoadAdditive(ship, RawReloadIdentity.HashFile(ship));
        RawSceneLoader.created[5].onDestroy = () => session.UnloadAdditive(childB);
        currentRoot.onDestroy = () => RawProjectContent.UnloadScene(childA);
        RawSceneLoader.releasingNative = () =>
        { foreach (GameObject value in RawSceneLoader.created) Check(!value.alive, "Native assets were released before source callbacks completed"); };
        session.Dispose(); session.Dispose();
        Check(!currentRoot.alive && !childA.isLoaded && !childB.isLoaded && observer.alive,
            "Session disposal leaked owned content or deleted persistent observer");

        var ownership = new RawOwnedObjects<GameObject>();
        var a = new GameObject("Throwing callback"); var b = new GameObject("Second owned object"); ownership.Add(a); ownership.Add(b);
        try { ownership.Release(value => value != null, value => { if (value == a) throw new Exception("OnDisable"); }, Object.DestroyImmediate); }
        catch (InvalidOperationException) { }
        Check(!a.alive && !b.alive, "A failing callback prevented cleanup of other owned objects");
        var reentrant = new RawOwnedObjects<GameObject>(); var c = new GameObject("Reentrant destroy"); reentrant.Add(c);
        c.onDestroy = () => reentrant.Release(value => value != null, value => value.SetActive(false), Object.DestroyImmediate);
        reentrant.Release(value => value != null, value => value.SetActive(false), Object.DestroyImmediate);
        Check(!c.alive && observer.alive, "Reentrant owned-object cleanup affected unrelated state");
        Console.WriteLine("Raw session lifecycle: " + checks + " assertions passed; no Player or Editor started.");
    }
}
