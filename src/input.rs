use crate::ring::{Trigger, TriggerProducer};
use anyhow::{Context, Result};
use evdev::{Device, EventType, InputEvent};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// A key binding mapping: evdev key code -> (sample_index, gain).
pub type KeyMap = HashMap<u16, (usize, f32)>;

/// List all available evdev input devices with their names and paths.
///
/// Prints device information to stdout for the `--list-devices` CLI flag.
pub fn list_devices() {
    let devices: Vec<_> = evdev::enumerate().collect();

    if devices.is_empty() {
        println!("No input devices found.");
        println!("You may need to run as root or add your user to the 'input' group.");
        return;
    }

    println!("{:<30} {}", "PATH", "NAME");
    println!("{}", "-".repeat(70));

    for (path, device) in &devices {
        let name = device.name().unwrap_or("(unnamed)");
        println!("{:<30} {}", path.display(), name);
    }

    println!();
    println!("Tip: Use the path of your keyboard as the 'device' setting in config.toml");
}

/// Open an evdev device by path and validate it supports key events.
pub fn open_device(path: &Path) -> Result<Device> {
    let device = Device::open(path)
        .with_context(|| format!("Failed to open evdev device: {}", path.display()))?;

    let name = device.name().unwrap_or("(unnamed)");
    log::info!("Opened input device: {} ({})", path.display(), name);

    // Check that the device supports key events.
    let supported = device.supported_events();
    if !supported.contains(EventType::KEY) {
        anyhow::bail!(
            "Device {} ({}) does not support key events. \
             Make sure you're using a keyboard device.",
            path.display(),
            name,
        );
    }

    Ok(device)
}

/// Run the input reader loop.
///
/// This function blocks, reading events from the evdev device.
/// It should be called from a dedicated thread.
///
/// When a key-down event matches a binding in `key_map`, a Trigger is
/// pushed to the ring buffer producer.
///
/// The loop exits when `shutdown` is set to true.
pub fn run_input_loop(
    mut device: Device,
    key_map: &KeyMap,
    mut producer: TriggerProducer,
    shutdown: &AtomicBool,
) -> Result<()> {
    log::info!(
        "Input reader started, listening for {} key bindings",
        key_map.len()
    );

    // Passive listening: we do NOT grab the device.
    // Key events continue to pass through to other applications normally.
    log::info!("Listening passively (key events still reach other applications)");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            log::info!("Input reader shutting down");
            break;
        }

        // fetch_events() blocks until events are available.
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                // EINTR can happen from signal handlers — just retry.
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e).context("Error reading events from input device");
            }
        };

        for event in events {
            handle_event(&event, key_map, &mut producer);
        }
    }

    log::info!("Input reader stopped");

    Ok(())
}

/// Process a single input event. If it's a key-down matching a binding,
/// send a trigger to the audio thread.
#[inline]
fn handle_event(event: &InputEvent, key_map: &KeyMap, producer: &mut TriggerProducer) {
    // Only care about KEY events.
    if event.event_type() != EventType::KEY {
        return;
    }

    // value: 0 = key up, 1 = key down, 2 = key repeat.
    // We only trigger on key down (1).
    let value = event.value();
    if value != 1 {
        return;
    }

    let code = event.code();

    if let Some(&(sample_index, gain)) = key_map.get(&code) {
        log::debug!(
            "Key down: code={}, sample_index={}, gain={:.2}",
            code,
            sample_index,
            gain
        );

        producer.send(Trigger {
            sample_id: sample_index as u8,
            velocity: gain,
        });
    }
}

/// Build a KeyMap from the resolved config bindings.
///
/// Maps evdev key code (u16) -> (sample_index, gain).
pub fn build_key_map(key_map: &HashMap<u16, crate::config::ResolvedBinding>) -> KeyMap {
    key_map
        .iter()
        .map(|(&code, binding)| (code, (binding.sample_index, binding.gain)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring;

    #[test]
    fn test_handle_event_key_down_match() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 0.8)); // KEY_A = 30

        // Simulate a KEY_A down event (type=1 EV_KEY, code=30, value=1).
        let event = InputEvent::new(EventType::KEY.0, 30, 1);
        handle_event(&event, &key_map, &mut prod);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].sample_id, 0);
        assert!((buf[0].velocity - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_handle_event_key_up_ignored() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        // Key up event (value=0) should be ignored.
        let event = InputEvent::new(EventType::KEY.0, 30, 0);
        handle_event(&event, &key_map, &mut prod);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_handle_event_key_repeat_ignored() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        // Key repeat event (value=2) should be ignored.
        let event = InputEvent::new(EventType::KEY.0, 30, 2);
        handle_event(&event, &key_map, &mut prod);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_handle_event_unbound_key() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0)); // KEY_A

        // KEY_B (code=48) is not bound.
        let event = InputEvent::new(EventType::KEY.0, 48, 1);
        handle_event(&event, &key_map, &mut prod);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_handle_event_non_key_event() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        // A non-KEY event (EV_REL = 2).
        let event = InputEvent::new(EventType::RELATIVE.0, 0, 1);
        handle_event(&event, &key_map, &mut prod);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_build_key_map() {
        use crate::config::ResolvedBinding;
        use evdev::KeyCode;

        let mut config_map = HashMap::new();
        config_map.insert(
            KeyCode::KEY_A.code(),
            ResolvedBinding {
                key_code: KeyCode::KEY_A,
                sample_index: 0,
                gain: 0.9,
            },
        );
        config_map.insert(
            KeyCode::KEY_S.code(),
            ResolvedBinding {
                key_code: KeyCode::KEY_S,
                sample_index: 1,
                gain: 0.7,
            },
        );

        let key_map = build_key_map(&config_map);
        assert_eq!(key_map.len(), 2);

        let (idx, gain) = key_map[&KeyCode::KEY_A.code()];
        assert_eq!(idx, 0);
        assert!((gain - 0.9).abs() < f32::EPSILON);

        let (idx, gain) = key_map[&KeyCode::KEY_S.code()];
        assert_eq!(idx, 1);
        assert!((gain - 0.7).abs() < f32::EPSILON);
    }
}
