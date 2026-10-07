using System;
using System.Collections.Generic;
using System.IO;
using System.Security.Cryptography;

namespace UnityMicroEditor.Raw
{
    // Content identity, rather than revision paths, allows scene-only revisions
    // to reuse assemblies that are already registered with Unity's native host.
    internal sealed class RawReloadIdentity
    {
        private readonly Dictionary<string, string> assemblies;
        private readonly Dictionary<string, string> packs;
        private readonly string projectRoot;

        internal RawReloadIdentity(string projectRoot, Dictionary<string, string> assemblies, Dictionary<string, string> packs)
        { this.projectRoot = projectRoot; this.assemblies = assemblies; this.packs = packs; }

        internal void RequireCompatible(RawReloadIdentity next)
        {
            if (next == null || projectRoot != next.projectRoot)
                throw new InvalidDataException("A live reload cannot switch source projects; restart the Player.");
            RequireEqual(assemblies, next.assemblies, "source assemblies");
            RequireEqual(packs, next.packs, "native importer packs");
        }

        private static void RequireEqual(Dictionary<string, string> expected, Dictionary<string, string> actual, string name)
        {
            if (expected.Count != actual.Count) throw new InvalidDataException("Changed " + name + " require a Player restart.");
            foreach (var pair in expected)
                if (!actual.TryGetValue(pair.Key, out string value) || value != pair.Value)
                    throw new InvalidDataException("Changed " + name + " require a Player restart: " + pair.Key);
        }

        internal static string HashFile(string path)
        {
            using (var hash = SHA256.Create())
            using (var stream = File.OpenRead(path)) return Hex(hash.ComputeHash(stream));
        }

        internal static string Hex(byte[] bytes)
        {
            var characters = new char[bytes.Length * 2];
            const string digits = "0123456789abcdef";
            for (int index = 0; index < bytes.Length; index++)
            { characters[index * 2] = digits[bytes[index] >> 4]; characters[index * 2 + 1] = digits[bytes[index] & 15]; }
            return new string(characters);
        }
    }
}
