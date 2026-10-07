using System;
using UnityEngine;

namespace UnityMicroEditor.Raw
{
    // Native importer output is content only. Scene objects are built by RawSceneLoader.
    public sealed class ImportedAssetRegistry : ScriptableObject
    {
        public int schemaVersion = 1;
        public Entry[] entries;

        [Serializable]
        public sealed class Entry
        {
            public string guid;
            public string fileId;
            public string typeName;
            public string name;
            public UnityEngine.Object value;
        }
    }
}
