//! The producer side of an instance's streams: what the machine released since the last look
//! becomes `event.*` notifications, `FRM1` frames and `AUD1` chunks for every WebSocket
//! subscriber, and the `serial.log` and `events.ndjson` artifacts.
//!
//! [`Hub::collect`] runs on the instance thread after every machine slice (through
//! [`observe_slice`]) and once more after the command, so a subscriber sees a long `run` as it
//! goes. The slice look passes no lifecycle, because the registry is behind the pool lock some
//! slice loops hold.
//!
//! An observer never slows or changes a run: the producer only reads the rings and never waits
//! on a subscriber; a full queue drops its oldest entry and counts the loss. The one thing it
//! takes is the frame port's dirty span, of which the daemon is the only native reader.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use pemu_api::error::{ApiError, E_GUEST_PANIC};
use pemu_api::instance::InstanceId;
use pemu_core::hostio::{EventKind, HostIo, SerialStream};
use pemu_core::time::VTime;

use crate::artifacts::{EVENTS_FILE, SERIAL_FILE, SharedDir};
use crate::ws::{
    AUD1, AudioHeader, FrameHeader, PixelFormat, Rect, Session, Topic, encode_audio, encode_frame,
    frame_flags, frame_payload, full_panel,
};

pub const TEXT_QUEUE: usize = 256;
pub const AUDIO_QUEUE: usize = 64;
/// Largest `AUD1` chunk, in frames, so one long playback is several binary messages.
pub const AUDIO_CHUNK_FRAMES: usize = 4096;

#[derive(Debug)]
pub struct FrameSnapshot {
    /// RGB565, row-major.
    pub pixels: Box<[u16]>,
    pub flags: u8,
    /// Backlight 0 to 1023: the raw LEDC duty shifted right by 4, as the wasm layout publishes it.
    pub backlight: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Push {
    Text(serde_json::Value),
    Binary(Vec<u8>),
}

#[derive(Debug, Default)]
struct Outbox {
    session: Session,
    texts: VecDeque<serde_json::Value>,
    audio: VecDeque<Vec<u8>>,
    dropped: u64,
}

#[derive(Debug, Default)]
pub struct Subscriber {
    outbox: Mutex<Outbox>,
    notify: tokio::sync::Notify,
}

impl Subscriber {
    fn lock(&self) -> std::sync::MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn with_session<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        f(&mut self.lock().session)
    }

    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    fn push_text(&self, value: serde_json::Value) {
        let mut outbox = self.lock();
        if outbox.texts.len() >= TEXT_QUEUE {
            outbox.texts.pop_front();
            outbox.dropped += 1;
        }
        outbox.texts.push_back(value);
    }

    fn push_audio(&self, bytes: Vec<u8>) {
        let mut outbox = self.lock();
        if outbox.audio.len() >= AUDIO_QUEUE {
            outbox.audio.pop_front();
            outbox.dropped += 1;
        }
        outbox.audio.push_back(bytes);
    }

    /// Everything queued for this client, frames last. The frame is the union of every rectangle
    /// offered since the last drain, with pixels read from `latest`.
    pub fn drain(&self, latest: Option<&FrameSnapshot>) -> Vec<Push> {
        let mut outbox = self.lock();
        let mut out: Vec<Push> = outbox.texts.drain(..).map(Push::Text).collect();
        out.extend(outbox.audio.drain(..).map(Push::Binary));
        if let Some(header) = outbox.session.take()
            && let Some(snapshot) = latest
        {
            let payload = frame_payload(&snapshot.pixels, header.rect);
            if let Ok(bytes) = encode_frame(&header, &payload) {
                out.push(Push::Binary(bytes));
            }
        }
        out
    }
}

#[derive(Debug, Default)]
struct Cursors {
    serial: u64,
    events: u64,
    audio: u64,
    frame_no: u32,
    lifecycle: Option<String>,
    waiting: Option<bool>,
    latest: Option<Arc<FrameSnapshot>>,
    now: VTime,
}

#[derive(Debug)]
pub struct Hub {
    id: InstanceId,
    cursors: Mutex<Cursors>,
    subscribers: Mutex<Vec<Weak<Subscriber>>>,
}

impl Hub {
    pub fn new(id: InstanceId) -> Arc<Hub> {
        Arc::new(Hub {
            id,
            cursors: Mutex::new(Cursors::default()),
            subscribers: Mutex::new(Vec::new()),
        })
    }

    pub fn id(&self) -> InstanceId {
        self.id
    }

    /// Registers a client. The hub keeps a weak reference, so a closed socket unsubscribes by
    /// being dropped.
    pub fn subscribe(&self) -> Arc<Subscriber> {
        let subscriber = Arc::new(Subscriber::default());
        let mut list = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|weak| weak.strong_count() > 0);
        list.push(Arc::downgrade(&subscriber));
        subscriber
    }

    pub fn latest(&self) -> Option<Arc<FrameSnapshot>> {
        self.cursors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest
            .clone()
    }

    /// Offers the whole panel to a client that just subscribed to `frames`: the dirty rows it
    /// would otherwise wait for may never come.
    pub fn offer_full_frame(&self, subscriber: &Subscriber) {
        let (latest, frame_no, now) = {
            let cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
            (cursors.latest.clone(), cursors.frame_no, cursors.now)
        };
        if let Some(snapshot) = latest {
            subscriber.with_session(|s| s.offer(header(&snapshot, frame_no, now, full_panel())));
            subscriber.notify.notify_one();
        }
    }

    /// Pushes the last lifecycle state to a client that just subscribed to `state`, since
    /// `event.state` is otherwise only sent on a change.
    pub fn offer_state(&self, subscriber: &Subscriber) {
        let (state, waiting, now) = {
            let cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
            (cursors.lifecycle.clone(), cursors.waiting, cursors.now)
        };
        if let Some(state) = state {
            let note = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "event.state",
                "params": {
                    "state": state,
                    "waiting_for_input": waiting.unwrap_or(false),
                    "vt_ms": now.as_us() as f64 / 1000.0,
                },
            });
            subscriber.push_text(with_frames_dropped(&note, subscriber));
            subscriber.notify.notify_one();
        }
    }

    fn live(&self) -> Vec<Arc<Subscriber>> {
        let mut list = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|weak| weak.strong_count() > 0);
        list.iter().filter_map(Weak::upgrade).collect()
    }

    /// Publishes what `io` released since the last look.
    ///
    /// `lifecycle` and `waiting` each become an `event.state` when they changed. An artifact write
    /// failure is ignored here so notifications are not lost; it surfaces on
    /// [`crate::artifacts::ArtifactDir::finish`].
    pub fn collect(
        &self,
        io: &mut HostIo,
        now: VTime,
        lifecycle: Option<&str>,
        waiting: Option<bool>,
        artifacts: Option<&SharedDir>,
    ) {
        let subscribers = self.live();
        let wants = |topic: Topic| -> Vec<&Arc<Subscriber>> {
            subscribers
                .iter()
                .filter(|s| s.with_session(|session| session.wants(topic)))
                .collect()
        };
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        cursors.now = now;
        let vt_ms = now.as_us() as f64 / 1000.0;

        let ring = io.serial_ring(SerialStream::UsjTx);
        let slices = ring.slices(cursors.serial);
        if !slices.is_empty() {
            let bytes: Vec<u8> = slices.iter().copied().collect();
            if let Some(dir) = artifacts {
                let _ = dir
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append(SERIAL_FILE, &bytes);
            }
            let note = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "event.serial",
                "params": {
                    "cursor": slices.start,
                    "vt_ms": vt_ms,
                    "text": String::from_utf8_lossy(&bytes),
                },
            });
            for s in wants(Topic::Serial) {
                s.push_text(note.clone());
            }
        }
        cursors.serial = slices.next;

        let events = io.events.slices(cursors.events);
        let mut ndjson = String::new();
        for event in events.iter() {
            let kind = kind_name(event.kind);
            ndjson.push_str(
                &serde_json::json!({ "vt_us": event.vt.as_us(), "kind": kind, "arg": event.arg })
                    .to_string(),
            );
            ndjson.push('\n');
            let at = event.vt.as_us() as f64 / 1000.0;
            let (topic, method, params) = match event.kind {
                EventKind::Reset => (
                    Topic::State,
                    "event.state",
                    serde_json::json!({
                        "state": "reset",
                        "reset_reason": reset_reason_name(event.arg),
                        "reset_cause": event.arg,
                        "vt_ms": at,
                    }),
                ),
                EventKind::Power => (
                    Topic::State,
                    "event.state",
                    serde_json::json!({ "state": power_state(event.arg), "power": event.arg, "vt_ms": at }),
                ),
                EventKind::Sleep => (
                    Topic::State,
                    "event.state",
                    serde_json::json!({ "state": "sleep", "vt_ms": at }),
                ),
                EventKind::Panic => (
                    Topic::Fault,
                    "event.fault",
                    serde_json::json!({
                        "error": ApiError::new(E_GUEST_PANIC, "the guest panicked")
                            .at_vt_us(event.vt.as_us())
                            .to_json(),
                        "vt_ms": at,
                    }),
                ),
                EventKind::UiSettled => (
                    Topic::UiChanged,
                    "event.ui_changed",
                    serde_json::json!({ "ui_rev": event.arg, "vt_ms": at }),
                ),
                EventKind::FidelityWarning => (
                    Topic::Fidelity,
                    "event.fidelity",
                    serde_json::json!({ "warning": format!("fidelity warning {}", event.arg), "vt_ms": at }),
                ),
                EventKind::Frame => continue,
            };
            let note = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
            for s in wants(topic) {
                s.push_text(if topic == Topic::State {
                    with_frames_dropped(&note, s)
                } else {
                    note.clone()
                });
            }
        }
        cursors.events = events.next;
        if !ndjson.is_empty()
            && let Some(dir) = artifacts
        {
            let _ = dir
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append(EVENTS_FILE, ndjson.as_bytes());
        }

        if let Some(state) = lifecycle
            && cursors.lifecycle.as_deref() != Some(state)
        {
            cursors.lifecycle = Some(state.to_string());
            let note = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "event.state",
                "params": { "state": state, "vt_ms": vt_ms },
            });
            for s in wants(Topic::State) {
                s.push_text(with_frames_dropped(&note, s));
            }
        }
        if let Some(waiting) = waiting
            && cursors.waiting != Some(waiting)
        {
            // The first look only records it: a fresh instance is not waiting, and a later
            // subscriber gets it from [`Hub::offer_state`].
            let changed = cursors.waiting.is_some() || waiting;
            cursors.waiting = Some(waiting);
            if changed {
                let mut params =
                    serde_json::json!({ "waiting_for_input": waiting, "vt_ms": vt_ms });
                if let Some(state) = &cursors.lifecycle {
                    params["state"] = serde_json::Value::from(state.as_str());
                }
                let note = serde_json::json!({ "jsonrpc": "2.0", "method": "event.state", "params": params });
                for s in wants(Topic::State) {
                    s.push_text(with_frames_dropped(&note, s));
                }
            }
        }

        // A flags or backlight change with no row painted (DISPON, INVON, dimming) still changes
        // the glass, so it is published as the whole panel.
        let flags = frame_flags(&io.frame);
        let backlight = (io.frame.backlight() >> 4).min(1023);
        let shown = cursors
            .latest
            .as_ref()
            .map_or((0, 0), |latest| (latest.flags, latest.backlight));
        let rect = match io.frame.take_dirty() {
            Some((first, last)) => Some(Rect {
                x: 0,
                y: first,
                w: io.frame.width() as u16,
                h: last - first + 1,
            }),
            None if shown != (flags, backlight) => Some(full_panel()),
            None => None,
        };
        if let Some(rect) = rect {
            cursors.frame_no = cursors.frame_no.wrapping_add(1);
            let snapshot = Arc::new(FrameSnapshot {
                pixels: io.frame.pixels().into(),
                flags,
                backlight,
            });
            let header = header(&snapshot, cursors.frame_no, now, rect);
            cursors.latest = Some(snapshot);
            for s in wants(Topic::Frames) {
                s.with_session(|session| session.offer(header));
            }
        }

        let head = io.audio_out.head();
        let start = cursors.audio.max(io.audio_out.tail());
        let audio_subscribers = wants(Topic::Audio);
        if head > start && !audio_subscribers.is_empty() {
            let records: Vec<_> = io
                .audio_out
                .record_slices(io.audio_out.record_tail())
                .iter()
                .copied()
                .collect();
            let samples: Vec<i16> = io.audio_out.slices(start).iter().copied().collect();
            for (i, record) in records.iter().enumerate() {
                let end = records.get(i + 1).map_or(head, |next| next.first).min(head);
                let from = record.first.max(start);
                if end <= from || record.channels == 0 {
                    continue;
                }
                let run = &samples[(from - start) as usize..(end - start) as usize];
                let frame = usize::from(record.channels) * AUDIO_CHUNK_FRAMES;
                let mut at = from;
                for chunk in run.chunks(frame) {
                    let n_frames = (chunk.len() / usize::from(record.channels)) as u32;
                    let header = AudioHeader {
                        magic: AUD1,
                        vt_ns: record.time_of(at).0 / 1_000,
                        sample_rate: record.fs,
                        channels: record.channels as u8,
                        bits: 16,
                        dir: 0,
                        n_frames,
                    };
                    let pcm: Vec<u8> = chunk
                        .iter()
                        .take(n_frames as usize * usize::from(record.channels))
                        .flat_map(|s| s.to_le_bytes())
                        .collect();
                    if let Ok(bytes) = encode_audio(&header, &pcm) {
                        for s in &audio_subscribers {
                            s.push_audio(bytes.clone());
                        }
                    }
                    at += chunk.len() as u64;
                }
            }
        }
        cursors.audio = head;
        drop(cursors);

        for s in &subscribers {
            s.notify.notify_one();
        }
    }
}

fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Reset => "reset",
        EventKind::Panic => "panic",
        EventKind::Sleep => "sleep",
        EventKind::Power => "power",
        EventKind::Frame => "frame",
        EventKind::UiSettled => "ui_settled",
        EventKind::FidelityWarning => "fidelity_warning",
    }
}

fn header(snapshot: &FrameSnapshot, frame_no: u32, now: VTime, rect: Rect) -> FrameHeader {
    FrameHeader {
        frame_no,
        // VTime counts picoseconds; the wire counts nanoseconds.
        vt_ns: now.0 / 1_000,
        rect,
        format: PixelFormat::Rgb565Le,
        flags: snapshot.flags,
        backlight: snapshot.backlight,
    }
}

/// The ROM name of a reset cause (`POWERON_RESET`, ...), as the boot banner prints it; an
/// unknown code is named by value.
fn reset_reason_name(cause: u64) -> String {
    u8::try_from(cause)
        .ok()
        .and_then(|value| pemu_core::reset::ResetCause(value).spec())
        .map_or_else(
            || format!("UNKNOWN_RESET_0x{cause:02x}"),
            |spec| spec.rom_name.to_string(),
        )
}

/// The word for an `EventKind::Power` record: 1 on, 0 off, 2 a brownout.
fn power_state(arg: u64) -> &'static str {
    match arg {
        0 => "powered_off",
        1 => "powered_on",
        2 => "brownout",
        _ => "power_unknown",
    }
}

/// An `event.state` note with the subscriber's own dropped-frame count.
fn with_frames_dropped(note: &serde_json::Value, subscriber: &Subscriber) -> serde_json::Value {
    let mut note = note.clone();
    note["params"]["frames_dropped"] =
        serde_json::Value::from(subscriber.with_session(|session| session.frames_dropped()));
    note
}

type Binding = Option<(InstanceId, Arc<Hub>, Option<SharedDir>)>;

thread_local! {
    static CURRENT: RefCell<Binding> = const { RefCell::new(None) };
}

/// Runs `f` with `hub` bound as this thread's producer for instance `id`, restoring the previous
/// binding afterwards, panic or not.
pub fn bind_current<R>(
    id: InstanceId,
    hub: Arc<Hub>,
    artifacts: Option<SharedDir>,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(Binding);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|slot| slot.borrow_mut().replace((id, hub, artifacts)));
    let _restore = Restore(previous);
    f()
}

/// The per-slice observer the daemon installs in `pemu-api`. Does nothing on a thread not bound to
/// the session's instance (a CLI-hosted run, or another instance's slice).
pub fn observe_slice(session: &mut pemu_api::commands::start::Session) {
    let bound = CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|(id, _, _)| *id == session.id)
            .map(|(_, hub, artifacts)| (Arc::clone(hub), artifacts.clone()))
    });
    if let Some((hub, artifacts)) = bound {
        let now = session.now();
        let waiting = session.waiting_for_input;
        hub.collect(
            session.machine().io(),
            now,
            None,
            Some(waiting),
            artifacts.as_ref(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactDir;
    use crate::ws::{
        FLAG_DISPLAY_ON, FLAG_GLASS_COMPLEMENT, FLAG_POWERED, decode_audio, decode_frame,
    };
    use pemu_core::hostio::HostEvent;

    fn id() -> InstanceId {
        InstanceId::parse("p1").expect("an id")
    }

    fn texts(pushes: &[Push]) -> Vec<&serde_json::Value> {
        pushes
            .iter()
            .filter_map(|p| match p {
                Push::Text(v) => Some(v),
                Push::Binary(_) => None,
            })
            .collect()
    }

    fn binaries(pushes: &[Push]) -> Vec<&[u8]> {
        pushes
            .iter()
            .filter_map(|p| match p {
                Push::Binary(b) => Some(b.as_slice()),
                Push::Text(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_subscriber_receives_serial_state_frames_and_audio_after_a_look() {
        let hub = Hub::new(id());
        let sub = hub.subscribe();
        let quiet = hub.subscribe();
        sub.with_session(|s| s.subscribe(&["serial", "state", "frames", "audio"]));
        let mut io = HostIo::new(8192);

        io.serial_write(
            SerialStream::UsjTx,
            b"entry 0x403cbf1a\n",
            VTime::from_ms(24),
        );
        io.events.emit(HostEvent {
            kind: EventKind::Reset,
            vt: VTime::from_ms(1),
            arg: 1,
        });
        io.frame.set_display_on(true);
        io.frame.set_inverted(true);
        io.frame.set_sleeping(false);
        io.frame.pixels_mut()[240 * 7 + 3] = 0x145D;
        io.frame.mark_dirty(7, 8);
        io.audio_out
            .write(VTime::from_ms(30), 16_000, 1, &[100, -100, 200]);
        hub.collect(&mut io, VTime::from_ms(40), Some("paused"), None, None);

        let pushes = sub.drain(hub.latest().as_deref());
        let notes = texts(&pushes);
        let methods: Vec<&str> = notes
            .iter()
            .map(|n| n["method"].as_str().expect("a method"))
            .collect();
        assert_eq!(methods, ["event.serial", "event.state", "event.state"]);
        assert_eq!(notes[0]["params"]["text"], "entry 0x403cbf1a\n");
        assert_eq!(notes[0]["params"]["cursor"], 0);
        assert_eq!(notes[1]["params"]["reset_reason"], "POWERON_RESET");
        assert_eq!(notes[1]["params"]["reset_cause"], 1);
        assert_eq!(notes[1]["params"]["frames_dropped"], 0);
        assert_eq!(notes[2]["params"]["state"], "paused");

        let bins = binaries(&pushes);
        assert_eq!(bins.len(), 2, "one AUD1 chunk and one FRM1 frame");
        let (audio, pcm) = decode_audio(bins[0]).expect("AUD1");
        assert_eq!(
            (
                audio.magic,
                audio.sample_rate,
                audio.channels,
                audio.n_frames
            ),
            (AUD1, 16_000, 1, 3)
        );
        assert_eq!(audio.vt_ns, 30_000_000);
        assert_eq!(pcm, [100, 0, 156, 255, 200, 0]);
        let (frame, payload) = decode_frame(bins[1]).expect("FRM1");
        assert_eq!((frame.rect.y, frame.rect.h, frame.rect.w), (7, 2, 240));
        assert_ne!(frame.flags & FLAG_DISPLAY_ON, 0);
        assert_eq!(frame.flags & FLAG_GLASS_COMPLEMENT, 0, "INVON shows memory");
        assert_eq!(frame.vt_ns, 40_000_000);
        assert_eq!(&payload[6..8], &0x145Du16.to_le_bytes());

        assert!(
            quiet.drain(hub.latest().as_deref()).is_empty(),
            "no topic, no push"
        );

        hub.collect(&mut io, VTime::from_ms(50), Some("paused"), None, None);
        assert!(sub.drain(hub.latest().as_deref()).is_empty());
    }

    #[test]
    fn a_client_that_falls_behind_gets_the_union_and_the_run_is_not_held_up() {
        let hub = Hub::new(id());
        let sub = hub.subscribe();
        sub.with_session(|s| s.subscribe(&["frames"]));
        let mut io = HostIo::new(1024);
        io.frame.mark_dirty(10, 10);
        hub.collect(&mut io, VTime::from_ms(1), None, None, None);
        io.frame.mark_dirty(300, 319);
        hub.collect(&mut io, VTime::from_ms(2), None, None, None);
        let pushes = sub.drain(hub.latest().as_deref());
        let (frame, _) = decode_frame(binaries(&pushes)[0]).expect("FRM1");
        assert_eq!((frame.rect.y, frame.rect.h), (10, 310));
        assert_eq!(frame.frame_no, 2, "the newest frame's metadata");
        assert_eq!(sub.with_session(|s| s.frames_dropped()), 1);
    }

    #[test]
    fn a_flag_or_backlight_change_without_painted_rows_publishes_the_whole_panel() {
        let hub = Hub::new(id());
        let sub = hub.subscribe();
        sub.with_session(|s| s.subscribe(&["frames"]));
        let mut io = HostIo::new(1024);
        io.frame.mark_dirty(5, 5);
        hub.collect(&mut io, VTime::from_ms(1), None, None, None);
        let first = sub.drain(hub.latest().as_deref());
        let (frame, _) = decode_frame(binaries(&first)[0]).expect("FRM1");
        assert_eq!((frame.rect.y, frame.rect.h), (5, 1));

        hub.collect(&mut io, VTime::from_ms(2), None, None, None);
        assert!(sub.drain(hub.latest().as_deref()).is_empty());

        io.frame.set_display_on(true);
        hub.collect(&mut io, VTime::from_ms(3), None, None, None);
        sub.drain(hub.latest().as_deref());
        io.frame.set_powered(!io.frame.powered());
        hub.collect(&mut io, VTime::from_ms(3), None, None, None);
        let pushes = sub.drain(hub.latest().as_deref());
        let (frame, _) = decode_frame(binaries(&pushes)[0]).expect("FRM1");
        assert_eq!(frame.rect, full_panel());
        assert_eq!(
            frame.flags & FLAG_POWERED,
            u8::from(io.frame.powered()) << 5
        );
        io.frame.set_display_on(false);
        hub.collect(&mut io, VTime::from_ms(3), None, None, None);
        sub.drain(hub.latest().as_deref());
        io.frame.set_display_on(true);
        hub.collect(&mut io, VTime::from_ms(3), None, None, None);
        let pushes = sub.drain(hub.latest().as_deref());
        let (frame, payload) = decode_frame(binaries(&pushes)[0]).expect("FRM1");
        assert_eq!(frame.rect, full_panel());
        assert_ne!(frame.flags & FLAG_DISPLAY_ON, 0);
        assert_eq!(payload.len(), 240 * 320 * 2);

        io.frame.set_backlight(0x2000);
        hub.collect(&mut io, VTime::from_ms(4), None, None, None);
        let pushes = sub.drain(hub.latest().as_deref());
        let (frame, _) = decode_frame(binaries(&pushes)[0]).expect("FRM1");
        assert_eq!(frame.rect, full_panel());
        assert_eq!(frame.backlight, 0x200);
        hub.collect(&mut io, VTime::from_ms(5), None, None, None);
        assert!(sub.drain(hub.latest().as_deref()).is_empty());
    }

    #[test]
    fn state_and_fault_notes_carry_names_the_error_envelope_and_the_wait_for_input() {
        let hub = Hub::new(id());
        let sub = hub.subscribe();
        sub.with_session(|s| s.subscribe(&["state", "fault"]));
        let mut io = HostIo::new(1024);
        for (kind, arg, ms) in [
            (EventKind::Power, 1, 1),
            (EventKind::Reset, 0x0C, 2),
            (EventKind::Reset, 0x3F, 3),
            (EventKind::Power, 2, 4),
            (EventKind::Panic, 0, 5),
        ] {
            io.events.emit(HostEvent {
                kind,
                vt: VTime::from_ms(ms),
                arg,
            });
        }
        hub.collect(
            &mut io,
            VTime::from_ms(6),
            Some("running"),
            Some(false),
            None,
        );
        let pushes = sub.drain(hub.latest().as_deref());
        let notes = texts(&pushes);
        let params: Vec<&serde_json::Value> = notes.iter().map(|n| &n["params"]).collect();
        assert_eq!(params[0]["state"], "powered_on");
        assert_eq!(params[0]["power"], 1);
        assert_eq!(params[1]["reset_reason"], "RTC_SW_CPU_RESET");
        assert_eq!(params[2]["reset_reason"], "UNKNOWN_RESET_0x3f");
        assert_eq!(params[3]["state"], "brownout");
        assert_eq!(notes[4]["method"], "event.fault");
        let error = &params[4]["error"];
        assert_eq!(error["code"], "E_GUEST_PANIC");
        assert_eq!(error["vt_us"], 5_000);
        for key in ["number", "retryable", "detail", "serial_tail", "backtrace"] {
            assert!(
                error.get(key).is_some(),
                "the ApiError envelope has `{key}`: {error}"
            );
        }
        assert!(
            params[4].get("frames_dropped").is_none(),
            "only state notes count frames"
        );
        assert_eq!(params[5]["state"], "running");
        for p in [params[0], params[1], params[2], params[3], params[5]] {
            assert_eq!(p["frames_dropped"], 0, "{p}");
        }
        assert_eq!(
            notes.len(),
            6,
            "the first `waiting: false` is only recorded"
        );

        hub.collect(
            &mut io,
            VTime::from_ms(7),
            Some("running"),
            Some(true),
            None,
        );
        hub.collect(
            &mut io,
            VTime::from_ms(8),
            Some("running"),
            Some(true),
            None,
        );
        hub.collect(
            &mut io,
            VTime::from_ms(9),
            Some("running"),
            Some(false),
            None,
        );
        let pushes = sub.drain(hub.latest().as_deref());
        let notes = texts(&pushes);
        let waits: Vec<&serde_json::Value> = notes
            .iter()
            .map(|n| &n["params"]["waiting_for_input"])
            .collect();
        assert_eq!(waits, [true, false]);
        assert_eq!(notes[0]["params"]["state"], "running");

        let late = hub.subscribe();
        late.with_session(|s| s.subscribe(&["state"]));
        hub.offer_state(&late);
        let offered = texts(&late.drain(None))[0].clone();
        assert_eq!(offered["params"]["waiting_for_input"], false);
        assert_eq!(offered["params"]["frames_dropped"], 0);
    }

    #[test]
    fn a_new_frames_subscriber_is_offered_the_whole_panel() {
        let hub = Hub::new(id());
        let mut io = HostIo::new(1024);
        io.frame.mark_dirty(0, 0);
        hub.collect(&mut io, VTime::from_ms(1), None, None, None);
        let late = hub.subscribe();
        late.with_session(|s| s.subscribe(&["frames"]));
        hub.offer_full_frame(&late);
        let pushes = late.drain(hub.latest().as_deref());
        let (frame, payload) = decode_frame(binaries(&pushes)[0]).expect("FRM1");
        assert_eq!(frame.rect, full_panel());
        assert_eq!(payload.len(), 240 * 320 * 2);
    }

    #[test]
    fn serial_and_events_reach_the_instance_artifacts() {
        let root = std::env::temp_dir().join(format!(
            "pemu-hub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        let dir: SharedDir = Arc::new(Mutex::new(
            ArtifactDir::create(&root, "run-1", "p1").expect("create"),
        ));
        let hub = Hub::new(id());
        let mut io = HostIo::new(1024);
        io.serial_write(SerialStream::UsjTx, b"a\n", VTime::from_ms(1));
        io.events.emit(HostEvent {
            kind: EventKind::UiSettled,
            vt: VTime::from_ms(2),
            arg: 9,
        });
        hub.collect(&mut io, VTime::from_ms(3), None, None, Some(&dir));
        io.serial_write(SerialStream::UsjTx, b"b\n", VTime::from_ms(4));
        hub.collect(&mut io, VTime::from_ms(5), None, None, Some(&dir));
        dir.lock().expect("lock").flush().expect("flush");
        let base = root.join("run-1/p1");
        assert_eq!(
            std::fs::read(base.join(SERIAL_FILE)).expect("serial"),
            b"a\nb\n"
        );
        let events = std::fs::read_to_string(base.join(EVENTS_FILE)).expect("events");
        let line: serde_json::Value =
            serde_json::from_str(events.lines().next().expect("a line")).expect("json");
        assert_eq!(line["kind"], "ui_settled");
        assert_eq!(line["arg"], 9);
        assert_eq!(line["vt_us"], 2_000);
    }
}
