//! Keeps an unopened default input ready for the next Recording.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use super::{ResolvedInput, microphone_blocked};

pub(super) struct InputCache {
    cache: Arc<Cache>,
    events: mpsc::Sender<Event>,
}

#[derive(Default)]
struct Cache {
    revision: AtomicU64,
    input: Mutex<Option<(u64, ResolvedInput)>>,
}

enum Event {
    Refresh,
    Shutdown,
}

impl InputCache {
    pub(super) fn new() -> Self {
        let cache = Arc::new(Cache::default());
        let worker_cache = Arc::clone(&cache);
        let (events, requests) = mpsc::channel();
        let worker_events = events.clone();
        let worker = thread::Builder::new()
            .name("voisu-input-cache".into())
            .spawn(move || {
                // Invalidate immediately, even if a press arrives before the
                // worker has finished resolving the new default.
                let changes = Arc::clone(&worker_cache);
                let _watch = watch_default(move || {
                    changes.revision.fetch_add(1, Ordering::Release);
                    let _ = worker_events.send(Event::Refresh);
                });
                // CoreAudio's default input is a concrete device. If watching
                // fails, resolve it per press so a still-connected old mic
                // cannot be used after the user picks a different one.
                #[cfg(target_os = "macos")]
                if _watch.is_none() {
                    return;
                }
                let host = cpal::default_host();
                loop {
                    let revision = worker_cache.revision.load(Ordering::Acquire);
                    let input = if microphone_blocked() {
                        None
                    } else {
                        ResolvedInput::resolve(&host).ok()
                    };
                    // Drop the old device outside the lock used on a press.
                    {
                        let mut input = input.map(|input| (revision, input));
                        std::mem::swap(&mut *worker_cache.input.lock().unwrap(), &mut input);
                    }
                    if !matches!(requests.recv(), Ok(Event::Refresh)) {
                        break;
                    }
                }
            });
        if let Err(error) = worker {
            tracing::warn!(%error, "microphone cache thread could not start");
        }
        Self { cache, events }
    }

    pub(super) fn take(&self) -> Option<ResolvedInput> {
        let (revision, input) = self.cache.input.lock().unwrap().take()?;
        (revision == self.cache.revision.load(Ordering::Acquire)).then_some(input)
    }

    /// The capture worker sends this after dropping the stream. WASAPI
    /// consumes the prewarmed client when building the stream.
    pub(super) fn refresh_after_capture(&self) -> impl FnOnce() + Send + 'static {
        let events = self.events.clone();
        move || {
            let _ = events.send(Event::Refresh);
        }
    }
}

impl Drop for InputCache {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Shutdown);
    }
}

#[cfg(windows)]
mod notifications {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eConsole,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::core::{PCWSTR, Result, implement};

    pub(super) fn watch_default(on_change: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
        match Watch::new(on_change) {
            Ok(watch) => Some(watch),
            Err(error) => {
                tracing::warn!(%error, "default microphone notifications unavailable");
                None
            }
        }
    }

    pub(super) struct Watch {
        enumerator: IMMDeviceEnumerator,
        client: IMMNotificationClient,
        _com: Com,
    }

    impl Watch {
        fn new(on_change: impl Fn() + Send + Sync + 'static) -> Result<Self> {
            // SAFETY: this dedicated thread keeps its COM apartment until
            // the enumerator and callback are released.
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
                let _com = Com;
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let client = DefaultInputChanged(Box::new(on_change)).into();
                enumerator.RegisterEndpointNotificationCallback(&client)?;
                Ok(Self {
                    enumerator,
                    client,
                    _com,
                })
            }
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            // SAFETY: this is the same enumerator and callback registration.
            let _ = unsafe {
                self.enumerator
                    .UnregisterEndpointNotificationCallback(&self.client)
            };
        }
    }

    struct Com;

    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: balances the successful initialization on this thread.
            unsafe { CoUninitialize() };
        }
    }

    #[implement(IMMNotificationClient)]
    struct DefaultInputChanged(Box<dyn Fn() + Send + Sync>);

    impl IMMNotificationClient_Impl for DefaultInputChanged_Impl {
        fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> Result<()> {
            // WASAPI's default_input_device uses the console role.
            if flow == eCapture && role == eConsole {
                (self.0)();
            }
            Ok(())
        }

        fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
            Ok(())
        }

        fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
            Ok(())
        }

        fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
            Ok(())
        }

        fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
mod notifications {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2_core_audio::{
        AudioObjectAddPropertyListenerBlock, AudioObjectPropertyAddress,
        AudioObjectRemovePropertyListenerBlock, kAudioHardwarePropertyDefaultInputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    };

    const SYSTEM: u32 = kAudioObjectSystemObject as u32;
    const DEFAULT_INPUT: AudioObjectPropertyAddress = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    type Listener = dyn Fn(u32, NonNull<AudioObjectPropertyAddress>);

    pub(super) fn watch_default(on_change: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
        let listener: RcBlock<Listener> = RcBlock::new(move |_, _| on_change());
        // SAFETY: the address is valid for the call. CoreAudio retains a copy
        // of the block, which only touches thread-safe state.
        let status = unsafe {
            AudioObjectAddPropertyListenerBlock(
                SYSTEM,
                NonNull::from(&DEFAULT_INPUT),
                None,
                RcBlock::as_ptr(&listener),
            )
        };
        if status != 0 {
            tracing::warn!(status, "default microphone notifications unavailable");
            return None;
        }
        Some(Watch(listener))
    }

    pub(super) struct Watch(RcBlock<Listener>);

    impl Drop for Watch {
        fn drop(&mut self) {
            // SAFETY: the same live block and property used to register.
            unsafe {
                AudioObjectRemovePropertyListenerBlock(
                    SYSTEM,
                    NonNull::from(&DEFAULT_INPUT),
                    None,
                    RcBlock::as_ptr(&self.0),
                );
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
use notifications::watch_default;

#[cfg(not(any(windows, target_os = "macos")))]
fn watch_default(_: impl Fn() + Send + Sync + 'static) -> Option<()> {
    None
}
