using System;
using System.Collections.Generic;
using System.IO;

namespace Dungeon.CLIPlay.Raw
{
    internal static class RawActivationPlanner
    {
        // Unity's batch API still runs Awake between individual root activations.
        // An explicit source-reference graph supplies a deterministic supported
        // ordering. Cycles require deferred lifecycle dispatch, not public SetActive.
        public static string[] Order(IList<string> roots, ISet<string> scriptRoots,
            IDictionary<string, HashSet<string>> prerequisites, bool deferredLifecycle = false)
        {
            if (deferredLifecycle)
            {
                var authored = new string[roots.Count];
                roots.CopyTo(authored, 0);
                return authored;
            }
            var result = new List<string>(roots.Count);
            var completed = new HashSet<string>(StringComparer.Ordinal);
            while (result.Count != roots.Count)
            {
                string candidate = Select(roots, completed, prerequisites, scriptRoots, false)
                    ?? Select(roots, completed, prerequisites, scriptRoots, true);
                if (candidate == null)
                    throw new InvalidDataException("Active scene roots contain cyclic serialized references. "
                        + "The raw loader cannot establish all active states before Awake with public activation APIs. "
                        + "Move the dependent initialization to Start, remove the cycle, or place these objects under one authored root.");
                result.Add(candidate); completed.Add(candidate);
            }
            return result.ToArray();
        }

        private static string Select(IList<string> roots, HashSet<string> completed,
            IDictionary<string, HashSet<string>> prerequisites, ISet<string> scriptRoots, bool scripts)
        {
            foreach (string root in roots)
            {
                if (completed.Contains(root) || scriptRoots.Contains(root) != scripts) continue;
                bool ready = true;
                if (prerequisites.TryGetValue(root, out var required))
                    foreach (string dependency in required)
                        if (!completed.Contains(dependency)) { ready = false; break; }
                if (ready) return root;
            }
            return null;
        }
    }
}
