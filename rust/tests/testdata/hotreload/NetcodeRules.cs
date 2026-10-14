// A game assembly that uses Netcode for GameObjects, for `hot_reload_netcode_sdk`: the test
// compiles it once without a symbol and once per CASE_* symbol, runs the package's own
// post-processor on every image and gives the pairs to the patch builder.
using Unity.Netcode;
using UnityEngine;

namespace Rules
{
#if CASE_CLASS_ORDER
    // The same class before the others: the post-processor meets the types of the network
    // variables in another order and registers their serializers in that order.
    public sealed class Spinner : NetworkBehaviour
    {
        public readonly NetworkVariable<Quaternion> Turn = new NetworkVariable<Quaternion>();
    }
#endif

    public struct Shot : INetworkSerializable
    {
        public int Power;
        public float Angle;

        public void NetworkSerialize<T>(BufferSerializer<T> serializer) where T : IReaderWriter
        {
#if CASE_SERIALIZE_BODY
            serializer.SerializeValue(ref Angle);
            serializer.SerializeValue(ref Power);
#else
            serializer.SerializeValue(ref Power);
            serializer.SerializeValue(ref Angle);
#endif
        }
    }

    public sealed class Ball : NetworkBehaviour
    {
        public readonly NetworkVariable<int> Counter = new NetworkVariable<int>();
        public readonly NetworkVariable<float> Speed = new NetworkVariable<float>();
#if CASE_VARIABLE_ADDED
        public readonly NetworkVariable<int> Level = new NetworkVariable<int>();
#endif
#if CASE_VARIABLE_TYPE
        public readonly NetworkVariable<Vector3> Aim = new NetworkVariable<Vector3>();
#endif
        public int seen;

        public int Compute(int value)
        {
#if CASE_BODY
            return value + 2;
#else
            return value + 1;
#endif
        }

        public override void OnNetworkSpawn()
        {
#if CASE_SPAWN
            seen = 2;
#else
            seen = 1;
#endif
        }

#if CASE_RPC_REORDERED
        // The two RPCs below in the other order: the post-processor registers them in the order
        // of their declarations.
        [ServerRpc(RequireOwnership = false)]
        public void PingServerRpc(int value) { seen = value; }
#endif

#if CASE_RPC_TARGET
        [Rpc(SendTo.NotServer)]
#else
        [Rpc(SendTo.Everyone)]
#endif
        public void AnnounceRpc(int value)
        {
#if CASE_RPC_BODY
            seen = value + 1;
#else
            seen = value;
#endif
        }

#if !CASE_RPC_REORDERED
        [ServerRpc(RequireOwnership = false)]
#if CASE_RPC_PARAMETER
        public void PingServerRpc(int value, int extra) { seen = value + extra; }
#else
        public void PingServerRpc(int value) { seen = value; }
#endif
#endif

        // The post-processor names the struct of this very assembly through a reference scoped to
        // the module; the body gains locals, which moves the indices of the send code's own.
        [Rpc(SendTo.Server)]
        public void ShootRpc(Shot shot, RpcParams rpc = default)
        {
#if CASE_RPC_STRUCT_BODY
            int doubled = shot.Power * 2;
            string text = doubled.ToString();
            seen = text.Length + (int)rpc.Receive.SenderClientId;
#else
            seen = shot.Power;
#endif
        }

#if CASE_RPC_ADDED
        [ClientRpc]
        public void FreshClientRpc() { seen = -1; }
#endif
    }

    public static class Plain
    {
#if CASE_PLAIN
        public static int Twice(int value) => value * 3;
#else
        public static int Twice(int value) => value * 2;
#endif
    }

#if !CASE_CLASS_ORDER
    public sealed class Spinner : NetworkBehaviour
    {
        public readonly NetworkVariable<Quaternion> Turn = new NetworkVariable<Quaternion>();
    }
#endif
}
