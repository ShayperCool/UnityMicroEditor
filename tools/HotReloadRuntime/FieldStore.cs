using System;
using System.Runtime.CompilerServices;

namespace UnityMicroEditor.HotReload
{
    /// <summary>
    /// Storage of the fields a code patch added to types that are already loaded. Patch assemblies
    /// bind to it by name and reach every added field through one of the two accessors.
    /// Field id: assembly|type id|field.
    /// </summary>
    public static class FieldStore
    {
        private sealed class Slot<T> { internal T Value; }
        private struct Cell { internal string Id; internal object Slot; }
        private sealed class Bag { internal volatile Cell[] Cells = new Cell[0]; }

        // Per value type, so a lookup only passes the static fields of that type.
        private static class Statics<T> { internal static readonly Bag Bag = new Bag(); }

        // Weak keys: an owner and its added fields are collected together.
        private static readonly ConditionalWeakTable<object, Bag> instances = new ConditionalWeakTable<object, Bag>();

        /// <summary>The added instance field of a class instance. A new owner starts with the default value.</summary>
        public static ref T Instance<T>(object owner, string id)
        {
            if (owner == null) throw new NullReferenceException("An added field was accessed on a null reference: " + id);
            if (!instances.TryGetValue(owner, out Bag bag)) bag = instances.GetOrCreateValue(owner);
            return ref Find<T>(bag, id).Value;
        }

        /// <summary>The added static field. It starts with the default value.</summary>
        public static ref T Static<T>(string id)
        {
            return ref Find<T>(Statics<T>.Bag, id).Value;
        }

        private static Slot<T> Find<T>(Bag bag, string id)
        {
            // Readers take no lock: the array is replaced, never changed. Ids are
            // string literals of patch code, which the runtime interns, so a
            // reference compare decides. A field whose type changed keeps its id
            // and gets a slot of its own; code of an older patch keeps the old one.
            Cell[] cells = bag.Cells;
            for (int index = 0; index < cells.Length; index++)
                if ((object)cells[index].Id == (object)id && cells[index].Slot is Slot<T> slot) return slot;
            return Add<T>(bag, id);
        }

        private static Slot<T> Add<T>(Bag bag, string id)
        {
            if (id == null) throw new ArgumentNullException(nameof(id));
            id = string.Intern(id);
            lock (bag)
            {
                Cell[] cells = bag.Cells;
                for (int index = 0; index < cells.Length; index++)
                    if ((object)cells[index].Id == (object)id && cells[index].Slot is Slot<T> slot) return slot;
                var added = new Slot<T>();
                var grown = new Cell[cells.Length + 1];
                Array.Copy(cells, grown, cells.Length);
                grown[cells.Length] = new Cell { Id = id, Slot = added };
                bag.Cells = grown;
                return added;
            }
        }
    }
}
