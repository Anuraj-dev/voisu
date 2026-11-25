//! Tells the ready mic when Windows changes the default microphone.
//!
//! The watcher only signals and logs. It never calls cpal and never opens an
//! audio client: its thread joins the multithreaded COM apartment to
//! register the notification callback, while cpal opens microphones from
//! single-threaded apartments. #348's input cache opened the new default from
//! a thread like this one at every switch, which is the suspected start of
//! the `RPC_E_CHANGED_MODE` failures that lasted until a restart. Presses
//! resolve the default mic themselves, on their own capture thread.

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

static LAST_CHANGE: Mutex<Option<Instant>> = Mutex::new(None);

/// How long ago Windows last changed the default microphone while Voisu
/// ran, for the log of a failed open.
pub(super) fn since_last_change() -> Option<Duration> {
    LAST_CHANGE.lock().unwrap().map(|at| at.elapsed())
}

/// Watches until dropped.
pub(super) struct DefaultWatch {
    /// Dropping it ends the watching thread, which unregisters.
    _stop: mpsc::Sender<()>,
}

impl DefaultWatch {
    /// Runs `on_change` on every default-mic change, from a Windows
    /// notification thread: it must only signal. Off Windows there is
    /// nothing to watch: the Mac never keeps a ready stream.
    pub(super) fn start(on_change: impl Fn() + Send + Sync + 'static) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        #[cfg(windows)]
        {
            let spawned = std::thread::Builder::new()
                .name("voisu-mic-watch".into())
                .spawn(move || {
                    let Some(_watch) = notifications::watch(on_change) else {
                        return;
                    };
                    // Nothing is ever sent: this returns once the handle
                    // drops, and the registration is undone on this thread.
                    let _ = stopped.recv();
                });
            if let Err(error) = spawned {
                tracing::warn!(%error, "default microphone watch thread could not start");
            }
        }
        #[cfg(not(windows))]
        drop((on_change, stopped));
        Self { _stop: stop }
    }
}

#[cfg(windows)]
mod notifications {
    use std::sync::Mutex;
    use std::time::Instant;

    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eConsole,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::core::{PCWSTR, Result, implement};

    use super::LAST_CHANGE;

    pub(super) fn watch(on_change: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
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
            // the enumerator and callback are released, and makes no cpal
            // call that would expect another apartment.
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
                let _com = Com;
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                // Reading the ID opens no audio client.
                let current = enumerator
                    .GetDefaultAudioEndpoint(eCapture, eConsole)
                    .and_then(|device| device.GetId())
                    .ok()
                    .and_then(|id| {
                        let text = id.to_string().ok();
                        CoTaskMemFree(Some(id.0.cast()));
                        text
                    });
                tracing::info!(
                    endpoint = current.as_deref().unwrap_or("none"),
                    "default microphone"
                );
                let client = DefaultInputChanged {
                    previous: Mutex::new(current),
                    on_change: Box::new(on_change),
                }
                .into();
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
    struct DefaultInputChanged {
        /// The default mic's endpoint ID as last seen, for the log.
        previous: Mutex<Option<String>>,
        on_change: Box<dyn Fn() + Send + Sync>,
    }

    impl IMMNotificationClient_Impl for DefaultInputChanged_Impl {
        fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, id: &PCWSTR) -> Result<()> {
            // WASAPI's default_input_device uses the console role.
            if flow != eCapture || role != eConsole {
                return Ok(());
            }
            *LAST_CHANGE.lock().unwrap() = Some(Instant::now());
            // SAFETY: Windows passes a valid string, or null when no default
            // mic is left, for the duration of the call.
            let current = (!id.is_null())
                .then(|| unsafe { id.to_string() }.ok())
                .flatten();
            let previous = std::mem::replace(&mut *self.previous.lock().unwrap(), current.clone());
            tracing::info!(
                from = previous.as_deref().unwrap_or("none"),
                to = current.as_deref().unwrap_or("none"),
                "default microphone changed"
            );
            (self.on_change)();
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
