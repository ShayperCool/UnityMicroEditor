using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Threading;
using UnityMicroEditor.Raw;
using UnityEngine;
using UnityEngine.SceneManagement;
using UnityEngine.Scripting;

namespace UnityMicroEditor
{
    [Preserve]
    [AttributeUsage(AttributeTargets.Method, AllowMultiple = false, Inherited = false)]
    public sealed class AgentToolAttribute : Attribute
    {
        public AgentToolAttribute(string name) { Name = name; }
        public string Name { get; }
        public string Description { get; set; } = string.Empty;
        public bool ReadOnly { get; set; }
    }

    [Preserve]
    [AttributeUsage(AttributeTargets.Field, AllowMultiple = false, Inherited = true)]
    public sealed class AgentToolArgumentAttribute : Attribute
    {
        public string Description { get; set; } = string.Empty;
        public bool Required { get; set; } = true;
        public double Minimum { get; set; } = double.NaN;
        public double Maximum { get; set; } = double.NaN;
        public int MinLength { get; set; } = -1;
        public int MaxLength { get; set; } = -1;
    }

    public sealed class AgentToolExecutionException : InvalidOperationException
    {
        public AgentToolExecutionException(string message, Exception inner) : base(message, inner) { }
    }

    /// <summary>Only explicitly attributed public methods and public DTO fields form the agent contract.</summary>
    [Preserve]
    public static class AgentRuntimeTools
    {
        private sealed class Tool
        {
            internal MethodInfo Method;
            internal AgentToolAttribute Attribute;
            internal Type Input;
            internal Dictionary<string, object> InputSchema, OutputSchema;
        }

        private static int mainThread;
        private static string assemblyIdentity;
        private static readonly Dictionary<string, Tool> tools = new Dictionary<string, Tool>(StringComparer.Ordinal);
        private static readonly List<object> discoveryErrors = new List<object>();

        [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.SubsystemRegistration)]
        private static void Initialize()
        { mainThread = Thread.CurrentThread.ManagedThreadId; assemblyIdentity = null; tools.Clear(); discoveryErrors.Clear(); }

        internal static void BindMainThread()
        {
            if (mainThread == 0) mainThread = Thread.CurrentThread.ManagedThreadId;
            RequireMainThread();
        }

        private static void RequireMainThread()
        {
            if (mainThread == 0 || mainThread != Thread.CurrentThread.ManagedThreadId)
                throw new InvalidOperationException("Agent tools must execute on Unity's bound main thread.");
        }

        public static string DescribeJson() { return RawJson.Write(Describe()); }

        public static string InvokeJson(string name, string parametersJson, GameObject target = null)
        { return RawJson.Write(Invoke(name, RawJson.Parse(parametersJson), target)); }

        internal static Dictionary<string, object> Describe()
        {
            RequireMainThread(); Discover();
            var values = new List<object>(); var errors = new List<object>(discoveryErrors);
            var legacy = new HashSet<string>(StringComparer.Ordinal);
            foreach (string name in AgentRuntimeCommands.Names()) legacy.Add(name);
            var names = new List<string>(tools.Keys); names.Sort(StringComparer.Ordinal);
            foreach (string name in names)
            {
                Tool tool = tools[name];
                if (legacy.Contains(name))
                { errors.Add(Error(name, "collision", "An attributed tool and a legacy registration use the same name.")); continue; }
                var descriptor = new Dictionary<string, object>
                {
                    ["name"] = name, ["description"] = tool.Attribute.Description ?? string.Empty,
                    ["readOnly"] = tool.Attribute.ReadOnly, ["inputSchema"] = tool.InputSchema,
                    ["outputSchema"] = tool.OutputSchema, ["binding"] = tool.Method.IsStatic ? "static" : "component",
                    ["declaringType"] = tool.Method.DeclaringType.FullName, ["method"] = tool.Method.Name,
                };
                if (!tool.Method.IsStatic)
                {
                    var targets = new List<object>();
                    foreach (MonoBehaviour component in Instances(tool.Method.DeclaringType))
                    {
                        var target = new Dictionary<string, object> { ["path"] = AgentRuntimeSnapshot.PathOf(component.transform), ["enabled"] = component.enabled,
                            ["activeInHierarchy"] = component.gameObject.activeInHierarchy };
                        targets.Add(target);
                    }
                    if (targets.Count == 0) continue;
                    descriptor["requiresTarget"] = true; descriptor["targets"] = targets;
                }
                values.Add(descriptor);
            }
            foreach (string name in AgentRuntimeCommands.Names())
            {
                bool rejected = tools.ContainsKey(name);
                foreach (Dictionary<string, object> error in discoveryErrors) if ((string)error["name"] == name) rejected = true;
                if (rejected) continue;
                values.Add(new Dictionary<string, object> { ["name"] = name, ["description"] = "Legacy explicitly registered project command.",
                    ["readOnly"] = false, ["binding"] = "legacy", ["inputSchema"] = ObjectSchema(true), ["outputSchema"] = new Dictionary<string, object>() });
            }
            return new Dictionary<string, object> { ["tools"] = values, ["errors"] = errors,
                ["instanceLifetime"] = "current_live_scene_components", ["thread"] = "unity_main_thread" };
        }

        internal static object Invoke(string name, object parameters, GameObject target = null)
        {
            RequireMainThread(); Discover();
            bool legacy = false; foreach (string entry in AgentRuntimeCommands.Names()) if (entry == name) legacy = true;
            if (!tools.TryGetValue(name ?? string.Empty, out Tool tool))
            {
                foreach (Dictionary<string, object> error in discoveryErrors)
                    if ((string)error["name"] == name) throw new InvalidOperationException("Tool has a rejected contract or collision: " + name);
                if (legacy)
                {
                    try { return AgentRuntimeCommands.Execute(name, parameters); }
                    catch (Exception error) { throw new AgentToolExecutionException("Legacy project tool failed after execution began: " + error.Message, error); }
                }
                throw new InvalidOperationException("Unknown attributed project tool: " + name);
            }
            if (legacy) throw new InvalidOperationException("Tool name collides with a legacy registration: " + name);
            object receiver = null;
            if (tool.Method.IsStatic)
            { if (target != null) throw new InvalidDataException("A static tool does not accept a component target."); }
            else
            {
                if (target == null) throw new InvalidDataException("An instance tool requires an explicit unique source object target.");
                foreach (Component component in target.GetComponents(tool.Method.DeclaringType))
                {
                    if (component == null) continue;
                    if (receiver != null) throw new InvalidOperationException("Tool component target is ambiguous.");
                    receiver = component;
                }
                if (receiver == null) throw new InvalidOperationException("Tool component is no longer present on its target.");
            }
            object[] args;
            if (tool.Input == null)
            {
                if (!(parameters is Dictionary<string, object> empty) || empty.Count != 0) throw new InvalidDataException("A parameterless tool requires an empty arguments object.");
                args = Array.Empty<object>();
            }
            else { ValidateInput(parameters, tool.Input, 0); args = null; }
            try
            {
                // Validate the entire JSON contract before any project DTO
                // constructor or attributed method can execute.
                if (tool.Input != null) args = new[] { Read(parameters, tool.Input, 0) };
                object result = tool.Method.Invoke(receiver, args);
                object encoded = tool.Method.ReturnType == typeof(void) ? new Dictionary<string, object>() : Write(result, tool.Method.ReturnType, 0);
                if (RawJson.Write(encoded).Length > 1024 * 1024) throw new InvalidDataException("Project tool output exceeds one MiB.");
                return encoded;
            }
            catch (Exception error)
            { throw new AgentToolExecutionException("Project tool failed after execution began: " + (error.InnerException ?? error).Message, error.InnerException ?? error); }
        }

        private static void Discover()
        {
            var assemblies = EngineCompat.LoadedAssemblies();
            var selected = new List<Assembly>(); var identities = new List<string>(); string runtimeName = typeof(AgentToolAttribute).Assembly.GetName().Name;
            foreach (Assembly assembly in assemblies)
            {
                // An assembly loaded from memory is the code reload applier or one of
                // its patches. A patch repeats types of the game; it adds no tool and
                // must not make the catalog look like another set of assemblies.
                if (assembly.IsDynamic || assembly == typeof(AgentToolAttribute).Assembly || string.IsNullOrEmpty(assembly.Location)) continue;
                bool references = false;
                foreach (AssemblyName reference in assembly.GetReferencedAssemblies()) if (reference.Name == runtimeName) { references = true; break; }
                if (!references) continue;
                selected.Add(assembly); identities.Add(assembly.FullName + ":" + assembly.ManifestModule.ModuleVersionId);
            }
            identities.Sort(StringComparer.Ordinal); string signature = string.Join("\n", identities);
            if (signature == assemblyIdentity) return;
            assemblyIdentity = signature; tools.Clear(); discoveryErrors.Clear();
            var rejected = new HashSet<string>(StringComparer.Ordinal);
            foreach (Assembly assembly in selected)
            {
                Type[] types;
                try { types = assembly.GetExportedTypes(); }
                catch (ReflectionTypeLoadException error)
                { discoveryErrors.Add(Error(assembly.GetName().Name, "assembly_load", error.Message)); continue; }
                foreach (Type type in types)
                    foreach (MethodInfo method in type.GetMethods(BindingFlags.Public | BindingFlags.Static | BindingFlags.Instance | BindingFlags.DeclaredOnly))
                    {
                        var attribute = Attribute.GetCustomAttribute(method, typeof(AgentToolAttribute), false) as AgentToolAttribute;
                        if (attribute == null) continue;
                        string name = attribute.Name ?? string.Empty;
                        try
                        {
                            if (!ValidName(name)) throw new InvalidDataException("Tool name must use 1..128 ASCII letters, digits, dots, underscores or hyphens.");
                            if (method.ContainsGenericParameters || method.IsAbstract || method.IsSpecialName || !type.IsVisible)
                                throw new InvalidDataException("A tool must be a concrete, public, non-generic method on a public type.");
                            if (!method.IsStatic && !typeof(MonoBehaviour).IsAssignableFrom(type)) throw new InvalidDataException("Instance tools must belong to MonoBehaviour types.");
                            ParameterInfo[] parameters = method.GetParameters();
                            if (parameters.Length > 1 || (parameters.Length == 1 && (parameters[0].ParameterType.IsByRef || parameters[0].IsOut)))
                                throw new InvalidDataException("A tool accepts zero parameters or one public DTO parameter.");
                            Type input = parameters.Length == 0 ? null : parameters[0].ParameterType;
                            Dictionary<string, object> inputSchema = input == null ? ObjectSchema(false) : BuildSchema(input);
                            if (!inputSchema.TryGetValue("type", out object inputKind) || (string)inputKind != "object")
                                throw new InvalidDataException("The input DTO must be a JSON object.");
                            var output = method.ReturnType == typeof(void) ? ObjectSchema(false) : BuildSchema(method.ReturnType);
                            if (tools.ContainsKey(name) || rejected.Contains(name)) throw new InvalidDataException("Multiple attributed methods use this name.");
                            if (tools.Count >= 256) throw new InvalidDataException("Project tool count exceeds 256.");
                            tools.Add(name, new Tool { Method = method, Attribute = attribute, Input = input, InputSchema = inputSchema, OutputSchema = output });
                        }
                        catch (Exception error)
                        { tools.Remove(name); rejected.Add(name); discoveryErrors.Add(Error(name, "contract_rejected", type.FullName + "." + method.Name + ": " + error.Message)); }
                    }
            }
        }

        private static List<MonoBehaviour> Instances(Type type)
        {
            var values = new List<MonoBehaviour>();
            foreach (GameObject root in AgentRuntimeSnapshot.LiveRoots())
                foreach (MonoBehaviour component in root.GetComponentsInChildren<MonoBehaviour>(true))
                    if (component != null && component.GetComponent<RawSceneResources>() == null && !(component is AgentCaptureRunner) && !(component is AgentAudioTap) && type.IsInstanceOfType(component))
                        values.Add(component);
            return values;
        }

        private static Dictionary<string, object> BuildSchema(Type type)
        { int nodes = 0; return Schema(type, new HashSet<Type>(), 0, ref nodes); }
        private static Dictionary<string, object> Schema(Type type, HashSet<Type> parents, int depth, ref int nodes)
        {
            if (depth > 8 || ++nodes > 512) throw new InvalidDataException("DTO schema exceeds depth eight or 512 schema nodes.");
            Type nullable = Nullable.GetUnderlyingType(type);
            if (nullable != null) return new Dictionary<string, object> { ["anyOf"] = new List<object> { Schema(nullable, parents, depth + 1, ref nodes), new Dictionary<string, object> { ["type"] = "null" } } };
            if (type == typeof(string)) return new Dictionary<string, object> { ["type"] = "string", ["maxLength"] = 65536 };
            if (type == typeof(bool)) return new Dictionary<string, object> { ["type"] = "boolean" };
            if (Integral(type)) return new Dictionary<string, object> { ["type"] = "integer" };
            if (type == typeof(float) || type == typeof(double)) return new Dictionary<string, object> { ["type"] = "number" };
            if (type.IsEnum) { var names = new List<object>(); foreach (string name in Enum.GetNames(type)) names.Add(name); return new Dictionary<string, object> { ["type"] = "string", ["enum"] = names }; }
            Type element = Element(type);
            if (element != null) return new Dictionary<string, object> { ["type"] = "array", ["items"] = Schema(element, parents, depth + 1, ref nodes), ["maxItems"] = 1024 };
            if (type == typeof(Dictionary<string, object>)) return ObjectSchema(true);
            if (!type.IsVisible || typeof(UnityEngine.Object).IsAssignableFrom(type) || type.IsInterface || type.IsAbstract || type.IsPointer || type.IsGenericType
                || (!type.IsValueType && type.GetConstructor(Type.EmptyTypes) == null)) throw new InvalidDataException("Unsupported public DTO type: " + type.FullName);
            if (!parents.Add(type)) throw new InvalidDataException("Recursive DTO types are unsupported.");
            var properties = new Dictionary<string, object>(); var required = new List<object>();
            foreach (FieldInfo field in Fields(type))
            {
                if (field.IsInitOnly) throw new InvalidDataException("DTO fields must be writable: " + field.Name);
                var schema = Schema(field.FieldType, parents, depth + 1, ref nodes); var annotation = FieldAttribute(field);
                ApplyConstraints(schema, field.FieldType, annotation);
                properties.Add(field.Name, schema); if (annotation == null || annotation.Required) required.Add(field.Name);
            }
            parents.Remove(type);
            var result = ObjectSchema(false); result["properties"] = properties; result["required"] = required;
            return result;
        }

        private static void ValidateInput(object value, Type type, int depth)
        {
            if (depth > 8) throw new InvalidDataException("DTO input exceeds depth eight.");
            Type nullable = Nullable.GetUnderlyingType(type);
            if (nullable != null) { if (value != null) ValidateInput(value, nullable, depth + 1); return; }
            Type element = Element(type);
            if (element != null)
            { if (!(value is List<object> list) || list.Count > 1024) throw new InvalidDataException("Expected an array with at most 1024 items."); foreach (object entry in list) ValidateInput(entry, element, depth + 1); return; }
            if (type == typeof(string) || type == typeof(bool) || Integral(type) || type == typeof(float) || type == typeof(double) || type.IsEnum)
            { Read(value, type, depth); return; }
            if (!(value is Dictionary<string, object> map)) throw new InvalidDataException("Expected a DTO object: " + type.Name);
            if (type == typeof(Dictionary<string, object>)) { int nodes = 0; JsonValue(map, depth, ref nodes); return; }
            var accepted = new HashSet<string>(StringComparer.Ordinal);
            foreach (FieldInfo field in Fields(type))
            {
                accepted.Add(field.Name); AgentToolArgumentAttribute annotation = FieldAttribute(field);
                if (!map.TryGetValue(field.Name, out object raw))
                { if (annotation == null || annotation.Required) throw new InvalidDataException("Missing required DTO field: " + field.Name); continue; }
                ValidateConstraints(raw, annotation, field.Name); ValidateInput(raw, field.FieldType, depth + 1);
            }
            foreach (string name in map.Keys) if (!accepted.Contains(name)) throw new InvalidDataException("Unknown DTO field: " + name);
        }

        private static object Read(object value, Type type, int depth)
        {
            if (depth > 8) throw new InvalidDataException("DTO input exceeds depth eight.");
            Type nullable = Nullable.GetUnderlyingType(type);
            if (nullable != null) return value == null ? null : Read(value, nullable, depth + 1);
            if (type == typeof(string)) { if (!(value is string text) || text.Length > 65536) throw new InvalidDataException("Expected a bounded string."); return text; }
            if (type == typeof(bool)) { if (!(value is bool)) throw new InvalidDataException("Expected a boolean."); return value; }
            if (Integral(type)) { if (!(value is long)) throw new InvalidDataException("Expected an integer."); return Convert.ChangeType(value, type, CultureInfo.InvariantCulture); }
            if (type == typeof(float) || type == typeof(double))
            { if (!(value is long) && !(value is double)) throw new InvalidDataException("Expected a number."); double number = Convert.ToDouble(value, CultureInfo.InvariantCulture); if (double.IsNaN(number) || double.IsInfinity(number) || (type == typeof(float) && Math.Abs(number) > float.MaxValue)) throw new InvalidDataException("Expected a finite number."); return Convert.ChangeType(number, type, CultureInfo.InvariantCulture); }
            if (type.IsEnum) { if (!(value is string name) || !Enum.IsDefined(type, name)) throw new InvalidDataException("Expected a declared enum name."); return Enum.Parse(type, name, false); }
            Type element = Element(type);
            if (element != null)
            {
                if (!(value is List<object> list) || list.Count > 1024) throw new InvalidDataException("Expected an array with at most 1024 items.");
                Array array = Array.CreateInstance(element, list.Count); for (int index = 0; index < list.Count; index++) array.SetValue(Read(list[index], element, depth + 1), index);
                if (type.IsArray) return array;
                var result = (IList)Activator.CreateInstance(type); foreach (object entry in array) result.Add(entry); return result;
            }
            if (!(value is Dictionary<string, object> map)) throw new InvalidDataException("Expected a DTO object: " + type.Name);
            if (type == typeof(Dictionary<string, object>)) { int nodes = 0; return JsonValue(map, depth, ref nodes); }
            object dto = Activator.CreateInstance(type); var accepted = new HashSet<string>(StringComparer.Ordinal);
            foreach (FieldInfo field in Fields(type))
            {
                accepted.Add(field.Name); var annotation = FieldAttribute(field);
                if (!map.TryGetValue(field.Name, out object raw))
                { if (annotation == null || annotation.Required) throw new InvalidDataException("Missing required DTO field: " + field.Name); continue; }
                ValidateConstraints(raw, annotation, field.Name); field.SetValue(dto, Read(raw, field.FieldType, depth + 1));
            }
            foreach (string name in map.Keys) if (!accepted.Contains(name)) throw new InvalidDataException("Unknown DTO field: " + name);
            return dto;
        }

        private static object Write(object value, Type type, int depth)
        {
            if (depth > 8) throw new InvalidDataException("DTO output exceeds depth eight.");
            Type nullable = Nullable.GetUnderlyingType(type);
            if (value == null) { if (nullable != null) return null; throw new InvalidDataException("Tool output is null for a non-nullable schema: " + type.Name); }
            if (nullable != null) return Write(value, nullable, depth + 1);
            if (type == typeof(string)) { if (((string)value).Length > 65536) throw new InvalidDataException("Tool output string exceeds bounds."); return value; }
            if (type == typeof(bool) || Integral(type)) return value;
            if (type == typeof(float) || type == typeof(double)) { double number = Convert.ToDouble(value, CultureInfo.InvariantCulture); if (double.IsNaN(number) || double.IsInfinity(number)) throw new InvalidDataException("Tool output number is nonfinite."); return number; }
            if (type.IsEnum) { if (!Enum.IsDefined(type, value)) throw new InvalidDataException("Tool output contains an undeclared enum value."); return value.ToString(); }
            Type element = Element(type);
            if (element != null) { var list = new List<object>(); foreach (object item in (IEnumerable)value) { if (list.Count >= 1024) throw new InvalidDataException("Tool output array exceeds bounds."); list.Add(Write(item, element, depth + 1)); } return list; }
            if (type == typeof(Dictionary<string, object>)) { int nodes = 0; return JsonValue(value, depth, ref nodes); }
            var result = new Dictionary<string, object>();
            foreach (FieldInfo field in Fields(type))
            {
                object entry = field.GetValue(value); var annotation = FieldAttribute(field);
                if (entry == null && annotation != null && !annotation.Required) continue;
                object encoded = Write(entry, field.FieldType, depth + 1); ValidateConstraints(encoded, annotation, field.Name); result.Add(field.Name, encoded);
            }
            return result;
        }

        // Arbitrary JSON dictionaries still have a bounded JSON-only contract.
        // Never let RawJson's fallback formatting serialize project objects.
        private static object JsonValue(object value, int depth, ref int nodes)
        {
            if (depth > 8 || ++nodes > 4096) throw new InvalidDataException("JSON dictionary exceeds depth eight or 4096 values.");
            if (value == null || value is bool) return value;
            if (value is string text) { if (text.Length > 65536) throw new InvalidDataException("JSON string exceeds bounds."); return text; }
            if (Integral(value.GetType())) return Convert.ToInt64(value, CultureInfo.InvariantCulture);
            if (value is double || value is float)
            { double number = Convert.ToDouble(value, CultureInfo.InvariantCulture); if (double.IsNaN(number) || double.IsInfinity(number)) throw new InvalidDataException("JSON number must be finite."); return number; }
            if (value is Dictionary<string, object> map)
            {
                if (map.Count > 1024) throw new InvalidDataException("JSON object exceeds 1024 properties.");
                var result = new Dictionary<string, object>();
                foreach (var entry in map)
                { if (entry.Key.Length > 65536) throw new InvalidDataException("JSON property name exceeds bounds."); result.Add(entry.Key, JsonValue(entry.Value, depth + 1, ref nodes)); }
                return result;
            }
            if (value is IList list)
            { if (list.Count > 1024) throw new InvalidDataException("JSON array exceeds 1024 items."); var result = new List<object>(); foreach (object entry in list) result.Add(JsonValue(entry, depth + 1, ref nodes)); return result; }
            throw new InvalidDataException("Arbitrary tool dictionaries may contain only JSON values: " + value.GetType().FullName);
        }

        private static FieldInfo[] Fields(Type type)
        {
            var fields = new List<FieldInfo>(); foreach (FieldInfo field in type.GetFields(BindingFlags.Public | BindingFlags.Instance)) if (!field.IsNotSerialized) fields.Add(field);
            if (fields.Count > 64) throw new InvalidDataException("DTO exceeds 64 public fields.");
            fields.Sort((a, b) => StringComparer.Ordinal.Compare(a.Name, b.Name)); return fields.ToArray();
        }
        private static AgentToolArgumentAttribute FieldAttribute(FieldInfo field) { return Attribute.GetCustomAttribute(field, typeof(AgentToolArgumentAttribute), true) as AgentToolArgumentAttribute; }
        private static void ApplyConstraints(Dictionary<string, object> schema, Type type, AgentToolArgumentAttribute annotation)
        {
            if (annotation == null) return;
            if (!string.IsNullOrEmpty(annotation.Description)) schema["description"] = annotation.Description;
            bool numeric = Integral(type) || type == typeof(float) || type == typeof(double);
            if (!double.IsNaN(annotation.Minimum)) { if (!numeric || double.IsInfinity(annotation.Minimum)) throw new InvalidDataException("Minimum requires a finite numeric DTO field."); schema["minimum"] = annotation.Minimum; }
            if (!double.IsNaN(annotation.Maximum)) { if (!numeric || double.IsInfinity(annotation.Maximum)) throw new InvalidDataException("Maximum requires a finite numeric DTO field."); schema["maximum"] = annotation.Maximum; }
            if (!double.IsNaN(annotation.Minimum) && !double.IsNaN(annotation.Maximum) && annotation.Minimum > annotation.Maximum) throw new InvalidDataException("Minimum exceeds maximum.");
            if (annotation.MinLength >= 0) { if (type != typeof(string)) throw new InvalidDataException("MinLength requires a string DTO field."); schema["minLength"] = annotation.MinLength; }
            if (annotation.MaxLength >= 0) { if (type != typeof(string) || annotation.MaxLength > 65536) throw new InvalidDataException("MaxLength requires a bounded string DTO field."); schema["maxLength"] = annotation.MaxLength; }
            if (annotation.MinLength > 65536 || (annotation.MaxLength >= 0 && annotation.MinLength > annotation.MaxLength)) throw new InvalidDataException("String length constraints are inconsistent.");
        }
        private static void ValidateConstraints(object value, AgentToolArgumentAttribute annotation, string field)
        {
            if (annotation == null) return;
            if ((!double.IsNaN(annotation.Minimum) || !double.IsNaN(annotation.Maximum)) && value != null
                && (Integral(value.GetType()) || value is double || value is float))
            { double number = Convert.ToDouble(value, CultureInfo.InvariantCulture); if ((!double.IsNaN(annotation.Minimum) && number < annotation.Minimum) || (!double.IsNaN(annotation.Maximum) && number > annotation.Maximum)) throw new InvalidDataException("DTO numeric constraint failed: " + field); }
            if (value is string text && ((annotation.MinLength >= 0 && text.Length < annotation.MinLength) || (annotation.MaxLength >= 0 && text.Length > annotation.MaxLength))) throw new InvalidDataException("DTO string length constraint failed: " + field);
        }
        private static Type Element(Type type) { if (type.IsArray && type.GetArrayRank() == 1) return type.GetElementType(); if (type.IsGenericType && type.GetGenericTypeDefinition() == typeof(List<>)) return type.GetGenericArguments()[0]; return null; }
        private static bool Integral(Type type) { return type == typeof(byte) || type == typeof(sbyte) || type == typeof(short) || type == typeof(ushort) || type == typeof(int) || type == typeof(uint) || type == typeof(long); }
        private static Dictionary<string, object> ObjectSchema(bool arbitrary) { return new Dictionary<string, object> { ["type"] = "object", ["properties"] = new Dictionary<string, object>(), ["additionalProperties"] = arbitrary }; }
        private static Dictionary<string, object> Error(string name, string code, string message) { return new Dictionary<string, object> { ["name"] = name, ["code"] = code, ["message"] = message }; }
        private static bool ValidName(string name) { if (name.Length < 1 || name.Length > 128) return false; foreach (char c in name) if (c > 127 || (!char.IsLetterOrDigit(c) && c != '_' && c != '-' && c != '.')) return false; return true; }
    }
}
