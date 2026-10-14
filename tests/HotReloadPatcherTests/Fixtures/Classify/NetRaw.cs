// Assemblies that reference the Netcode runtime and are not in the form its post-processor
// leaves, one RAW_* symbol per way of not being in it. Nothing says what such an assembly
// registered at start-up, so no edit of it is patched. CASE_RAW_BODY is the edit.
using Unity.Netcode;

namespace NetRaw
{
#if RAW_NO_REGISTRATION
    // No start-up registration at all: the assembly was not post-processed.
    public static class Uses
    {
#if CASE_RAW_BODY
        public static SendTo Target() => SendTo.Everyone;
#else
        public static SendTo Target() => SendTo.Server;
#endif
    }
#else
    public class Thing : NetworkBehaviour
    {
        public int seen;

#if CASE_RAW_BODY
        public int Value() => 2;
#else
        public int Value() => 1;
#endif

#if RAW_HANDLER || RAW_HEAD
        [Rpc(SendTo.Server)]
        public void PingRpc(int value)
        {
#if RAW_HANDLER
            if (__rpc_exec_stage != __RpcExecStage.Execute)
            {
                FastBufferWriter writer = __beginSendRpc(101u, SendTo.Server, RpcDelivery.Reliable);
                writer.WriteValueSafe(value);
                __endSendRpc(ref writer, 101u, SendTo.Server, RpcDelivery.Reliable);
            }
            __rpc_exec_stage = __RpcExecStage.Send;
#endif
            seen = value;
        }
#endif

#if RAW_HEAD
        // The RPC above has its handler, and its body does not start with the code that sends it.
        static void __rpc_handler_101(NetworkBehaviour target, FastBufferReader reader, __RpcParams parameters)
        {
            reader.ReadValueSafe(out int value);
            ((Thing)target).PingRpc(value);
        }
#endif

#if !RAW_BEHAVIOUR
        protected override void __initializeRpcs() { base.__initializeRpcs(); }
        protected override void __initializeVariables() { base.__initializeVariables(); }
        protected override string __getTypeName() => "Thing";
#endif
    }
#endif
}

#if !RAW_NO_REGISTRATION
namespace __GEN
{
    internal class NetworkVariableSerializationHelper
    {
        internal static void InitializeSerialization() { }
    }
}
#endif
