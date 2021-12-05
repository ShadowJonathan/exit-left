use std::{
    collections::{hash_map::Entry, HashMap},
    hash,
    ops::DerefMut,
    thread::{self, Thread},
    time::{Duration, Instant},
};

use parking_lot::{Mutex, MutexGuard};

type Queue<T> = Mutex<HashMap<T, Place>>;

// A ZST marker type that helps/ensures the right MutexGuard is passed into a new Director
struct Head;

enum Place {
    Parked(Thread),
    Ticket(bool),
}

/// A synchronization primitive to ensure a single thread has access to a resource,
/// but also be able to notify any waiting thread that their "condition" is satisfied.
///
/// A stage does 4 things;
/// 1. It maintains a queue of threads that want to perform a blocking operation on H.
/// 2. It allows only one thread to perform that blocking operation, and immediately queues up a next thread if the thread exits the operation.
/// 3. It guards this H object and gives a borrow to the operation that is currently "heading" the stage
/// 4. It holds a "buffer" B object that can be mutably borrowed by the current heading thread, or any other thread.
///
/// This model of parallel computation is based off of the idea of a "Multiplexed Concurrent Single-Threaded" access.
/// For example, the [UdpSocket](std::net::UdpSocket) can receive packets from many sources, and can also send it to those many sources,
/// however, there may be multiple threads interested in waiting for packets from a particular source,
/// and so threads must be able to coordinate which results are "for them", and which are not.
///
/// This object can help with that, by having every heading thread sort out these packets, and if the packet is not destined for them,
/// it gets put into a buffer, after which the thread that was waiting for a packet from that source gets notified.
///
/// The access model could technically also say "Any Concurrent Write", with [`backdoor`](Self::backdoor), but it requires holding invariants,
/// read that function's documentation before continuing.
pub struct Stage<T, R, B>
where
    T: hash::Hash + Eq,
    R: Send,
    B: Send,
{
    head: Mutex<Head>,
    queue: Queue<T>,

    resource: R,
    buffer: Mutex<B>,
}

impl<T, Res, B> Stage<T, Res, B>
where
    T: hash::Hash + Eq + Clone,
    Res: Send,
    B: Send,
{
    /// Constructs a new Stage from the provided backing resource and the buffer
    pub fn new(resource: Res, buffer: B) -> Stage<T, Res, B>
    where
        T: hash::Hash + Eq + Clone,
    {
        Stage {
            head: Mutex::new(Head),
            queue: Mutex::new(HashMap::new()),

            resource,
            buffer: Mutex::new(buffer),
        }
    }

    /// Attempts to get a ticket to enter the stage.
    ///
    /// This is useful for three reasons;
    ///
    /// First, if we return a Some, we can ensure that,
    /// inbetween this function returning and the ticket being passed to [`enter`](Stage::enter),
    /// all token activations will be recorded.
    ///
    /// Second, if this ticket is dropped, then the associated token is removed from the stage.
    ///
    /// Third, internally, we need to ensure that only one "someone" is associated with a token,
    /// when we return Some here, we ensure that you're the only one that will be activated by the token,
    /// next time you put it into enter, and that any other thread cannot "undercut" your resource.
    pub fn ticket<'s>(&'s self, token: T) -> Option<Ticket<'s, T>> {
        let mut map = self.queue.lock();

        match map.entry(token.clone()) {
            Entry::Occupied(_) => {
                // Something else already took this token, abort
                return None;
            }
            Entry::Vacant(entry) => {
                entry.insert(Place::Ticket(false));
                return Some(Ticket::new(&self.queue, token));
            }
        }
    }

    /// This "enters" the stage.
    ///
    /// From this point on, in your thread, one of three things are guaranteed to happen.
    /// 1. Your thread gets to "head" the stage, and `head` is executed.
    /// 2. Another thread activates your token, and this returns then immidiately.
    /// 3. If you supplied a `timeout`, and nothing happened until then, this returns.
    ///
    /// Correspondingly, for each, a [`Exit`] variant exists, with [`Exit::Finished`] returning
    /// the result from a head, if any has ran.
    pub fn enter<Ret>(
        &self,
        ticket: Ticket<'_, T>,
        timeout: Option<Duration>,
        head: impl FnOnce(&Director<T, Res, B>) -> Ret,
    ) -> Exit<Ret> {
        // We need an extra context as to make extra sure seat is the last one to get dropped.
        {
            let deadline = timeout.map(|d| Instant::now() + d);

            let head_lock = {
                let mut map = self.queue.lock();

                if let Some(place) = map.get(ticket.get()) {
                    match place {
                        Place::Parked(t) => panic!("We got into an impossible situation; the ticket ensures we are the only one to potentially wait on a token, but another thread was already parked on it; {:?}", t.id()),
                        Place::Ticket(state) => if *state {
                            // Our token has been called inbetween getting a ticket and entering the stage, returning early with Token
                            return Exit::Token;
                        },
                    }
                }

                let nobody_parked = map.values().all(|v| match v {
                    Place::Parked(_) => false,
                    Place::Ticket(_) => true,
                });

                map.insert(ticket.get().clone(), Place::Parked(thread::current()));

                // Nobody has parked yet, so we assume head
                if nobody_parked {
                    Some(self.head.try_lock().expect("The map was empty, everything 'closes the door behind them', so we're in an impossible state"))
                } else {
                    None
                }
            };

            if let Some(lock) = head_lock {
                // We got in first, so we're allowed to execute head here.
                let dir = Director::new(self, ticket, lock);
                return Exit::Finished(head(&dir));
            }

            loop {
                // We joined late, so we wait.
                // This will immediately proceed if we somehow got race-conditioned between inserting the token and waiting,
                // as thread parking will immediately return if someone else unparked the thread while it has been running.
                //
                // There is a small but real possibility that something other than us unparks this thread,
                // so we don't rely on this to *make sure* we are head now, but that we are simply waiting for something.
                if let Some(deadline) = deadline {
                    let dur = if let Some(x) = deadline.checked_duration_since(Instant::now()) {
                        x
                    } else {
                        // We already passed the deadline (somehow?), return
                        return Exit::Timeout;
                    };

                    thread::park_timeout(dur);

                    if Instant::now() >= deadline {
                        // We timed out, return
                        return Exit::Timeout;
                    }
                } else {
                    thread::park();
                }

                // We signal that the token is ready by not having it in the map anymore, so here we check.
                if !self.queue.lock().contains_key(ticket.get()) {
                    // We got woken up because of a token, return for that.
                    return Exit::Token;
                } else {
                    // We got woken up not because of the token, that's for sure.

                    // Try to acquire the head lock.
                    let lock = self.head.try_lock();

                    if let Some(lock) = lock {
                        // We have the head lock, we're allowed to execute it now.
                        let dir = Director::new(self, ticket, lock);
                        return Exit::Finished(head(&dir));
                    } else {
                        // We don't have a head lock???
                        // We may have been woken up by something random.
                        //
                        // Before we loop, a few things are clear here;
                        // 1. The token is still in the map, so we can get woken up again
                        // 2. We have tried headlock, which means that there is a head out there, churning away
                    }
                }
            }
        }
    }

    /// Grantes a mutex guard (mutable) reference to the buffer.
    pub fn buffer(&self) -> MutexGuard<'_, B> {
        self.buffer.lock()
    }

    /// Provides "backdoor" access to the stage's protected resource.
    ///
    /// This is not really "unsafe" in the memory way, but rather "unsafe" in the semantic way.
    ///
    /// The stage ultimately assumes a backing resource such as [`UdpSocket`](std::net::UdpSocket) to be primarily used with this,
    /// this resource only needs `&`-access for all of its functions, and to ensure that only one thread can `recv`, this struct was created.
    ///
    /// However, to *write* values, the same applies, and for `UdpSocket` specifically, any thread can write to it simultaneously.
    ///
    /// Thus, this function is marked as unsafe as to *draw attention to make sure what you're doing*.
    ///
    /// The invariant that is being upheld here is **that you do not perform functions on this resource, via this access, that a "heading" thread should do**.
    ///
    /// For the example of `UdpSocket`, this means that this access should only have `send`s, no `recv`s, this semantic is up to the caller to decide and uphold,
    /// this is only marked unsafe as to draw extra attention in code review.
    pub unsafe fn backdoor(&self) -> &Res {
        &self.resource
    }
}

/// The control flow variable to mark how the thread exited the stage.
pub enum Exit<R> {
    // The thread has headed the stage, and then returned with a return variable.
    Finished(R),
    // The thread has given a timeout, which expired.
    Timeout,
    // The thread's token was activated.
    Token,
}

/// A "Permit" to enter the stage.
///
/// See [`Stage::ticket`] for the guarantees this brings.
///
/// Dropping this removes the token from the stage.
pub struct Ticket<'s, T>
where
    T: hash::Hash + Eq,
{
    token: Option<T>,
    queue: &'s Queue<T>,
}

impl<T> Ticket<'_, T>
where
    T: hash::Hash + Eq,
{
    fn new<'s>(queue: &'s Queue<T>, token: T) -> Ticket<'_, T> {
        Ticket {
            token: Some(token),
            queue,
        }
    }

    fn take(mut self) -> T {
        self.token
            .take()
            .expect("Nobody takes the token other than this function")
    }

    fn get(&self) -> &T {
        self.token
            .as_ref()
            .expect("Nobody takes the token other than .take()")
    }
}

impl<T> Drop for Ticket<'_, T>
where
    T: hash::Hash + Eq,
{
    fn drop(&mut self) {
        if let Some(token) = &self.token {
            // stage.lock().retain(|_, v| {
            //     match v {
            //         Place::Parked(v) => v.id() != id,
            //         _ => true,
            //     }
            // });
            self.queue.lock().remove(token);
        }
    }
}

/// A helper struct to make heading slightly easier.
///
/// This provides access to notifying threads, getting a borrow to the resource, and getting a mutable borrow to the buffer.
pub struct Director<'d, T, R, B>
where
    T: hash::Hash + Eq,
    R: Send,
    B: Send,
{
    stage: &'d Stage<T, R, B>,
    token: T,
    _guard: Option<MutexGuard<'d, Head>>,
}

impl<T, R, B> Director<'_, T, R, B>
where
    T: hash::Hash + Eq,
    R: Send,
    B: Send,
{
    fn new<'s>(
        stage: &'s Stage<T, R, B>,
        ticket: Ticket<'s, T>,
        head_lock: MutexGuard<'s, Head>,
    ) -> Director<'s, T, R, B> {
        let token = ticket.take();

        Director {
            stage,
            token,
            _guard: Some(head_lock),
        }
    }

    /// Get scoped access to the buffer.
    pub fn buffer(&self, func: impl FnOnce(&mut B)) {
        func(self.stage.buffer.lock().deref_mut())
    }

    /// Get borrow access to the resource.
    pub fn resource(&self) -> &R {
        &self.stage.resource
    }

    /// Notify a thread with a particular token.
    ///
    /// Be sure to call and modify [`buffer`](Self::buffer) before calling this, as when this returns, the thread might be running already.
    pub fn notify(&self, token: T) {
        if let Entry::Occupied(mut o) = self.stage.queue.lock().entry(token) {
            match o.get_mut() {
                Place::Parked(_) => {
                    // We already know this is Parked, but eh, guarantees.
                    if let Place::Parked(t) = o.remove() {
                        t.unpark()
                    }
                }
                Place::Ticket(state) => {
                    // We only set state to true, nothing else
                    *state = true
                }
            }
        }
    }
}

impl<T, R, B> Drop for Director<'_, T, R, B>
where
    T: hash::Hash + Eq,
    R: Send,
    B: Send,
{
    fn drop(&mut self) {
        let mut map = self.stage.queue.lock();

        map.remove(&self.token);

        // Drop the guard before unparking the next thread, to make sure it can "immediately" acquire it.
        drop(self._guard.take());

        for (_, p) in map.iter() {
            if let Place::Parked(t) = p {
                t.unpark();

                return;
            }
        }
    }
}
