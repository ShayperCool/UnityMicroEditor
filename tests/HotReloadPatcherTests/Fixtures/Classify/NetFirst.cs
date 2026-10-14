// An assembly that starts to use Netcode for GameObjects in ordinary code (USES), and one that
// also has something to register when the Player starts (REGISTERS). The post-processor writes
// its two start-up types into every image it rewrites, also when there is nothing to register;
// they are written here by hand, as in Net.cs. The image without USES references no Netcode: the
// compiler drops a reference that no code uses.
namespace NetFirst
{
    public static class Lobby
    {
#if USES
        public static bool Hosting() => Unity.Netcode.NetworkManager.Singleton != null && Unity.Netcode.NetworkManager.Singleton.IsServer;
#else
        public static bool Hosting() => false;
#endif
    }
}

#if USES
namespace __GEN
{
    // The post-processor makes it without a constructor.
    internal static class NetworkVariableSerializationHelper
    {
        internal static void InitializeSerialization()
        {
#if REGISTERS
            Unity.Netcode.NetworkVariableSerializationTypes.InitializeSerializer_UnmanagedByMemcpy<float>();
#endif
        }
    }

    public static class UnityPlayNetcodeInitializers
    {
        public static void __UnityPlay_RuntimeInitialize_Netcode() => NetworkVariableSerializationHelper.InitializeSerialization();
    }
}
#endif
