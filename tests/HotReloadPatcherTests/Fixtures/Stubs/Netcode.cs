// Stand-in for Unity.Netcode.Runtime: the members the package's post-processor names in the code
// it generates, as the Player's assembly has them (its own post-processing made them accessible
// to derived classes). An assembly that references it is one the Netcode post-processor rewrites.
using System;
using System.Collections.Generic;

namespace Unity.Netcode
{
    public enum SendTo { Server, Everyone }

    public class NetworkManager : UnityEngine.MonoBehaviour
    {
        public static NetworkManager Singleton;
        public bool IsServer;
        // CASE_RUNTIME_BODY: an edit of the runtime itself.
#if CASE_RUNTIME_BODY
        public static int Version() => 2;
#else
        public static int Version() => 1;
#endif
    }
    public enum RpcDelivery { Reliable, Unreliable }
    public enum RpcInvokePermission { Everyone, Owner, Server }

    [AttributeUsage(AttributeTargets.Method)]
    public class RpcAttribute : Attribute
    {
        public RpcDelivery Delivery;
        public RpcAttribute(SendTo target) { }
    }

    [AttributeUsage(AttributeTargets.Method)]
    public class ServerRpcAttribute : RpcAttribute
    {
        public bool RequireOwnership = true;
        public ServerRpcAttribute() : base(SendTo.Server) { }
    }

    [AttributeUsage(AttributeTargets.Method)]
    public class ClientRpcAttribute : RpcAttribute
    {
        public ClientRpcAttribute() : base(SendTo.Everyone) { }
    }

    public struct FastBufferWriter
    {
        public void WriteValueSafe(int value) { }
        public void WriteValueSafe(long value) { }
        public void WritePacked(int value) { }
    }

    public struct FastBufferReader
    {
        public void ReadValueSafe(out int value) { value = 0; }
        public void ReadValueSafe(out long value) { value = 0; }
        public void ReadPacked(out int value) { value = 0; }
    }

    public struct __RpcParams { }

    public abstract class NetworkVariableBase
    {
        public void Initialize(NetworkBehaviour behaviour) { }
    }

    public class NetworkVariable<T> : NetworkVariableBase
    {
        public T Value;
    }

    public static class NetworkVariableSerializationTypes
    {
        public static void InitializeSerializer_UnmanagedByMemcpy<T>() { }
        public static void InitializeEqualityChecker_UnmanagedValueEquals<T>() { }
    }

    public class NetworkBehaviour : UnityEngine.MonoBehaviour
    {
        public delegate void RpcReceiveHandler(NetworkBehaviour behaviour, FastBufferReader reader, __RpcParams parameters);
        protected enum __RpcExecStage { Send = 0, Execute = 1 }

        protected __RpcExecStage __rpc_exec_stage = __RpcExecStage.Send;
        protected readonly List<NetworkVariableBase> NetworkVariableFields = new List<NetworkVariableBase>();

        protected FastBufferWriter __beginSendRpc(uint id, SendTo target, RpcDelivery delivery) => default;
        protected void __endSendRpc(ref FastBufferWriter writer, uint id, SendTo target, RpcDelivery delivery) { }
        protected void __registerRpc(uint id, RpcReceiveHandler handler, string name, RpcInvokePermission permission) { }
        protected void __nameNetworkVariable(NetworkVariableBase variable, string name) { }
        protected virtual void __initializeRpcs() { }
        protected virtual void __initializeVariables() { }
        protected internal virtual string __getTypeName() => "NetworkBehaviour";
        public virtual void OnNetworkSpawn() { }
    }
}
