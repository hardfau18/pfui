use anyhow::{anyhow, Result};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    thread::sleep,
    time::Duration,
};

use pulse::{
    callbacks::ListResult,
    context::{
        introspect::{SinkInfo, SourceInfo},
        Context,
    },
    mainloop::standard::{IterateResult, Mainloop},
};
use serde::Serialize;

use crate::Module;

macro_rules! volume {
    ($dev:ident) => {
        ($dev.volume.avg().0 * 100) / 0xffff
    };
}

#[derive(Debug)]
enum WaitError {
    Quit,
    Error(pulse::error::PAErr),
}

/// Waiter trait for pulse operation till it gets executed
trait WaitOp {
    /// Wait for Operation to finish or get cancelled while mainloop running in background
    /// recommended for callbacks
    fn wait(&self);
    /// Wait for Operation to finish and execute mainloop
    /// if mainloop returns error then breakout
    fn wait_with_loop(
        &self,
        mnloop: &mut pulse::mainloop::standard::Mainloop,
    ) -> Result<(), WaitError>;
}

impl<T: ?Sized> WaitOp for pulse::operation::Operation<T> {
    fn wait(&self) {
        while self.get_state() == pulse::operation::State::Running {
            std::thread::sleep(std::time::Duration::from_millis(50))
        }
    }

    fn wait_with_loop(
        &self,
        mnloop: &mut pulse::mainloop::standard::Mainloop,
    ) -> Result<(), WaitError> {
        while self.get_state() == pulse::operation::State::Running {
            std::thread::sleep(std::time::Duration::from_millis(50));
            match mnloop.iterate(false) {
                IterateResult::Quit(_) => return Err(WaitError::Quit),
                IterateResult::Err(e) => return Err(WaitError::Error(e)),
                _ => (),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct Data {
    volume: u32,
    muted: bool,
}

/// Sink/Source State,
/// since libpulse_bindings doesn't implements Hash
#[derive(Hash, Serialize)]
enum State {
    Invalid,
    Running,
    Idle,
    Suspended,
}

impl From<pulse::def::SinkState> for State {
    fn from(value: pulse::def::SinkState) -> Self {
        match value {
            pulse::def::SinkState::Invalid => Self::Invalid,
            pulse::def::SinkState::Running => Self::Running,
            pulse::def::SinkState::Idle => Self::Idle,
            pulse::def::SinkState::Suspended => Self::Suspended,
        }
    }
}
impl From<pulse::def::SourceState> for State {
    fn from(value: pulse::def::SourceState) -> Self {
        match value {
            pulse::def::SourceState::Invalid => Self::Invalid,
            pulse::def::SourceState::Running => Self::Running,
            pulse::def::SourceState::Idle => Self::Idle,
            pulse::def::SourceState::Suspended => Self::Suspended,
        }
    }
}

/// pulseAudio Sink representation
/// since libpulse_bindings implements neither PartialEq nor Clone to store it in vec or hashset
#[derive(Serialize)]
struct Sink {
    name: String,
    index: u32,
    volume: u32,
    muted: bool,
    monitor_index: u32,
    monitor_name: String,
    state: State,
}

impl From<&SinkInfo<'_>> for Sink {
    fn from(value: &SinkInfo) -> Self {
        Self {
            name: value
                .name
                .clone()
                .map_or(String::from("Unknown"), |name| name.into_owned()),
            index: value.index,
            volume: volume!(value),
            muted: value.mute,
            monitor_index: value.monitor_source,
            monitor_name: value
                .monitor_source_name
                .clone()
                .map_or(String::from("Unknown"), |name| name.into_owned()),
            state: State::from(value.state),
        }
    }
}

impl PartialEq for Sink {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl std::hash::Hash for Sink {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl Eq for Sink {}

#[derive(Serialize)]
struct Source {
    name: String,
    index: u32,
    volume: u32,
    muted: bool,
    monitor_index: Option<u32>,
    monitor_name: Option<String>,
    state: State,
}
impl From<&SourceInfo<'_>> for Source {
    fn from(value: &SourceInfo) -> Self {
        Self {
            name: value
                .name
                .clone()
                .map_or(String::from("Unknown"), |name| name.into_owned()),
            index: value.index,
            volume: volume!(value),
            muted: value.mute,
            monitor_index: value.monitor_of_sink,
            monitor_name: value
                .monitor_of_sink_name
                .clone()
                .map(|name| name.into_owned()),
            state: State::from(value.state),
        }
    }
}

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for Source {}

impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

pub struct Connection {
    cnxt: Context,
    mnlp: Mainloop,
}

#[derive(Serialize)]
struct Information {
    sinks: HashSet<Sink>,
    sources: HashSet<Source>,
    /// default sink index
    default_sink: Option<Sink>,
    /// default source index
    default_source: Option<Source>,
}
impl Connection {
    fn new(timeout: u64) -> Result<Self> {
        let mnlp = Mainloop::new().unwrap();
        for _ in 0..10 {
            let mut cnxt = Context::new(&mnlp, "pfui_listener").unwrap();
            if cnxt
                .connect(None, pulse::context::FlagSet::NOAUTOSPAWN, None)
                .is_ok()
            {
                return Ok(Self { cnxt, mnlp });
            }
            sleep(Duration::from_secs(timeout));
        }
        Err(anyhow!("Timed out creating connection"))
    }
    fn connect(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            match self.mnlp.iterate(false) {
                IterateResult::Err(e) => {
                    return Err(Box::new(e));
                }
                IterateResult::Quit(_) => {
                    return Err(Box::new(pulse::error::Code::BadState));
                }
                IterateResult::Success(_) => {}
            }
            match self.cnxt.get_state() {
                pulse::context::State::Ready => {
                    return Ok(());
                }
                pulse::context::State::Failed | pulse::context::State::Terminated => {
                    return Err(Box::new(pulse::error::Code::BadState));
                }
                _ => {}
            }
        }
    }
}

/// pulse operation which are sent to another thread to wait for
type OpsMsgs = (
    Vec<pulse::operation::Operation<dyn FnMut(ListResult<&SinkInfo<'_>>)>>,
    Vec<pulse::operation::Operation<dyn FnMut(ListResult<&SourceInfo<'_>>)>>,
);
pub struct PulseAudio {}

impl Module for PulseAudio {
    type Connection = Connection;
    fn connect(&mut self, timeout: u64) -> Result<Self::Connection> {
        Connection::new(timeout)
    }

    fn start(&mut self, timeout: u64) -> Result<()> {
        let mut conn = self.connect(timeout)?;
        if conn.connect().is_err() {
            return Err(anyhow!("Error establishing connection"));
        }
        let interest = pulse::context::subscribe::InterestMaskSet::SINK
            | pulse::context::subscribe::InterestMaskSet::SOURCE;
        conn.cnxt.subscribe(interest, |_| {});
        // print the data for initialization
        // sources and sinks
        let devices = Arc::new(Mutex::new(Information {
            sinks: HashSet::new(),
            sources: HashSet::new(),
            default_sink: None,
            default_source: None,
        }));
        let introspector = conn.cnxt.introspect();
        {
            let dclone = devices.clone();
            introspector
                .get_sink_info_list(move |res| {
                    let ListResult::Item(sink) = res else{ return};
                    let mut dlock = dclone.lock().unwrap();
                    dlock.sinks.insert(Sink::from(sink));
                })
                .wait_with_loop(&mut conn.mnlp)
                .unwrap();
            let dclone = devices.clone();
            let introspector = conn.cnxt.introspect();
            introspector
                .get_source_info_list(move |res| {
                    let ListResult::Item(source) = res else{ return};
                    let mut dlock = dclone.lock().unwrap();
                    dlock.sources.insert(Source::from(source));
                })
                .wait_with_loop(&mut conn.mnlp)
                .unwrap();

            let device_c = Arc::clone(&devices);
            introspector
                .get_sink_info_by_name("@DEFAULT_SINK@", move |list| {
                    if let pulse::callbacks::ListResult::Item(sink) = list {
                        device_c.lock().unwrap().default_sink = Some(Sink::from(sink));
                    }
                })
                .wait_with_loop(&mut conn.mnlp)
                .unwrap();
            let device_c = Arc::clone(&devices);
            introspector
                .get_source_info_by_name("@DEFAULT_SOURCE@", move |list| {
                    if let pulse::callbacks::ListResult::Item(source) = list {
                        device_c.lock().unwrap().default_source = Some(source.into());
                    }
                })
                .wait_with_loop(&mut conn.mnlp)
                .unwrap();
            let dlock = devices.lock().unwrap();
            crate::print(&Some(std::ops::Deref::deref(&dlock)))
        }
        let (tx, rx): (
            std::sync::mpsc::Sender<OpsMsgs>,
            std::sync::mpsc::Receiver<OpsMsgs>,
        ) = std::sync::mpsc::channel();
        let dclone = Arc::clone(&devices);
        // had to create separate thread for waiting for operations to finish, in call back if we wait then they will be
        // blocked forever. If we don't wait for them then Information printed will be of last operation, i.e. until
        // the event call back is not finished othercallbacks requesting information won't get executed. This is fine if
        // the volume differs by marginal but won't work for mute/unmute that will show exact opposite, so had to move it to another thread
        std::thread::spawn(move || {
            for msg in rx.iter() {
                let (sink_ops, src_ops) = msg;
                sink_ops.iter().for_each(|op| op.wait());
                src_ops.iter().for_each(|op| op.wait());
                let dlock = dclone.lock().unwrap();
                crate::print(&Some(std::ops::Deref::deref(&dlock)));
            }
        });
        conn.cnxt
            .set_subscribe_callback(Some(Box::new(move |facility, operation, index| {
                let mut sink_ops = Vec::with_capacity(4);
                let mut src_ops = Vec::with_capacity(4);
                let Some(operation) = operation else {
                    return;
                };
                let Some(facility) = facility else {
                    return;
                };
                let device_c = Arc::clone(&devices);
                sink_ops.push(introspector.get_sink_info_by_name("@DEFAULT_SINK@", move |list| {
                    if let pulse::callbacks::ListResult::Item(sink) = list {
                        device_c.lock().unwrap().default_sink = Some(sink.into());
                    }
                }));
                let device_c = Arc::clone(&devices);
                src_ops.push(introspector.get_source_info_by_name("@DEFAULT_SOURCE@", move |list| {
                    if let pulse::callbacks::ListResult::Item(source) = list {
                        device_c.lock().unwrap().default_source = Some(source.into());
                    }
                }));
                match operation {
                    pulse::context::subscribe::Operation::New => {
                        match facility{
                            pulse::context::subscribe::Facility::Sink => {
                                let dclone = devices.clone();
                                sink_ops.push(introspector.get_sink_info_by_index(index, move |res|{
                                    let ListResult::Item(sink) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sinks.insert(Sink::from(sink));
                                }));
                            },
                            pulse::context::subscribe::Facility::Source => {
                                let dclone = devices.clone();
                                src_ops.push(introspector.get_source_info_by_index(index, move |res|{
                                    let ListResult::Item(source) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sources.insert(Source::from(source));
                                }));
                            },
                            _ => eprintln!("{facility:?} is not handled when inserted, This was not supposed to enabled also"),
                        };
                    },
                    pulse::context::subscribe::Operation::Changed => {
                        match facility{
                            pulse::context::subscribe::Facility::Sink => {
                                let dclone = devices.clone();
                                sink_ops.push(introspector.get_sink_info_by_index(index, move |res|{
                                    let ListResult::Item(sink) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sinks.replace(Sink::from(sink));
                                }));

                            },
                            pulse::context::subscribe::Facility::Source => {
                                let dclone = devices.clone();
                                src_ops.push(introspector.get_source_info_by_index(index, move |res|{
                                    let ListResult::Item(source) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sources.replace(Source::from(source));
                                }));
                            },
                            _ => panic!("We are not expecting {facility:?}, this was supposed to be masked"),
                        }
                    },
                    pulse::context::subscribe::Operation::Removed => todo!(),
                }
                tx.send((sink_ops, src_ops)).unwrap();
            })));
        match conn.mnlp.run() {
            Ok(_retval) => Ok(()),
            Err((e, _retval)) => Err(anyhow::Error::new(e)),
        }
    }

    #[allow(unused)]
    fn output(&self, conn: &mut Self::Connection) {}
}
