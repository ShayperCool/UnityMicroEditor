using System;
using System.Collections.Generic;

namespace Dungeon.CLIPlay.Raw
{
    internal sealed class RawLifecycleQueue<T> where T : class
    {
        private sealed class State
        {
            public bool awake, enabled, destroyed, pendingAwake, pendingEnable;
            public int enableGeneration;
        }
        private sealed class Call
        {
            public T target;
            public int phase, order, generation;
            public long sequence;
            public Action action;
        }
        private readonly Dictionary<T, State> states = new Dictionary<T, State>();
        private readonly List<Call> calls = new List<Call>();
        private readonly Func<T, bool> alive, active, enabled;
        private readonly Action<Exception, T> reportException;
        private long sequence;
        private bool dispatching;
        private bool complete;
        private T current;
        private int currentPhase = -1;

        public RawLifecycleQueue(Func<T, bool> alive, Func<T, bool> active, Func<T, bool> enabled,
            Action<Exception, T> reportException = null)
        { this.alive = alive; this.active = active; this.enabled = enabled; this.reportException = reportException; }

        public bool IsCollecting { get { return !dispatching; } }
        public bool IsComplete { get { return complete; } }
        public bool HasPendingAwake
        {
            get
            {
                foreach (var pair in states)
                    if (pair.Value.pendingAwake && !pair.Value.destroyed && alive(pair.Key)) return true;
                return false;
            }
        }
        public bool HasPendingActiveAwake
        {
            get
            {
                foreach (var pair in states)
                    if (pair.Value.pendingAwake && !pair.Value.destroyed && alive(pair.Key) && active(pair.Key)) return true;
                return false;
            }
        }
        public bool HasPendingAwakeFor(T target)
        { return states.TryGetValue(target, out State state) && state.pendingAwake && !state.destroyed; }
        public bool ContainsRegisteredTarget(T target) { return states.ContainsKey(target); }
        public bool ShouldCollect(T target, int phase)
        { return (!complete || ReferenceEquals(current, target) || HasPendingAwakeFor(target))
            && (!dispatching || !ReferenceEquals(current, target) || currentPhase != phase); }

        public bool TryGetAwakeState(T target, out bool awake)
        {
            if ((!complete || ReferenceEquals(current, target) || HasPendingAwakeFor(target)) && states.TryGetValue(target, out State state))
            { awake = state.awake; return true; }
            awake = false; return false;
        }

        public void Register(T target)
        { if (!states.ContainsKey(target)) states.Add(target, new State()); }

        public void Enqueue(T target, int phase, Action callback, int order)
        {
            Register(target);
            State state = states[target];
            if (state.destroyed) return;
            if (phase == 2)
            {
                state.pendingEnable = false; state.enableGeneration++;
                if (state.enabled) { state.enabled = false; Invoke(target, phase, callback); }
                return;
            }
            if (phase == 3)
            {
                state.destroyed = true;
                if (state.awake) Invoke(target, phase, callback);
                return;
            }
            if (phase == 0)
            {
                if (state.awake || state.pendingAwake) return;
                if (current != null)
                {
                    state.awake = true;
                    Invoke(target, phase, callback);
                    return;
                }
                state.pendingAwake = true;
            }
            else if (phase == 1)
            {
                if (state.enabled || state.pendingEnable) return;
                if (current != null && state.pendingAwake && alive(target) && active(target))
                    Drain(candidate => ReferenceEquals(candidate, target));
                if (state.destroyed || !alive(target) || !active(target) || !enabled(target)) return;
                if (current != null && state.awake)
                {
                    state.enabled = true;
                    Invoke(target, phase, callback);
                    return;
                }
                state.pendingEnable = true;
            }
            else throw new ArgumentOutOfRangeException(nameof(phase));
            calls.Add(new Call { target = target, phase = phase, order = order,
                generation = state.enableGeneration, sequence = sequence++, action = callback });
        }

        public void Complete()
        {
            if (complete) throw new InvalidOperationException("Raw lifecycle queue is already complete.");
            dispatching = true;
            try { Drain(); }
            finally { complete = true; }
        }

        public void FlushPending()
        {
            if (!complete) throw new InvalidOperationException("Raw lifecycle queue has not completed scene initialization.");
            Drain();
        }

        public void FlushActivated(Func<T, bool> belongs)
        {
            if (!dispatching) return;
            Drain(belongs);
        }

        private void Drain(Func<T, bool> belongs = null)
        {
            int dispatched = 0;
            while (calls.Count != 0)
            {
                if (++dispatched > 100000) throw new InvalidOperationException("Raw lifecycle callbacks exceeded the initialization bound.");
                int selected = -1;
                for (int index = 0; index < calls.Count; index++)
                {
                    Call candidate = calls[index];
                    if (belongs != null && !belongs(candidate.target)) continue;
                    // A previous callback may deactivate and later reactivate a
                    // peer. Keep its undelivered Awake until dispatch quiesces,
                    // because Unity has already consumed the native wrapper.
                    if (candidate.phase == 0 && alive(candidate.target) && !active(candidate.target)) continue;
                    if (selected < 0 || Before(candidate, calls[selected])) selected = index;
                }
                if (selected < 0) break;
                Call call = calls[selected]; calls.RemoveAt(selected);
                State state = states[call.target];
                if (state.destroyed || !alive(call.target)) continue;
                if (call.phase == 0)
                {
                    state.pendingAwake = false;
                    if (state.awake || !active(call.target)) continue;
                    state.awake = true;
                }
                else
                {
                    if (!state.pendingEnable || call.generation != state.enableGeneration) continue;
                    state.pendingEnable = false;
                    if (state.enabled || !active(call.target) || !enabled(call.target)) continue;
                    state.awake = true; state.enabled = true;
                }
                Invoke(call.target, call.phase, call.action);
            }
        }

        private static bool Before(Call left, Call right)
        { return left.order < right.order || left.order == right.order && left.sequence < right.sequence; }

        private void Invoke(T target, int phase, Action callback)
        {
            T previous = current; int previousPhase = currentPhase;
            current = target; currentPhase = phase;
            try { callback(); }
            catch (Exception exception)
            {
                if (reportException == null) throw;
                reportException(exception, target);
            }
            finally { current = previous; currentPhase = previousPhase; }
        }
    }
}
