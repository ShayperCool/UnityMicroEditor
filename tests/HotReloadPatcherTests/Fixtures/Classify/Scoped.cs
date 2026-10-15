// A type of the assembly named through a type reference instead of its definition. The source
// names Scoped.Ball of another assembly (ScopedOther.cs, alias `other`); the test then gives
// that reference the module as its scope, so that it means the Ball defined here. This is what a
// post-processor writes when it imports a type of the assembly it rewrites.
extern alias other;

namespace Scoped
{
    public struct Ball { public int size; }

    public static class Sink
    {
        public static int Take<T>(T value) where T : struct => 1;
    }

    public class Thrower
    {
        public int Throw()
        {
            var ball = new other::Scoped.Ball();
#if CASE_SCOPED_BODY
            return Sink.Take(ball) + 2;
#else
            return Sink.Take(ball) + 1;
#endif
        }
    }
}
