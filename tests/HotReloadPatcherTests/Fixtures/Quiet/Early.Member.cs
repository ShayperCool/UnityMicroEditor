using System.Linq;

namespace Sorted
{
    // A new file that sorts first and adds a member to an existing type.
    public partial class Late
    {
        public int FromNewFile() => items.Where(i => i > limit).Count();
    }
}
