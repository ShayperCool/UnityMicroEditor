using System.Collections.Generic;
using UnityEngine;
using Smb.Test;
public class SmbDict : StateMachineBehaviour
{
    [SerializeField] private Dictionary<string, int> d; [SerializeField] private Dictionary<int, Inner> d2; public Dictionary<int, int> pub; public int tail;
    public void Set() { d = new Dictionary<string, int> { { "one", 1 }, { "two", 2 } }; d2 = new Dictionary<int, Inner> { { 5, new Inner { a = 5, name = "five" } } }; tail = 9; }
}
