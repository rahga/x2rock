//! Commands in the order they were sent, where order changes the outcome.
//!
//! Each command used to run on a task of its own, so two aimed at the same
//! room could finish in either order: bass 1, 2, 3 sent fast could end at 2.
//! Here each mutation names a lane, and a lane runs its jobs one at a time in
//! the order they came. Lanes run beside each other; reads take no lane and
//! run at once. A barrier (a topology change) waits for every lane's earlier
//! work, and every lane's later work waits for it.
//!
//! A pending job that sets an absolute value may name what it sets: a newer
//! one for the same thing, sent before the pending one has started, takes its
//! place, so the last value wins without the ones between being sent. Jobs
//! that name nothing (toggles, transport, deltas) are never dropped.
//!
//! [`Lanes`] is the ordering alone, with no I/O and no clock, so the tests can
//! step it; [`spawn`] drives it on the runtime.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::Hash;

use tokio::sync::mpsc;
use tokio::task::JoinSet;

/// Where a job runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Order<K> {
    /// A read: at once, beside anything.
    Free,
    /// After the earlier jobs in the same lane.
    Lane(K),
    /// After every earlier job in every lane, and before every later one.
    Barrier,
}

/// A started job, handed back to [`Lanes::finished`] when it is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ticket(u64);

struct Pending<C, J> {
    ticket: Ticket,
    /// Barriers sent before this job: it starts once that many have finished.
    epoch: u64,
    replaces: Option<C>,
    job: J,
}

struct Lane<C, J> {
    /// The job running, and its epoch.
    running: Option<(Ticket, u64)>,
    queue: VecDeque<Pending<C, J>>,
}

/// The ordering: which jobs may start now.
pub struct Lanes<K, C, J> {
    lanes: HashMap<K, Lane<C, J>>,
    barriers: VecDeque<(Ticket, J)>,
    barrier_running: Option<Ticket>,
    /// Running lane jobs, by ticket, and their lane.
    running: HashMap<Ticket, K>,
    /// Barriers sent so far.
    sent: u64,
    /// Barriers finished so far.
    passed: u64,
    next: u64,
}

impl<K: Eq + Hash + Clone, C: PartialEq, J> Lanes<K, C, J> {
    pub fn new() -> Self {
        Lanes {
            lanes: HashMap::new(),
            barriers: VecDeque::new(),
            barrier_running: None,
            running: HashMap::new(),
            sent: 0,
            passed: 0,
            next: 0,
        }
    }

    fn ticket(&mut self) -> Ticket {
        self.next += 1;
        Ticket(self.next)
    }

    /// Take a job; what may start now, the job itself included or not.
    pub fn submit(&mut self, order: Order<K>, replaces: Option<C>, job: J) -> Vec<(Ticket, J)> {
        match order {
            Order::Free => vec![(self.ticket(), job)],
            Order::Lane(key) => {
                let epoch = self.sent;
                let lane = self.lanes.entry(key).or_insert_with(|| Lane {
                    running: None,
                    queue: VecDeque::new(),
                });
                // Not across a barrier: the one waiting before it keeps its place.
                if let Some(replaces) = replaces.as_ref()
                    && let Some(pending) = lane
                        .queue
                        .iter_mut()
                        .find(|p| p.epoch == epoch && p.replaces.as_ref() == Some(replaces))
                {
                    pending.job = job;
                    return Vec::new();
                }
                self.next += 1;
                let ticket = Ticket(self.next);
                lane.queue.push_back(Pending {
                    ticket,
                    epoch,
                    replaces,
                    job,
                });
                self.start()
            }
            Order::Barrier => {
                let ticket = self.ticket();
                self.sent += 1;
                self.barriers.push_back((ticket, job));
                self.start()
            }
        }
    }

    /// A started job is done, however it went; what may start now.
    pub fn finished(&mut self, ticket: Ticket) -> Vec<(Ticket, J)> {
        if self.barrier_running == Some(ticket) {
            self.barrier_running = None;
            self.passed += 1;
        } else if let Some(key) = self.running.remove(&ticket)
            && let Some(lane) = self.lanes.get_mut(&key)
        {
            lane.running = None;
            if lane.queue.is_empty() {
                self.lanes.remove(&key);
            }
        }
        self.start()
    }

    fn start(&mut self) -> Vec<(Ticket, J)> {
        if self.barrier_running.is_some() {
            return Vec::new();
        }
        // Every lane job left is of an epoch not yet passed, so the next
        // barrier waits only on those of the current one.
        let current = |lane: &Lane<C, J>, passed: u64| {
            lane.running.is_some_and(|(_, e)| e == passed)
                || lane.queue.front().is_some_and(|p| p.epoch == passed)
        };
        if !self.barriers.is_empty() && !self.lanes.values().any(|l| current(l, self.passed)) {
            let (ticket, job) = self.barriers.pop_front().expect("a barrier");
            self.barrier_running = Some(ticket);
            return vec![(ticket, job)];
        }
        let mut started = Vec::new();
        for (key, lane) in self.lanes.iter_mut() {
            if lane.running.is_none() && lane.queue.front().is_some_and(|p| p.epoch == self.passed)
            {
                let pending = lane.queue.pop_front().expect("a pending job");
                lane.running = Some((pending.ticket, pending.epoch));
                self.running.insert(pending.ticket, key.clone());
                started.push((pending.ticket, pending.job));
            }
        }
        started
    }
}

/// The runtime's end: sending only queues, it never waits for a job.
pub struct Sender<K, C, J> {
    jobs: mpsc::UnboundedSender<(Order<K>, Option<C>, J)>,
}

impl<K, C, J> Sender<K, C, J> {
    pub fn submit(&self, order: Order<K>, replaces: Option<C>, job: J) {
        let _ = self.jobs.send((order, replaces, job));
    }
}

/// Run each job sent with `run`, in order. Once every sender is gone, what
/// was sent still runs, and then the task ends.
pub fn spawn<K, C, J, F, Fut>(run: F) -> Sender<K, C, J>
where
    K: Eq + Hash + Clone + Send + 'static,
    C: PartialEq + Send + 'static,
    J: Send + 'static,
    F: Fn(J) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (jobs, mut inbox) = mpsc::unbounded_channel::<(Order<K>, Option<C>, J)>();
    tokio::spawn(async move {
        let mut lanes = Lanes::new();
        // By task id, so a job that panics still frees its lane.
        let mut tickets = HashMap::new();
        let mut running = JoinSet::new();
        let mut open = true;
        loop {
            let ready = tokio::select! {
                sent = inbox.recv(), if open => match sent {
                    Some((order, replaces, job)) => lanes.submit(order, replaces, job),
                    None => {
                        open = false;
                        continue;
                    }
                },
                Some(done) = running.join_next_with_id() => {
                    let id = match done {
                        Ok((id, ())) => id,
                        Err(e) => e.id(),
                    };
                    match tickets.remove(&id) {
                        Some(ticket) => lanes.finished(ticket),
                        None => Vec::new(),
                    }
                }
                else => return,
            };
            for (ticket, job) in ready {
                let handle = running.spawn(run(job));
                tickets.insert(handle.id(), ticket);
            }
        }
    });
    Sender { jobs }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::oneshot;

    type Core = Lanes<&'static str, &'static str, &'static str>;

    fn names(started: Vec<(Ticket, &'static str)>) -> Vec<&'static str> {
        let mut names: Vec<_> = started.into_iter().map(|(_, j)| j).collect();
        names.sort();
        names
    }

    #[test]
    fn one_lane_runs_in_the_order_sent_and_lanes_run_beside_each_other() {
        let mut lanes = Core::new();
        let a = lanes.submit(Order::Lane("kitchen"), None, "a");
        assert_eq!(names(a.clone()), ["a"]);
        assert!(
            lanes.submit(Order::Lane("kitchen"), None, "b").is_empty(),
            "b waits for a"
        );
        assert_eq!(
            names(lanes.submit(Order::Lane("office"), None, "c")),
            ["c"],
            "another lane is not held"
        );
        assert_eq!(names(lanes.finished(a[0].0)), ["b"]);
    }

    #[test]
    fn a_read_starts_at_once_beside_a_stalled_lane() {
        let mut lanes = Core::new();
        lanes.submit(Order::Lane("kitchen"), None, "write");
        assert_eq!(names(lanes.submit(Order::Free, None, "read")), ["read"]);
    }

    #[test]
    fn a_newer_value_takes_a_pending_ones_place_and_nothing_else_is_dropped() {
        let mut lanes = Core::new();
        let first = lanes.submit(Order::Lane("kitchen"), Some("bass"), "bass 1");
        assert!(
            lanes
                .submit(Order::Lane("kitchen"), Some("bass"), "bass 2")
                .is_empty()
        );
        assert!(
            lanes
                .submit(Order::Lane("kitchen"), None, "loudness on")
                .is_empty()
        );
        assert!(
            lanes
                .submit(Order::Lane("kitchen"), None, "loudness off")
                .is_empty()
        );
        assert!(
            lanes
                .submit(Order::Lane("kitchen"), Some("treble"), "treble 4")
                .is_empty()
        );
        assert!(
            lanes
                .submit(Order::Lane("kitchen"), Some("bass"), "bass 3")
                .is_empty()
        );
        let mut ran = Vec::new();
        let mut next = lanes.finished(first[0].0);
        while let Some((ticket, job)) = next.pop() {
            ran.push(job);
            next = lanes.finished(ticket);
        }
        assert_eq!(ran, ["bass 3", "loudness on", "loudness off", "treble 4"]);
    }

    #[test]
    fn a_barrier_waits_for_earlier_work_and_holds_back_later_work() {
        let mut lanes = Core::new();
        let play = lanes.submit(Order::Lane("kitchen"), None, "play");
        let bass = lanes.submit(Order::Lane("office"), None, "bass");
        assert!(
            lanes.submit(Order::Barrier, None, "group").is_empty(),
            "the barrier waits for both"
        );
        assert!(
            lanes.submit(Order::Lane("den"), None, "next").is_empty(),
            "a later job waits for the barrier, in any lane"
        );
        assert_eq!(
            names(lanes.submit(Order::Free, None, "read")),
            ["read"],
            "a read is not held"
        );
        assert!(
            lanes.finished(play[0].0).is_empty(),
            "the barrier still waits for the office"
        );
        let group = lanes.finished(bass[0].0);
        assert_eq!(names(group.clone()), ["group"]);
        assert_eq!(names(lanes.finished(group[0].0)), ["next"]);
    }

    #[test]
    fn barriers_run_one_at_a_time_in_order() {
        let mut lanes = Core::new();
        let first = lanes.submit(Order::Barrier, None, "split");
        assert_eq!(names(first.clone()), ["split"]);
        assert!(
            lanes
                .submit(Order::Lane("kitchen"), None, "play")
                .is_empty()
        );
        assert!(lanes.submit(Order::Barrier, None, "group").is_empty());
        let play = lanes.finished(first[0].0);
        assert_eq!(
            names(play.clone()),
            ["play"],
            "work sent between two barriers runs between them"
        );
        assert_eq!(names(lanes.finished(play[0].0)), ["group"]);
    }

    #[test]
    fn a_newer_value_does_not_jump_a_barrier() {
        let mut lanes = Core::new();
        let busy = lanes.submit(Order::Lane("kitchen"), None, "busy");
        lanes.submit(Order::Lane("kitchen"), Some("bass"), "bass 1");
        lanes.submit(Order::Barrier, None, "preset");
        lanes.submit(Order::Lane("kitchen"), Some("bass"), "bass 2");
        let mut ran = Vec::new();
        let mut next = lanes.finished(busy[0].0);
        while let Some((ticket, job)) = next.pop() {
            ran.push(job);
            next = lanes.finished(ticket);
        }
        assert_eq!(ran, ["bass 1", "preset", "bass 2"]);
    }

    /// A job for the driver: it finishes when its gate opens, if it has one,
    /// and then says so.
    struct Gated {
        name: &'static str,
        gate: Option<oneshot::Receiver<()>>,
    }

    fn driver() -> (
        Sender<&'static str, &'static str, Gated>,
        mpsc::UnboundedReceiver<&'static str>,
    ) {
        let (done, finished) = mpsc::unbounded_channel();
        let sender = spawn(move |job: Gated| {
            let done = done.clone();
            async move {
                if let Some(gate) = job.gate {
                    let _ = gate.await;
                }
                let _ = done.send(job.name);
            }
        });
        (sender, finished)
    }

    fn gated(name: &'static str) -> (Gated, oneshot::Sender<()>) {
        let (open, gate) = oneshot::channel();
        (
            Gated {
                name,
                gate: Some(gate),
            },
            open,
        )
    }

    fn free(name: &'static str) -> Gated {
        Gated { name, gate: None }
    }

    /// The next job to finish. The timeout only turns a hang into a failure.
    async fn next(finished: &mut mpsc::UnboundedReceiver<&'static str>) -> &'static str {
        tokio::time::timeout(Duration::from_secs(10), finished.recv())
            .await
            .expect("a job finished")
            .expect("the driver is running")
    }

    #[tokio::test]
    async fn a_second_write_does_not_finish_before_a_stalled_first() {
        let (lanes, mut finished) = driver();
        let (first, open) = gated("first");
        lanes.submit(Order::Lane("kitchen"), None, first);
        lanes.submit(Order::Lane("kitchen"), None, free("second"));
        // A read sent after both: once it is done, the second write would
        // have been done too, had it not waited.
        lanes.submit(Order::Free, None, free("read"));
        assert_eq!(
            next(&mut finished).await,
            "read",
            "a read beside a stalled lane is answered"
        );
        open.send(()).unwrap();
        assert_eq!(next(&mut finished).await, "first");
        assert_eq!(next(&mut finished).await, "second");
    }

    #[tokio::test]
    async fn three_bass_values_with_the_first_stalled_end_at_the_last() {
        let level = Arc::new(Mutex::new(0));
        let (done, mut finished) = mpsc::unbounded_channel();
        let lanes = spawn({
            let level = level.clone();
            move |(value, gate): (i8, Option<oneshot::Receiver<()>>)| {
                let level = level.clone();
                let done = done.clone();
                async move {
                    if let Some(gate) = gate {
                        let _ = gate.await;
                    }
                    *level.lock().unwrap() = value;
                    let _ = done.send(value);
                }
            }
        });
        let (open, gate) = oneshot::channel();
        lanes.submit(Order::Lane("kitchen"), Some("bass"), (1, Some(gate)));
        lanes.submit(Order::Lane("kitchen"), Some("bass"), (2, None));
        lanes.submit(Order::Lane("kitchen"), Some("bass"), (3, None));
        open.send(()).unwrap();
        let mut sent = Vec::new();
        while sent.last() != Some(&3) {
            sent.push(
                tokio::time::timeout(Duration::from_secs(10), finished.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(sent, [1, 3], "the 2 was replaced before it was sent");
        assert_eq!(*level.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn a_topology_change_waits_for_group_work_and_holds_back_what_follows() {
        let (lanes, mut finished) = driver();
        let (play, open) = gated("play");
        lanes.submit(Order::Lane("kitchen"), None, play);
        lanes.submit(Order::Barrier, None, free("group"));
        lanes.submit(Order::Lane("office"), None, free("after"));
        lanes.submit(Order::Free, None, free("read"));
        assert_eq!(next(&mut finished).await, "read");
        open.send(()).unwrap();
        assert_eq!(next(&mut finished).await, "play");
        assert_eq!(next(&mut finished).await, "group");
        assert_eq!(next(&mut finished).await, "after");
    }

    #[tokio::test]
    async fn a_job_that_panics_frees_its_lane() {
        let (done, mut finished) = mpsc::unbounded_channel();
        let lanes = spawn(move |name: &'static str| {
            let done = done.clone();
            async move {
                assert_ne!(name, "panics", "this job panics on purpose");
                let _ = done.send(name);
            }
        });
        lanes.submit(Order::Lane("kitchen"), None::<&str>, "panics");
        lanes.submit(Order::Lane("kitchen"), None, "after");
        assert_eq!(next(&mut finished).await, "after");
    }
}
