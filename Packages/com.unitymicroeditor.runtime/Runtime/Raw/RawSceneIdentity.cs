using System;
using System.Collections.Generic;
using UnityEngine;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        public sealed partial class LiveSession
        {
            public Dictionary<string, object> DescribeSourceObject(Object value)
            {
                if (disposed) throw new ObjectDisposedException(nameof(LiveSession));
                Dictionary<string, object> description = current.DescribeSourceObject(value);
                if (description != null) return description;
                foreach (ImportContext context in additive.Values)
                {
                    description = context.DescribeSourceObject(value);
                    if (description != null) return description;
                }
                return null;
            }
        }

        private sealed partial class ImportContext
        {
            private RawSceneObject[] agentIdentityRecords;
            private Dictionary<Object, RawSceneObject> agentSourceIdentities;

            internal Dictionary<string, object> DescribeSourceObject(Object value)
            {
                if (value == null) return null;
                if (!ReferenceEquals(agentIdentityRecords, currentRecords))
                {
                    agentSourceIdentities = new Dictionary<Object, RawSceneObject>();
                    foreach (RawSceneObject candidate in currentRecords)
                        if (sceneObjects.TryGetValue(candidate.fileId, out Object live) && live != null)
                            agentSourceIdentities[live] = candidate;
                    agentIdentityRecords = currentRecords;
                }
                if (!agentSourceIdentities.TryGetValue(value, out RawSceneObject record)) return null;
                return new Dictionary<string, object>
                {
                    ["fileId"] = record.fileId, ["guid"] = record.guid ?? string.Empty,
                    ["sourcePath"] = record.sourcePath ?? string.Empty, ["classId"] = record.classId,
                };
            }
        }
    }
}
