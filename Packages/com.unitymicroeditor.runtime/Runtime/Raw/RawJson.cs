using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text;

namespace UnityMicroEditor.Raw
{
    // The manifest stores Unity's serialized field map as a JSON string. This small
    // parser lets us replace source GUID/fileID references with runtime instance IDs
    // before handing serialization to Unity's public JsonUtility API.
    internal static class RawJson
    {
        public static object Parse(string source)
        {
            var reader = new Reader(source);
            object value = reader.ReadValue(0);
            reader.Finish();
            return value;
        }

        public static string Write(object value)
        {
            var output = new StringBuilder();
            Append(output, value);
            return output.ToString();
        }

        private static void Append(StringBuilder output, object value)
        {
            if (value == null) { output.Append("null"); return; }
            if (value is string text) { Quote(output, text); return; }
            if (value is bool boolean) { output.Append(boolean ? "true" : "false"); return; }
            if (value is Dictionary<string, object> map)
            {
                output.Append('{');
                bool first = true;
                foreach (var pair in map)
                {
                    if (!first) output.Append(',');
                    first = false;
                    Quote(output, pair.Key); output.Append(':'); Append(output, pair.Value);
                }
                output.Append('}'); return;
            }
            if (value is List<object> list)
            {
                output.Append('[');
                for (int index = 0; index < list.Count; index++)
                { if (index != 0) output.Append(','); Append(output, list[index]); }
                output.Append(']'); return;
            }
            output.Append(Convert.ToString(value, CultureInfo.InvariantCulture));
        }

        private static void Quote(StringBuilder output, string value)
        {
            output.Append('"');
            foreach (char character in value)
            {
                switch (character)
                {
                    case '"': output.Append("\\\""); break;
                    case '\\': output.Append("\\\\"); break;
                    case '\n': output.Append("\\n"); break;
                    case '\r': output.Append("\\r"); break;
                    case '\t': output.Append("\\t"); break;
                    default:
                        if (character < 32) output.Append("\\u").Append(((int)character).ToString("x4"));
                        else output.Append(character);
                        break;
                }
            }
            output.Append('"');
        }

        private sealed class Reader
        {
            private readonly string source;
            private int index;
            public Reader(string source) { this.source = source ?? throw new ArgumentNullException(nameof(source)); }
            public void Finish() { Space(); if (index != source.Length) Fail("Trailing JSON data"); }
            public object ReadValue(int depth)
            {
                if (depth > 128) Fail("JSON nesting limit exceeded");
                Space();
                if (index == source.Length) Fail("Unexpected end of JSON");
                char next = source[index];
                if (next == '"') return ReadString();
                if (next == '{')
                {
                    index++; var map = new Dictionary<string, object>(StringComparer.Ordinal); Space();
                    if (Consume('}')) return map;
                    do
                    {
                        Space(); string name = ReadString(); Space(); Require(':');
                        if (map.ContainsKey(name)) Fail("Duplicate JSON property " + name);
                        map.Add(name, ReadValue(depth + 1)); Space();
                        if (Consume('}')) return map;
                        Require(',');
                    } while (true);
                }
                if (next == '[')
                {
                    index++; var list = new List<object>(); Space();
                    if (Consume(']')) return list;
                    do
                    {
                        list.Add(ReadValue(depth + 1)); Space();
                        if (Consume(']')) return list;
                        Require(',');
                    } while (true);
                }
                if (next == 't') { Literal("true"); return true; }
                if (next == 'f') { Literal("false"); return false; }
                if (next == 'n') { Literal("null"); return null; }
                int start = index;
                if (Consume('-')) { }
                if (index == source.Length || !char.IsDigit(source[index])) Fail("Invalid JSON number");
                if (Consume('0')) { }
                else while (index < source.Length && char.IsDigit(source[index])) index++;
                if (Consume('.'))
                {
                    int digits = index;
                    while (index < source.Length && char.IsDigit(source[index])) index++;
                    if (digits == index) Fail("Invalid JSON fraction");
                }
                if (index < source.Length && (source[index] == 'e' || source[index] == 'E'))
                {
                    index++; if (index < source.Length && (source[index] == '+' || source[index] == '-')) index++;
                    int digits = index;
                    while (index < source.Length && char.IsDigit(source[index])) index++;
                    if (digits == index) Fail("Invalid JSON exponent");
                }
                string token = source.Substring(start, index - start);
                if (long.TryParse(token, NumberStyles.Integer, CultureInfo.InvariantCulture, out long integer)) return integer;
                if (!double.TryParse(token, NumberStyles.Float, CultureInfo.InvariantCulture, out double number)
                    || double.IsInfinity(number) || double.IsNaN(number)) Fail("Invalid finite JSON number");
                return number;
            }
            private string ReadString()
            {
                Require('"'); var text = new StringBuilder();
                while (index < source.Length)
                {
                    char value = source[index++];
                    if (value == '"') return text.ToString();
                    if (value < 32) Fail("Control character in JSON string");
                    if (value != '\\') { text.Append(value); continue; }
                    if (index == source.Length) Fail("Unfinished JSON escape");
                    switch (source[index++])
                    {
                        case '"': text.Append('"'); break;
                        case '\\': text.Append('\\'); break;
                        case '/': text.Append('/'); break;
                        case 'b': text.Append('\b'); break;
                        case 'f': text.Append('\f'); break;
                        case 'n': text.Append('\n'); break;
                        case 'r': text.Append('\r'); break;
                        case 't': text.Append('\t'); break;
                        case 'u':
                            ushort unicode = 0;
                            if (index + 4 > source.Length || !ushort.TryParse(source.Substring(index, 4), NumberStyles.HexNumber,
                                CultureInfo.InvariantCulture, out unicode)) Fail("Invalid Unicode JSON escape");
                            text.Append((char)unicode); index += 4; break;
                        default: Fail("Invalid JSON escape"); break;
                    }
                }
                Fail("Unterminated JSON string"); return null;
            }
            private void Literal(string value)
            {
                if (index + value.Length > source.Length || source.Substring(index, value.Length) != value) Fail("Invalid JSON literal");
                index += value.Length;
            }
            private void Space() { while (index < source.Length && char.IsWhiteSpace(source[index])) index++; }
            private bool Consume(char value)
            { if (index < source.Length && source[index] == value) { index++; return true; } return false; }
            private void Require(char value) { if (!Consume(value)) Fail("Expected JSON character " + value); }
            private void Fail(string message) { throw new InvalidDataException(message + " at character " + index + "."); }
        }
    }
}
