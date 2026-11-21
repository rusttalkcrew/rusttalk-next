//! "All system audio": every app's playback except RustTalk's own.
//!
//! A sink monitor would also carry RustTalk's playback (other people's voices and its
//! sounds), so the call would echo. Instead an unlinked capture stream gets a link from
//! each playback stream's ports, which PipeWire mixes in the stream's input ports. This
//! needs no virtual sink, and links from a client die with it, so nothing is left behind.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{self, Write},
    rc::Rc,
};

use anyhow::Result;
use pipewire::{
    self as pw,
    core::CoreRc,
    link::Link,
    main_loop::MainLoop,
    node::{Node, NodeChangeMask, NodeListener},
    properties::properties,
    stream::StreamBox,
    types::ObjectType,
};

use crate::linux::{FRAME_BYTES, PARENT, capture};

/// What the registry and the playback nodes' info say about the graph.
#[derive(Default)]
struct Graph {
    /// Playback streams by node ID.
    playback: HashMap<u32, Playback>,
    /// Each client's process ID, as the kernel reported it for its socket.
    clients: HashMap<u32, u32>,
    ports: HashMap<u32, Port>,
}

#[derive(Default)]
struct Playback {
    /// Set by the app; PulseAudio apps all share pipewire-pulse's client.
    pid: Option<u32>,
    client: Option<u32>,
    name: String,
}

struct Port {
    node: u32,
    output: bool,
    channel: String,
}

impl Graph {
    /// Another process's playback, never RustTalk's own (its voices and sounds).
    fn is_source(&self, playback: &Playback, parent: u32) -> bool {
        let pid = (playback.pid).or_else(|| self.clients.get(&playback.client?).copied());
        // cpal's PulseAudio streams carry RustTalk's process ID in their name too.
        pid != Some(parent) && playback.name != format!("cpal-pulseaudio-{parent}")
    }

    /// Every (output port, input port) pair that feeds a playback stream into `own`.
    fn wanted(&self, own: u32, parent: u32) -> HashSet<(u32, u32)> {
        let inputs: Vec<_> = (self.ports.iter())
            .filter(|(_, port)| port.node == own && !port.output)
            .collect();
        (self.ports.iter())
            .filter(|(_, port)| {
                port.output
                    && (self.playback.get(&port.node)).is_some_and(|p| self.is_source(p, parent))
            })
            .flat_map(|(&out, port)| {
                (inputs.iter())
                    .filter(|(_, input)| feeds(&port.channel, &input.channel))
                    .map(move |&(&input, _)| (out, input))
            })
            .collect()
    }
}

/// Whether a playback channel goes into one of our stereo inputs: left and right
/// channels to their side, mono, centre and the rest to both.
fn feeds(from: &str, to: &str) -> bool {
    match from.chars().last() {
        Some('L') => to == "FL",
        Some('R') => to == "FR",
        _ => true,
    }
}

fn number(value: Option<&str>) -> Option<u32> {
    value?.parse().ok()
}

pub fn run(loop_: &MainLoop, core: &CoreRc) -> Result<()> {
    let parent = *PARENT.get().expect("parent recorded at start");
    let graph = Rc::new(RefCell::new(Graph::default()));
    let nodes = Rc::new(RefCell::new(HashMap::<u32, (Node, NodeListener)>::new()));
    let registry = core.get_registry_rc()?;
    let weak = registry.downgrade();
    let _registry = registry
        .add_listener_local()
        .global({
            let graph = graph.clone();
            let nodes = nodes.clone();
            move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    ObjectType::Client => {
                        if let Some(pid) = number(props.get("pipewire.sec.pid")) {
                            graph.borrow_mut().clients.insert(global.id, pid);
                        }
                    }
                    ObjectType::Node
                        if props.get(*pw::keys::MEDIA_CLASS) == Some("Stream/Output/Audio") =>
                    {
                        // The registry leaves out the process: read the node's own info.
                        let Some(Ok(node)) = weak
                            .upgrade()
                            .map(|registry| registry.bind::<Node, _>(global))
                        else {
                            return;
                        };
                        let id = global.id;
                        let info_graph = graph.clone();
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                let Some(props) = info.props() else { return };
                                if !info.change_mask().contains(NodeChangeMask::PROPS) {
                                    return;
                                }
                                let name = props
                                    .get(*pw::keys::APP_NAME)
                                    .or_else(|| props.get(*pw::keys::NODE_NAME));
                                let playback = Playback {
                                    pid: number(props.get(*pw::keys::APP_PROCESS_ID)),
                                    client: number(props.get(*pw::keys::CLIENT_ID)),
                                    name: name.unwrap_or_default().into(),
                                };
                                info_graph.borrow_mut().playback.insert(id, playback);
                            })
                            .register();
                        nodes.borrow_mut().insert(id, (node, listener));
                    }
                    ObjectType::Port => {
                        let output = match props.get(*pw::keys::PORT_DIRECTION) {
                            Some("out") => true,
                            Some("in") => false,
                            _ => return,
                        };
                        // A sink's monitor ports are outputs too, but sinks are not playback.
                        if let Some(node) = number(props.get(*pw::keys::NODE_ID)) {
                            let channel = props.get(*pw::keys::AUDIO_CHANNEL).unwrap_or_default();
                            graph.borrow_mut().ports.insert(
                                global.id,
                                Port {
                                    node,
                                    output,
                                    channel: channel.into(),
                                },
                            );
                        }
                    }
                    _ => (),
                }
            }
        })
        .global_remove({
            let graph = graph.clone();
            let nodes = nodes.clone();
            move |id| {
                nodes.borrow_mut().remove(&id);
                let mut graph = graph.borrow_mut();
                graph.playback.remove(&id);
                graph.clients.remove(&id);
                graph.ports.remove(&id);
            }
        })
        .register();

    let stream = StreamBox::new(
        core,
        "RustTalk system audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            "node.autoconnect" => "false",
            // One port per channel to link to, even without a session manager.
            "adapter.auto-port-config" => "{ mode = dsp position = preserve }",
        },
    )?;
    let mut links: HashMap<(u32, u32), Link> = HashMap::new();
    let mut announced = false;
    capture(loop_, core, &stream, false, |stream| {
        let own = stream.node_id();
        if own == pw::constants::ID_ANY {
            return Ok(());
        }
        if !announced {
            // Nothing may be playing yet: one silent frame tells RustTalk the capture runs.
            io::stdout().lock().write_all(&[0; FRAME_BYTES])?;
            announced = true;
        }
        let wanted = graph.borrow().wanted(own, parent);
        // A removed port takes its link with it; drop our proxy for it too.
        links.retain(|pair, _| wanted.contains(pair));
        for (out, input) in wanted {
            if links.contains_key(&(out, input)) {
                continue;
            }
            let link = core.create_object::<Link>(
                "link-factory",
                &properties! {
                    *pw::keys::LINK_OUTPUT_PORT => out.to_string(),
                    *pw::keys::LINK_INPUT_PORT => input.to_string(),
                    *pw::keys::OBJECT_LINGER => "false",
                },
            );
            match link {
                Ok(link) => {
                    links.insert((out, input), link);
                }
                Err(error) => eprintln!("could not link a playback stream: {error}"),
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: u32 = 100;

    fn port(node: u32, output: bool, channel: &str) -> Port {
        Port {
            node,
            output,
            channel: channel.into(),
        }
    }

    fn playback(pid: Option<u32>, client: Option<u32>, name: &str) -> Playback {
        Playback {
            pid,
            client,
            name: name.into(),
        }
    }

    #[test]
    fn only_other_apps_playback_is_mixed_in() {
        let mut graph = Graph::default();
        graph.clients.insert(7, PARENT);
        graph.clients.insert(8, 300);
        let source = |p: Playback| graph.is_source(&p, PARENT);
        assert!(source(playback(Some(200), None, "Spotify")));
        assert!(source(playback(None, Some(8), "Firefox")));
        assert!(source(playback(None, None, "Unknown")));
        // RustTalk's own voices and sounds: by process, by client, or by cpal's name.
        assert!(!source(playback(Some(PARENT), None, "RustTalk")));
        assert!(!source(playback(None, Some(7), "RustTalk")));
        assert!(!source(playback(None, None, "cpal-pulseaudio-100")));
        assert!(source(playback(None, None, "cpal-pulseaudio-101")));
        // A PulseAudio app names its own process, though its client is pipewire-pulse's.
        assert!(source(playback(Some(200), Some(7), "Spotify")));
    }

    #[test]
    fn channels_go_to_their_side() {
        assert!(feeds("FL", "FL") && !feeds("FL", "FR"));
        assert!(feeds("FR", "FR") && !feeds("FR", "FL"));
        assert!(feeds("RL", "FL") && feeds("SR", "FR"));
        for both in ["MONO", "FC", "LFE", "AUX0", ""] {
            assert!(feeds(both, "FL") && feeds(both, "FR"), "{both}");
        }
    }

    #[test]
    fn links_each_playback_port_to_matching_inputs() {
        let own = 1;
        let mut graph = Graph::default();
        graph.ports.insert(10, port(own, false, "FL"));
        graph.ports.insert(11, port(own, false, "FR"));
        // A stereo app, a mono app, RustTalk's own playback and a sink.
        graph
            .playback
            .insert(2, playback(Some(200), None, "Spotify"));
        graph.playback.insert(3, playback(Some(300), None, "Game"));
        graph
            .playback
            .insert(4, playback(Some(PARENT), None, "RustTalk"));
        graph.ports.insert(20, port(2, true, "FL"));
        graph.ports.insert(21, port(2, true, "FR"));
        graph.ports.insert(30, port(3, true, "MONO"));
        graph.ports.insert(40, port(4, true, "FL"));
        graph.ports.insert(50, port(5, true, "FL"));
        // An input port of a playback node is never linked from.
        graph.ports.insert(22, port(2, false, "FL"));
        let expected = HashSet::from([(20, 10), (21, 11), (30, 10), (30, 11)]);
        assert_eq!(graph.wanted(own, PARENT), expected);
        // Nothing to link before our own ports exist.
        assert!(graph.wanted(6, PARENT).is_empty());
    }
}
