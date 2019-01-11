mod graph;

use std::mem;
use std::ops::Add;
use std::hash::{ Hash, BuildHasher };
use std::vec::IntoIter;
use std::collections::hash_map::RandomState;
use futures::stream::futures_unordered::FuturesUnordered;
use futures::sync::{oneshot, BiLock};
use futures::prelude::*;
use crate::graph::Graph;
pub use crate::graph::Index;


pub struct TaskGraph<T, I=u32, S=RandomState> {
    dag: Graph<State<T>, I, S>,
    pending: Vec<IndexFuture<T, I>>
}

enum State<T> {
    Pending {
        count: usize,
        task: T
    },
    Running,
}

impl<T, I, S> Default for TaskGraph<T, I, S>
where
    I: Default + Hash + PartialEq + Eq,
    S: Default + BuildHasher
{
    fn default() -> TaskGraph<T, I, S> {
        TaskGraph { dag: Graph::default(), pending: Vec::new() }
    }
}

impl<T> TaskGraph<T> {
    pub fn new() -> Self {
        TaskGraph::default()
    }
}

impl<T, I, S> TaskGraph<T, I, S>
where
    T: Future,
    for<'a> &'a I: Add<I>,
    for<'a> <&'a I as Add<I>>::Output: Into<I>,
    I: From<u32> + Hash + PartialEq + Eq + Clone,
    S: BuildHasher
{
    pub fn add_task(&mut self, deps: &[Index<I>], task: T) -> Index<I> {
        if deps.is_empty() {
            let index = self.dag.add_node(State::Running);
            self.pending.push(IndexFuture::new(index.clone(), task));
            index
        } else {
            let index = self.dag.add_node(State::Pending { count: deps.len(), task });
            for parent in deps {
                self.dag.add_edge(parent, index.clone());
            }
            index
        }
    }

    pub fn execute(mut self) -> (AddTask<T, I, S>, Execute<T, I, S>) {
        let mut queue = FuturesUnordered::new();
        for fut in self.pending.drain(..) {
            queue.push(fut);
        }
        let (g1, g2) = BiLock::new(self);
        let (tx, rx) = oneshot::channel();
        (
            AddTask { inner: g1, tx },
            Execute { inner: g2, done: Vec::new(), queue, rx }
        )
    }

    fn walk(&mut self, index: &Index<I>) -> TaskWalker<'_, T, I, S> {
        let walker = self.dag.walk(index);
        TaskWalker { dag: &mut self.dag, walker }
    }
}

pub struct AddTask<T, I=u32, S=RandomState> {
    inner: BiLock<TaskGraph<T, I, S>>,
    tx: oneshot::Sender<()>
}

impl<T, I, S> AddTask<T, I, S>
where
    for<'a> &'a I: Add<I>,
    for<'a> <&'a I as Add<I>>::Output: Into<I>,
    I: From<u32> + Hash + PartialEq + Eq + Clone,
    S: BuildHasher
{
    pub fn add_task(&self, deps: &[Index<I>], task: T) -> Async<Index<I>> {
        let mut graph = match self.inner.poll_lock() {
            Async::Ready(graph) => graph,
            Async::NotReady => return Async::NotReady
        };

        let count = deps.iter()
            .filter(|&i| graph.dag.contains(i))
            .count();
        if count == 0 {
            let index = graph.dag.add_node(State::Running);
            graph.pending.push(IndexFuture::new(index.clone(), task));
            Async::Ready(index)
        } else {
            let index = graph.dag.add_node(State::Pending { count, task });
            for parent in deps {
                graph.dag.add_edge(parent, index.clone());
            }
            Async::Ready(index)
        }
    }

    pub fn abort(self) {
        let _ = self.tx.send(());
    }
}

pub struct Execute<T, I=u32, S=RandomState> {
    inner: BiLock<TaskGraph<T, I, S>>,
    queue: FuturesUnordered<IndexFuture<T, I>>,
    done: Vec<Index<I>>,
    rx: oneshot::Receiver<()>
}

impl<T, I, S> Execute<T, I, S>
where
    T: Future,
    for<'a> &'a I: Add<I>,
    for<'a> <&'a I as Add<I>>::Output: Into<I>,
    I: From<u32> + Hash + PartialEq + Eq + Clone,
    S: BuildHasher
{
    fn enqueue(&mut self) -> Async<()> {
        let mut graph = match self.inner.poll_lock() {
            Async::Ready(graph) => graph,
            Async::NotReady => return Async::NotReady
        };

        for fut in graph.pending.drain(..) {
            self.queue.push(fut);
        }

        for index in self.done.drain(..) {
            for fut in graph.walk(&index) {
                self.queue.push(fut);
            }
            graph.dag.remove_node(&index);
        }

        Async::Ready(())
    }
}

impl<F, I, S> Stream for Execute<F, I, S>
where
    F: Future,
    for<'i> &'i I: Add<I>,
    for<'i> <&'i I as Add<I>>::Output: Into<I>,
    I: From<u32> + Hash + PartialEq + Eq + Clone,
    S: BuildHasher
{
    type Item = (Index<I>, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.rx.poll() {
            Ok(Async::NotReady) => (),
            Ok(Async::Ready(())) | Err(_) => return Ok(Async::Ready(None))
        }

        // TODO keep poll ?
        let _ = self.enqueue();

        match self.queue.poll() {
            Ok(Async::Ready(Some((i, item)))) => {
                self.done.push(i.clone());
                Ok(Async::Ready(Some((i, item))))
            },
            Ok(Async::Ready(None)) | Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err)
        }
    }
}

struct IndexFuture<F, I> {
    index: Index<I>,
    fut: F
}

impl<F, I> IndexFuture<F, I> {
    pub fn new(index: Index<I>, fut: F) -> IndexFuture<F, I> {
        IndexFuture { index, fut }
    }
}

impl<F: Future, I: Clone> Future for IndexFuture<F, I> {
    type Item = (Index<I>, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.fut.poll() {
            Ok(Async::Ready(item)) => Ok(Async::Ready((self.index.clone(), item))),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err)
        }
    }
}

struct TaskWalker<'a, T, I, S> {
    dag: &'a mut Graph<State<T>, I, S>,
    walker: IntoIter<Index<I>>
}

impl<'a, T, I, S> Iterator for TaskWalker<'a, T, I, S>
where
    for<'i> &'i I: Add<I>,
    for<'i> <&'i I as Add<I>>::Output: Into<I>,
    I: From<u32> + Hash + PartialEq + Eq + Clone,
    S: BuildHasher
{
    type Item = IndexFuture<T, I>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(index) = self.walker.next() {
            let state = match self.dag.get_node_mut(&index) {
                Some(node) => node,
                None => continue
            };

            if let State::Pending { count, .. } = state {
                *count -= 1;
            }

            match state {
                State::Pending { count, .. } if *count == 0 => (),
                _ => continue
            }

            if let State::Pending { task, .. } = mem::replace(state, State::Running) {
                return Some(IndexFuture::new(index, task));
            }
        }

        None
    }
}
