using UnityEngine;

namespace UnityMicroEditor.Raw
{
    // Scene ownership keeps imported textures/materials bounded across reloads.
    public sealed class RawSceneResources : MonoBehaviour
    {
        private Object[] owned;
        private AssetBundle[] bundles;
        private Object[] preloaded;
        private Transform[] prefabTransforms;
        public void Initialize(Object[] assets, AssetBundle[] importedBundles = null, Object[] preloadedAssets = null,
            Transform[] registeredPrefabTransforms = null)
        { owned = assets; bundles = importedBundles; preloaded = preloadedAssets; prefabTransforms = registeredPrefabTransforms; }
        private void OnDestroy()
        {
            RawPrefabTransforms.Unregister(prefabTransforms);
            prefabTransforms = null;
            if (owned != null)
                foreach (Object asset in owned) if (asset != null) Destroy(asset);
            owned = null;
            if (bundles != null)
                foreach (AssetBundle bundle in bundles) if (bundle != null) bundle.Unload(true);
            bundles = null;
            if (preloaded != null)
                for (int index = 0; index < preloaded.Length; index++) preloaded[index] = null;
            preloaded = null;
        }
    }
}
