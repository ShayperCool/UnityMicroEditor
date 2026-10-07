using System;
using Dungeon.CLIPlay.Raw;
using Dungeon.Content;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace UnityPlay.MultiplayerProof
{
    // Opt-in adapter for the game's public content contract. Source scenes and
    // prefabs are supplied by the CLI loader; no Addressables scene/player bundle.
    public sealed class RawContentProvider : ContentRuntimeProvider
    {
        private Scene? loadedShip;
        private string error;
        private bool initialized;

        public override float Progress => initialized ? 1f : 0f;
        public override long PendingDownloadBytes => 0;
        public override string Error => error;
        public override bool ShipSceneLoaded => loadedShip.HasValue && loadedShip.Value.IsValid() && loadedShip.Value.isLoaded;

        public override Awaitable<bool> InitializeAsync(ContentCatalog catalog)
        {
            error = null;
            initialized = catalog != null && catalog.shipScene != null && !string.IsNullOrEmpty(catalog.shipScene.AssetGUID)
                && catalog.networkPlayer != null && !string.IsNullOrEmpty(catalog.networkPlayer.AssetGUID);
            if (!initialized) error = "Raw content catalog is missing a ship scene or network avatar GUID.";
            return Completed(initialized);
        }

        public override Awaitable<GameObject> LoadNetworkPlayerAsync(ContentCatalog catalog)
        {
            error = null;
            GameObject prefab = null;
            try
            {
                if (!initialized || catalog == null || catalog.networkPlayer == null
                    || !RawProjectContent.TryLoadPrefab(catalog.networkPlayer.AssetGUID, out prefab))
                    error = "Network avatar GUID is not registered in the raw project content.";
            }
            catch (Exception failure) { error = failure.Message; }
            return Completed(prefab);
        }

        public override Awaitable<Scene?> LoadShipSceneAsync(ContentCatalog catalog)
        {
            error = null;
            if (ShipSceneLoaded) return Completed(loadedShip);
            loadedShip = null;
            try
            {
                if (!initialized || catalog == null || catalog.shipScene == null
                    || !RawProjectContent.TryLoadScene(catalog.shipScene.AssetGUID, out Scene scene))
                    error = "Ship scene GUID is not registered in the raw project content.";
                else loadedShip = scene;
            }
            catch (Exception failure) { error = failure.ToString(); }
            return Completed(loadedShip);
        }

        public override async Awaitable UnloadShipSceneAsync()
        {
            Scene? previous = loadedShip;
            loadedShip = null;
            if (!previous.HasValue || !previous.Value.IsValid() || !previous.Value.isLoaded) return;
            AsyncOperation operation = RawProjectContent.UnloadScene(previous.Value);
            if (operation != null)
                while (!operation.isDone) await Awaitable.NextFrameAsync();
        }

        private static Awaitable<T> Completed<T>(T value)
        {
            var source = new AwaitableCompletionSource<T>();
            source.SetResult(value);
            return source.Awaitable;
        }
    }
}
