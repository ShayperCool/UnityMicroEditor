using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace UnityMicroEditor.HotReload
{
    // Requests, descriptors and acks are small documents of a fixed shape. An own
    // reader keeps the applier free of engine types, so it also runs on plain Mono.
    // Objects are Dictionary<string, object>, arrays List<object>, numbers long or double.
    internal static class Json
    {
        private const int DepthLimit = 32;

        internal static object Parse(string text)
        {
            int at = 0;
            object value = Value(text, ref at, 0);
            Space(text, ref at);
            if (at != text.Length) throw new FormatException("JSON has content after its value at offset " + at + ".");
            return value;
        }

        internal static string Text(Dictionary<string, object> map, string key)
        {
            if (!map.TryGetValue(key, out object value) || value == null) return string.Empty;
            return value as string ?? throw new FormatException("JSON member " + key + " must be a string.");
        }

        internal static long Integer(Dictionary<string, object> map, string key)
        {
            if (!map.TryGetValue(key, out object value) || value == null) return 0;
            return value is long number ? number : throw new FormatException("JSON member " + key + " must be an integer.");
        }

        internal static List<object> Array(Dictionary<string, object> map, string key)
        {
            if (!map.TryGetValue(key, out object value) || value == null) return new List<object>();
            return value as List<object> ?? throw new FormatException("JSON member " + key + " must be an array.");
        }

        internal static string Write(object value)
        {
            var output = new StringBuilder();
            Append(output, value);
            return output.ToString();
        }

        private static void Append(StringBuilder output, object value)
        {
            if (value == null) output.Append("null");
            else if (value is string text) Quote(output, text);
            else if (value is bool boolean) output.Append(boolean ? "true" : "false");
            else if (value is double number) output.Append(number.ToString("R", CultureInfo.InvariantCulture));
            else if (value is Dictionary<string, object> map)
            {
                output.Append('{');
                bool first = true;
                foreach (KeyValuePair<string, object> pair in map)
                {
                    if (!first) output.Append(',');
                    first = false;
                    Quote(output, pair.Key); output.Append(':'); Append(output, pair.Value);
                }
                output.Append('}');
            }
            else if (value is List<object> list)
            {
                output.Append('[');
                for (int index = 0; index < list.Count; index++)
                { if (index != 0) output.Append(','); Append(output, list[index]); }
                output.Append(']');
            }
            else output.Append(Convert.ToString(value, CultureInfo.InvariantCulture));
        }

        private static void Quote(StringBuilder output, string value)
        {
            output.Append('"');
            foreach (char character in value)
            {
                if (character == '"') output.Append("\\\"");
                else if (character == '\\') output.Append("\\\\");
                else if (character == '\n') output.Append("\\n");
                else if (character == '\r') output.Append("\\r");
                else if (character == '\t') output.Append("\\t");
                else if (character < ' ') output.Append("\\u").Append(((int)character).ToString("x4", CultureInfo.InvariantCulture));
                else output.Append(character);
            }
            output.Append('"');
        }

        private static object Value(string text, ref int at, int depth)
        {
            if (depth > DepthLimit) throw new FormatException("JSON nesting exceeds " + DepthLimit + " levels.");
            Space(text, ref at);
            if (at >= text.Length) throw new FormatException("JSON ends before its value.");
            char head = text[at];
            if (head == '{')
            {
                var map = new Dictionary<string, object>(StringComparer.Ordinal);
                at++; Space(text, ref at);
                if (Take(text, ref at, '}')) return map;
                do
                {
                    Space(text, ref at);
                    string key = String(text, ref at);
                    Space(text, ref at);
                    if (!Take(text, ref at, ':')) throw new FormatException("JSON object lacks ':' at offset " + at + ".");
                    map[key] = Value(text, ref at, depth + 1);
                    Space(text, ref at);
                }
                while (Take(text, ref at, ','));
                if (!Take(text, ref at, '}')) throw new FormatException("JSON object is not closed at offset " + at + ".");
                return map;
            }
            if (head == '[')
            {
                var list = new List<object>();
                at++; Space(text, ref at);
                if (Take(text, ref at, ']')) return list;
                do
                {
                    list.Add(Value(text, ref at, depth + 1));
                    Space(text, ref at);
                }
                while (Take(text, ref at, ','));
                if (!Take(text, ref at, ']')) throw new FormatException("JSON array is not closed at offset " + at + ".");
                return list;
            }
            if (head == '"') return String(text, ref at);
            if (Word(text, ref at, "true")) return true;
            if (Word(text, ref at, "false")) return false;
            if (Word(text, ref at, "null")) return null;
            int start = at;
            while (at < text.Length && (char.IsDigit(text[at]) || text[at] == '-' || text[at] == '+' || text[at] == '.' || text[at] == 'e' || text[at] == 'E')) at++;
            string number = text.Substring(start, at - start);
            if (long.TryParse(number, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long integer)) return integer;
            if (double.TryParse(number, NumberStyles.Float, CultureInfo.InvariantCulture, out double real)) return real;
            throw new FormatException("JSON has an unexpected token at offset " + start + ".");
        }

        private static string String(string text, ref int at)
        {
            if (!Take(text, ref at, '"')) throw new FormatException("JSON expects a string at offset " + at + ".");
            var value = new StringBuilder();
            while (at < text.Length)
            {
                char character = text[at++];
                if (character == '"') return value.ToString();
                if (character != '\\') { value.Append(character); continue; }
                if (at >= text.Length) break;
                char escape = text[at++];
                if (escape == 'n') value.Append('\n');
                else if (escape == 'r') value.Append('\r');
                else if (escape == 't') value.Append('\t');
                else if (escape == 'b') value.Append('\b');
                else if (escape == 'f') value.Append('\f');
                else if (escape == 'u')
                {
                    if (at + 4 > text.Length || !int.TryParse(text.Substring(at, 4), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out int code))
                        throw new FormatException("JSON has an invalid \\u escape at offset " + at + ".");
                    value.Append((char)code); at += 4;
                }
                else if (escape == '"' || escape == '\\' || escape == '/') value.Append(escape);
                else throw new FormatException("JSON has an invalid escape at offset " + (at - 1) + ".");
            }
            throw new FormatException("JSON string is not closed.");
        }

        private static void Space(string text, ref int at)
        {
            while (at < text.Length && (text[at] == ' ' || text[at] == '\n' || text[at] == '\r' || text[at] == '\t')) at++;
        }

        private static bool Take(string text, ref int at, char expected)
        {
            if (at >= text.Length || text[at] != expected) return false;
            at++; return true;
        }

        private static bool Word(string text, ref int at, string word)
        {
            if (string.CompareOrdinal(text, at, word, 0, word.Length) != 0) return false;
            at += word.Length; return true;
        }
    }
}
