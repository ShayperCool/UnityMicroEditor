using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;

namespace UnityMicroEditor.Raw
{
    // Unity's binary-YAML collections use hex UInt8 ('y') or little-endian Int32 ('i').
    // Only field type metadata is inspected: object values stay with JsonUtility.
    internal static class RawCompactCollections
    {
        internal sealed class Expectation
        {
            internal readonly object[] Path;
            internal readonly List<object> Values;
            internal Expectation(List<object> path, List<object> values) { Path = path.ToArray(); Values = values; }
        }

        private static readonly Dictionary<Type, Dictionary<string, Type>> FieldTypes = new Dictionary<Type, Dictionary<string, Type>>();

        internal static List<Expectation> Normalize(Type type, Dictionary<string, object> data)
        {
            var expected = new List<Expectation>();
            NormalizeValue(type, data, new List<object>(), expected, 0);
            return expected;
        }

        private static object NormalizeValue(Type type, object value, List<object> path, List<Expectation> expected, int depth)
        {
            if (depth > 64) throw new InvalidDataException("Compact collection nesting exceeds the supported bound at " + Describe(path));
            Type element = type.IsArray && type.GetArrayRank() == 1 ? type.GetElementType() :
                type.IsGenericType && type.GetGenericTypeDefinition() == typeof(List<>) ? type.GetGenericArguments()[0] : null;
            if (element != null)
            {
                if (value is string text)
                {
                    Type storage = element.IsEnum ? Enum.GetUnderlyingType(element) : element;
                    int bytes = storage == typeof(byte) && text.EndsWith("y", StringComparison.Ordinal) ? 1 :
                        storage == typeof(int) && text.EndsWith("i", StringComparison.Ordinal) ? 4 : 0;
                    if (bytes == 0)
                        throw new InvalidDataException("Unsupported compact collection encoding for " + type.FullName + " at " + Describe(path));
                    int digits = text.Length - 1;
                    int elementDigits = bytes * 2;
                    if (digits % elementDigits != 0 || digits > 2 * 1024 * 1024)
                        throw new InvalidDataException("Invalid or oversized compact " + storage.Name + " collection at " + Describe(path));
                    var decoded = new List<object>(digits / elementDigits);
                    for (int index = 0; index < digits; index += elementDigits)
                    {
                        uint word = 0;
                        for (int offset = 0; offset < bytes; offset++)
                        {
                            int high = Hex(text[index + offset * 2]), low = Hex(text[index + offset * 2 + 1]);
                            if (high < 0 || low < 0) throw new InvalidDataException("Invalid compact " + storage.Name + " hex digit at " + Describe(path));
                            word |= (uint)((high << 4) | low) << (offset * 8);
                        }
                        decoded.Add(bytes == 4 ? (long)unchecked((int)word) : (long)word);
                    }
                    expected.Add(new Expectation(path, decoded));
                    return decoded;
                }
                if (value is List<object> list)
                    for (int index = 0; index < list.Count; index++)
                    {
                        path.Add(index); list[index] = NormalizeValue(element, list[index], path, expected, depth + 1); path.RemoveAt(path.Count - 1);
                    }
                else if (value != null) throw new InvalidDataException("Serialized collection is not a list at " + Describe(path));
                return value;
            }
            if (!(value is Dictionary<string, object> map) || map.ContainsKey("fileID") || type == typeof(string)) return value;
            Dictionary<string, Type> fields = SerializedFieldTypes(type);
            foreach (string key in new List<string>(map.Keys))
            {
                if (!fields.TryGetValue(key, out Type field)) continue;
                path.Add(key); map[key] = NormalizeValue(field, map[key], path, expected, depth + 1); path.RemoveAt(path.Count - 1);
            }
            return value;
        }

        private static Dictionary<string, Type> SerializedFieldTypes(Type type)
        {
            lock (FieldTypes)
            {
                if (FieldTypes.TryGetValue(type, out var cached)) return cached;
                var fields = new Dictionary<string, Type>(StringComparer.Ordinal);
                for (Type current = type; current != null && current != typeof(object); current = current.BaseType)
                    foreach (FieldInfo field in current.GetFields(BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly))
                    {
                        if (field.IsStatic || field.IsInitOnly || field.IsLiteral || Attribute.IsDefined(field, typeof(NonSerializedAttribute))) continue;
                        bool serialized = field.IsPublic;
                        foreach (CustomAttributeData attribute in field.GetCustomAttributesData())
                            if (attribute.AttributeType.FullName == "UnityEngine.SerializeField" || attribute.AttributeType.FullName == "UnityEngine.SerializeReference") serialized = true;
                        if (serialized && !fields.ContainsKey(field.Name)) fields.Add(field.Name, field.FieldType);
                    }
                FieldTypes.Add(type, fields);
                return fields;
            }
        }

        internal static void Verify(List<Expectation> expected, object serialized)
        {
            foreach (Expectation collection in expected)
            {
                object current = serialized;
                foreach (object segment in collection.Path)
                {
                    if (segment is string key && current is Dictionary<string, object> map && map.TryGetValue(key, out object nested)) current = nested;
                    else if (segment is int index && current is List<object> list && index >= 0 && index < list.Count) current = list[index];
                    else throw new InvalidDataException("Unity omitted restored compact collection at " + Describe(collection.Path));
                }
                if (!(current is List<object> actual) || actual.Count != collection.Values.Count)
                    throw new InvalidDataException("Unity compact collection length readback differs at " + Describe(collection.Path));
                for (int index = 0; index < actual.Count; index++)
                    if (!(actual[index] is long number) || number != (long)collection.Values[index])
                        throw new InvalidDataException("Unity compact collection value readback differs at " + Describe(collection.Path) + "[" + index + "]");
            }
        }

        private static int Hex(char digit) => digit >= '0' && digit <= '9' ? digit - '0' : digit >= 'a' && digit <= 'f' ? digit - 'a' + 10 : digit >= 'A' && digit <= 'F' ? digit - 'A' + 10 : -1;
        private static string Describe(IReadOnlyList<object> path)
        {
            string result = "$";
            foreach (object segment in path) result += segment is int index ? "[" + index.ToString(CultureInfo.InvariantCulture) + "]" : "." + segment;
            return result;
        }
    }
}
