mod graph;

use std::mem;
use std::vec::IntoIter;
use futures::stream::futures_unordered::FuturesUnordered;
use futures::sync::{oneshot, BiLock};
use futures::prelude::*;
use crate::graph::Graph;
pub use crate::graph::Index;


pub struct TaskGraph<T: Future> {
    dag: Graph<State<T>>,
    pending: Vec<IndexFuture<T>>
}

enum State<T: Future> {
    Pending {
        count: usize,
        task: T
    },
    Running,
}

impl<T: Future> Default for TaskGraph<T> {
    fn default() -> TaskGraph<T> {
        TaskGraph { dag: Graph::default(), pending: Vec::new() }
    }
}

impl<T: Future> TaskGraph<T> {
    pub fn add_task(&mut self, deps: &[Index], task: T) -> Index {
        if deps.is_empty() {
            let index = self.dag.add_node(State::Running);
            self.pending.push(IndexFuture::new(index, task));
            index
        } else {
            let index = self.dag.add_node(State::Pending { count: deps.len(), task });
            for &parent in deps {
                self.dag.add_edge(parent, index);
            }
            index
        }
    }

    pub fn execute(mut self) -> (AddTask<T>, Execute<T>) {
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

    fn walk(&mut self, index: Index) -> TaskWalker<'_, T> {
        let walker = self.dag.walk(index);
        TaskWalker { dag: &mut self.dag, walker }
    }
}

pub struct AddTask<T: Future> {
    inner: BiLock<TaskGraph<T>>,
    tx: oneshot::Sender<()>
}

impl<T: Future> AddTask<T> {
    pub fn add_task(&self, deps: &[Index], task: T) -> Async<Index> {
        let mut graph = match self.inner.poll_lock() {
            Async::Ready(graph) => graph,
            Async::NotReady => return Async::NotReady
        };

        let count = deps.iter()
            .filter(|&&i| graph.dag.contains(i))
            .count();
        if count == 0 {
            let index = graph.dag.add_node(State::Running);
            graph.pending.push(IndexFuture::new(index, task));
            Async::Ready(index)
        } else {
            let index = graph.dag.add_node(State::Pending { count, task });
            for &parent in deps {
                graph.dag.add_edge(parent, index);
            }
            Async::Ready(index)
        }
    }

    pub fn abort(self) {
        let _ = self.tx.send(());
    }
}

pub struct Execute<T: Future> {
    inner: BiLock<TaskGraph<T>>,
    queue: FuturesUnordered<IndexFuture<T>>,
    done: Vec<Index>,
    rx: oneshot::Receiver<()>
}

impl<T: Future> Execute<T> {
    fn enqueue(&mut self) -> Async<()> {
        let mut graph = match self.inner.poll_lock() {
            Async::Ready(graph) => graph,
            Async::NotReady => return Async::NotReady
        };

        for fut in graph.pending.drain(..) {
            self.queue.push(fut);
        }

        for index in self.done.drain(..) {
            for fut in graph.walk(index) {
                self.queue.push(fut);
            }
            graph.dag.remove_node(index);
        }

        Async::Ready(())
    }
}

impl<T: Future> Stream for Execute<T> {
    type Item = (Index, T::Item);
    type Error = T::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.rx.poll() {
            Ok(Async::NotReady) => (),
            Ok(Async::Ready(())) | Err(_) => return Ok(Async::Ready(None))
        }

        // TODO keep poll ?
        let _ = self.enqueue();

        match self.queue.poll() {
            Ok(Async::Ready(Some((i, item)))) => {
                self.done.push(i);
                Ok(Async::Ready(Some((i, item))))
            },
            Ok(Async::Ready(None)) | Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err)
        }
    }
}

struct IndexFuture<F: Future> {
    index: Index,
    fut: F
}

impl<F: Future> IndexFuture<F> {
    pub fn new(index: Index, fut: F) -> IndexFuture<F> {
        IndexFuture { index, fut }
    }
}

impl<F: Future> Future for IndexFuture<F> {
    type Item = (Index, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.fut.poll() {
            Ok(Async::Ready(item)) => Ok(Async::Ready((self.index, item))),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err)
        }
    }
}

struct TaskWalker<'a, T: Future> {
    dag: &'a mut Graph<State<T>>,
    walker: IntoIter<Index>
}

impl<'a, T: Future> Iterator for TaskWalker<'a, T> {
    type Item = IndexFuture<T>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(index) = self.walker.next() {
            let state = match self.dag.get_node_mut(index) {
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
