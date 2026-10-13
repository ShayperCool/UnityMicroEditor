// An assembly the Netcode post-processor would rewrite: it references the Netcode runtime.
namespace Net
{
    public class NetThing : Unity.Netcode.NetworkBehaviour
    {
#if CASE_NET_BODY
        public int Value() => 2;
#else
        public int Value() => 1;
#endif
    }
}
