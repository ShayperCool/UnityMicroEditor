using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;
using UnityEngine;
using Object = UnityEngine.Object;

namespace UnityMicroEditor
{
    /// <summary>
    /// Changes the running scene through Unity's public runtime API, for an
    /// agent: objects are named by their hierarchy path or name, as the
    /// hierarchy query lists them. Nothing is written back to the sources.
    /// </summary>
    internal static class AgentRuntimeEdit
    {
        internal static string[] Actions = { "set", "create", "destroy", "add_component", "remove_component", "set_component" };

        internal static object Execute(Dictionary<string, object> arguments, Func<Dictionary<string, object>, GameObject> find)
        {
            string action = Text(arguments, "action");
            GameObject target;
            switch (action)
            {
                case "set":
                    target = Target(arguments, find);
                    SetObject(target, arguments, find);
                    break;
                case "create":
                {
                    string name = Text(arguments, "name");
                    string primitive = Text(arguments, "primitive");
                    target = primitive.Length == 0 ? new GameObject(name.Length == 0 ? "GameObject" : name)
                        : GameObject.CreatePrimitive(ParseEnum<PrimitiveType>(primitive, "primitive"));
                    if (primitive.Length != 0 && name.Length != 0) target.name = name;
                    try { SetObject(target, arguments, find); }
                    catch { Object.Destroy(target); throw; }
                    break;
                }
                case "destroy":
                    target = Target(arguments, find);
                    string path = AgentRuntimeSnapshot.PathOf(target.transform);
                    Object.Destroy(target);
                    return new Dictionary<string, object> { ["destroyed"] = path };
                case "add_component":
                {
                    target = Target(arguments, find);
                    Type type = ComponentType(Text(arguments, "type"));
                    Component added = target.AddComponent(type);
                    if (added == null) throw new InvalidOperationException("Unity did not add " + type.FullName + " to " + target.name + ".");
                    if (arguments.TryGetValue("values", out object values)) SetMembers(added, values, find);
                    break;
                }
                case "remove_component":
                    target = Target(arguments, find);
                    Object.Destroy(FindComponent(target, arguments));
                    break;
                case "set_component":
                    target = Target(arguments, find);
                    if (!arguments.TryGetValue("values", out object fields)) throw new InvalidDataException("set_component requires values.");
                    SetMembers(FindComponent(target, arguments), fields, find);
                    break;
                default:
                    throw new InvalidDataException("Edit action must be one of: " + string.Join(", ", Actions) + ".");
            }
            return AgentRuntimeSnapshot.Capture(target);
        }

        private static GameObject Target(Dictionary<string, object> arguments, Func<Dictionary<string, object>, GameObject> find)
        {
            if (!arguments.TryGetValue("target", out object value) || !(value is Dictionary<string, object> target))
                throw new InvalidDataException("Edit requires a target with a path or name.");
            return find(target);
        }

        private static void SetObject(GameObject target, Dictionary<string, object> arguments, Func<Dictionary<string, object>, GameObject> find)
        {
            if (arguments.TryGetValue("parent", out object parent))
            {
                Transform transform = null;
                if (parent is Dictionary<string, object> reference) transform = find(reference).transform;
                else if (parent != null) throw new InvalidDataException("parent must be a path/name reference or null.");
                bool keepWorld = arguments.TryGetValue("worldPositionStays", out object stays) && stays is bool keep && keep;
                target.transform.SetParent(transform, keepWorld);
            }
            if (arguments.TryGetValue("rename", out object name)) target.name = name as string ?? throw new InvalidDataException("rename must be text.");
            if (arguments.TryGetValue("layer", out object layer))
                target.layer = layer is string layerName ? LayerMask.NameToLayer(layerName) is int found && found >= 0 ? found
                    : throw new InvalidDataException("Unknown layer " + layerName) : Convert.ToInt32(layer, CultureInfo.InvariantCulture);
            if (arguments.TryGetValue("tag", out object tag)) target.tag = tag as string ?? throw new InvalidDataException("tag must be text.");
            var transformValues = new Dictionary<string, object>();
            foreach (string key in new[] { "localPosition", "localEulerAngles", "localRotation", "localScale", "position", "eulerAngles", "rotation" })
                if (arguments.TryGetValue(key, out object value)) transformValues[key] = value;
            if (transformValues.Count != 0) SetMembers(target.transform, transformValues, find);
            if (arguments.TryGetValue("active", out object active))
                target.SetActive(active is bool on ? on : throw new InvalidDataException("active must be true or false."));
        }

        private static Component FindComponent(GameObject target, Dictionary<string, object> arguments)
        {
            Type type = ComponentType(Text(arguments, "type"));
            int index = arguments.TryGetValue("index", out object value) ? Convert.ToInt32(value, CultureInfo.InvariantCulture) : 0;
            Component[] components = target.GetComponents(type);
            if (index < 0 || index >= components.Length)
                throw new InvalidOperationException(target.name + " has " + components.Length + " " + type.Name + " components; index " + index + " is not one.");
            return components[index];
        }

        // A component class by its full name, or by its name when exactly one
        // loaded class has it.
        private static Type ComponentType(string name)
        {
            if (string.IsNullOrEmpty(name)) throw new InvalidDataException("A component type name is required.");
            var matches = new List<Type>();
            foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies())
            {
                Type[] types;
                try { types = assembly.GetTypes(); }
                catch (ReflectionTypeLoadException partial) { types = partial.Types; }
                foreach (Type type in types)
                {
                    if (type == null || !typeof(Component).IsAssignableFrom(type) || type.IsAbstract || type.ContainsGenericParameters) continue;
                    if (type.FullName == name) return type;
                    if (type.Name == name) matches.Add(type);
                }
            }
            if (matches.Count == 1) return matches[0];
            throw new InvalidOperationException(matches.Count == 0 ? "No component class is named " + name + "."
                : "Several component classes are named " + name + "; give the full name.");
        }

        // Fields (public or serialized) and public writable properties, by
        // name, with the values converted to their types.
        private static void SetMembers(Object target, object values, Func<Dictionary<string, object>, GameObject> find)
        {
            if (!(values is Dictionary<string, object> map)) throw new InvalidDataException("values must be an object of member names.");
            const BindingFlags flags = BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic;
            foreach (var entry in map)
            {
                bool set = false;
                for (Type type = target.GetType(); type != null && !set; type = type.BaseType)
                {
                    PropertyInfo property = type.GetProperty(entry.Key, flags | BindingFlags.DeclaredOnly);
                    if (property != null && property.CanWrite && property.GetSetMethod() != null && property.GetIndexParameters().Length == 0)
                    {
                        property.SetValue(target, ConvertValue(entry.Value, property.PropertyType, entry.Key, find));
                        set = true;
                        break;
                    }
                    FieldInfo field = type.GetField(entry.Key, flags | BindingFlags.DeclaredOnly);
                    if (field != null && !field.IsInitOnly && (field.IsPublic || field.GetCustomAttribute<SerializeField>() != null))
                    {
                        field.SetValue(target, ConvertValue(entry.Value, field.FieldType, entry.Key, find));
                        set = true;
                    }
                }
                if (!set) throw new InvalidOperationException(target.GetType().FullName + " has no writable member " + entry.Key + ".");
            }
        }

        private static object ConvertValue(object value, Type type, string name, Func<Dictionary<string, object>, GameObject> find)
        {
            if (value == null)
            {
                if (type.IsValueType) throw new InvalidDataException(name + " cannot be null.");
                return null;
            }
            if (type == typeof(string)) return value as string ?? throw new InvalidDataException(name + " must be text.");
            if (type == typeof(bool)) return value is bool flag ? flag : throw new InvalidDataException(name + " must be true or false.");
            if (type.IsEnum)
                return value is string text ? Enum.Parse(type, text, true) : Enum.ToObject(type, Convert.ToInt64(value, CultureInfo.InvariantCulture));
            if (type.IsPrimitive) return Convert.ChangeType(value, type, CultureInfo.InvariantCulture);
            if (type == typeof(Vector2)) { float[] v = Floats(value, 2, name, "x", "y"); return new Vector2(v[0], v[1]); }
            if (type == typeof(Vector3)) { float[] v = Floats(value, 3, name, "x", "y", "z"); return new Vector3(v[0], v[1], v[2]); }
            if (type == typeof(Vector4)) { float[] v = Floats(value, 4, name, "x", "y", "z", "w"); return new Vector4(v[0], v[1], v[2], v[3]); }
            if (type == typeof(Quaternion))
            {
                // Three numbers are Euler angles, four a quaternion.
                if (value is List<object> list && list.Count == 3 || value is Dictionary<string, object> map && !map.ContainsKey("w"))
                { float[] e = Floats(value, 3, name, "x", "y", "z"); return Quaternion.Euler(e[0], e[1], e[2]); }
                float[] q = Floats(value, 4, name, "x", "y", "z", "w"); return new Quaternion(q[0], q[1], q[2], q[3]);
            }
            if (type == typeof(Color)) { float[] c = Floats(value, 4, name, "r", "g", "b", "a"); return new Color(c[0], c[1], c[2], c[3]); }
            if (typeof(Object).IsAssignableFrom(type))
            {
                if (!(value is Dictionary<string, object> reference)) throw new InvalidDataException(name + " must be a path/name reference.");
                GameObject found = find(reference);
                if (type.IsAssignableFrom(typeof(GameObject))) return found;
                if (typeof(Component).IsAssignableFrom(type))
                    return found.GetComponent(type) ?? throw new InvalidOperationException(found.name + " has no " + type.Name + ".");
                throw new InvalidDataException(name + " refers to an asset, which edits cannot name.");
            }
            throw new InvalidDataException(name + " has a type edits do not set: " + type.FullName + ".");
        }

        private static float[] Floats(object value, int count, string name, params string[] keys)
        {
            var result = new float[count];
            if (value is List<object> list && list.Count == count)
                for (int index = 0; index < count; index++) result[index] = Convert.ToSingle(list[index], CultureInfo.InvariantCulture);
            else if (value is Dictionary<string, object> map)
                for (int index = 0; index < count; index++)
                    result[index] = map.TryGetValue(keys[index], out object part) ? Convert.ToSingle(part, CultureInfo.InvariantCulture)
                        : keys[index] == "a" ? 1 : throw new InvalidDataException(name + " lacks " + keys[index] + ".");
            else throw new InvalidDataException(name + " must be " + count + " numbers or an object of " + string.Join(", ", keys) + ".");
            foreach (float part in result)
                if (float.IsNaN(part) || float.IsInfinity(part)) throw new InvalidDataException(name + " must be finite.");
            return result;
        }

        private static T ParseEnum<T>(string text, string name) where T : struct
        {
            if (Enum.TryParse(text, true, out T value)) return value;
            throw new InvalidDataException(name + " must be one of: " + string.Join(", ", Enum.GetNames(typeof(T))) + ".");
        }

        private static string Text(Dictionary<string, object> value, string key)
        { return value.TryGetValue(key, out object item) ? item as string ?? string.Empty : string.Empty; }
    }
}
