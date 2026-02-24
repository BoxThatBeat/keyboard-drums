use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

/// Number of trigger slots in the ring buffer.
/// 128 is more than enough for even the fastest human drumming.
const RING_BUFFER_SIZE: usize = 128;

/// A trigger message sent from the input thread to the audio thread.
/// Kept small to fit in a cache line and avoid allocations.
#[derive(Debug, Clone, Copy)]
pub struct Trigger {
    /// Index into the preloaded samples array.
    pub sample_id: u8,

    /// Velocity/volume multiplier (0.0 to 1.0).
    /// Currently always 1.0 (key-down only), but extensible for
    /// future velocity-sensitive input.
    pub velocity: f32,
}

/// Producer half of the trigger ring buffer (used by the input thread).
pub struct TriggerProducer {
    inner: ringbuf::HeapProd<Trigger>,
}

/// Consumer half of the trigger ring buffer (used by the audio thread).
pub struct TriggerConsumer {
    inner: ringbuf::HeapCons<Trigger>,
}

/// Create a new trigger ring buffer, returning the producer and consumer halves.
///
/// The producer is meant for the input thread (evdev reader).
/// The consumer is meant for the audio callback.
/// Communication is lock-free SPSC — no mutexes involved.
pub fn create_trigger_channel() -> (TriggerProducer, TriggerConsumer) {
    let rb = HeapRb::<Trigger>::new(RING_BUFFER_SIZE);
    let (prod, cons) = rb.split();

    log::info!("Created trigger ring buffer with {} slots", RING_BUFFER_SIZE);

    (
        TriggerProducer { inner: prod },
        TriggerConsumer { inner: cons },
    )
}

impl TriggerProducer {
    /// Push a trigger into the ring buffer.
    ///
    /// Returns true if the trigger was successfully enqueued.
    /// Returns false if the buffer is full (trigger dropped — logged as warning).
    pub fn send(&mut self, trigger: Trigger) -> bool {
        match self.inner.try_push(trigger) {
            Ok(()) => {
                log::debug!(
                    "Trigger sent: sample_id={}, velocity={:.2}",
                    trigger.sample_id,
                    trigger.velocity
                );
                true
            }
            Err(_) => {
                log::warn!(
                    "Trigger ring buffer full! Dropped trigger for sample_id={}. \
                     This may indicate the audio thread is not consuming fast enough.",
                    trigger.sample_id
                );
                false
            }
        }
    }
}

impl TriggerConsumer {
    /// Drain all available triggers from the ring buffer.
    ///
    /// This is called from the audio callback and must be lock-free.
    /// Returns triggers in a pre-allocated buffer to avoid heap allocation.
    pub fn drain(&mut self, out: &mut Vec<Trigger>) {
        out.clear();
        while let Some(trigger) = self.inner.try_pop() {
            out.push(trigger);
        }
    }
}

// Mark as Send so they can be moved to different threads.
// ringbuf's HeapProd/HeapCons are already Send, but our wrappers
// inherit it automatically. This is just a compile-time assertion.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_types() {
        assert_send::<TriggerProducer>();
        assert_send::<TriggerConsumer>();
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_and_receive() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = create_trigger_channel();

        let trigger = Trigger {
            sample_id: 3,
            velocity: 0.75,
        };

        assert!(prod.send(trigger));

        let mut buf = Vec::new();
        cons.drain(&mut buf);

        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].sample_id, 3);
        assert!((buf[0].velocity - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn test_drain_empty() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (_prod, mut cons) = create_trigger_channel();

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_multiple_triggers() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = create_trigger_channel();

        for i in 0..10 {
            prod.send(Trigger {
                sample_id: i,
                velocity: 1.0,
            });
        }

        let mut buf = Vec::new();
        cons.drain(&mut buf);

        assert_eq!(buf.len(), 10);
        for (i, trigger) in buf.iter().enumerate() {
            assert_eq!(trigger.sample_id, i as u8);
        }
    }

    #[test]
    fn test_buffer_full() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, _cons) = create_trigger_channel();

        // Fill the buffer completely.
        let mut sent = 0;
        for i in 0..200 {
            if prod.send(Trigger {
                sample_id: (i % 256) as u8,
                velocity: 1.0,
            }) {
                sent += 1;
            }
        }

        // Should have filled up to RING_BUFFER_SIZE.
        assert_eq!(sent, RING_BUFFER_SIZE);
    }

    #[test]
    fn test_drain_clears_for_reuse() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = create_trigger_channel();

        prod.send(Trigger {
            sample_id: 1,
            velocity: 1.0,
        });

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert_eq!(buf.len(), 1);

        // Second drain should be empty.
        cons.drain(&mut buf);
        assert!(buf.is_empty());

        // Send more and drain again.
        prod.send(Trigger {
            sample_id: 2,
            velocity: 1.0,
        });
        cons.drain(&mut buf);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].sample_id, 2);
    }

    #[test]
    fn test_cross_thread() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = create_trigger_channel();

        let handle = std::thread::spawn(move || {
            for i in 0..50 {
                prod.send(Trigger {
                    sample_id: (i % 256) as u8,
                    velocity: 1.0,
                });
            }
        });

        handle.join().unwrap();

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert_eq!(buf.len(), 50);
    }
}
