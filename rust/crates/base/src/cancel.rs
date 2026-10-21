//! Blocking cancellation contexts for the process-owning modules.
//!
//! A `Context` carries cancellation and an optional deadline and propagates both
//! to derived contexts. `select` waits on several contexts and one-shot `Flag`s
//! at once, without polling.
use std::{
    fmt,
    sync::{Arc, Condvar, Mutex, Weak},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Canceled {
    Canceled,
    DeadlineExceeded,
}
impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Canceled::Canceled => "canceled",
            Canceled::DeadlineExceeded => "deadline exceeded",
        })
    }
}
impl std::error::Error for Canceled {}

struct Signal {
    fired: Mutex<bool>,
    cond: Condvar,
}
#[derive(Default)]
struct FlagState {
    set: bool,
    waiters: Vec<Arc<Signal>>,
}
/// One-shot event: set once, it stays set and wakes every waiter.
#[derive(Clone, Default)]
pub struct Flag(Arc<Mutex<FlagState>>);
impl Flag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self) {
        let waiters = {
            let mut state = self.0.lock().unwrap();
            if state.set {
                return;
            }
            state.set = true;
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            *waiter.fired.lock().unwrap() = true;
            waiter.cond.notify_all();
        }
    }
    pub fn is_set(&self) -> bool {
        self.0.lock().unwrap().set
    }
    /// Blocks until set or until the timeout elapses; true when set.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        select(&[Source::Flag(self)], timeout).is_some()
    }
    fn subscribe(&self, signal: &Arc<Signal>) -> bool {
        let mut state = self.0.lock().unwrap();
        if !state.set {
            state.waiters.push(signal.clone());
        }
        state.set
    }
    fn unsubscribe(&self, signal: &Arc<Signal>) {
        self.0
            .lock()
            .unwrap()
            .waiters
            .retain(|waiter| !Arc::ptr_eq(waiter, signal));
    }
}

#[derive(Default)]
struct ContextState {
    error: Option<Canceled>,
    children: Vec<Weak<ContextInner>>,
}
struct ContextInner {
    done: Flag,
    deadline: Option<Instant>,
    state: Mutex<ContextState>,
}
impl ContextInner {
    fn cancel(&self, error: Canceled) {
        let children = {
            let mut state = self.state.lock().unwrap();
            if state.error.is_some() {
                return;
            }
            state.error = Some(error);
            std::mem::take(&mut state.children)
        };
        self.done.set();
        for child in children.iter().filter_map(Weak::upgrade) {
            child.cancel(error);
        }
    }
    fn error(&self) -> Option<Canceled> {
        if let Some(error) = self.state.lock().unwrap().error {
            return Some(error);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.cancel(Canceled::DeadlineExceeded);
            return self.state.lock().unwrap().error;
        }
        None
    }
}

/// Cancellation handle. Clones share one state; derived contexts are cancelled
/// with their parent and inherit its deadline.
#[derive(Clone)]
pub struct Context(Arc<ContextInner>);
impl Context {
    /// Never cancelled on its own and without a deadline.
    pub fn background() -> Self {
        Self(Arc::new(ContextInner {
            done: Flag::new(),
            deadline: None,
            state: Mutex::default(),
        }))
    }
    fn derive(&self, deadline: Option<Instant>) -> Self {
        let deadline = match (self.0.deadline, deadline) {
            (Some(parent), Some(own)) => Some(parent.min(own)),
            (parent, own) => parent.or(own),
        };
        let child = Self(Arc::new(ContextInner {
            done: Flag::new(),
            deadline,
            state: Mutex::default(),
        }));
        let inherited = self.error();
        let mut state = self.0.state.lock().unwrap();
        match state.error.or(inherited) {
            Some(error) => {
                drop(state);
                child.0.cancel(error);
            }
            None => {
                state.children.retain(|item| item.strong_count() > 0);
                state.children.push(Arc::downgrade(&child.0));
            }
        }
        child
    }
    /// A cancellable child: call `cancel` on the result.
    pub fn with_cancel(&self) -> Self {
        self.derive(None)
    }
    /// A child that is cancelled after `timeout`.
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        self.derive(Some(Instant::now() + timeout))
    }
    /// A child that is cancelled at `deadline`.
    pub fn with_deadline(&self, deadline: Instant) -> Self {
        self.derive(Some(deadline))
    }
    pub fn cancel(&self) {
        self.0.cancel(Canceled::Canceled);
    }
    /// Why the context is done; `None` while it is live.
    pub fn error(&self) -> Option<Canceled> {
        self.0.error()
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.0.deadline
    }
    /// [`Context::error`] as a `Result`, for `?` in polling loops.
    pub fn err(&self) -> Result<(), Canceled> {
        self.error().map_or(Ok(()), Err)
    }
    pub fn remaining(&self) -> Option<Duration> {
        self.0
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
    /// Blocks until the context is done.
    pub fn wait(&self) -> Canceled {
        loop {
            select(&[Source::Context(self)], None);
            if let Some(error) = self.error() {
                return error;
            }
        }
    }
}

pub enum Source<'a> {
    Context(&'a Context),
    Flag(&'a Flag),
}
impl Source<'_> {
    fn ready(&self) -> bool {
        match self {
            Source::Context(context) => context.error().is_some(),
            Source::Flag(flag) => flag.is_set(),
        }
    }
    fn flag(&self) -> &Flag {
        match self {
            Source::Context(context) => &context.0.done,
            Source::Flag(flag) => flag,
        }
    }
}

/// Waits on several sources plus an optional timer. Returns the index of
/// the first ready source, or `None` when the timeout elapsed first.
pub fn select(sources: &[Source<'_>], timeout: Option<Duration>) -> Option<usize> {
    let limit = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        if let Some(index) = sources.iter().position(Source::ready) {
            return Some(index);
        }
        let now = Instant::now();
        if limit.is_some_and(|limit| now >= limit) {
            return None;
        }
        let wake = sources
            .iter()
            .filter_map(|source| match source {
                Source::Context(context) => context.0.deadline,
                Source::Flag(_) => None,
            })
            .chain(limit)
            .min();
        let signal = Arc::new(Signal {
            fired: Mutex::new(false),
            cond: Condvar::new(),
        });
        let mut ready = false;
        for source in sources {
            ready |= source.flag().subscribe(&signal);
        }
        if !ready {
            let mut fired = signal.fired.lock().unwrap();
            while !*fired {
                match wake {
                    None => fired = signal.cond.wait(fired).unwrap(),
                    Some(wake) => {
                        let now = Instant::now();
                        if now >= wake {
                            break;
                        }
                        fired = signal.cond.wait_timeout(fired, wake - now).unwrap().0;
                    }
                }
            }
        }
        for source in sources {
            source.flag().unsubscribe(&signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn cancellation_propagates_to_derived_contexts_and_wakes_select() {
        let root = Context::background();
        let lifetime = root.with_cancel();
        let request = lifetime.with_timeout(Duration::from_secs(30));
        assert!(request.error().is_none());
        let waiter = {
            let request = request.clone();
            thread::spawn(move || request.wait())
        };
        thread::sleep(Duration::from_millis(10));
        lifetime.cancel();
        assert_eq!(waiter.join().unwrap(), Canceled::Canceled);
        assert_eq!(request.error(), Some(Canceled::Canceled));
        assert!(root.error().is_none());
        assert_eq!(lifetime.with_cancel().error(), Some(Canceled::Canceled));
    }

    #[test]
    fn deadlines_are_inherited_and_reported_distinctly() {
        let root = Context::background();
        let short = root.with_timeout(Duration::from_millis(15));
        let child = short.with_timeout(Duration::from_secs(30));
        let started = Instant::now();
        assert_eq!(child.wait(), Canceled::DeadlineExceeded);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(short.error(), Some(Canceled::DeadlineExceeded));
    }

    #[test]
    fn select_reports_flags_timeouts_and_priority() {
        let context = Context::background().with_cancel();
        let flag = Flag::new();
        let sources = [Source::Context(&context), Source::Flag(&flag)];
        assert_eq!(select(&sources, Some(Duration::from_millis(5))), None);
        let setter = {
            let flag = flag.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(10));
                flag.set();
            })
        };
        assert_eq!(select(&sources, Some(Duration::from_secs(5))), Some(1));
        setter.join().unwrap();
        context.cancel();
        assert_eq!(select(&sources, None), Some(0));
        assert!(flag.wait(Some(Duration::ZERO)));
    }
}
