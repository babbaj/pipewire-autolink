use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::ops::Deref;
use std::process::exit;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender};
use std::thread;
use clap::{Arg, Command};

use pipewire as pw;
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire_sys as sys;
// spa_interface_call_method! needs this
use libspa_sys as spa_sys;
use pipewire::proxy::ProxyT;
use pipewire::registry::{GlobalObject, Registry};
use pipewire::spa::{ForeignDict, ParsableValue};

#[derive(Clone)]
struct LinkCfg {
    // TODO: allow single node
    input: String,
    output: String,
    replace: bool // disconnect other links
}

#[derive(Debug)]
struct Link {
    port_in: u32,
    port_out: u32,
    node_in: u32,
    node_out: u32
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum Direction {
    IN,
    OUT
}
#[derive(Debug, Clone)]
struct Port {
    id: u32,
    node: u32,
    channel: String,
    direction: Direction
}

#[derive(Debug)]
enum Type {
    Node(String),
    Link(Link),
    Port(Port)
}

#[derive(Debug)]
struct Object {
    id: u32,
    thing: Type
}

#[derive(Debug)]
enum Event<'a> {
    Create(&'a GlobalObject<ForeignDict>, Object),
    Delete(u32)
}

fn parse_cmdline() -> Vec<LinkCfg> {
    let link = Arg::new("link")
        .long("link")
        .help("Comma separated pair of node names (input,output)");
    let override_link = Arg::new("link-override")
        .long("link-override")
        .help("links but existing links between ports will be disconnected");
    let matches = Command::new("pw-autolink")
        .arg(link)
        .arg(override_link)
        .get_matches();

    return matches.get_many::<String>("link").unwrap_or(Default::default()).map(|s| (s, false))
        .chain(matches.get_many::<String>("link-override").unwrap_or(Default::default()).map(|s| (s, true)))
        .map(|(s, b)| (s.split_once(',').expect("link arguments must be comma separated pairs"), b))
        .map(|((a, b), replace)| LinkCfg{input: a.to_owned(), output: b.to_owned(), replace})
        .collect()
}

#[derive(Debug)]
struct NodeData {
    name: String,
    id: u32,
    ports: Vec<Port>
}

fn create_link(core: &pw::Core, pin: &Port, pout: &Port) -> pw::link::Link {
    let link = core
        .create_object::<pw::link::Link, _>(
            "link-factory", // TODO: find the link factory the same way the example does
            &pw::properties! {
                "link.output.port" => pout.id.to_string(),
                "link.input.port" => pin.id.to_string(),
                "link.output.node" => pout.node.to_string(),
                "link.input.node" => pin.node.to_string(),
                // Don't remove the object on the remote when we destroy our proxy.
                "object.linger" => "1"
            },
        )
        .expect("Failed to create object");
    /*let listener = link.add_listener_local()
        .info(|info| {
            println!("linkinfo {:?}", info);
        })
        .register();*/
    return link;
}

fn main() {
    let cfg = parse_cmdline();
    if cfg.is_empty() {
        println!("No link arguments given");
        //exit(1);
    }
    let cfg_by_name = cfg.iter()
        .flat_map(|cfg| [
            (cfg.input.clone(), (cfg.clone(), cfg.output.clone())),
            (cfg.output.clone(), (cfg.clone(), cfg.input.clone()))
        ])
        .collect::<HashMap<String, (LinkCfg, String)>>();

    listener_thread(cfg_by_name);
}

#[derive(Default)]
struct State {
    relevant_nodes: HashMap<u32, NodeData>,
    node_by_name: HashMap<String, u32>,
    created_links:  HashSet<u32>, // this should be a vec tbh
    temp_links: Vec<(pw::link::Link, pw::link::LinkListener)>
}

fn on_event(core: &pw::Core, registry: &Registry, state_rc: &Rc<RefCell<State>>, cfg_by_name: &HashMap<String, (LinkCfg, String)>, event: Event) {
    println!("{:?}", &event);
    let mut state = state_rc.borrow_mut();

    match event {
        Event::Create(global, obj) => match obj.thing {
            Type::Node(name) => {
                if cfg_by_name.contains_key(name.as_str()) {
                    let name_copy = name.clone();
                    state.relevant_nodes.insert(obj.id, NodeData { name, id: obj.id, ports: Vec::new() });
                    state.node_by_name.insert(name_copy, obj.id);
                }
            },
            Type::Port(port) => {
                // borrow checker reeee
                let mut push = false;
                if let Some(parent) = state.relevant_nodes.get(&port.node) {
                    push = true;
                    let (cfg, other_name) = cfg_by_name.get(parent.name.as_str()).unwrap();
                    // check if the direction of this port is the direction that we configured for
                    if port.direction == Direction::IN && cfg.input != parent.name {
                        return;
                    }
                    if port.direction == Direction::OUT && cfg.output != parent.name {
                        return;
                    }

                    if let Some(other_node) = state.node_by_name.get(other_name) {
                        if let Some(other_port) = state.relevant_nodes.get(other_node).unwrap().ports.iter().find(|p| p.channel == port.channel && p.direction != port.direction)
                        {
                            if port.direction != other_port.direction {
                                println!("trying to create link");
                                let link = if port.direction == Direction::IN {
                                    create_link(core, &port, other_port)
                                } else {
                                    create_link(core, other_port, &port)
                                };
                                let local_id = link.upcast_ref().id();
                                let state_copy = state_rc.clone();
                                let listener = link.add_listener_local()
                                    .info(move |info| {
                                        let mut state = state_copy.borrow_mut();
                                        state.created_links.insert(info.id());
                                        state.temp_links.retain(|(l, _)| l.upcast_ref().id() != local_id);
                                    })
                                    .register();
                                state.temp_links.push((link, listener));
                            }
                        }
                    }
                }

                if push {
                    state.relevant_nodes.get_mut(&port.node).unwrap().ports.push(port);
                }
            },
            Type::Link(link) => {
                if state.created_links.contains(&obj.id) {
                    return;
                }
                if let Some(node_in) = state.relevant_nodes.get(&link.node_in) {
                    if let Some((cfg, other_name)) = cfg_by_name.get(node_in.name.as_str()) {
                        if cfg.replace && node_in.name == cfg.input {
                            println!("trying to delete {}", global.id);
                            registry.destroy_global(global.id);
                       }
                    }
                }
            }
        },
        Event::Delete(id) => {
            if let Some(data) = state.relevant_nodes.remove(&id) {
                state.node_by_name.remove(&data.name);
            }
            state.created_links.remove(&id);
        }
    }
}

fn get_props<'a, const N: usize>(dict: &'a ForeignDict, keys: [&str; N]) -> Option<[&'a str; N]> {
    let opts = keys.map(|k| dict.get(k));
    return unwrap_arr(opts);
}

fn unwrap_arr<const N: usize, T>(arr: [Option<T>; N]) -> Option<[T; N]> {
    if arr.iter().any(|x| x.is_none()) {
        return None;
    }
    return Some(arr.map(|x| unsafe { x.unwrap_unchecked() }));
}

fn listener_thread(cfg: HashMap<String, (LinkCfg, String)>) {
    let mainloop = pw::MainLoop::new().expect("Failed to create MainLoop for listener thread");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");
    let registry = Rc::new(core.get_registry().expect("Failed to get Registry"));
    let state = Rc::new(RefCell::new(State::default()));
    let state2 = state.clone();

    let registry2 = registry.clone();
    let registry3 = registry.clone();
    let core2 = core.clone();
    //let vec_copy = out.clone();
    let cfg1 = Arc::new(cfg);
    let cfg2 = cfg1.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.props.is_none() { return }
            let props = global.props.as_ref().unwrap();
            let id = global.id;
            match global.type_ {
                ObjectType::Node => {
                    if let Some(name) = props.get("node.name") {
                        let obj = Object{id, thing: Type::Node(name.to_owned())};
                        on_event(&core, &registry2, &state, &cfg1, Event::Create(global, obj));
                    }
                },
                ObjectType::Port => {
                    if let Some([node_id, channel, dir_str]) = get_props(props, ["node.id", "audio.channel", "port.direction"]) {
                        if let Some(node) = u32::parse_value(node_id) {
                            let direction = if dir_str == "in" { Direction::IN } else { Direction::OUT };
                            let obj = Object{id, thing: Type::Port(Port{id, node, channel: channel.to_owned(), direction})};
                            on_event(&core, &registry2, &state, &cfg1, Event::Create(global, obj));
                        }
                    }
                },
                ObjectType::Link => {
                    if let Some(vals) = get_props(props, ["link.output.port", "link.input.port", "link.output.node", "link.input.node"]) {
                        let ids = vals.map(u32::parse_value);
                        if let Some([pout, pin, nout, nin]) = unwrap_arr(ids) {
                            let obj = Object{ id, thing: Type::Link(Link{port_in: pin, port_out: pout, node_in: nin, node_out: nout})};
                            on_event(&core, &registry2,  &state, &cfg1, Event::Create(global, obj));
                        }
                    }
                },
                _ => {}
            }
        })
        .global_remove(move |id| {
            on_event(&core2, &registry3, &state2, &cfg2, Event::Delete(id));
        })
        .register();

    mainloop.run();
}
