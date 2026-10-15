// A game assembly that does not use Netcode for GameObjects, for `hot_reload_netcode_sdk`. With
// CASE_USES an ordinary method starts to call the Netcode API: the compiled image references the
// runtime from then on, and the package's post-processor rewrites it and writes its start-up types,
// which register nothing here. With CASE_REGISTERS the assembly also asks for a serializer.
namespace First
{
#if CASE_REGISTERS
    [Unity.Netcode.GenerateSerializationForType(typeof(UnityEngine.Vector3))]
#endif
    public static class Lobby
    {
#if CASE_USES || CASE_REGISTERS
        public static bool Hosting() => Unity.Netcode.NetworkManager.Singleton != null && Unity.Netcode.NetworkManager.Singleton.IsServer;
#else
        public static bool Hosting() => false;
#endif
    }
}
