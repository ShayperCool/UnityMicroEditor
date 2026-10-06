using System;
using UnityEngine;

public sealed class RawIterationProbe : MonoBehaviour
{
    [SerializeField] private int revision;
    [SerializeField] private MeshRenderer meshRenderer;
    [SerializeField] private Texture2D texture;
    [SerializeField] private RawIterationData data;
    [SerializeField] private Transform prefabTransform;
    [SerializeField] private Camera sceneCamera;
    [SerializeField] private float degreesPerSecond;
    [SerializeField] private bool enabledProof;
    [SerializeField] private ProbeKind kind;
    [SerializeField] private Transform[] linked;
    private enum ProbeKind { None, Imported }
    private const int CodeRevision = 1;

    private void Awake()
    {
        if (meshRenderer == null || texture == null || data == null || prefabTransform == null || sceneCamera == null)
            throw new InvalidOperationException("Raw serialized references must be assigned before Awake.");
        if (!enabledProof || kind != ProbeKind.Imported || linked == null || linked.Length != 2 || linked[0] != prefabTransform || linked[1] != sceneCamera.transform)
            throw new InvalidOperationException("Raw bool, enum and reference arrays must preserve serialized values.");
        if (transform.parent != null || !sceneCamera.gameObject.activeSelf || !prefabTransform.gameObject.activeSelf)
            throw new InvalidOperationException("Awake must observe source root parents and active states.");
        if (texture.name != "Probe" || meshRenderer.sharedMaterial.name != "Raw Checker" || data.name != "Raw tuning")
            throw new InvalidOperationException("Raw asset names must preserve their source values.");
    }

    private void Start()
    {
        var proof = new Proof
        {
            codeRevision = CodeRevision, revision = revision, dataRevision = data.Revision,
            materialColor = meshRenderer.sharedMaterial.color, texturePixel = texture.GetPixel(0, 0),
            prefabScale = prefabTransform.localScale, background = sceneCamera.backgroundColor,
            textureName = texture.name, materialName = meshRenderer.sharedMaterial.name, dataName = data.name,
        };
        Debug.Log("[unity-raw-proof] " + JsonUtility.ToJson(proof));
    }

    private void Update()
    {
        prefabTransform.Rotate(0, degreesPerSecond * Time.deltaTime, 0, Space.Self);
    }

    [Serializable] private sealed class Proof
    {
        public int codeRevision;
        public int revision;
        public int dataRevision;
        public Color materialColor;
        public Color texturePixel;
        public Vector3 prefabScale;
        public Color background;
        public string textureName;
        public string materialName;
        public string dataName;
    }
}
