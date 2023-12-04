use std::cell::{RefCell};
use std::collections::{HashMap, HashSet};
use std::process::exit;
use std::rc::Rc;
use clap::{Arg, ArgAction, Command};

use pipewire as pw;
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire::proxy::ProxyT;
use pipewire::registry::{GlobalObject, Registry};
use pipewire::spa::{ForeignDict, ParsableValue};

#[derive(Debug, Default)]
struct ConfigCache {
    connect: HashMap<String, (String, Direction)>,
    delete_in: HashSet<String>,
    delete_out: HashSet<String>,
    all_names: HashSet<String>
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

fn parse_cmdline() -> (Command, ConfigCache) {
    let link = Arg::new("connect")
        .long("connect")
        .action(ArgAction::Append)
        .num_args(2)
        .value_names(["output", "input"])
        .help("Link a node's output ports to another node's input ports");
    let delete_in = Arg::new("delete-in")
        .long("delete-in")
        .value_name("node.name")
        .help("Delete links from this node's input (links created by autolink are not deleted)");
    let delete_out = Arg::new("delete-out")
        .long("delete-out")
        .value_name("node.name")
        .help("Like delete-in but deletes the output");
    let cmd = Command::new("pw-autolink")
        .arg(link)
        .arg(delete_in)
        .arg(delete_out);
    let cmd_clone = cmd.clone();
    let matches = cmd.get_matches();
    let connects = matches.get_occurrences::<String>("connect").unwrap_or_default()
        .map(|mut it| (it.next().unwrap(), it.next().unwrap()));

    let mut config = ConfigCache {
        connect: connects.flat_map(|(output, input)| [
              (output.clone(), (input.clone(), Direction::IN)),
              (input.clone(), (output.clone(), Direction::OUT))
          ])
          .collect(),
        delete_in: matches.get_many::<String>("delete-in").unwrap_or_default().cloned().collect(),
        delete_out: matches.get_many::<String>("delete-out").unwrap_or_default().cloned().collect(),
        all_names: Default::default()
    };
    config.connect.keys().chain(config.delete_in.iter()).chain(config.delete_out.iter()).for_each(|name| {
        config.all_names.insert(name.clone());
    });

    return (cmd_clone, config);
}

#[derive(Debug)]
struct NodeData {
    name: String,
    id: u32,
    ports: Vec<Port>
}

fn create_link(core: &pw::Core, pin: &Port, pout: &Port) -> pw::link::Link {
    return core.create_object::<pw::link::Link, _>(
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
}

fn main() {
    let (mut command, config) = parse_cmdline();
    if config.all_names.is_empty() {
        command.print_long_help().unwrap();
        exit(1);
    }

    listener_thread(config);
}

#[derive(Default)]
struct State {
    relevant_nodes: HashMap<u32, NodeData>,
    node_by_name: HashMap<String, u32>,
    created_links:  HashSet<u32>, // this should be a vec tbh
    temp_links: Vec<(pw::link::Link, pw::link::LinkListener)>
}

fn on_event(core: &pw::Core, registry: &Registry, state_rc: &Rc<RefCell<State>>, cfg_by_name: &ConfigCache, event: Event) {
    //println!("{:?}", &event);
    let mut state = state_rc.borrow_mut();

    match event {
        Event::Create(global, obj) => match obj.thing {
            Type::Node(name) => {
                if cfg_by_name.all_names.contains(name.as_str()) {
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
                    if let Some((other_name, other_dir)) = cfg_by_name.connect.get(parent.name.as_str()) {
                        // check if the direction of this port is the direction that we configured for
                        if port.direction == *other_dir {
                            return;
                        }

                        if let Some(other_node) = state.node_by_name.get(other_name) {
                            if let Some(other_port) = state.relevant_nodes.get(other_node).unwrap().ports.iter()
                                .find(|p| p.channel == port.channel && p.direction != port.direction)
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
                    if cfg_by_name.delete_in.contains(node_in.name.as_str()) {
                        //println!("trying to delete {}", global.id);
                        registry.destroy_global(global.id);
                    }
                }
                if let Some(node_out) = state.relevant_nodes.get(&link.node_out) {
                    if cfg_by_name.delete_out.contains(node_out.name.as_str()) {
                        //println!("trying to delete {}", global.id);
                        registry.destroy_global(global.id);
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

fn listener_thread(cfg: ConfigCache) {
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
    let cfg1 = Rc::new(cfg);
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
