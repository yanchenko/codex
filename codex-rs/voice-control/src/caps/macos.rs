//! Public IOKit/CoreFoundation APIs. No global key-state or volume manipulation.
use super::CapsCommand;
use super::Edges;
use super::Mapping;
use super::mapping_plan;
use crate::Client;
use crate::Operation;
use crate::storage::private_open;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

type Ref = *const c_void;
const CAPS: i64 = 0x700000039;
const NO_ACTION: i64 = 0x700000000;
const LISTEN: i32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(kind: i32) -> i32;
    fn IOHIDRequestAccess(kind: i32) -> bool;
    fn IOHIDEventSystemClientCreateSimpleClient(allocator: Ref) -> Ref;
    fn IOHIDEventSystemClientCopyServices(client: Ref) -> Ref;
    fn IOHIDServiceClientConformsTo(service: Ref, page: u32, usage: u32) -> u8;
    fn IOHIDServiceClientGetRegistryID(service: Ref) -> Ref;
    fn IOHIDServiceClientCopyProperty(service: Ref, key: Ref) -> Ref;
    fn IOHIDServiceClientSetProperty(service: Ref, key: Ref, property: Ref) -> u8;
    fn IOHIDManagerCreate(allocator: Ref, options: u32) -> Ref;
    fn IOHIDManagerSetDeviceMatching(manager: Ref, matching: Ref);
    fn IOHIDManagerRegisterInputValueCallback(
        manager: Ref,
        callback: extern "C" fn(*mut c_void, i32, Ref, Ref),
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: Ref,
        callback: extern "C" fn(*mut c_void, i32, Ref, Ref),
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(manager: Ref, run_loop: Ref, mode: Ref);
    fn IOHIDManagerUnscheduleFromRunLoop(manager: Ref, run_loop: Ref, mode: Ref);
    fn IOHIDManagerOpen(manager: Ref, options: u32) -> i32;
    fn IOHIDManagerClose(manager: Ref, options: u32) -> i32;
    fn IOHIDManagerCopyDevices(manager: Ref) -> Ref;
    fn IOHIDValueGetElement(value: Ref) -> Ref;
    fn IOHIDValueGetIntegerValue(value: Ref) -> isize;
    fn IOHIDElementGetDevice(element: Ref) -> Ref;
    fn IOHIDElementGetUsagePage(element: Ref) -> u32;
    fn IOHIDElementGetUsage(element: Ref) -> u32;
    fn IOHIDDeviceConformsTo(device: Ref, page: u32, usage: u32) -> u8;
    fn IOHIDDeviceGetService(device: Ref) -> u32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
    fn IORegistryEntryGetParentEntry(entry: u32, plane: *const i8, parent: *mut u32) -> i32;
    fn IORegistryEntryIDMatching(id: u64) -> Ref;
    fn IOServiceGetMatchingService(port: u32, matching: Ref) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDDeviceCopyMatchingElements(device: Ref, matching: Ref, options: u32) -> Ref;
    fn IOHIDValueCreateWithIntegerValue(
        allocator: Ref,
        element: Ref,
        timestamp: u64,
        value: isize,
    ) -> Ref;
    fn IOHIDDeviceSetValue(device: Ref, element: Ref, value: Ref) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Ref);
    fn CFEqual(first: Ref, second: Ref) -> u8;
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFStringCreateWithCString(allocator: Ref, text: *const i8, encoding: u32) -> Ref;
    fn CFNumberCreate(allocator: Ref, kind: i32, value: Ref) -> Ref;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: Ref, kind: i32, value: *mut c_void) -> u8;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: Ref) -> isize;
    fn CFArrayGetValueAtIndex(array: Ref, index: isize) -> Ref;
    fn CFArrayCreateMutable(allocator: Ref, capacity: isize, callbacks: Ref) -> Ref;
    fn CFArrayAppendValue(array: Ref, value: Ref);
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: Ref, key: Ref) -> Ref;
    fn CFDictionaryGetCount(dictionary: Ref) -> isize;
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: Ref,
        value_callbacks: Ref,
    ) -> Ref;
    fn CFSetGetCount(set: Ref) -> isize;
    fn CFSetGetValues(set: Ref, values: *mut Ref);
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: u8) -> i32;
    static kCFRunLoopDefaultMode: Ref;
    static kCFTypeArrayCallBacks: u8;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceFlagsState(state: i32) -> u64;
}

struct Owned(Ref);
impl Owned {
    fn new(value: Ref) -> Result<Self> {
        ensure!(!value.is_null(), "IOKit object unavailable");
        Ok(Self(value))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: every Owned is a non-null Create/Copy result released once.
        unsafe { CFRelease(self.0) }
    }
}

fn string(value: &str) -> Result<Owned> {
    let text = CString::new(value)?;
    // SAFETY: text is NUL terminated and CF copies its contents.
    Owned::new(unsafe { CFStringCreateWithCString(ptr::null(), text.as_ptr(), 0x08000100) })
}

unsafe fn number(value: Ref) -> Option<i64> {
    if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut result = 0i64;
    // SAFETY: the type was checked and result points to the requested 64-bit integer.
    (unsafe { CFNumberGetValue(value, 4, (&mut result as *mut i64).cast()) } != 0).then_some(result)
}

unsafe fn entries(array: Ref) -> Result<Vec<Ref>> {
    if array.is_null() {
        return Ok(Vec::new());
    }
    ensure!(
        unsafe { CFGetTypeID(array) } == unsafe { CFArrayGetTypeID() },
        "unexpected keyboard mapping type"
    );
    let count = unsafe { CFArrayGetCount(array) };
    ensure!(
        (0..=512).contains(&count),
        "keyboard mapping exceeds safe bounds"
    );
    Ok((0..count)
        .map(|i| unsafe { CFArrayGetValueAtIndex(array, i) })
        .collect())
}

fn is_caps(entry: Ref, source: Ref) -> bool {
    // SAFETY: entry comes from the retained property array; check type before dictionary access.
    unsafe {
        !entry.is_null()
            && CFGetTypeID(entry) == CFDictionaryGetTypeID()
            && number(CFDictionaryGetValue(entry, source)) == Some(CAPS)
    }
}

fn boot_identity() -> Result<String> {
    let mut bytes = [0u8; 128];
    let mut size = bytes.len();
    // SAFETY: bounded writable buffer and a static NUL-terminated sysctl name.
    ensure!(
        unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut size,
                ptr::null_mut(),
                0,
            )
        } == 0,
        "macOS boot identity unavailable; cannot safely own keyboard mappings"
    );
    ensure!(
        (2..=bytes.len()).contains(&size),
        "invalid macOS boot identity"
    );
    Ok(std::str::from_utf8(&bytes[..size - 1])?.to_owned())
}

fn service_descends_from(service_id: u64, device_id: u64) -> bool {
    // MatchingService consumes the matching dictionary. Each returned registry
    // entry is released; only a bounded exact ancestry match admits raw input.
    let matching = unsafe { IORegistryEntryIDMatching(service_id) };
    if matching.is_null() {
        return false;
    }
    let mut entry = unsafe { IOServiceGetMatchingService(0, matching) };
    for _ in 0..32 {
        if entry == 0 {
            return false;
        }
        let mut id = 0;
        let matched =
            unsafe { IORegistryEntryGetRegistryEntryID(entry, &mut id) } == 0 && id == device_id;
        let mut parent = 0;
        if !matched {
            unsafe {
                IORegistryEntryGetParentEntry(entry, c"IOService".as_ptr(), &mut parent);
            }
        }
        unsafe {
            IOObjectRelease(entry);
        }
        if matched {
            return true;
        }
        entry = parent;
    }
    if entry != 0 {
        unsafe {
            IOObjectRelease(entry);
        }
    }
    false
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    #[serde(default)]
    boot: String,
    services: Vec<i64>,
}

impl Recovery {
    fn bind_boot(&mut self, boot: String) -> Result<()> {
        ensure!(
            self.services.is_empty() || self.boot == boot,
            "Caps recovery belongs to a different or unknown boot; refusing to alter keyboard mappings. Inspect existing remaps before explicitly archiving caps-recovery.json"
        );
        self.boot = boot;
        Ok(())
    }

    fn restored(&mut self, service: i64) {
        self.services.retain(|saved| *saved != service);
    }
}

struct Remaps {
    client: Owned,
    record_path: PathBuf,
    recovery: Recovery,
    key: Owned,
    source: Owned,
    destination: Owned,
}

impl Remaps {
    fn open(directory: &Path) -> Result<Self> {
        let mut record = private_open(&directory.join("caps-recovery.json"), true)?;
        ensure!(
            record.metadata()?.len() <= 32 * 1024,
            "invalid Caps recovery record"
        );
        let mut bytes = Vec::new();
        record.read_to_end(&mut bytes)?;
        let mut recovery: Recovery = if bytes.is_empty() {
            Recovery::default()
        } else {
            serde_json::from_slice(&bytes)?
        };
        recovery.bind_boot(boot_identity()?)?;
        // SAFETY: CreateSimpleClient has no preconditions and does not open input devices.
        Ok(Self {
            client: Owned::new(unsafe { IOHIDEventSystemClientCreateSimpleClient(ptr::null()) })?,
            record_path: directory.join("caps-recovery.json"),
            recovery,
            key: string("UserKeyMapping")?,
            source: string("HIDKeyboardModifierMappingSrc")?,
            destination: string("HIDKeyboardModifierMappingDst")?,
        })
    }

    fn save(&mut self) -> Result<()> {
        let temporary = self
            .record_path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut record = private_open(&temporary, true)?;
        record.write_all(&serde_json::to_vec(&self.recovery)?)?;
        record.sync_all()?;
        std::fs::rename(&temporary, &self.record_path)?;
        File::open(
            self.record_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("recovery directory missing"))?,
        )?
        .sync_all()?;
        Ok(())
    }

    fn update(&mut self, acquire: bool) -> Result<usize> {
        // SAFETY: client is retained by self and the returned array owns services for this call.
        let services = Owned::new(unsafe { IOHIDEventSystemClientCopyServices(self.client.0) })?;
        let mut keyboards = 0;
        for service in unsafe { entries(services.0)? } {
            if unsafe { IOHIDServiceClientConformsTo(service, 1, 6) } == 0 {
                continue;
            }
            keyboards += 1;
            let id = unsafe { number(IOHIDServiceClientGetRegistryID(service)) }
                .ok_or_else(|| anyhow::anyhow!("keyboard registry identity unavailable"))?;
            let raw = unsafe { IOHIDServiceClientCopyProperty(service, self.key.0) };
            let property = (!raw.is_null()).then(|| Owned(raw));
            let current = unsafe { entries(raw)? };
            let ours = self.recovery.services.contains(&id);
            let shapes = current
                .iter()
                .map(|entry| unsafe {
                    if !is_caps(*entry, self.source.0) {
                        Mapping::Other
                    } else if CFDictionaryGetCount(*entry) == 2
                        && number(CFDictionaryGetValue(*entry, self.destination.0))
                            == Some(NO_ACTION)
                    {
                        Mapping::OwnedShape
                    } else {
                        Mapping::ForeignCaps
                    }
                })
                .collect::<Vec<_>>();
            let plan = mapping_plan(&shapes, ours, acquire)?;
            if plan.unchanged {
                continue;
            }
            if plan.append {
                self.recovery.services.push(id);
                self.save()?;
            }
            let array = Owned::new(unsafe {
                CFArrayCreateMutable(ptr::null(), 0, (&raw const kCFTypeArrayCallBacks).cast())
            })?;
            for index in plan.retain {
                unsafe { CFArrayAppendValue(array.0, current[index]) };
            }
            if plan.append {
                let source = Owned::new(unsafe {
                    CFNumberCreate(ptr::null(), 4, (&CAPS as *const i64).cast())
                })?;
                let destination = Owned::new(unsafe {
                    CFNumberCreate(ptr::null(), 4, (&NO_ACTION as *const i64).cast())
                })?;
                let keys = [self.source.0, self.destination.0];
                let values = [source.0, destination.0];
                let pair = Owned::new(unsafe {
                    CFDictionaryCreate(
                        ptr::null(),
                        keys.as_ptr(),
                        values.as_ptr(),
                        2,
                        (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                        (&raw const kCFTypeDictionaryValueCallBacks).cast(),
                    )
                })?;
                unsafe { CFArrayAppendValue(array.0, pair.0) };
            }
            // Re-read before writing to avoid overwriting an already-observed
            // external remap change. IOKit does not provide cross-process CAS.
            let reread_raw = unsafe { IOHIDServiceClientCopyProperty(service, self.key.0) };
            let reread = (!reread_raw.is_null()).then(|| Owned(reread_raw));
            ensure!(
                (raw.is_null() && reread_raw.is_null())
                    || (!raw.is_null()
                        && !reread_raw.is_null()
                        && unsafe { CFEqual(raw, reread_raw) } != 0),
                "keyboard mappings changed during acquisition; refusing to overwrite"
            );
            ensure!(
                unsafe { IOHIDServiceClientSetProperty(service, self.key.0, array.0) } != 0,
                "keyboard remap write failed"
            );

            let confirmed =
                Owned::new(unsafe { IOHIDServiceClientCopyProperty(service, self.key.0) })?;
            ensure!(
                unsafe { CFEqual(confirmed.0, array.0) } != 0,
                "keyboard mapping write was not confirmed"
            );
            drop(reread);
            drop(property);
            if !acquire {
                self.recovery.restored(id);
                self.save()?;
            }
        }
        Ok(keyboards)
    }
}

struct Callback {
    edges: Edges,
    owned_devices: BTreeSet<usize>,
    events: mpsc::SyncSender<()>,
    overflow: std::sync::Arc<AtomicBool>,
}

extern "C" fn input(context: *mut c_void, result: i32, _sender: Ref, value: Ref) {
    if result != 0 || context.is_null() || value.is_null() {
        return;
    }
    // SAFETY: callback context stays boxed until the manager is unscheduled and
    // closed. IOKit supplies a live value; only Caps elements are examined.
    unsafe {
        let element = IOHIDValueGetElement(value);
        if element.is_null()
            || IOHIDElementGetUsagePage(element) != 7
            || IOHIDElementGetUsage(element) != 0x39
        {
            return;
        }
        let callback = &mut *context.cast::<Callback>();
        let device = IOHIDElementGetDevice(element) as usize;
        if CGEventSourceFlagsState(0) & 0x10000 != 0 {
            callback.overflow.store(true, Ordering::Release);
            return;
        }
        if callback.edges.update_owned(
            device,
            IOHIDValueGetIntegerValue(value) != 0,
            callback.owned_devices.contains(&device),
        ) && callback.events.try_send(()).is_err()
        {
            callback.overflow.store(true, Ordering::Release);
        }
    }
}

extern "C" fn removed(context: *mut c_void, _result: i32, _sender: Ref, device: Ref) {
    if !context.is_null() {
        // SAFETY: removal callbacks use the same boxed context and run-loop thread.
        unsafe {
            let callback = &mut *context.cast::<Callback>();
            callback.owned_devices.remove(&(device as usize));
            callback.edges.update(device as usize, false);
        }
    }
}

struct Monitor {
    manager: Owned,
    run_loop: Ref,
    callback: Box<Callback>,
}
impl Monitor {
    fn open(events: mpsc::SyncSender<()>, overflow: std::sync::Arc<AtomicBool>) -> Result<Self> {
        // SAFETY: preflight does not request access. Never Open an undetermined
        // manager because IOKit may implicitly prompt from that operation.
        ensure!(
            unsafe { IOHIDCheckAccess(LISTEN) } == 0,
            "Input Monitoring is not granted; run voice-control caps setup explicitly"
        );
        let manager = Owned::new(unsafe { IOHIDManagerCreate(ptr::null(), 0) })?;
        let mut callback = Box::new(Callback {
            edges: Edges::default(),
            owned_devices: BTreeSet::new(),
            events,
            overflow,
        });
        let run_loop = unsafe { CFRunLoopGetCurrent() };
        unsafe {
            IOHIDManagerSetDeviceMatching(manager.0, ptr::null());
            IOHIDManagerRegisterInputValueCallback(
                manager.0,
                input,
                (&mut *callback as *mut Callback).cast(),
            );
            IOHIDManagerRegisterDeviceRemovalCallback(
                manager.0,
                removed,
                (&mut *callback as *mut Callback).cast(),
            );
            IOHIDManagerScheduleWithRunLoop(manager.0, run_loop, kCFRunLoopDefaultMode);
        }
        let result = unsafe { IOHIDManagerOpen(manager.0, 0) };
        if result != 0 {
            unsafe {
                IOHIDManagerUnscheduleFromRunLoop(manager.0, run_loop, kCFRunLoopDefaultMode)
            };
            anyhow::bail!(
                "keyboard monitor open failed ({result:#x}); verify Input Monitoring and relaunch this installed binary"
            );
        }
        Ok(Self {
            manager,
            run_loop,
            callback,
        })
    }

    fn poll(&self) {
        // SAFETY: manager is scheduled on this thread's run loop for its lifetime.
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.025, 0) };
        std::thread::sleep(Duration::from_millis(5));
    }

    fn confirm_owned_devices(&mut self, services: &[i64]) -> Result<()> {
        // Drain pre-ownership input while admission is closed. A new keyboard
        // cannot act as a voice switch until its remap has been read back.
        self.callback.owned_devices.clear();
        self.poll();
        ensure!(
            unsafe { CGEventSourceFlagsState(0) } & 0x10000 == 0,
            "logical Caps changed before ownership was confirmed; disable logical Caps before retrying"
        );
        let devices = Owned::new(unsafe { IOHIDManagerCopyDevices(self.manager.0) })?;
        let count = unsafe { CFSetGetCount(devices.0) };
        ensure!((0..=256).contains(&count), "unexpected HID device count");
        let mut values = vec![ptr::null(); count as usize];
        unsafe { CFSetGetValues(devices.0, values.as_mut_ptr()) };
        for device in values {
            let raw = unsafe { IOHIDDeviceGetService(device) };
            let mut id = 0;
            if raw == 0 || unsafe { IORegistryEntryGetRegistryEntryID(raw, &mut id) } != 0 {
                continue;
            }
            if services
                .iter()
                .any(|service| service_descends_from(*service as u64, id))
            {
                self.callback.owned_devices.insert(device as usize);
            }
        }
        ensure!(
            !self.callback.owned_devices.is_empty(),
            "no raw keyboard could be matched to a confirmed Caps mapping; refusing unowned input"
        );
        Ok(())
    }

    fn set_led(&self, on: bool) -> Result<usize> {
        let devices = Owned::new(unsafe { IOHIDManagerCopyDevices(self.manager.0) })?;
        let count = unsafe { CFSetGetCount(devices.0) };
        ensure!((0..=256).contains(&count), "unexpected HID device count");
        let mut values = vec![ptr::null(); count as usize];
        unsafe { CFSetGetValues(devices.0, values.as_mut_ptr()) };
        let mut written = 0;
        for device in values {
            if !self.callback.owned_devices.contains(&(device as usize))
                || unsafe { IOHIDDeviceConformsTo(device, 1, 6) } == 0
            {
                continue;
            }
            let raw = unsafe { IOHIDDeviceCopyMatchingElements(device, ptr::null(), 0) };
            if raw.is_null() {
                continue;
            }
            let elements = Owned(raw);
            let count = unsafe { CFArrayGetCount(elements.0) };
            ensure!((0..=4096).contains(&count), "unexpected HID element count");
            for index in 0..count {
                let element = unsafe { CFArrayGetValueAtIndex(elements.0, index) };
                if unsafe { IOHIDElementGetUsagePage(element) } != 8
                    || unsafe { IOHIDElementGetUsage(element) } != 2
                {
                    continue;
                }
                let value = Owned::new(unsafe {
                    IOHIDValueCreateWithIntegerValue(ptr::null(), element, 0, isize::from(on))
                })?;
                let result = unsafe { IOHIDDeviceSetValue(device, element, value.0) };
                ensure!(
                    result == 0,
                    "Caps LED output report failed ({result:#x}); physical indicator is unavailable"
                );
                written += 1;
            }
        }
        ensure!(
            written > 0,
            "no writable Caps LED was discovered; physical indicator is unavailable"
        );
        Ok(written)
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: context is still alive until after callbacks are unscheduled
        // and the manager has closed. Owned then releases the retained manager.
        unsafe {
            IOHIDManagerUnscheduleFromRunLoop(self.manager.0, self.run_loop, kCFRunLoopDefaultMode);
            IOHIDManagerClose(self.manager.0, 0);
        }
        let _ = &self.callback;
    }
}

#[derive(Serialize)]
struct Status {
    input_monitoring: &'static str,
    running: bool,
    recovery_pending: bool,
}

fn permission() -> &'static str {
    // SAFETY: read-only documented preflight, never requests a prompt.
    match unsafe { IOHIDCheckAccess(LISTEN) } {
        0 => "granted",
        1 => "denied",
        _ => "undetermined",
    }
}

pub(super) async fn command(directory: &Path, command: CapsCommand) -> Result<()> {
    let mut client = crate::connect_or_start(directory, &std::env::current_exe()?).await?;
    match command {
        CapsCommand::Status => {
            let lock = private_open(&directory.join("caps.lock"), true)?;
            let running = match lock.try_lock() {
                Ok(()) => false,
                Err(std::fs::TryLockError::WouldBlock) => true,
                Err(error) => return Err(error.into()),
            };
            let recovery_path = directory.join("caps-recovery.json");
            let recovery_pending = if recovery_path.try_exists()? {
                let record = private_open(&recovery_path, false)?;
                ensure!(
                    record.metadata()?.len() <= 32 * 1024,
                    "invalid Caps recovery record"
                );
                if record.metadata()?.len() == 0 {
                    false
                } else {
                    let recovery: Recovery = serde_json::from_reader(record)?;
                    !recovery.services.is_empty()
                }
            } else {
                false
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&Status {
                    input_monitoring: permission(),
                    running,
                    recovery_pending
                })?
            );
            Ok(())
        }
        CapsCommand::Setup => {
            match permission() {
                "granted" => println!(
                    "Input Monitoring is granted. Run voice-control caps run to explicitly acquire Caps."
                ),
                "denied" => anyhow::bail!(
                    "Input Monitoring is denied. Enable the responsible terminal/Codex binary in System Settings > Privacy & Security > Input Monitoring, then relaunch. No Accessibility or Full Disk Access grant is requested."
                ),
                _ => {
                    eprintln!(
                        "Requesting Input Monitoring to observe physical Caps presses. No remapping is applied by setup. Rebuilt or relocated binaries may need reauthorization."
                    );
                    // SAFETY: user explicitly invoked setup; request only listening access.
                    let granted = unsafe { IOHIDRequestAccess(LISTEN) };
                    ensure!(
                        granted,
                        "Input Monitoring was not granted; use System Settings and relaunch before running Caps"
                    );
                }
            }
            Ok(())
        }
        CapsCommand::Disable => {
            let lock = private_open(&directory.join("caps.lock"), true)?;
            match lock.try_lock() {
                Ok(()) => {
                    let mut remaps = Remaps::open(directory)?;
                    remaps.update(false)?;
                    let stop = directory.join("caps.stop");
                    if stop.try_exists()? {
                        std::fs::remove_file(stop)?;
                    }
                    ensure!(
                        remaps.recovery.services.is_empty(),
                        "present keyboards restored; detached keyboard recovery remains pending until reconnect"
                    );
                    println!("Owned Caps mappings restored; voice calls were not stopped.");
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    private_open(&directory.join("caps.stop"), true)?.sync_all()?;
                    println!(
                        "Caps release requested. Check caps status for confirmed monitor exit."
                    );
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        }
        CapsCommand::Run => run(directory, &mut client).await,
    }
}

async fn run(directory: &Path, client: &mut Client) -> Result<()> {
    let lock = private_open(&directory.join("caps.lock"), true)?;
    lock.try_lock()
        .map_err(|_| anyhow::anyhow!("another Codex Caps monitor is already running"))?;
    ensure!(
        permission() == "granted",
        "Input Monitoring is not granted; invoke caps setup explicitly first"
    );
    // SAFETY: flags-state inspection does not synthesize or modify key events.
    ensure!(
        unsafe { CGEventSourceFlagsState(0) } & 0x10000 == 0,
        "turn logical Caps Lock off before enabling voice control; Codex does not synthesize uppercase toggles"
    );
    let stop_path = directory.join("caps.stop");
    ensure!(
        !stop_path.try_exists()?,
        "a previous Caps release request remains; run caps disable to finish recovery before enabling again"
    );
    let (events, received) = mpsc::sync_channel(8);
    let (led, led_rx) = mpsc::sync_channel(8);
    let stopping = std::sync::Arc::new(AtomicBool::new(false));
    let overflow = std::sync::Arc::new(AtomicBool::new(false));
    let worker_stop = stopping.clone();
    let worker_overflow = overflow.clone();
    let path = directory.to_owned();
    let initial_indicator = client.snapshot().await?.confirmed_active;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let _lock = lock;
        let mut monitor = Monitor::open(events, worker_overflow.clone())?;
        let mut remaps = Remaps::open(&path)?;
        // A previous hard crash is recovered under the singleton ownership lock.
        remaps.update(false)?;
        let result = (|| -> Result<()> {
            ensure!(
                remaps.update(true)? > 0,
                "no keyboard services available for Caps ownership"
            );
            let mut desired = initial_indicator;
            let mut last_reconcile = std::time::Instant::now() - Duration::from_secs(2);
            while !worker_stop.load(Ordering::Acquire) {
                ensure!(
                    !worker_overflow.load(Ordering::Acquire),
                    "Caps input overflow or unexpected logical Caps state; monitor stopped safely"
                );
                ensure!(
                    permission() == "granted",
                    "Input Monitoring was revoked; releasing Caps ownership"
                );
                while let Ok(next) = led_rx.try_recv() {
                    desired = next;
                    last_reconcile = std::time::Instant::now() - Duration::from_secs(2);
                }
                if last_reconcile.elapsed() >= Duration::from_secs(1) {
                    remaps.update(true)?; // Re-enumerate after attach/detach or wake.
                    monitor.confirm_owned_devices(&remaps.recovery.services)?;
                    monitor.set_led(desired)?;
                    last_reconcile = std::time::Instant::now();
                }
                monitor.poll();
            }
            Ok(())
        })();
        let restoration = remaps.update(false);
        let indicator_cleanup = monitor.set_led(false);
        if restoration.is_ok() {
            let marker = path.join("caps.stop");
            if marker.try_exists().unwrap_or(false) {
                let _ = std::fs::remove_file(marker);
            }
        }
        result
            .and(restoration.map(|_| ()))
            .and(indicator_cleanup.map(|_| ()))
    });
    eprintln!(
        "Caps monitor starting. Ctrl-C or voice-control caps disable releases ownership. A dark LED is not proof of silence while lifecycle is unknown."
    );
    let mut interval = tokio::time::interval(Duration::from_millis(200));

    let result = loop {
        tokio::select! {
            _ = terminate.recv() => { break Ok(()); }
            signal = tokio::signal::ctrl_c() => { break signal.map_err(anyhow::Error::from); }
            _ = interval.tick() => {
                match stop_path.try_exists() {
                    Ok(true) => break Ok(()),
                    Ok(false) => {},
                    Err(error) => break Err(error.into()),
                }
                if worker.is_finished() { break Ok(()); }
                let mut state = match client.snapshot().await { Ok(state) => state, Err(error) => break Err(error) };
                if led.try_send(state.confirmed_active).is_err() { break Err(anyhow::anyhow!("Caps indicator worker unavailable")); }
                while received.try_recv().is_ok() {
                    let operation = if state.leases.is_empty() && !state.stopping && !state.instances.values().any(|instance| instance.start_requested) {
                        Operation::Start { epoch: state.epoch }
                    } else { Operation::GlobalOff };
                    match client.request(operation).await {
                        Ok(next) => state = next,
                        Err(error) => eprintln!("Caps request failed: {error}"),
                    }
                }
            }
        }
    };
    stopping.store(true, Ordering::Release);
    let cleanup = worker.await?;
    result.and(cleanup)
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn stale_boot_or_legacy_record_never_authorizes_mapping_cleanup() {
        let mut record = Recovery {
            boot: "previous".into(),
            services: vec![42],
        };
        assert!(record.bind_boot("current".into()).is_err());
        assert_eq!(record.services, vec![42]);
        let mut legacy: Recovery = serde_json::from_str(r#"{"services":[42]}"#).unwrap();
        assert!(legacy.bind_boot("current".into()).is_err());
        let mut empty = Recovery::default();
        empty.bind_boot("current".into()).unwrap();
        assert_eq!(empty.boot, "current");
    }

    #[test]
    fn restoring_present_keyboard_preserves_absent_recovery_evidence() {
        let mut record = Recovery {
            boot: "current".into(),
            services: vec![1, 2],
        };
        record.restored(1);
        assert_eq!(record.services, vec![2]);
        record.bind_boot("current".into()).unwrap();
        record.restored(2);
        assert!(record.services.is_empty());
    }
}
