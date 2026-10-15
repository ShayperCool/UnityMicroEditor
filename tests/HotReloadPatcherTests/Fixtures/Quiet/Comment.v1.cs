using System;
using System.Collections.Generic;
using System.Linq;

namespace Quiet
{
    public class Commented
    {
        private readonly List<int> items = new List<int> { 1, 2, 3 };
        public int Sum() => items.Where(i => i > 1).Sum();
        public IEnumerable<int> All() { foreach (var i in items) yield return i; }
    }
}
