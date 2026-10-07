using System;
using System.Collections.Generic;
using System.IO;
using System.Text.RegularExpressions;

namespace UnityMicroEditor.Raw
{
    internal static class ImportedAssetRuntimePaths
    {
        internal static void Set(Dictionary<string, object> root, string path, object value)
        {
            if (string.IsNullOrEmpty(path)) throw new InvalidDataException("Deferred binding property path is empty.");
            string normalized = path.Replace(".Array.data[", "[");
            MatchCollection matches = Regex.Matches(normalized, @"([^\.\[\]]+)|\[(\d+)\]");
            if (matches.Count == 0) throw new InvalidDataException("Deferred binding property path is empty.");
            int consumed = 0;
            foreach (Match token in matches)
            {
                string separator = normalized.Substring(consumed, token.Index - consumed);
                if (separator != "" && separator != ".") throw new InvalidDataException("Unsupported deferred binding property path: " + path);
                if (token.Index != 0 && token.Groups[1].Success && separator != ".")
                    throw new InvalidDataException("Unsupported deferred binding property path: " + path);
                if (token.Groups[2].Success && separator != "")
                    throw new InvalidDataException("Unsupported deferred binding property path: " + path);
                consumed = token.Index + token.Length;
            }
            if (consumed != normalized.Length || matches[0].Index != 0 || !matches[0].Groups[1].Success)
                throw new InvalidDataException("Unsupported deferred binding property path: " + path);
            object current = root;
            for (int index = 0; index < matches.Count; index++)
            {
                Match token = matches[index];
                bool last = index == matches.Count - 1;
                if (token.Groups[2].Success)
                {
                    if (!(current is List<object> list) || !int.TryParse(token.Groups[2].Value, out int item) || item < 0 || item >= list.Count)
                        throw new InvalidDataException("Deferred binding array index is missing: " + path);
                    if (last) list[item] = value; else current = list[item];
                }
                else
                {
                    string field = token.Groups[1].Value;
                    if (!(current is Dictionary<string, object> map) || !map.TryGetValue(field, out object child))
                        throw new InvalidDataException("Deferred binding serialized field is missing: " + path);
                    if (last) map[field] = value; else current = child;
                }
            }
        }
    }
}
