using System;
using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;

namespace Sorted
{
    // Compiled after whatever sorts before this file: every token of it moves when a file is added.
    public partial class Late
    {
        private readonly List<int> items = new List<int> { 4, 5, 6 };
        private int limit = 5;
        public int Count() => items.Count(i => i < limit);
        public string Join() => string.Join("+", items.Select(i => i.ToString()));
        public IEnumerator Walk() { foreach (var i in items) yield return i; }
        public async Task<int> Later() { await Task.Yield(); return limit; }
        public int Pick(int i) { int[] table = { 10, 20, 30, 40, 50 }; return table[i % table.Length]; }
    }
}
