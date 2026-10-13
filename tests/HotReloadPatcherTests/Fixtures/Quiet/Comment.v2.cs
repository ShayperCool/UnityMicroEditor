using System;
using System.Collections.Generic;
using System.Linq;

// A comment-only edit: other line numbers, another PDB, another MVID, the same code.
namespace Quiet
{
    public class Commented
    {
        private readonly List<int> items = new List<int> { 1, 2, 3 };

        /// Sums what is larger than one.
        public int Sum() => items.Where(i => i > 1)
                                 .Sum();

        public IEnumerable<int> All() { foreach (var i in items) yield return i; } // unchanged
    }
}
