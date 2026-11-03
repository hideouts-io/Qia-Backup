//! macOS sleep/wake and battery monitoring.
//!
//! The policy itself lives in the core crate so it can be tested on any
//! platform. This module only observes the operating system and feeds the
//! policy. Observer tokens are intentionally kept for the lifetime of the
//! process; they are small and the app exits as a unit.

use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::time::Duration;
use std::time::Instant;

use parking_lot::Mutex;
use qia_backup::core::power::PowerPolicy;

#[derive(Clone)]
pub struct PowerMonitor {
    policy: Arc<Mutex<PowerPolicy>>,
}

impl PowerMonitor {
    pub fn start() -> Self {
        let policy = Arc::new(Mutex::new(PowerPolicy::new(Instant::now())));
        #[cfg(target_os = "macos")]
        {
            install_macos_observers(policy.clone());
            spawn_battery_poller(policy.clone());
        }
        Self { policy }
    }

    pub fn can_run(&self) -> bool {
        self.policy.lock().can_run(Instant::now())
    }

    pub fn message(&self) -> Option<String> {
        self.policy.lock().message(Instant::now())
    }
}

#[cfg(target_os = "macos")]
fn install_macos_observers(policy: Arc<Mutex<PowerPolicy>>) {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    use objc2_foundation::NSNotification;

    let center = NSWorkspace::sharedWorkspace().notificationCenter();

    let sleep_policy = policy.clone();
    let sleep_block = RcBlock::new(move |_note: NonNull<NSNotification>| {
        sleep_policy.lock().set_asleep(true, Instant::now());
    });
    let sleep_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceWillSleepNotification),
            None,
            None,
            &sleep_block,
        )
    };

    let wake_policy = policy;
    let wake_block = RcBlock::new(move |_note: NonNull<NSNotification>| {
        wake_policy.lock().set_asleep(false, Instant::now());
    });
    let wake_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidWakeNotification),
            None,
            None,
            &wake_block,
        )
    };

    // Keep the observer registrations alive for the app's lifetime.
    std::mem::forget(sleep_observer);
    std::mem::forget(wake_observer);
}

#[cfg(target_os = "macos")]
fn spawn_battery_poller(policy: Arc<Mutex<PowerPolicy>>) {
    std::thread::spawn(move || {
        loop {
            if let Some(on_battery) = read_on_battery() {
                policy.lock().set_on_battery(on_battery, Instant::now());
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

#[cfg(target_os = "macos")]
fn read_on_battery() -> Option<bool> {
    use battery::{Manager, State};

    let manager = Manager::new().ok()?;
    let mut found = false;
    let mut discharging = false;
    for battery in manager.batteries().ok()? {
        let battery = battery.ok()?;
        found = true;
        if battery.state() == State::Discharging {
            discharging = true;
        }
    }
    if found {
        Some(discharging)
    } else {
        Some(false)
    }
}
