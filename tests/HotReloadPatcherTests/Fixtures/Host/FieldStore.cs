// Stand-in for the applier's side table (design 5.2: assembly UnityMicroEditor.HotReload, the same
// namespace and signatures), so the patcher tests do not depend on the applier's sources.
using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;

namespace UnityMicroEditor.HotReload
{
    public static class FieldStore
    {
        sealed class Slot<T> { public T Value; }
        sealed class Bag { public readonly Dictionary<string, object> Slots = new Dictionary<string, object>(StringComparer.Ordinal); }

        static readonly ConditionalWeakTable<object, Bag> instances = new ConditionalWeakTable<object, Bag>();
        static readonly Bag statics = new Bag();

        public static ref T Instance<T>(object owner, string id)
        {
            if (owner == null) throw new NullReferenceException();
            return ref Find<T>(instances.GetOrCreateValue(owner), id).Value;
        }

        public static ref T Static<T>(string id) => ref Find<T>(statics, id).Value;

        static Slot<T> Find<T>(Bag bag, string id)
        {
            lock (bag)
            {
                // A field whose type changed keeps its id: the slot of the old type is replaced.
                if (!bag.Slots.TryGetValue(id, out object slot) || !(slot is Slot<T>)) bag.Slots[id] = slot = new Slot<T>();
                return (Slot<T>)slot;
            }
        }
    }
}
