// The attribute types the compiler embeds into an assembly: the first nullable annotation brings
// three, the first `nint` another, and the last use of each takes them away again. Compiled with
// and without CASE_EMBEDDED; nothing but those types and the attributes on the members differs.
namespace Embedded
{
    public class Annotated
    {
#if CASE_EMBEDDED
#nullable enable
        public string? name;
        public string Get(string? fallback) => name ?? fallback ?? "";
#nullable restore
        public nint Size(nint from) => from;
#else
        public string name;
        public string Get(string fallback) => name ?? fallback ?? "";
        public System.IntPtr Size(System.IntPtr from) => from;
#endif
    }
}
