//! Non-RT user-session recovery. Never opens physical PCMs or restarts PipeWire.
use serde_json::Value;
use sidealsa_client::SideAlsaClient;
use std::{
    collections::HashSet, error::Error, path::PathBuf, process::Command, thread, time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn command(program: &str, args: &[String]) -> Result<Vec<u8>> {
    let output = Command::new("timeout")
        .args(["--kill-after=1s", "5s", program])
        .args(args)
        .output()?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line.starts_with("Error:"))
        || String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.starts_with("Error:"))
    {
        return Err(format!("{program}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(output.stdout)
}

fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

struct Graph(Vec<Value>);
impl Graph {
    fn read() -> Result<Self> {
        Ok(Self(serde_json::from_slice(&command("pw-dump", &[])?)?))
    }
    fn object(&self, id: u64) -> Option<&Value> {
        self.0.iter().find(|o| number(&o["id"]) == Some(id))
    }
    fn cookie(&self) -> Option<u64> {
        number(&self.object(0)?["info"]["cookie"])
    }
    fn identity(&self, id: u64) -> Option<Identity> {
        Some(Identity {
            id,
            serial: number(&self.object(id)?["info"]["props"]["object.serial"])?,
        })
    }
    fn current(&self, identity: Identity) -> bool {
        self.identity(identity.id) == Some(identity)
    }
    fn links(&self) -> impl Iterator<Item = &Value> {
        self.0
            .iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Link")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    id: u64,
    serial: u64,
}

#[derive(Clone, Debug)]
struct Link {
    identity: Identity,
    output: Identity,
    input: Identity,
    output_node: Identity,
    input_node: Identity,
    output_is_sidealsa: bool,
    input_is_sidealsa: bool,
    passive: bool,
    original_routes: Vec<(Identity, Identity)>,
}

fn plan(graph: &Graph, pcms: &HashSet<String>) -> Vec<Link> {
    let nodes: HashSet<u64> = graph
        .0
        .iter()
        .filter_map(|o| {
            let p = &o["info"]["props"];
            (o["type"] == "PipeWire:Interface:Node"
                && pcms.contains(p["api.alsa.path"].as_str()?)
                && p["device.api"] == "alsa")
                .then(|| number(&o["id"]))?
        })
        .collect();
    graph
        .links()
        .filter_map(|o| {
            let i = &o["info"];
            let output_node = graph.identity(number(&i["output-node-id"])?)?;
            let input_node = graph.identity(number(&i["input-node-id"])?)?;
            let output_is_sidealsa = nodes.contains(&output_node.id);
            let input_is_sidealsa = nodes.contains(&input_node.id);
            if !output_is_sidealsa && !input_is_sidealsa {
                return None;
            }
            Some(Link {
                identity: graph.identity(number(&o["id"])?)?,
                output: graph.identity(number(&i["output-port-id"])?)?,
                input: graph.identity(number(&i["input-port-id"])?)?,
                output_node,
                input_node,
                output_is_sidealsa,
                input_is_sidealsa,
                passive: i["props"]["link.passive"] == true || i["props"]["link.passive"] == "true",
                original_routes: graph
                    .links()
                    .filter_map(|other| {
                        Some((
                            graph.identity(number(&other["info"]["output-port-id"])?)?,
                            graph.identity(number(&other["info"]["input-port-id"])?)?,
                        ))
                    })
                    .collect(),
            })
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Restore {
    Create,
    AlreadyLinked,
    Obsolete,
}

fn restoration(graph: &Graph, cookie: u64, link: &Link) -> Restore {
    if graph.cookie() != Some(cookie)
        || ![link.output, link.input, link.output_node, link.input_node]
            .into_iter()
            .all(|id| graph.current(id))
    {
        return Restore::Obsolete;
    }
    for o in graph.links() {
        let i = &o["info"];
        let out = number(&i["output-port-id"]);
        let input = number(&i["input-port-id"]);
        if out == Some(link.output.id) && input == Some(link.input.id) {
            return Restore::AlreadyLinked;
        }
        // Do not undo a user's device switch or a session manager's new route.
        if (!link.output_is_sidealsa && out == Some(link.output.id))
            || (!link.input_is_sidealsa && input == Some(link.input.id))
        {
            // Preserve pre-existing fan-out; newly chosen routes supersede it.
            let old_route = out
                .zip(input)
                .and_then(|(out, input)| Some((graph.identity(out)?, graph.identity(input)?)));
            if old_route.is_some_and(|route| link.original_routes.contains(&route)) {
                continue;
            }
            return Restore::Obsolete;
        }
    }
    Restore::Create
}

fn restore_links(cookie: u64, pending: &mut Vec<Link>) -> Result<()> {
    let mut remaining = Vec::new();
    for link in pending.iter() {
        let graph = Graph::read()?;
        match restoration(&graph, cookie, link) {
            Restore::Create => {
                let mut args = Vec::new();
                if link.passive {
                    args.push("--passive".into());
                }
                args.extend([link.output.id.to_string(), link.input.id.to_string()]);
                if let Err(e) = command("pw-link", &args) {
                    eprintln!("link restore pending: {e}");
                    remaining.push(link.clone());
                }
            }
            Restore::AlreadyLinked | Restore::Obsolete => {}
        }
    }
    *pending = remaining;
    Ok(())
}

fn recover(pcms: &HashSet<String>) -> Result<()> {
    let graph = Graph::read()?;
    let cookie = graph.cookie().ok_or("PipeWire core cookie unavailable")?;
    let mut removed = Vec::new();
    for link in plan(&graph, pcms) {
        // Check identity again before destroying anything: global IDs are reused.
        let fresh = match Graph::read() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("cannot continue unlinking: {e}");
                break;
            }
        };
        if fresh.cookie() != Some(cookie) {
            break;
        }
        if fresh.current(link.identity) && fresh.current(link.output) && fresh.current(link.input) {
            match command("pw-cli", &["destroy".into(), link.identity.id.to_string()]) {
                Ok(_) => removed.push(link),
                Err(e) => eprintln!("unlink skipped: {e}"),
            }
        }
    }
    let count = removed.len();
    // Let the adapter become idle and close its disconnected ALSA handle.
    thread::sleep(Duration::from_secs(1));
    for _ in 0..5 {
        if removed.is_empty() {
            break;
        }
        if let Err(e) = restore_links(cookie, &mut removed) {
            eprintln!("restore retry: {e}");
        }
        if !removed.is_empty() {
            thread::sleep(Duration::from_millis(500));
        }
    }
    if !removed.is_empty() {
        return Err(format!(
            "{} links could not be restored; reselect the affected device",
            removed.len()
        )
        .into());
    }
    println!("Refreshed {count} SideALSA links; PipeWire and application processes left running.");
    Ok(())
}

fn run() -> Result<()> {
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");
    let mut once = false;
    let mut initial_refresh = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().ok_or("--socket requires a path")?.into(),
            "--once" => once = true,
            "--initial-refresh" => initial_refresh = true,
            "--help" | "-h" => {
                println!(
                    "sidealsa-reconnect [--socket PATH] [--once] [--initial-refresh]\nWatch daemon reconnections and refresh only matching SHARED PipeWire links.\n--once refreshes immediately and exits; --initial-refresh also refreshes on watcher startup.\nRequires pw-dump, pw-cli, pw-link and timeout. Run as the desktop user."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option: {arg}").into()),
        }
    }
    let mut observed = initial_refresh;
    loop {
        let ready = (|| -> Result<_> {
            let mut client = SideAlsaClient::connect(&socket)?;
            let info = client.get_info()?;
            if client.get_stats()?.periods_processed == 0 {
                return Err("hardware not ready".into());
            }
            let pcms = info
                .playback_ports
                .iter()
                .chain(info.capture_ports.iter())
                .map(|p| format!("sidealsa_{}", p.id))
                .collect::<HashSet<_>>();
            Ok((client, pcms))
        })();
        let (mut client, pcms) = match ready {
            Ok(r) => r,
            Err(e) if once => return Err(e),
            Err(_) => {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if once || observed {
            recover(&pcms)?;
        }
        if once {
            return Ok(());
        }
        observed = true;
        println!("Watching SideALSA daemon PID {}", client.peer_pid()?);
        loop {
            thread::sleep(Duration::from_millis(500));
            if client.get_stats().is_err() {
                break;
            }
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("sidealsa-reconnect: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn graph() -> Graph {
        Graph(vec![
            json!({"id":0,"info":{"cookie":123}}),
            json!({"id":1,"type":"PipeWire:Interface:Node","info":{"props":{"object.serial":11,"device.api":"alsa","api.alsa.path":"sidealsa_line1"}}}),
            json!({"id":2,"type":"PipeWire:Interface:Node","info":{"props":{"object.serial":12}}}),
            json!({"id":3,"info":{"props":{"object.serial":13}}}),
            json!({"id":4,"info":{"props":{"object.serial":14}}}),
            json!({"id":5,"type":"PipeWire:Interface:Link","info":{"props":{"object.serial":15},"output-node-id":2,"input-node-id":1,"output-port-id":3,"input-port-id":4}}),
        ])
    }
    #[test]
    fn only_selected_alsa_pcm_links_are_planned() {
        assert!(plan(&graph(), &HashSet::from(["sidealsa_other".into()])).is_empty());
        let links = plan(&graph(), &HashSet::from(["sidealsa_line1".into()]));
        assert_eq!(links.len(), 1);
        assert!(!links[0].output_is_sidealsa);
        assert!(links[0].input_is_sidealsa);
    }
    #[test]
    fn restore_respects_current_routes_and_reused_ids() {
        let mut graph = graph();
        let link = plan(&graph, &HashSet::from(["sidealsa_line1".into()])).remove(0);
        assert_eq!(restoration(&graph, 123, &link), Restore::AlreadyLinked);
        graph.0.pop();
        assert_eq!(restoration(&graph, 123, &link), Restore::Create);
        assert_eq!(restoration(&graph, 124, &link), Restore::Obsolete);
        graph.0[3]["info"]["props"]["object.serial"] = json!(100);
        assert_eq!(restoration(&graph, 123, &link), Restore::Obsolete);
        graph.0[3]["info"]["props"]["object.serial"] = json!(13);
        graph.0.push(json!({"type":"PipeWire:Interface:Link","info":{"output-port-id":3,"input-port-id":99}}));
        assert_eq!(restoration(&graph, 123, &link), Restore::Obsolete);
    }

    #[test]
    fn capture_retarget_and_missing_ports_are_not_reconnected() {
        let mut graph = graph();
        graph.0[5]["info"]["output-node-id"] = json!(1);
        graph.0[5]["info"]["input-node-id"] = json!(2);
        let link = plan(&graph, &HashSet::from(["sidealsa_line1".into()])).remove(0);
        assert!(link.output_is_sidealsa);
        graph.0.pop();
        assert_eq!(restoration(&graph, 123, &link), Restore::Create);
        graph.0.push(json!({"type":"PipeWire:Interface:Link","info":{"output-port-id":99,"input-port-id":4}}));
        assert_eq!(restoration(&graph, 123, &link), Restore::Obsolete);
        graph.0.pop();
        graph.0.retain(|o| o["id"] != 4);
        assert_eq!(restoration(&graph, 123, &link), Restore::Obsolete);
    }

    #[test]
    fn original_fanout_does_not_suppress_restoration() {
        let mut graph = graph();
        graph
            .0
            .push(json!({"id":6,"info":{"props":{"object.serial":16}}}));
        graph.0.push(
            json!({"type":"PipeWire:Interface:Link","info":{"output-port-id":3,"input-port-id":6}}),
        );
        let link = plan(&graph, &HashSet::from(["sidealsa_line1".into()])).remove(0);
        graph.0.retain(|o| o["id"] != 5);
        assert_eq!(restoration(&graph, 123, &link), Restore::Create);
    }
}
