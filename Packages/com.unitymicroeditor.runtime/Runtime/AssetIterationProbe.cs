using System;
using UnityEngine;

namespace Dungeon.CLIPlay
{
    /// <summary>Serialized test content used to verify a cooked bundle, once at scene startup.</summary>
    public sealed class AssetIterationProbe : MonoBehaviour
    {
        [SerializeField] private Material material;
        [SerializeField] private Texture2D texture;
        [SerializeField] private GameObject prefab;
        [SerializeField] private AssetIterationData data;
        [SerializeField] private Camera sceneCamera;
        private static readonly int BaseColor = Shader.PropertyToID("_BaseColor");

        private void Start()
        {
            if (material == null || texture == null || prefab == null || data == null || sceneCamera == null)
            {
                Debug.LogError("[unity-cli-play] asset-probe missing serialized content");
                return;
            }
            var proof = new Proof
            {
                revision = data.revision,
                materialColor = material.GetColor(BaseColor),
                texturePixel = texture.GetPixel(0, 0),
                textureWidth = texture.width,
                prefabName = prefab.name,
                prefabScale = prefab.transform.localScale,
                sceneBackground = sceneCamera.backgroundColor,
            };
            Debug.Log("[unity-cli-play] asset-probe " + JsonUtility.ToJson(proof));
        }

        [Serializable]
        private sealed class Proof
        {
            public int revision;
            public Color materialColor;
            public Color texturePixel;
            public int textureWidth;
            public string prefabName;
            public Vector3 prefabScale;
            public Color sceneBackground;
        }
    }
}
