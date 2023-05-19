use anyhow::{anyhow, Result};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    thread::sleep,
    time::Duration,
};

use pulse::{
    callbacks::ListResult,
    context::Context,
    mainloop::standard::{IterateResult, Mainloop},
};
use serde::Serialize;

use crate::Module;

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
    default_sink: u32,
    /// default source index
    default_source: u32,
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
            default_sink: 0,
            default_source: 0,
        }));
        {
            let introspector = conn.cnxt.introspect();
            let dclone = devices.clone();
            let status = introspector.get_sink_info_list(move |res| {
                let ListResult::Item(sink) = res else{ return};
                let mut dlock = dclone.lock().unwrap();
                dlock.sinks.insert(Sink {
                    name: sink
                        .name
                        .clone()
                        .map_or(String::from("Unknown"), |name| name.into_owned()),
                    index: sink.index,
                    volume: sink.volume.avg().0,
                    muted: sink.mute,
                    monitor_index: sink.monitor_source,
                    monitor_name: sink
                        .monitor_source_name
                        .clone()
                        .map_or(String::from("Unknown"), |name| name.into_owned()),
                    state: State::from(sink.state),
                });
            });
            // wait for call back to finish
            while status.get_state() == pulse::operation::State::Running {
                std::thread::sleep(std::time::Duration::from_millis(1));
                if !conn.mnlp.iterate(false).is_success(){
                    return Err(anyhow!("Failed to iterate mnloop for sources"));
                }
            }
            let dclone = devices.clone();
            let introspector = conn.cnxt.introspect();
            let status = introspector.get_source_info_list(move |res| {
                let ListResult::Item(source) = res else{ return};
                let mut dlock = dclone.lock().unwrap();
                dlock.sources.insert(Source {
                    name: source
                        .name
                        .clone()
                        .map_or(String::from("Unknown"), |name| name.into_owned()),
                    index: source.index,
                    volume: source.volume.avg().0,
                    muted: source.mute,
                    monitor_index: source.monitor_of_sink,
                    monitor_name: source
                        .monitor_of_sink_name
                        .clone()
                        .map(|name| name.into_owned()),
                    state: State::from(source.state),
                });
            });
            // wait for call back
            while status.get_state() == pulse::operation::State::Running {
                std::thread::sleep(std::time::Duration::from_millis(1));
                if !conn.mnlp.iterate(false).is_success(){
                    return Err(anyhow!("Failed to iterate mnloop for sources"));
                }
            }
            let dlock = devices.lock().unwrap();
            crate::print(&Some(std::ops::Deref::deref(&dlock)))
        }
        let introspector = conn.cnxt.introspect();
        conn.cnxt
            .set_subscribe_callback(Some(Box::new(move |facility, operation, index| {
                let Some(operation) = operation else {
                    return;
                };
                let Some(facility) = facility else {
                    return;
                };
                let device_c = Arc::clone(&devices);
                introspector.get_sink_info_by_name("@DEFAULT_SINK@", move |list| {
                    if let pulse::callbacks::ListResult::Item(sink) = list {
                        device_c.lock().unwrap().default_sink = sink.index;
                    }
                });
                let device_c = Arc::clone(&devices);
                introspector.get_sink_info_by_name("@DEFAULT_SOURCE@", move |list| {
                    if let pulse::callbacks::ListResult::Item(sink) = list {
                        device_c.lock().unwrap().default_source = sink.index;
                    }
                });
                match operation {
                    pulse::context::subscribe::Operation::New => {
                        match facility{
                            pulse::context::subscribe::Facility::Sink => {
                                let dclone = devices.clone();
                                introspector.get_sink_info_by_index(index, move |res|{
                                    let ListResult::Item(sink) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sinks.insert(Sink{
                                        name: sink.name.clone().map_or(String::from("Unknown"), |name| name.into_owned()),
                                        index,
                                        volume: sink.volume.avg().0,
                                        muted: sink.mute,
                                        monitor_index: sink.monitor_source,
                                        monitor_name: sink.monitor_source_name.clone().map_or(String::from("Unknown"), |name| name.into_owned()),
                                        state: State::from(sink.state)
                                    });
                                });
                            },
                            pulse::context::subscribe::Facility::Source => {
                                let dclone = devices.clone();
                                introspector.get_source_info_by_index(index, move |res|{
                                    let ListResult::Item(source) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sources.insert(Source{
                                        name: source.name.clone().map_or(String::from("Unknown"), |name| name.into_owned()),
                                        index,
                                        volume: source.volume.avg().0,
                                        muted: source.mute,
                                        monitor_index: source.monitor_of_sink,
                                        monitor_name: source.monitor_of_sink_name.clone().map(|name| name.into_owned()),
                                        state: State::from(source.state)
                                    });
                                });
                            },
                            _ => eprintln!("{facility:?} is not handled when inserted, This was not supposed to enabled also"),
                        };
                    },
                    pulse::context::subscribe::Operation::Changed => {
                        match facility{
                            pulse::context::subscribe::Facility::Sink => {
                                let dclone = devices.clone();
                                introspector.get_sink_info_by_index(index, move |res|{
                                    let ListResult::Item(sink) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sinks.replace(Sink{
                                        name: sink.name.clone().map_or(String::from("Unknown"), |name| name.into_owned()),
                                        index,
                                        volume: sink.volume.avg().0,
                                        muted: sink.mute,
                                        monitor_index: sink.monitor_source,
                                        monitor_name: sink.monitor_source_name.clone().map_or(String::from("Unknown"), |name| name.into_owned()),
                                        state: State::from(sink.state)
                                    });
                                });

                            },
                            pulse::context::subscribe::Facility::Source => {
                                let dclone = devices.clone();
                                introspector.get_source_info_by_index(index, move |res|{
                                    let ListResult::Item(source) = res else{ return};
                                    let mut dlock = dclone.lock().unwrap();
                                    dlock.sources.replace(Source{
                                        name: source.name.clone().map_or(String::from("Unknown"), |name| name.into_owned()),
                                        index,
                                        volume: source.volume.avg().0,
                                        muted: source.mute,
                                        monitor_index: source.monitor_of_sink,
                                        monitor_name: source.monitor_of_sink_name.clone().map(|name| name.into_owned()),
                                        state: State::from(source.state)
                                    });
                                });
                            },
                            _ => panic!("We are not expecting {facility:?}, this was supposed to be masked"),
                        }
                    },
                    pulse::context::subscribe::Operation::Removed => todo!(),
                }
                let dlock = devices.lock().unwrap();
                crate::print(&Some(std::ops::Deref::deref(&dlock)))
            })));
        match conn.mnlp.run() {
            Ok(_retval) => Ok(()),
            Err((e, _retval)) => Err(anyhow::Error::new(e)),
        }
    }

    #[allow(unused)]
    fn output(&self, conn: &mut Self::Connection) {}
}
