using UnityEngine;

namespace Dungeon.CLIPlay.Benchmarks
{
    // Used only in the disposable source copy created by benchmark_scene_reload.py.
    public sealed class UnityPlaySceneReloadData : ScriptableObject
    {
        [SerializeField] private int revision;
        public int Revision => revision;
    }
}
