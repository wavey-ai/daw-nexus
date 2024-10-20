use bytes::Bytes;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{Arc, Mutex, RwLock};

pub(crate) struct Streams {
    consumers: Mutex<Vec<Option<Consumer<Bytes>>>>,
}

impl Streams {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            consumers: Mutex::new((0..capacity).map(|_| None).collect()),
        })
    }

    pub fn add(&self, index: usize) -> Producer<Bytes> {
        let (producer, consumer) = RingBuffer::new(1024 * 5 * 2);
        let mut consumers = self.consumers.lock().unwrap();

        if index < consumers.len() {
            consumers[index] = Some(consumer);
        } else {
            eprintln!("Error: Index out of bounds in 'add' method");
        }

        producer
    }

    pub fn take_consumer(&self, index: usize) -> Option<Consumer<Bytes>> {
        let mut consumers = self.consumers.lock().unwrap();
        consumers.get_mut(index)?.take()
    }
}

pub(crate) struct Channels {
    chans: Mutex<Vec<bool>>,
}

impl Channels {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            chans: Mutex::new(vec![false; capacity]),
        })
    }

    pub fn next(&self) -> Option<usize> {
        let mut lock = self.chans.lock().expect("Failed to lock mutex");
        for (index, value) in lock.iter_mut().enumerate() {
            if !*value {
                *value = true;
                return Some(index);
            }
        }
        None
    }

    pub fn rm(&self, idx: usize) -> Result<(), &'static str> {
        let mut lock = self.chans.lock().expect("Failed to lock mutex");
        if let Some(elem) = lock.get_mut(idx) {
            *elem = false;
            Ok(())
        } else {
            Err("Index out of bounds")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channels_next() {
        let channels = Channels::new(5); // Create Channels with a capacity of 5

        // Test getting the first available index and setting it to true
        assert_eq!(channels.next(), Some(0));
        assert_eq!(channels.next(), Some(1));
        assert_eq!(channels.next(), Some(2));

        // Check that we can exhaust all false values in the capacity
        assert_eq!(channels.next(), Some(3));
        assert_eq!(channels.next(), Some(4));

        // No more false values should be available
        assert_eq!(channels.next(), None);
    }

    #[test]
    fn test_channels_rm() {
        let channels = Channels::new(5); // Create Channels with a capacity of 5

        // Set some elements to true
        assert_eq!(channels.next(), Some(0));
        assert_eq!(channels.next(), Some(1));

        // Remove an index and check the result
        assert_eq!(channels.rm(1), Ok(())); // Should succeed
        assert_eq!(channels.rm(4), Ok(())); // Removing an index that is initially false should still succeed

        // Attempt to remove out-of-bounds index
        assert_eq!(channels.rm(5), Err("Index out of bounds"));

        // Check that index 1 was successfully set to false
        let mut lock = channels.chans.lock().expect("Failed to lock mutex");
        assert_eq!(lock[1], false);
        assert_eq!(lock[0], true); // Ensure that index 0 remains true
    }
}
