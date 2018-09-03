//! An Non-Blocking Concurrent Channel (NBCC) is a channel that allows users to send and receive
//! data from multiple threads using a ring buffer as the backing store for the channel. The
//! channel also allows skip semantics with a cursor which allows the user to skip dequeueing
//! messages in the buffer and process those messages later by using a cursor.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Type alias for half of a usize on 64 bit platform.
type HalfUsize = u32;

/// Number of bits in half of a usize on 64 bit platform.
const HALF_USIZE_BITS: u8 = 32;

/// Bitmask used to mask out the upper bits of a usize, leaving only lower bits.
const LOWER_USIZE_BITMASK: usize = 0xFFFFFFFF;

/// Value used to indicate that a position index points to no other node.
const NIL_NODE: usize = 1 << 63 as usize;

/// Errors potentially returned from run queue operations.
#[derive(Debug, Eq, PartialEq)]
pub enum ChannelErrors {
    Full,
    Empty,
}

/// A single node in the run queue's ring buffer.
struct ChannelNode<T: Sync + Send> {
    /// Contains a value set in the node in a Some or a None if empty. Note that this is unsafe
    /// in order to get around Rust mutability locks so that this data structure can be passed
    /// around immutably but also still be able to enqueue and dequeue.
    cell: UnsafeCell<Option<T>>,
    /// The pointer to the next node in the list.
    next: AtomicUsize,
    // FIXME Add tracking of time in channel by milliseconds.
}

impl<T: Sync + Send> ChannelNode<T> {
    /// Creates a new node where the next index is set to the nil node.
    pub fn new() -> ChannelNode<T> {
        ChannelNode {
            cell: UnsafeCell::new(None),
            next: AtomicUsize::new(NIL_NODE),
        }
    }

    /// Creates a new node where the next index is the given value.
    pub fn with_next(next: usize) -> ChannelNode<T> {
        ChannelNode {
            cell: UnsafeCell::new(None),
            next: AtomicUsize::new(next),
        }
    }
}

/// Operations that work on the core of the channel.
pub trait ChannelCoreOps<T: Sync + Send> {
    /// Fetch the core of the channel.
    fn core(&self) -> &ChannelCore<T>;

    /// Returns the capacity of the list.
    fn capacity(&self) -> usize {
        self.core().capacity
    }

    /// Returns the length indicating how many total items are in the queue currently.
    fn length(&self) -> usize {
        self.core().length.load(Ordering::Relaxed)
    }

    /// Returns the total number of objects that have been enqueued to the list.
    fn enqueued(&self) -> usize {
        self.core().enqueued.load(Ordering::Relaxed)
    }

    /// Returns the total number of objects that have been dequeued from the list.
    fn dequeued(&self) -> usize {
        self.core().dequeued.load(Ordering::Relaxed)
    }
}

/// Data structure that contains the core of the channel including tracking fo statistics
/// and data storage.
pub struct ChannelCore<T: Sync + Send> {
    /// Capacity of the channel, which is the total number of items that can be stored.
    /// Note that there are 2 more nodes than the capacity because neither the queue nor pool
    /// should ever be empty.
    capacity: usize,
    /// Node storage of the nodes. These nodes are never read directly except during
    /// allocation and tests. Therefore they can be stored in an [UnsafeCell]. It is critical
    /// that the nodes don't change memory location so they are in a `Box<[Node<T>]>` slice
    /// and the surrounding [Vec] allows for expanding the storage without moving existing.
    nodes: UnsafeCell<Vec<Box<[ChannelNode<T>]>>>,
    /// Pointers to the nodes in the channel. It is critical that these pointers never change
    /// order during the operations of the queue. If the channel has to be resized it should
    /// push the new pointers into the [Vec] at the back and never remove a pointer.
    node_ptrs: UnsafeCell<Vec<*mut ChannelNode<T>>>,
    /// Number of values currently in the list.
    length: AtomicUsize,
    /// Total number of values that have been enqueued.
    enqueued: AtomicUsize,
    /// Total number of values that have been dequeued.
    dequeued: AtomicUsize,
}

/// Sender side of the channel.
pub struct ChannelSender<T: Sync + Send> {
    /// The core of the channel.
    core: Arc<ChannelCore<T>>,
    /// Used to create a read lock on the channel.
    write_lock: Mutex<bool>,
    /// Indexes in the node_ptrs used for enqueue of elements in the channel. The upper 32
    /// bits encode the queue_tail and the lower 32 bits encode the pool_head.
    queue_tail_pool_head: AtomicUsize,
}

impl<T: Sync + Send> ChannelSender<T> {
    /// Sends a value into the mailbox, the value will be moved into the mailbox and it will take
    /// ownership of the value.
    pub fn send(&self, value: T) -> Result<usize, ChannelErrors> {
        unsafe {
            let _guard = self.write_lock.lock().unwrap();
            // Retrieve send pointers and the encoded indexes inside them.
            let send_ptrs = self.queue_tail_pool_head.load(Ordering::Acquire);
            let queue_tail = send_ptrs >> HALF_USIZE_BITS;
            let pool_head = send_ptrs & LOWER_USIZE_BITMASK;

            // Get a pointer to the current pool_head and see if we have space to send.
            let pool_head_ptr = (*self.core().node_ptrs.get())[pool_head] as *mut ChannelNode<T>;
            let next_pool_head = (*pool_head_ptr).next.load(Ordering::Acquire);
            if NIL_NODE == next_pool_head {
                return Err(ChannelErrors::Full);
            }

            // Pool head moves to become the queue tail or else loop and try again!
            let next_send_ptrs = (pool_head << HALF_USIZE_BITS) | next_pool_head;
            let queue_tail_ptr = (*self.core().node_ptrs.get())[queue_tail] as *mut ChannelNode<T>;
            (*(*queue_tail_ptr).cell.get()) = Some(value);
            (*pool_head_ptr).next.store(NIL_NODE, Ordering::Release);
            (*queue_tail_ptr).next.store(pool_head, Ordering::Release);
            self.queue_tail_pool_head
                .store(next_send_ptrs, Ordering::Release);

            // Once we complete the write we have to adjust the channel statistics.
            self.core.enqueued.fetch_add(1, Ordering::Relaxed);
            let old_length = self.core.length.fetch_add(1, Ordering::Relaxed);
            return Ok(old_length + 1);
        }
    }
}

impl<T: Sync + Send> ChannelCoreOps<T> for ChannelSender<T> {
    fn core(&self) -> &ChannelCore<T> {
        &self.core
    }
}

unsafe impl<T: Send + Sync> Send for ChannelSender<T> {}

unsafe impl<T: Send + Sync> Sync for ChannelSender<T> {}

/// Receiver side of the channel.
pub struct ChannelReceiver<T: Sync + Send> {
    /// The core of the channel.
    core: Arc<ChannelCore<T>>,
    /// Used to create a read lock on the channel.
    read_lock: Mutex<bool>,
    /// Position in the buffer where the nodes can be dequeued from the queue and put back
    /// on the pool. The upper 32 bits cover the pool head index in `nodes_ptr` and the lower
    /// 32 bits cover the pool tail index in `nodes_ptr`.
    queue_head_pool_tail: AtomicUsize,
}

impl<T: Sync + Send> ChannelReceiver<T> {
    /// Receives the head of the queue, removing it from the queue.
    pub fn receive(&self) -> Result<T, ChannelErrors> {
        unsafe {
            let _guard = self.read_lock.lock().unwrap();
            // Retrieve send pointers and the encoded indexes inside them.
            let receive_ptrs = self.queue_head_pool_tail.load(Ordering::Acquire);
            let queue_head = receive_ptrs >> HALF_USIZE_BITS;
            let pool_tail = receive_ptrs & LOWER_USIZE_BITMASK;

            // Get a pointer to the current queue_head and see if there is anything to read.
            let queue_head_ptr = (*self.core().node_ptrs.get())[queue_head] as *mut ChannelNode<T>;
            let next_queue_head = (*queue_head_ptr).next.load(Ordering::Acquire);
            if NIL_NODE == next_queue_head {
                return Err(ChannelErrors::Empty);
            }

            // Queue head moves to become the pool tail or else loop and try again!
            let next_receive_ptrs = (next_queue_head << HALF_USIZE_BITS) | queue_head;
            let value = (*(*queue_head_ptr).cell.get()).take().unwrap() as T;
            let pool_tail_ptr = (*self.core().node_ptrs.get())[pool_tail] as *mut ChannelNode<T>;
            (*pool_tail_ptr).next.store(queue_head, Ordering::Release);
            (*queue_head_ptr).next.store(NIL_NODE, Ordering::Release);
            self.queue_head_pool_tail
                .store(next_receive_ptrs, Ordering::Release);

            // Once we complete the write we have to adjust the channel statistics.
            self.core.dequeued.fetch_add(1, Ordering::Relaxed);
            self.core.length.fetch_sub(1, Ordering::Relaxed);
            return Ok(value);
        }
    }
}

impl<T: Sync + Send> ChannelCoreOps<T> for ChannelReceiver<T> {
    fn core(&self) -> &ChannelCore<T> {
        &self.core
    }
}

unsafe impl<T: Send + Sync> Send for ChannelReceiver<T> {}

unsafe impl<T: Send + Sync> Sync for ChannelReceiver<T> {}

/// Creates the sender and receiver sides of this channel.
pub fn create<T: Sync + Send>(capacity: HalfUsize) -> (ChannelSender<T>, ChannelReceiver<T>) {
    // FIXME support reallocation of size ?
    if capacity < 1 {
        panic!("capacity cannot be smaller than 1");
    }

    // We add two to the allocated capacity to account for the mandatory two placeholder nodes
    // that guarantee that both lists are never empty.
    let alloc_capacity = (capacity + 2) as usize;
    let mut nodes = Vec::<ChannelNode<T>>::with_capacity(alloc_capacity);
    let mut node_ptrs = Vec::<*mut ChannelNode<T>>::with_capacity(alloc_capacity);

    // The queue just gets one initial node with no data and the queue_tail is just
    // the same as the queue_head.
    nodes.push(ChannelNode::<T>::new());
    node_ptrs.push(nodes.last_mut().unwrap() as *mut ChannelNode<T>);
    let queue_head = nodes.len() - 1;
    let queue_tail = queue_head;

    // Allocate the tail in the pool of nodes that will be added to in order to form
    // the pool. Note that although this is expensive, it only has to be done once.
    nodes.push(ChannelNode::<T>::new());
    node_ptrs.push(nodes.last_mut().unwrap() as *mut ChannelNode<T>);
    let mut pool_head = nodes.len() - 1;
    let pool_tail = pool_head;

    // Allocate the rest of the pool pushing each node onto the previous node.
    for _ in 0..capacity {
        nodes.push(ChannelNode::<T>::with_next(pool_head));
        node_ptrs.push(nodes.last_mut().unwrap() as *mut ChannelNode<T>);
        pool_head = nodes.len() - 1;
    }

    // Create the channel structures to send back
    let queue_tail_pool_head = (queue_tail << HALF_USIZE_BITS) | pool_head;
    let queue_head_pool_tail = (queue_head << HALF_USIZE_BITS) | pool_tail;

    let core = Arc::new(ChannelCore {
        capacity: capacity as usize,
        nodes: UnsafeCell::new(vec![nodes.into_boxed_slice()]),
        node_ptrs: UnsafeCell::new(node_ptrs),
        length: AtomicUsize::new(0),
        enqueued: AtomicUsize::new(0),
        dequeued: AtomicUsize::new(0),
    });

    let sender = ChannelSender {
        core: core.clone(),
        write_lock: Mutex::new(true),
        queue_tail_pool_head: AtomicUsize::new(queue_tail_pool_head),
    };

    let receiver = ChannelReceiver {
        core,
        read_lock: Mutex::new(true),
        queue_head_pool_tail: AtomicUsize::new(queue_head_pool_tail),
    };

    (sender, receiver)
}

/// Creates the sender and receiver sides of the channel for multiple producers and
/// multiple consumers by returning sender and receiver each wrapped in [Arc] instances.
pub fn create_with_arcs<T: Sync + Send>(
    capacity: HalfUsize,
) -> (Arc<ChannelSender<T>>, Arc<ChannelReceiver<T>>) {
    let (sender, receiver) = create(capacity);
    (Arc::new(sender), Arc::new(receiver))
}

// --------------------- Test Cases ---------------------

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;
    use super::*;

    /// A macro to assert that pointers point to the right nodes.
    macro_rules! assert_pointer_nodes {
        (
            $sender:expr,
            $receiver:expr,
            $queue_head:expr,
            $queue_tail:expr,
            $pool_head:expr,
            $pool_tail:expr
        ) => {
            let queue_tail_pool_head = $sender.queue_tail_pool_head.load(Ordering::Relaxed);
            let queue_head_pool_tail = $receiver.queue_head_pool_tail.load(Ordering::Relaxed);

            assert_eq!(
                $queue_head,
                queue_head_pool_tail >> HALF_USIZE_BITS,
                "<== queue_head mismatch\n"
            );
            assert_eq!(
                $queue_tail,
                queue_tail_pool_head >> HALF_USIZE_BITS,
                "<== queue_tail mismatch\n"
            );
            assert_eq!(
                $pool_head,
                queue_tail_pool_head & LOWER_USIZE_BITMASK,
                "<== pool_head mismatch\n"
            );
            assert_eq!(
                $pool_tail,
                queue_head_pool_tail & LOWER_USIZE_BITMASK,
                "<== pool_tail mismatch\n"
            );
        };
    }

    /// Asserts that the given node in the queue has the expected next pointer.
    macro_rules! assert_node_next {
        ($pointers:expr, $node:expr, $next:expr) => {
            unsafe { assert_eq!((*$pointers[$node]).next.load(Ordering::Relaxed), $next) }
        };
    }

    /// Asserts that the given node in the queue has the expected next pointing to null_mut().
    macro_rules! assert_node_next_nil {
        ($pointers:expr, $node:expr) => {
            unsafe { assert_eq!((*$pointers[$node]).next.load(Ordering::Relaxed), NIL_NODE) }
        };
    }

    #[derive(Debug, Eq, PartialEq)]
    enum Items {
        A,
        B,
        C,
        D,
        E,
        F,
    }

    /// Tests the basics of the queue.
    #[test]
    fn test_queue_dequeue() {
        let channel = create::<Items>(5);
        let (sender, receiver) = channel;

        // fetch the pointers for easy checking of the nodes.
        let pointers = unsafe { &*sender.core.node_ptrs.get() };

        assert_eq!(7, pointers.len());
        assert_eq!(5, sender.core.capacity);
        assert_eq!(5, sender.capacity());
        assert_eq!(5, receiver.capacity());

        // Check the initial structure.
        assert_eq!(0, sender.length());
        assert_eq!(0, sender.enqueued());
        assert_eq!(0, sender.dequeued());
        assert_node_next_nil!(pointers, 0);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 0, 6, 1); // ( qh, qt, ph, pt)

        // Check that enqueueing removes pool head and appends to queue tail and changes
        // nothing else in the node structure.
        assert_eq!(Ok(1), sender.send(Items::A));
        assert_eq!(1, sender.length());
        assert_eq!(1, sender.enqueued());
        assert_eq!(0, sender.dequeued());
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 6, 5, 1);

        // Second sender should also move the pool_head node.
        assert_eq!(Ok(2), sender.send(Items::B));
        assert_eq!(2, sender.length());
        assert_eq!(2, sender.enqueued());
        assert_eq!(0, sender.dequeued());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 5, 4, 1);

        assert_eq!(Ok(3), sender.send(Items::C));
        assert_eq!(3, sender.length());
        assert_eq!(3, sender.enqueued());
        assert_eq!(0, sender.dequeued());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next_nil!(pointers, 4);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 4, 3, 1);

        assert_eq!(Ok(4), sender.send(Items::D));
        assert_eq!(4, sender.length());
        assert_eq!(4, sender.enqueued());
        assert_eq!(0, sender.dequeued());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 3, 2, 1);

        assert_eq!(Ok(5), sender.send(Items::E));
        assert_eq!(5, sender.length());
        assert_eq!(5, sender.enqueued());
        assert_eq!(0, sender.dequeued());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 2, 1, 1);

        assert_eq!(Err(ChannelErrors::Full), sender.send(Items::F));
        assert_eq!(5, sender.length());
        assert_eq!(5, sender.enqueued());
        assert_eq!(0, sender.dequeued());

        assert_eq!(Ok(Items::A), receiver.receive());
        assert_eq!(4, receiver.length());
        assert_eq!(5, receiver.enqueued());
        assert_eq!(1, receiver.dequeued());
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next_nil!(pointers, 0);
        assert_pointer_nodes!(sender, receiver, 6, 2, 1, 0);

        assert_eq!(Ok(Items::B), receiver.receive());
        assert_eq!(3, receiver.length());
        assert_eq!(5, receiver.enqueued());
        assert_eq!(2, receiver.dequeued());
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_pointer_nodes!(sender, receiver, 5, 2, 1, 6);

        assert_eq!(Ok(Items::C), receiver.receive());
        assert_eq!(2, receiver.length());
        assert_eq!(5, receiver.enqueued());
        assert_eq!(3, receiver.dequeued());
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_pointer_nodes!(sender, receiver, 4, 2, 1, 5);

        assert_eq!(Ok(Items::D), receiver.receive());
        assert_eq!(1, receiver.length());
        assert_eq!(5, receiver.enqueued());
        assert_eq!(4, receiver.dequeued());
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next_nil!(pointers, 4);
        assert_pointer_nodes!(sender, receiver, 3, 2, 1, 4);

        assert_eq!(Ok(Items::E), receiver.receive());
        assert_eq!(0, receiver.length());
        assert_eq!(5, receiver.enqueued());
        assert_eq!(5, receiver.dequeued());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3);

        assert_eq!(Err(ChannelErrors::Empty), receiver.receive());
        assert_eq!(0, receiver.length());
        assert_eq!(5, receiver.enqueued());
        assert_eq!(5, receiver.dequeued());

        assert_eq!(Ok(1), sender.send(Items::F));
        assert_eq!(1, receiver.length());
        assert_eq!(6, receiver.enqueued());
        assert_eq!(5, receiver.dequeued());
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 1, 0, 3);

        assert_eq!(Ok(Items::F), receiver.receive());
        assert_eq!(0, receiver.length());
        assert_eq!(6, receiver.enqueued());
        assert_eq!(6, receiver.dequeued());
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 1, 0, 2);
    }

    #[test]
    fn test_single_producer_single_receiver() {
        let message_count = 100;
        let capacity = 32;
        let (sender, receiver) = create_with_arcs::<u32>(capacity);

        let rx = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let mut count = 0;
            while count < message_count {
                match receiver.receive() {
                    Ok(v) => {
                        println!("====> Received: {:?}", v);
                        count += 1;
                    }
                    _ => (),
                };
            }
        });

        let tx = thread::spawn(move || {
            for i in 0..message_count {
                let result = sender.send(i);
                println!("----> Sent: {:?}", result);
                thread::sleep(Duration::from_millis(1));
            }
        });

        tx.join().unwrap();
        rx.join().unwrap();
    }
}
