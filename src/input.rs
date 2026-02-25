use crate::config::ResolvedCyclingKeys;
use crate::ring::{Trigger, TriggerProducer};
use crate::samples::{KitLibrary, SampleBank};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, UinputAbsSetup};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A key binding mapping: evdev key code -> (sample_index, gain).
pub type KeyMap = HashMap<u16, (usize, f32)>;

/// The set of evdev key codes that should be suppressed (not forwarded to
/// other applications). This includes both sample-bound keys and cycling keys.
pub type SuppressedKeys = HashSet<u16>;

/// Tracks the current kit and variant selection for cycling.
struct KitState {
    library: KitLibrary,
    sample_bank: Arc<ArcSwap<SampleBank>>,
    kit_index: usize,
    variant_index: usize,
}

impl KitState {
    /// Cycle to the next or previous kit. Resets variant to 0.
    fn cycle_kit(&mut self, forward: bool) {
        let count = self.library.kit_count();
        if count == 0 {
            return;
        }

        if forward {
            self.kit_index = (self.kit_index + 1) % count;
        } else {
            self.kit_index = (self.kit_index + count - 1) % count;
        }
        // Default to first variant when switching kits.
        self.variant_index = 0;

        self.reload();
    }

    /// Cycle to the next or previous variant within the current kit.
    fn cycle_variant(&mut self, forward: bool) {
        let count = self.library.variant_count(self.kit_index);
        if count == 0 {
            return;
        }

        if forward {
            self.variant_index = (self.variant_index + 1) % count;
        } else {
            self.variant_index = (self.variant_index + count - 1) % count;
        }

        self.reload();
    }

    /// Load the samples for the current kit/variant and swap them in.
    fn reload(&mut self) {
        let kit_name = self
            .library
            .kits
            .get(self.kit_index)
            .map(|k| k.name.as_str())
            .unwrap_or("?");
        let variant_name = self
            .library
            .kits
            .get(self.kit_index)
            .and_then(|k| k.variants.get(self.variant_index))
            .map(|v| v.as_str())
            .unwrap_or("?");

        log::info!(
            "Switching to kit '{}' variant '{}' (kit {}/{}, variant {}/{})",
            kit_name,
            variant_name,
            self.kit_index + 1,
            self.library.kit_count(),
            self.variant_index + 1,
            self.library.variant_count(self.kit_index),
        );

        match self.library.load_bank(self.kit_index, self.variant_index) {
            Ok(bank) => {
                self.sample_bank.store(Arc::new(bank));
            }
            Err(e) => {
                log::error!(
                    "Failed to load kit '{}' variant '{}': {:#}",
                    kit_name,
                    variant_name,
                    e,
                );
                // Keep the previous bank — don't crash.
            }
        }
    }
}

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
/// The physical device is grabbed exclusively so that bound key events
/// do not reach other applications. All other key events are forwarded
/// through a uinput virtual keyboard.
///
/// When a key-down event matches a binding in `key_map`, a Trigger is
/// pushed to the ring buffer producer. When a cycling key is pressed,
/// the sample bank is swapped atomically.
///
/// The loop exits when `shutdown` is set to true.
pub fn run_input_loop(
    mut device: Device,
    key_map: &KeyMap,
    mut producer: TriggerProducer,
    shutdown: &AtomicBool,
    cycling_keys: &ResolvedCyclingKeys,
    library: KitLibrary,
    sample_bank: Arc<ArcSwap<SampleBank>>,
    suppressed_keys: &SuppressedKeys,
    mut virtual_device: VirtualDevice,
) -> Result<()> {
    log::info!(
        "Input reader started, listening for {} key bindings ({} keys suppressed)",
        key_map.len(),
        suppressed_keys.len(),
    );

    // Grab the device exclusively so key events don't reach other apps.
    device
        .grab()
        .context("Failed to grab input device exclusively")?;
    log::info!("Device grabbed exclusively — bound keys will not reach other applications");

    let mut kit_state = KitState {
        library,
        sample_bank,
        kit_index: 0,
        variant_index: 0,
    };

    let result = run_event_loop(
        &mut device,
        key_map,
        &mut producer,
        shutdown,
        cycling_keys,
        &mut kit_state,
        suppressed_keys,
        &mut virtual_device,
    );

    // Always ungrab the device on exit so the keyboard works normally again.
    if let Err(e) = device.ungrab() {
        log::warn!("Failed to ungrab device: {}", e);
    } else {
        log::info!("Device ungrabbed");
    }

    result
}

/// Inner event loop, separated so that grab/ungrab cleanup is guaranteed
/// in `run_input_loop` regardless of how this function exits.
fn run_event_loop(
    device: &mut Device,
    key_map: &KeyMap,
    producer: &mut TriggerProducer,
    shutdown: &AtomicBool,
    cycling_keys: &ResolvedCyclingKeys,
    kit_state: &mut KitState,
    suppressed_keys: &SuppressedKeys,
    virtual_device: &mut VirtualDevice,
) -> Result<()> {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            log::info!("Input reader shutting down");
            break;
        }

        // fetch_events() blocks until events are available.
        let events: Vec<InputEvent> = match device.fetch_events() {
            Ok(events) => events.collect(),
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

        // Process events and forward non-suppressed ones to the virtual device.
        //
        // Physical device events arrive in batches delimited by SYN_REPORT.
        // A typical key-press batch looks like:
        //   EV_MSC MSC_SCAN <scancode>
        //   EV_KEY KEY_A 1
        //   EV_SYN SYN_REPORT 0
        //
        // We must suppress the entire batch for bound keys (including the
        // accompanying MSC_SCAN), otherwise orphaned non-KEY events cause
        // spurious input on the virtual device.
        //
        // Strategy: collect each batch, then filter and forward.
        let mut batch: Vec<InputEvent> = Vec::new();

        for event in &events {
            // Always run our handler for drum triggering / kit cycling.
            handle_event(event, key_map, producer, cycling_keys, kit_state);

            if event.event_type() == EventType::SYNCHRONIZATION {
                // End of batch — filter and forward.
                forward_batch(&batch, suppressed_keys, virtual_device);
                batch.clear();
            } else {
                batch.push(*event);
            }
        }

        // Flush any trailing events (shouldn't normally happen, but be safe).
        if !batch.is_empty() {
            forward_batch(&batch, suppressed_keys, virtual_device);
        }
    }

    log::info!("Input reader stopped");

    Ok(())
}

/// Filter and forward a single batch of events to the virtual device.
///
/// Removes KEY events for suppressed key codes. If removing those KEY
/// events leaves only non-KEY "companion" events (like MSC_SCAN) with
/// nothing meaningful to deliver, the entire batch is dropped to avoid
/// sending orphaned events.
fn forward_batch(
    batch: &[InputEvent],
    suppressed_keys: &SuppressedKeys,
    virtual_device: &mut VirtualDevice,
) {
    if batch.is_empty() {
        return;
    }

    // Partition: keep events that are NOT suppressed KEY events.
    let forward: Vec<InputEvent> = batch
        .iter()
        .filter(|ev| !(ev.event_type() == EventType::KEY && suppressed_keys.contains(&ev.code())))
        .copied()
        .collect();

    if forward.is_empty() {
        // Entire batch was suppressed keys (+ possibly MSC_SCAN companions).
        // Drop the whole batch — nothing meaningful to forward.
        return;
    }

    // Check if any KEY events survived filtering. If none did, the batch
    // contains only companion events (MSC_SCAN, etc.) for suppressed keys.
    // Drop those too — they are meaningless without their KEY event.
    let has_key_event = forward.iter().any(|ev| ev.event_type() == EventType::KEY);
    let original_had_key = batch.iter().any(|ev| ev.event_type() == EventType::KEY);

    if original_had_key && !has_key_event {
        // All KEY events were suppressed; remaining events are just
        // companions (MSC_SCAN). Drop the whole batch.
        return;
    }

    // emit() writes the events + appends a SYN_REPORT.
    if let Err(e) = virtual_device.emit(&forward) {
        log::warn!("Failed to forward events to virtual device: {}", e);
    }
}

/// Process a single input event. If it's a key-down matching a binding,
/// send a trigger to the audio thread. If it matches a cycling key,
/// cycle the kit or variant.
#[inline]
fn handle_event(
    event: &InputEvent,
    key_map: &KeyMap,
    producer: &mut TriggerProducer,
    cycling_keys: &ResolvedCyclingKeys,
    kit_state: &mut KitState,
) {
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

    // Check cycling keys first.
    if Some(code) == cycling_keys.next_kit {
        log::debug!("Cycling: next kit");
        kit_state.cycle_kit(true);
        return;
    }
    if Some(code) == cycling_keys.prev_kit {
        log::debug!("Cycling: previous kit");
        kit_state.cycle_kit(false);
        return;
    }
    if Some(code) == cycling_keys.next_variant {
        log::debug!("Cycling: next variant");
        kit_state.cycle_variant(true);
        return;
    }
    if Some(code) == cycling_keys.prev_variant {
        log::debug!("Cycling: previous variant");
        kit_state.cycle_variant(false);
        return;
    }

    // Check sample bindings.
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

/// Build the set of key codes that should be suppressed (not forwarded).
///
/// This includes all sample-bound keys and all cycling keys.
pub fn build_suppressed_keys(
    key_map: &KeyMap,
    cycling_keys: &ResolvedCyclingKeys,
) -> SuppressedKeys {
    let mut suppressed = SuppressedKeys::new();

    // Add all sample-bound keys.
    for &code in key_map.keys() {
        suppressed.insert(code);
    }

    // Add all configured cycling keys.
    if let Some(code) = cycling_keys.next_kit {
        suppressed.insert(code);
    }
    if let Some(code) = cycling_keys.prev_kit {
        suppressed.insert(code);
    }
    if let Some(code) = cycling_keys.next_variant {
        suppressed.insert(code);
    }
    if let Some(code) = cycling_keys.prev_variant {
        suppressed.insert(code);
    }

    suppressed
}

/// Create a uinput virtual device that mirrors ALL capabilities of the
/// physical device. Since we grab the physical device exclusively, the
/// virtual device must be able to emit every event type the physical one
/// can, so that non-suppressed events (including any relative/absolute
/// axes, switches, LEDs, etc.) are forwarded transparently.
pub fn create_virtual_device(device: &Device) -> Result<VirtualDevice> {
    let mut builder = VirtualDevice::builder()
        .map_err(|e| anyhow::anyhow!("Failed to open /dev/uinput: {}", e))?
        .name(b"keyboard-drums passthrough");

    // Mirror key capabilities.
    if let Some(keys) = device.supported_keys() {
        let mut key_set = AttributeSet::<KeyCode>::new();
        for key in keys.iter() {
            key_set.insert(key);
        }
        builder = builder
            .with_keys(&key_set)
            .map_err(|e| anyhow::anyhow!("Failed to set virtual device keys: {}", e))?;
    }

    // Mirror relative axis capabilities (mouse movement, scroll wheel, etc.).
    if let Some(rel_axes) = device.supported_relative_axes() {
        builder = builder
            .with_relative_axes(rel_axes)
            .map_err(|e| anyhow::anyhow!("Failed to set virtual device relative axes: {}", e))?;
    }

    // Mirror absolute axis capabilities (touchpad, etc.).
    if let Ok(absinfo_iter) = device.get_absinfo() {
        for (axis_code, abs_info) in absinfo_iter {
            let setup = UinputAbsSetup::new(axis_code, abs_info);
            builder = builder.with_absolute_axis(&setup).map_err(|e| {
                anyhow::anyhow!("Failed to set virtual device absolute axis: {}", e)
            })?;
        }
    }

    // Mirror switch capabilities.
    if let Some(switches) = device.supported_switches() {
        builder = builder
            .with_switches(switches)
            .map_err(|e| anyhow::anyhow!("Failed to set virtual device switches: {}", e))?;
    }

    // Mirror LED capabilities.
    // Note: VirtualDeviceBuilder doesn't have with_leds(), so LEDs are
    // not mirrored. This is acceptable — LED state is managed by the
    // kernel and doesn't affect input forwarding.

    let virt = builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create virtual device: {}", e))?;

    log::info!("Created virtual device mirroring physical device capabilities");

    Ok(virt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring;
    use crate::samples::SampleBank;

    fn make_dummy_cycling_keys() -> ResolvedCyclingKeys {
        ResolvedCyclingKeys {
            next_kit: None,
            prev_kit: None,
            next_variant: None,
            prev_variant: None,
        }
    }

    fn make_dummy_kit_state() -> KitState {
        use crate::samples::{KitInfo, KitLibrary, SampleData};
        use std::path::PathBuf;

        let bank = Arc::new(ArcSwap::from_pointee(SampleBank {
            samples: vec![Arc::new(SampleData {
                data: vec![0.0],
                channels: 1,
                sample_rate: 48000,
            })],
            sample_gains: vec![1.0],
            kit_name: "test".to_string(),
            variant_name: "v1".to_string(),
        }));

        KitState {
            library: KitLibrary {
                samples_dir: PathBuf::from("/tmp"),
                kits: vec![KitInfo {
                    name: "test".to_string(),
                    variants: vec!["v1".to_string()],
                }],
                sample_names: vec!["kick.wav".to_string()],
                sample_gains: vec![1.0],
            },
            sample_bank: bank,
            kit_index: 0,
            variant_index: 0,
        }
    }

    #[test]
    fn test_handle_event_key_down_match() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let cycling = make_dummy_cycling_keys();
        let mut kit_state = make_dummy_kit_state();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 0.8)); // KEY_A = 30

        // Simulate a KEY_A down event (type=1 EV_KEY, code=30, value=1).
        let event = InputEvent::new(EventType::KEY.0, 30, 1);
        handle_event(&event, &key_map, &mut prod, &cycling, &mut kit_state);

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
        let cycling = make_dummy_cycling_keys();
        let mut kit_state = make_dummy_kit_state();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        // Key up event (value=0) should be ignored.
        let event = InputEvent::new(EventType::KEY.0, 30, 0);
        handle_event(&event, &key_map, &mut prod, &cycling, &mut kit_state);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_handle_event_key_repeat_ignored() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let cycling = make_dummy_cycling_keys();
        let mut kit_state = make_dummy_kit_state();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        // Key repeat event (value=2) should be ignored.
        let event = InputEvent::new(EventType::KEY.0, 30, 2);
        handle_event(&event, &key_map, &mut prod, &cycling, &mut kit_state);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_handle_event_unbound_key() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let cycling = make_dummy_cycling_keys();
        let mut kit_state = make_dummy_kit_state();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0)); // KEY_A

        // KEY_B (code=48) is not bound.
        let event = InputEvent::new(EventType::KEY.0, 48, 1);
        handle_event(&event, &key_map, &mut prod, &cycling, &mut kit_state);

        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_handle_event_non_key_event() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let cycling = make_dummy_cycling_keys();
        let mut kit_state = make_dummy_kit_state();

        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        // A non-KEY event (EV_REL = 2).
        let event = InputEvent::new(EventType::RELATIVE.0, 0, 1);
        handle_event(&event, &key_map, &mut prod, &cycling, &mut kit_state);

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

    #[test]
    fn test_cycling_key_does_not_trigger_sample() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let mut kit_state = make_dummy_kit_state();

        // KEY_RIGHT (code=106) is next_kit cycling key.
        let cycling = ResolvedCyclingKeys {
            next_kit: Some(106),
            prev_kit: None,
            next_variant: None,
            prev_variant: None,
        };

        // Also bind KEY_RIGHT as a sample key (should be prevented by config,
        // but verify cycling takes priority).
        let mut key_map = KeyMap::new();
        key_map.insert(106, (0, 1.0));

        let event = InputEvent::new(EventType::KEY.0, 106, 1);
        handle_event(&event, &key_map, &mut prod, &cycling, &mut kit_state);

        // No trigger should be sent — cycling takes priority.
        let mut buf = Vec::new();
        cons.drain(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_build_suppressed_keys_includes_bindings_and_cycling() {
        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0)); // KEY_A
        key_map.insert(31, (1, 0.9)); // KEY_S

        let cycling = ResolvedCyclingKeys {
            next_kit: Some(106),     // KEY_RIGHT
            prev_kit: Some(105),     // KEY_LEFT
            next_variant: Some(103), // KEY_UP
            prev_variant: None,
        };

        let suppressed = build_suppressed_keys(&key_map, &cycling);

        // Should contain both sample keys.
        assert!(suppressed.contains(&30));
        assert!(suppressed.contains(&31));

        // Should contain configured cycling keys.
        assert!(suppressed.contains(&106));
        assert!(suppressed.contains(&105));
        assert!(suppressed.contains(&103));

        // Should NOT contain unconfigured cycling key.
        // Total: 2 sample keys + 3 cycling keys = 5.
        assert_eq!(suppressed.len(), 5);
    }

    #[test]
    fn test_build_suppressed_keys_empty_cycling() {
        let mut key_map = KeyMap::new();
        key_map.insert(30, (0, 1.0));

        let cycling = make_dummy_cycling_keys(); // all None

        let suppressed = build_suppressed_keys(&key_map, &cycling);

        assert_eq!(suppressed.len(), 1);
        assert!(suppressed.contains(&30));
    }
}
