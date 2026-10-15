// The code around Gens.dll, compiled against the loaded version and never patched. Step 0 runs
// before any patch, step 1 with the first patch applied, step 2 with the second.
using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using Gens;

public static class Driver
{
    static Thing thing;
    static Func<int> scaled0, scaled1, kept0, kept1;
    static IEnumerator<int> steps1;

    public static string Step(int step)
    {
        var log = new StringBuilder();
        void Line(string label, Func<object> call)
        {
            string value;
            try { value = "" + call(); }
            catch (Exception e) { value = "THROWS " + e.GetType().Name + ": " + e.Message; }
            log.Append(step).Append('.').Append(label).Append('=').Append(value).Append('\n');
        }

        if (step == 0)
        {
            thing = new Thing();
            scaled0 = thing.Scaled(10);
            kept0 = thing.Kept(1);
        }
        else if (step == 1)
        {
            thing.Subscribe();
            scaled1 = thing.Scaled(10);
            kept1 = thing.Kept(1);
            steps1 = thing.Steps().GetEnumerator();
            Line("steps1", () => { steps1.MoveNext(); return steps1.Current; });
        }
        else
        {
            thing.Unsubscribe();
            Thing.Fire();
            Line("handlers", () => string.Join(",", Thing.Log.Where(x => x.StartsWith("handler", StringComparison.Ordinal))));
            Line("steps1", () => { steps1.MoveNext(); return steps1.Current; });
        }
        Line("Edit", () => thing.Edit());
        Line("Count", () => thing.Count());
        Line("Steady", () => thing.UseSteady());
        Line("scaled0", () => scaled0());
        if (step >= 1) Line("scaled1", () => scaled1());
        Line("scaled", () => thing.Scaled(10)());
        Line("kept0", () => kept0());
        if (step >= 1) Line("kept1", () => kept1());
        Line("steps", () => string.Join(",", thing.Steps()));
        return log.ToString();
    }
}
