//! Supervised PulseAudio connection.
//!
//! A single long-lived `pa_context` is kept on a dedicated thread. This lets
//! device lookups run without a fresh connect round trip, and lets the daemon
//! survive a PulseAudio / PipeWire restart.
//!
//! No record stream is held between sessions - only the context stays open. A
//! record stream is still opened cold when a session starts, so the microphone
//! is not shown as in use while the daemon is idle.
//!
//! If the server goes away, the context reaches `Terminated` (or `Failed`) and
//! is rebuilt with exponential backoff. Sources are always addressed by name,
//! never by index: indexes are not stable across a restart.

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::threaded::Mainloop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Timeout for a single introspection round trip.
const CALL_TIMEOUT: Duration = Duration::from_millis(500);

/// First reconnect delay after the connection is lost.
const RECONNECT_MIN: Duration = Duration::from_millis(100);

/// Maximum reconnect delay.
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// Longest the manager sleeps while the connection is healthy. The context
/// state callback wakes it earlier; this is only a safety net.
const IDLE_TICK: Duration = Duration::from_secs(5);

/// Wakes the manager thread when a request arrives or the context state
/// changes, without losing a wake-up that races with the wait.
struct Notifier {
    generation: Mutex<u64>,
    condvar: Condvar,
}

impl Notifier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: Mutex::new(0),
            condvar: Condvar::new(),
        })
    }

    fn notify(&self) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *generation = generation.wrapping_add(1);
        self.condvar.notify_all();
    }

    fn current(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until `observed` changes or `timeout` elapses, then store the
    /// latest generation in `observed`.
    fn wait_for_change(&self, observed: &mut u64, timeout: Duration) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        while *generation == *observed {
            let (next, result) = self
                .condvar
                .wait_timeout(generation, timeout)
                .unwrap_or_else(|e| e.into_inner());
            generation = next;
            if result.timed_out() {
                break;
            }
        }
        *observed = *generation;
    }
}

enum Request {
    DefaultSourceName(Sender<Option<String>>),
    SourceDescription {
        name: String,
        reply: Sender<Option<String>>,
    },
    SourceMute {
        name: String,
        reply: Sender<Option<bool>>,
    },
    SetMute {
        name: String,
        mute: bool,
        reply: Sender<bool>,
    },
}

/// Handle to the supervised PulseAudio connection. Cheap to clone.
pub struct PulseManager {
    requests: Sender<Request>,
    ready: Arc<AtomicBool>,
    notifier: Arc<Notifier>,
}

impl PulseManager {
    /// Start the manager thread.
    ///
    /// Never fails: if PulseAudio is unavailable the manager keeps retrying in
    /// the background and the daemon still starts.
    pub fn spawn() -> Arc<Self> {
        let (requests, receiver) = mpsc::channel();
        let ready = Arc::new(AtomicBool::new(false));
        let notifier = Notifier::new();

        let ready_for_thread = Arc::clone(&ready);
        let notifier_for_thread = Arc::clone(&notifier);
        thread::Builder::new()
            .name("stentor-pulse".to_owned())
            .spawn(move || manager_loop(receiver, ready_for_thread, notifier_for_thread))
            .expect("failed to spawn PulseAudio manager thread");

        Arc::new(Self {
            requests,
            ready,
            notifier,
        })
    }

    /// Whether the context is currently connected and ready.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn request<T>(&self, make: impl FnOnce(Sender<T>) -> Request, timeout: Duration) -> Option<T> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.requests.send(make(reply_tx)).is_err() {
            return None;
        }
        self.notifier.notify();
        reply_rx.recv_timeout(timeout).ok()
    }

    /// Name of the current default source, or `None` if unavailable.
    pub fn default_source_name(&self) -> Option<String> {
        self.request(Request::DefaultSourceName, CALL_TIMEOUT)
            .flatten()
    }

    /// Description of the named source, or `None` if unavailable.
    pub fn source_description(&self, name: &str) -> Option<String> {
        let name = name.to_owned();
        self.request(
            move |reply| Request::SourceDescription { name, reply },
            CALL_TIMEOUT,
        )
        .flatten()
    }

    /// Mute state of the named source, or `None` if unavailable.
    pub fn source_mute(&self, name: &str) -> Option<bool> {
        let name = name.to_owned();
        self.request(
            move |reply| Request::SourceMute { name, reply },
            CALL_TIMEOUT,
        )
        .flatten()
    }

    /// Set the mute state of the named source. Returns whether it succeeded.
    pub fn set_mute(&self, name: &str, mute: bool) -> bool {
        let name = name.to_owned();
        self.request(
            move |reply| Request::SetMute { name, mute, reply },
            CALL_TIMEOUT,
        )
        .unwrap_or(false)
    }

    /// A human-readable microphone name for the UI, resolving the default
    /// source when `source` is `None`.
    pub fn device_name(&self, source: Option<&str>) -> String {
        let name = match source {
            Some(name) => name.to_owned(),
            None => match self.default_source_name() {
                Some(name) => name,
                None => return "Default".to_owned(),
            },
        };
        self.source_description(&name)
            .unwrap_or_else(|| "Default".to_owned())
    }
}

/// Create and connect a context. Locking is required before touching a context
/// whose mainloop is already running.
fn connect_context(mainloop: &mut Mainloop, notifier: &Arc<Notifier>) -> Option<Context> {
    let mut context = Context::new(&*mainloop, "stentor")?;

    mainloop.lock();
    {
        let notifier = Arc::clone(notifier);
        context.set_state_callback(Some(Box::new(move || notifier.notify())));
    }
    if let Err(e) = context.connect(None, FlagSet::NOFLAGS, None) {
        tracing::warn!(error = %e, "Failed to connect to PulseAudio");
    }
    mainloop.unlock();

    Some(context)
}

/// Issue a request's introspection call. The reply is delivered from the event
/// thread when the call completes; the returned operation is intentionally
/// dropped (the context keeps its own reference).
fn issue(context: &Option<Context>, request: Request) {
    let Some(context) = context else {
        // Connection is down; the caller will time out.
        return;
    };

    match request {
        Request::DefaultSourceName(reply) => {
            context.introspect().get_server_info(move |info| {
                let name = info.default_source_name.as_ref().map(|s| s.to_string());
                let _ = reply.send(name);
            });
        }
        Request::SourceDescription { name, reply } => {
            context
                .introspect()
                .get_source_info_by_name(&name, move |result| match result {
                    ListResult::Item(source) => {
                        let description = source.description.as_ref().map(|d| d.to_string());
                        let _ = reply.send(description);
                    }
                    ListResult::End | ListResult::Error => {
                        let _ = reply.send(None);
                    }
                });
        }
        Request::SourceMute { name, reply } => {
            context
                .introspect()
                .get_source_info_by_name(&name, move |result| match result {
                    ListResult::Item(source) => {
                        let _ = reply.send(Some(source.mute));
                    }
                    ListResult::End | ListResult::Error => {
                        let _ = reply.send(None);
                    }
                });
        }
        Request::SetMute { name, mute, reply } => {
            let mut introspect = context.introspect();
            introspect.set_source_mute_by_name(
                &name,
                mute,
                Some(Box::new(move |success| {
                    let _ = reply.send(success);
                })),
            );
        }
    }
}

fn manager_loop(
    receiver: mpsc::Receiver<Request>,
    ready: Arc<AtomicBool>,
    notifier: Arc<Notifier>,
) {
    let Some(mut mainloop) = Mainloop::new() else {
        tracing::error!("Failed to create PulseAudio mainloop");
        return;
    };

    let mut context = connect_context(&mut mainloop, &notifier);
    if let Err(e) = mainloop.start() {
        tracing::error!(error = %e, "Failed to start PulseAudio mainloop");
        return;
    }

    let mut observed = notifier.current();
    let mut backoff = RECONNECT_MIN;

    loop {
        if context.is_none() {
            context = connect_context(&mut mainloop, &notifier);
        }

        mainloop.lock();
        let state = context
            .as_ref()
            .map(Context::get_state)
            .unwrap_or(State::Failed);

        match state {
            State::Ready => {
                if !ready.swap(true, Ordering::AcqRel) {
                    tracing::info!("PulseAudio context ready");
                }
                backoff = RECONNECT_MIN;
                while let Ok(request) = receiver.try_recv() {
                    issue(&context, request);
                }
            }
            State::Failed | State::Terminated => {
                if ready.swap(false, Ordering::AcqRel) {
                    tracing::warn!(?state, "PulseAudio connection lost; will reconnect");
                }
                if let Some(mut context) = context.take() {
                    context.disconnect();
                }
            }
            State::Unconnected | State::Connecting | State::Authorizing | State::SettingName => {}
        }
        mainloop.unlock();

        if context.is_none() {
            notifier.wait_for_change(&mut observed, backoff);
            backoff = (backoff * 2).min(RECONNECT_MAX);
        } else {
            notifier.wait_for_change(&mut observed, IDLE_TICK);
        }
    }
}
