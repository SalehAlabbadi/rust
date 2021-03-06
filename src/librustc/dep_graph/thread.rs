// Copyright 2012-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Manages the communication between the compiler's main thread and
//! the thread that constructs the dependency graph. The basic idea is
//! to use double buffering to lower the cost of producing a message.
//! In the compiler thread, we accumulate messages in a vector until
//! the vector is full, or until we want to query the graph, and then
//! we send that vector over to the depgraph thread. At the same time,
//! we receive an empty vector from the depgraph thread that we can use
//! to accumulate more messages. This way we only ever have two vectors
//! allocated (and both have a fairly large capacity).

use rustc_data_structures::veccell::VecCell;
use std::sync::mpsc::{self, Sender, Receiver};
use std::thread;

use super::DepGraphQuery;
use super::DepNode;
use super::edges::DepGraphEdges;
use super::shadow::ShadowGraph;

#[derive(Debug)]
pub enum DepMessage {
    Read(DepNode),
    Write(DepNode),
    PushTask(DepNode),
    PopTask(DepNode),
    PushIgnore,
    PopIgnore,
    Query,
}

pub struct DepGraphThreadData {
    enabled: bool,

    // The "shadow graph" is a debugging aid. We give it each message
    // in real time as it arrives and it checks for various errors
    // (for example, a read/write when there is no current task; it
    // can also apply user-defined filters; see `shadow` module for
    // details). This only occurs if debug-assertions are enabled.
    //
    // Note that in some cases the same errors will occur when the
    // data is processed off the main thread, but that's annoying
    // because it lacks precision about the source of the error.
    shadow_graph: ShadowGraph,

    // current buffer, where we accumulate messages
    messages: VecCell<DepMessage>,

    // where to receive new buffer when full
    swap_in: Receiver<Vec<DepMessage>>,

    // where to send buffer when full
    swap_out: Sender<Vec<DepMessage>>,

    // where to receive query results
    query_in: Receiver<DepGraphQuery>,
}

const INITIAL_CAPACITY: usize = 2048;

impl DepGraphThreadData {
    pub fn new(enabled: bool) -> DepGraphThreadData {
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let (txq, rxq) = mpsc::channel();

        if enabled {
            thread::spawn(move || main(rx1, tx2, txq));
        }

        DepGraphThreadData {
            enabled,
            shadow_graph: ShadowGraph::new(),
            messages: VecCell::with_capacity(INITIAL_CAPACITY),
            swap_in: rx2,
            swap_out: tx1,
            query_in: rxq,
        }
    }

    /// True if we are actually building the full dep-graph.
    #[inline]
    pub fn is_fully_enabled(&self) -> bool {
        self.enabled
    }

    /// True if (a) we are actually building the full dep-graph, or (b) we are
    /// only enqueuing messages in order to sanity-check them (which happens
    /// when debug assertions are enabled).
    #[inline]
    pub fn is_enqueue_enabled(&self) -> bool {
        self.is_fully_enabled() || self.shadow_graph.enabled()
    }

    /// Sends the current batch of messages to the thread. Installs a
    /// new vector of messages.
    fn swap(&self) {
        assert!(self.is_fully_enabled(), "should never swap if not fully enabled");

        // should be a buffer waiting for us (though of course we may
        // have to wait for depgraph thread to finish processing the
        // old messages)
        let new_messages = self.swap_in.recv().unwrap();
        assert!(new_messages.is_empty());

        // swap in the empty buffer and extract the full one
        let old_messages = self.messages.swap(new_messages);

        // send full buffer to depgraph thread to be processed
        self.swap_out.send(old_messages).unwrap();
    }

    pub fn query(&self) -> DepGraphQuery {
        assert!(self.is_fully_enabled(), "should never query if not fully enabled");
        self.enqueue(DepMessage::Query);
        self.swap();
        self.query_in.recv().unwrap()
    }

    /// Enqueue a message to be sent when things are next swapped. (If
    /// the buffer is full, this may swap.)
    #[inline]
    pub fn enqueue(&self, message: DepMessage) {
        assert!(self.is_enqueue_enabled(), "should never enqueue if not enqueue-enabled");
        self.shadow_graph.enqueue(&message);
        if self.is_fully_enabled() {
            self.enqueue_enabled(message);
        }
    }

    // Outline this fn since I expect it may want to be inlined
    // separately.
    fn enqueue_enabled(&self, message: DepMessage) {
        let len = self.messages.push(message);
        if len == INITIAL_CAPACITY {
            self.swap();
        }
    }
}

/// Definition of the depgraph thread.
pub fn main(swap_in: Receiver<Vec<DepMessage>>,
            swap_out: Sender<Vec<DepMessage>>,
            query_out: Sender<DepGraphQuery>) {
    let mut edges = DepGraphEdges::new();

    // the compiler thread always expects a fresh buffer to be
    // waiting, so queue one up
    swap_out.send(Vec::with_capacity(INITIAL_CAPACITY)).unwrap();

    // process the buffers from compiler thread as we receive them
    for mut messages in swap_in {
        for msg in messages.drain(..) {
            match msg {
                DepMessage::Read(node) => edges.read(node),
                DepMessage::Write(node) => edges.write(node),
                DepMessage::PushTask(node) => edges.push_task(node),
                DepMessage::PopTask(node) => edges.pop_task(node),
                DepMessage::PushIgnore => edges.push_ignore(),
                DepMessage::PopIgnore => edges.pop_ignore(),
                DepMessage::Query => query_out.send(edges.query()).unwrap(),
            }
        }
        if let Err(_) = swap_out.send(messages) {
            // the receiver must have been dropped already
            break;
        }
    }
}
