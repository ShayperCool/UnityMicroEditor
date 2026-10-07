using System;
using System.Collections.Generic;

namespace UnityMicroEditor.Raw
{
    // Ownership follows the objects created by one import transaction, even
    // when their callbacks move them to another scene or DontDestroyOnLoad.
    internal sealed class RawOwnedObjects<T> where T : class
    {
        private readonly List<T> objects = new List<T>();

        internal void Add(T value)
        {
            if (value == null) throw new ArgumentNullException(nameof(value));
            objects.Add(value);
        }

        internal void Release(Func<T, bool> alive, Action<T> deactivate, Action<T> destroy)
        {
            T[] snapshot = objects.ToArray();
            objects.Clear();
            Exception failure = null;
            foreach (T value in snapshot)
                if (alive(value))
                    try { deactivate(value); }
                    catch (Exception exception) { if (failure == null) failure = exception; }
            foreach (T value in snapshot)
                if (alive(value))
                    try { destroy(value); }
                    catch (Exception exception) { if (failure == null) failure = exception; }
            if (failure != null) throw new InvalidOperationException("Owned raw object cleanup failed.", failure);
        }
    }

    internal static class RawCleanup
    {
        internal static void Run(Action cleanup, ref Exception failure)
        {
            try { cleanup(); }
            catch (Exception exception) { if (failure == null) failure = exception; }
        }
    }
}
