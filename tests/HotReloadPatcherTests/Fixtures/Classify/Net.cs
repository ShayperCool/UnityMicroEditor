// An assembly as the Netcode post-processor leaves it, written by hand: the fixtures are sources,
// and the package's own post-processor needs a prepared Netcode project (the test
// `hot_reload_netcode_sdk` of rust/src/hotreload/tests.rs runs it on real sources). Every class
// below NetworkBehaviour carries what the post-processor adds: `__initializeRpcs`,
// `__initializeVariables`, `__getTypeName`, one `__rpc_handler_<id>` per RPC, and in front of
// every RPC body the code that sends it, which ends with the store into `__rpc_exec_stage`.
// One CASE_* symbol per kind of edit; the loaded assembly is the file without a symbol.
using System;
using Unity.Netcode;

namespace Net
{
    /// A class that uses nothing of Netcode, in an assembly that does.
    public static class Plain
    {
#if CASE_NET_PLAIN
        public static int Twice(int value) => value * 3;
#else
        public static int Twice(int value) => value * 2;
#endif
    }

    public class NetThing : NetworkBehaviour
    {
#if CASE_NET_VARIABLE_RETYPED
        public NetworkVariable<long> Count = new NetworkVariable<long>();
#elif CASE_NET_VARIABLE_RENAMED
        public NetworkVariable<int> Total = new NetworkVariable<int>();
#else
        public NetworkVariable<int> Count = new NetworkVariable<int>();
#endif
#if !CASE_NET_VARIABLE_REMOVED
        public NetworkVariable<float> Speed = new NetworkVariable<float>();
#endif
#if CASE_NET_VARIABLE_ADDED
        public NetworkVariable<float> Extra = new NetworkVariable<float>();
#endif
#if CASE_NET_FIELD_ADDED
        public int plain;
#endif
        public int seen;

#if CASE_NET_BODY
        public int Value() => 2;
#elif CASE_NET_FIELD_ADDED
        public int Value() => plain;
#else
        public int Value() => 1;
#endif

#if CASE_NET_METHOD_ADDED
        public int Helper() => seen + 1;
#endif

        public override void OnNetworkSpawn()
        {
#if CASE_NET_SPAWN
            seen = 2;
#else
            seen = 1;
#endif
            Arm();
        }

        // NGO declares it with the NetworkManager by reference; it runs once per spawn (builder6-1).
        protected override void OnNetworkPreSpawn(ref NetworkManager networkManager)
        {
#if CASE_NET_PRESPAWN
            seen = 4;
#else
            seen = 3;
#endif
        }

        // Only OnNetworkSpawn calls it.
        private void Arm()
        {
#if CASE_NET_SPAWN_HOOK
            seen += 20;
#else
            seen += 10;
#endif
        }

        // ---- an RPC with one parameter ----------------------------------------------------

#if CASE_NET_RPC_PARAMETER
        [Rpc(SendTo.Server)]
        public void PingRpc(long value)
        {
            if (__rpc_exec_stage != __RpcExecStage.Execute)
            {
                FastBufferWriter writer = __beginSendRpc(202u, SendTo.Server, RpcDelivery.Reliable);
                writer.WriteValueSafe(value);
                __endSendRpc(ref writer, 202u, SendTo.Server, RpcDelivery.Reliable);
            }
            __rpc_exec_stage = __RpcExecStage.Send;
            seen = (int)value;
        }

        static void __rpc_handler_202(NetworkBehaviour target, FastBufferReader reader, __RpcParams parameters)
        {
            reader.ReadValueSafe(out long value);
            var thing = (NetThing)target;
            thing.__rpc_exec_stage = __RpcExecStage.Execute;
            thing.PingRpc(value);
            thing.__rpc_exec_stage = __RpcExecStage.Send;
        }
#else
#if CASE_NET_RPC_ATTRIBUTE
        [Rpc(SendTo.Everyone)]
#elif CASE_NET_RPC_DELIVERY
        [Rpc(SendTo.Server, Delivery = RpcDelivery.Unreliable)]
#elif CASE_NET_RPC_NO_LONGER
#else
        [Rpc(SendTo.Server)]
#endif
        public void PingRpc(int value)
        {
#if !CASE_NET_RPC_NO_LONGER
            if (__rpc_exec_stage != __RpcExecStage.Execute)
            {
#if CASE_NET_RPC_ATTRIBUTE
                FastBufferWriter writer = __beginSendRpc(101u, SendTo.Everyone, RpcDelivery.Reliable);
#elif CASE_NET_RPC_DELIVERY
                FastBufferWriter writer = __beginSendRpc(101u, SendTo.Server, RpcDelivery.Unreliable);
#else
                FastBufferWriter writer = __beginSendRpc(101u, SendTo.Server, RpcDelivery.Reliable);
#endif
#if CASE_NET_RPC_SEND || CASE_NET_RPC_SERIALIZATION
                writer.WritePacked(value);
#else
                writer.WriteValueSafe(value);
#endif
#if CASE_NET_RPC_ATTRIBUTE
                __endSendRpc(ref writer, 101u, SendTo.Everyone, RpcDelivery.Reliable);
#elif CASE_NET_RPC_DELIVERY
                __endSendRpc(ref writer, 101u, SendTo.Server, RpcDelivery.Unreliable);
#else
                __endSendRpc(ref writer, 101u, SendTo.Server, RpcDelivery.Reliable);
#endif
            }
            __rpc_exec_stage = __RpcExecStage.Send;
#endif
#if CASE_NET_RPC_BODY
            seen = value + 1;
#elif CASE_NET_RPC_BODY_LOCALS
            int doubled = value * 2;
            string text = doubled.ToString();
            seen = text.Length + doubled;
#elif CASE_NET_RPC_BODY_LAMBDA
            Func<int, int> twice = x => x * 2;
            seen = twice(value);
#else
            seen = value;
#endif
        }

#if !CASE_NET_RPC_NO_LONGER
        static void __rpc_handler_101(NetworkBehaviour target, FastBufferReader reader, __RpcParams parameters)
        {
#if CASE_NET_RPC_RECEIVE || CASE_NET_RPC_SERIALIZATION
            reader.ReadPacked(out int value);
#else
            reader.ReadValueSafe(out int value);
#endif
            var thing = (NetThing)target;
            thing.__rpc_exec_stage = __RpcExecStage.Execute;
            thing.PingRpc(value);
            thing.__rpc_exec_stage = __RpcExecStage.Send;
        }
#endif
#endif

        // ---- an RPC without parameters, removed or renamed by a case -----------------------

#if !CASE_NET_RPC_REMOVED
        [ClientRpc]
#if CASE_NET_RPC_RENAMED
        public void GoneClientRpc()
#else
        public void DropClientRpc()
#endif
        {
            if (__rpc_exec_stage != __RpcExecStage.Execute)
            {
#if CASE_NET_RPC_RENAMED
                FastBufferWriter writer = __beginSendRpc(304u, SendTo.Everyone, RpcDelivery.Reliable);
                __endSendRpc(ref writer, 304u, SendTo.Everyone, RpcDelivery.Reliable);
#else
                FastBufferWriter writer = __beginSendRpc(303u, SendTo.Everyone, RpcDelivery.Reliable);
                __endSendRpc(ref writer, 303u, SendTo.Everyone, RpcDelivery.Reliable);
#endif
            }
            __rpc_exec_stage = __RpcExecStage.Send;
            seen = 0;
        }

#if CASE_NET_RPC_RENAMED
        static void __rpc_handler_304(NetworkBehaviour target, FastBufferReader reader, __RpcParams parameters)
        {
            var thing = (NetThing)target;
            thing.__rpc_exec_stage = __RpcExecStage.Execute;
            thing.GoneClientRpc();
            thing.__rpc_exec_stage = __RpcExecStage.Send;
        }
#else
        static void __rpc_handler_303(NetworkBehaviour target, FastBufferReader reader, __RpcParams parameters)
        {
            var thing = (NetThing)target;
            thing.__rpc_exec_stage = __RpcExecStage.Execute;
            thing.DropClientRpc();
            thing.__rpc_exec_stage = __RpcExecStage.Send;
        }
#endif
#endif

#if CASE_NET_RPC_ADDED
        [ServerRpc]
        public void FreshServerRpc()
        {
            if (__rpc_exec_stage != __RpcExecStage.Execute)
            {
                FastBufferWriter writer = __beginSendRpc(405u, SendTo.Server, RpcDelivery.Reliable);
                __endSendRpc(ref writer, 405u, SendTo.Server, RpcDelivery.Reliable);
            }
            __rpc_exec_stage = __RpcExecStage.Send;
            seen = -1;
        }

        static void __rpc_handler_405(NetworkBehaviour target, FastBufferReader reader, __RpcParams parameters)
        {
            var thing = (NetThing)target;
            thing.__rpc_exec_stage = __RpcExecStage.Execute;
            thing.FreshServerRpc();
            thing.__rpc_exec_stage = __RpcExecStage.Send;
        }
#endif

        // ---- what the post-processor registers ---------------------------------------------

        protected override void __initializeRpcs()
        {
            // The post-processor registers the RPCs in the order of their declarations.
#if CASE_NET_RPC_REORDERED
            __registerRpc(303u, new RpcReceiveHandler(__rpc_handler_303), "DropClientRpc", RpcInvokePermission.Server);
            __registerRpc(101u, new RpcReceiveHandler(__rpc_handler_101), "PingRpc", RpcInvokePermission.Everyone);
#else
#if CASE_NET_RPC_PARAMETER
            __registerRpc(202u, new RpcReceiveHandler(__rpc_handler_202), "PingRpc", RpcInvokePermission.Everyone);
#elif CASE_NET_RPC_PERMISSION
            __registerRpc(101u, new RpcReceiveHandler(__rpc_handler_101), "PingRpc", RpcInvokePermission.Owner);
#elif !CASE_NET_RPC_NO_LONGER
            __registerRpc(101u, new RpcReceiveHandler(__rpc_handler_101), "PingRpc", RpcInvokePermission.Everyone);
#endif
#if CASE_NET_RPC_RENAMED
            __registerRpc(304u, new RpcReceiveHandler(__rpc_handler_304), "GoneClientRpc", RpcInvokePermission.Server);
#elif !CASE_NET_RPC_REMOVED
            __registerRpc(303u, new RpcReceiveHandler(__rpc_handler_303), "DropClientRpc", RpcInvokePermission.Server);
#endif
#if CASE_NET_RPC_ADDED
            __registerRpc(405u, new RpcReceiveHandler(__rpc_handler_405), "FreshServerRpc", RpcInvokePermission.Owner);
#endif
#endif
            base.__initializeRpcs();
        }

        protected override void __initializeVariables()
        {
#if CASE_NET_VARIABLE_REORDERED
            Speed.Initialize(this);
            __nameNetworkVariable(Speed, "Speed");
            NetworkVariableFields.Add(Speed);
#endif
#if CASE_NET_VARIABLE_RENAMED
            Total.Initialize(this);
            __nameNetworkVariable(Total, "Total");
            NetworkVariableFields.Add(Total);
#else
            Count.Initialize(this);
            __nameNetworkVariable(Count, "Count");
            NetworkVariableFields.Add(Count);
#endif
#if !CASE_NET_VARIABLE_REMOVED && !CASE_NET_VARIABLE_REORDERED
            Speed.Initialize(this);
            __nameNetworkVariable(Speed, "Speed");
            NetworkVariableFields.Add(Speed);
#endif
#if CASE_NET_VARIABLE_ADDED
            Extra.Initialize(this);
            __nameNetworkVariable(Extra, "Extra");
            NetworkVariableFields.Add(Extra);
#endif
            base.__initializeVariables();
        }

#if CASE_NET_TYPE_NAME
        protected override string __getTypeName() => "Renamed";
#else
        protected override string __getTypeName() => "NetThing";
#endif

#if CASE_NET_UNKNOWN_MEMBER
        protected void __initializeSomethingNew() { seen = 3; }
#endif
    }

    // The owner-authoritative NetworkTransform of the NGO documentation: NetworkTransform reads
    // the override when an object is spawned and keeps it.
    public class ClientMover : Unity.Netcode.Components.NetworkTransform
    {
#if CASE_NET_AUTHORITY
        protected override bool OnIsServerAuthoritative() => true;
#else
        protected override bool OnIsServerAuthoritative() => false;
#endif
    }

    // Network variables constructed with a read permission: in a field initializer, and a list
    // in Awake. The read permission decides what is in an object's spawn data.
    public class NetPerm : NetworkBehaviour
    {
#if CASE_NET_VARIABLE_READ
        public NetworkVariable<int> Secret = new NetworkVariable<int>(11, NetworkVariableReadPermission.Owner);
#elif CASE_NET_VARIABLE_INITIAL
        public NetworkVariable<int> Secret = new NetworkVariable<int>(12);
#elif CASE_NET_VARIABLE_WRITE
        public NetworkVariable<int> Secret = new NetworkVariable<int>(11, NetworkVariableReadPermission.Everyone, NetworkVariableWritePermission.Owner);
#else
        public NetworkVariable<int> Secret = new NetworkVariable<int>(11);
#endif
        public NetworkList<int> Items;
        // A read permission a property of another class supplies: constructor code, not compared
        // as the written argument is (netcode6-1, a documented limit); its note says so.
        public NetworkVariable<int> Hidden = new NetworkVariable<int>(33, NetRules.HiddenRead);
        // A string the constructor loads: the reading of its code must take it.
        public string label = "perm";

        private void Awake()
        {
#if CASE_NET_LIST_READ
            Items = new NetworkList<int>(null, NetworkVariableReadPermission.Owner);
#else
            Items = new NetworkList<int>();
#endif
        }

        protected override void __initializeRpcs() { base.__initializeRpcs(); }
        protected override void __initializeVariables()
        {
            Secret.Initialize(this);
            __nameNetworkVariable(Secret, "Secret");
            NetworkVariableFields.Add(Secret);
            Items.Initialize(this);
            __nameNetworkVariable(Items, "Items");
            NetworkVariableFields.Add(Items);
            Hidden.Initialize(this);
            __nameNetworkVariable(Hidden, "Hidden");
            NetworkVariableFields.Add(Hidden);
            base.__initializeVariables();
        }
        protected override string __getTypeName() => "NetPerm";
    }

    public static class NetRules
    {
#if CASE_NET_VARIABLE_READ_HELPER
        public static NetworkVariableReadPermission HiddenRead => NetworkVariableReadPermission.Owner;
#else
        public static NetworkVariableReadPermission HiddenRead => NetworkVariableReadPermission.Everyone;
#endif
    }

#if CASE_NET_BECAME
    public class Late : NetworkBehaviour
    {
        public int Value() => 1;
        protected override void __initializeRpcs() { base.__initializeRpcs(); }
        protected override void __initializeVariables() { base.__initializeVariables(); }
        protected override string __getTypeName() => "Late";
    }
#else
    public class Late : UnityEngine.MonoBehaviour
    {
        public int Value() => 1;
    }
#endif
}

namespace __GEN
{
    // The post-processor makes it without a constructor.
    internal static class NetworkVariableSerializationHelper
    {
        internal static void InitializeSerialization()
        {
            // One call per serializer and per comparison, in the order the post-processor met
            // the types of the network variables.
#if CASE_NET_SERIALIZATION_ORDER
            NetworkVariableSerializationTypes.InitializeEqualityChecker_UnmanagedValueEquals<float>();
            NetworkVariableSerializationTypes.InitializeSerializer_UnmanagedByMemcpy<float>();
#elif !CASE_NET_VARIABLE_REMOVED
            NetworkVariableSerializationTypes.InitializeSerializer_UnmanagedByMemcpy<float>();
            NetworkVariableSerializationTypes.InitializeEqualityChecker_UnmanagedValueEquals<float>();
#endif
#if CASE_NET_SERIALIZATION
            NetworkVariableSerializationTypes.InitializeSerializer_UnmanagedByMemcpy<double>();
            NetworkVariableSerializationTypes.InitializeEqualityChecker_UnmanagedValueEquals<double>();
#endif
        }
    }

    public static class UnityPlayNetcodeInitializers
    {
#if CASE_NET_BRIDGE
        public static void __UnityPlay_RuntimeInitialize_Netcode() { }
#else
        public static void __UnityPlay_RuntimeInitialize_Netcode() => NetworkVariableSerializationHelper.InitializeSerialization();
#endif
    }
}
