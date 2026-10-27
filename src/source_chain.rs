//! Resolve the physical microphone behind the source stentor records from.
//!
//! PulseAudio reports the description of the source node only. When that node
//! is virtual, the description names the processing stage (EasyEffects) and
//! hides the hardware behind it. That relationship lives in the PipeWire link
//! graph, which PulseAudio does not expose.
//!
//! The graph is read with `pw-dump` instead of the `pipewire` crate: this runs
//! once per recording session, off the capture path, and does not justify a
//! second mainloop and registry mirror inside the daemon.

use serde::Deserialize;
use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Longest `pw-dump` may take. A wedged server must not leak the caller's
/// thread, and the dialog label is not worth waiting longer for.
const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the child is reaped while waiting for `pw-dump`.
const PW_DUMP_POLL: Duration = Duration::from_millis(10);

/// Maximum number of nodes walked upstream. EasyEffects inserts one node per
/// plugin plus a spectrum and a level meter, so five to eight hops is normal;
/// the bound only stops a malformed graph.
const MAX_HOPS: usize = 32;

/// What the graph says about the source stentor records from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceChain {
    /// Description of the physical microphone at the head of the chain, when
    /// the source is virtual and its input could be followed.
    pub physical: Option<String>,
    /// Whether the source node itself is a virtual microphone.
    pub source_is_virtual: bool,
}

/// Follow the link graph upstream from `source_name`.
///
/// Returns `None` when the graph is unavailable or the source is not in it, so
/// callers keep the PulseAudio description.
pub fn resolve(source_name: &str) -> Option<SourceChain> {
    let objects = dump_objects()?;
    walk(&objects, source_name)
}

/// Run `pw-dump` and parse it, giving up if it does not finish in time.
fn dump_objects() -> Option<Vec<PwObject>> {
    let mut child = Command::new("pw-dump")
        .arg("--no-colors")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|error| tracing::debug!(%error, "could not run pw-dump"))
        .ok()?;

    // A dump is far larger than a pipe buffer, so it must be drained while the
    // child runs; waiting for exit first would deadlock on a full pipe.
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        stdout
            .read_to_end(&mut buffer)
            .inspect_err(|error| tracing::debug!(%error, "could not read pw-dump output"))
            .ok()
            .map(|_| buffer)
    });

    let deadline = Instant::now() + PW_DUMP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::debug!(%status, "pw-dump failed");
                    let _ = reader.join();
                    return None;
                }
                break;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(PW_DUMP_POLL),
            Ok(None) => {
                tracing::debug!("pw-dump timed out");
                let _ = child.kill();
                let _ = child.wait();
                // The reader is left to finish when the pipe closes: waiting
                // for it here would transfer any stall to the caller.
                return None;
            }
            Err(error) => {
                tracing::debug!(%error, "could not wait for pw-dump");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    let buffer = reader.join().ok()??;
    serde_json::from_slice(&buffer)
        .inspect_err(|error| tracing::debug!(%error, "could not parse pw-dump output"))
        .ok()
}

/// The subset of a `pw-dump` object this module needs.
#[derive(Debug, Clone, Deserialize)]
struct PwObject {
    id: u32,
    #[serde(rename = "type")]
    object_type: String,
    info: Option<PwInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PwInfo {
    #[serde(default)]
    props: Props,
    #[serde(rename = "output-node-id")]
    output_node_id: Option<u32>,
    #[serde(rename = "input-node-id")]
    input_node_id: Option<u32>,
    state: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Props {
    #[serde(rename = "node.name")]
    node_name: Option<String>,
    #[serde(rename = "node.description")]
    node_description: Option<String>,
    #[serde(rename = "device.description")]
    device_description: Option<String>,
    #[serde(rename = "media.class")]
    media_class: Option<String>,
    #[serde(rename = "node.virtual")]
    node_virtual: Option<serde_json::Value>,
}

impl PwObject {
    fn is_node(&self) -> bool {
        self.object_type == "PipeWire:Interface:Node"
    }

    fn is_link(&self) -> bool {
        self.object_type == "PipeWire:Interface:Link"
    }

    fn props(&self) -> Option<&Props> {
        self.info.as_ref().map(|info| &info.props)
    }

    fn node_name(&self) -> Option<&str> {
        self.props()?.node_name.as_deref()
    }

    fn media_class(&self) -> Option<&str> {
        self.props()?.media_class.as_deref()
    }

    fn description(&self) -> Option<&str> {
        let props = self.props()?;
        props
            .node_description
            .as_deref()
            .or(props.device_description.as_deref())
            .or(props.node_name.as_deref())
    }

    fn state(&self) -> Option<&str> {
        self.info.as_ref()?.state.as_deref()
    }

    /// Virtual nodes are marked either by their media class or by the
    /// `node.virtual` property, which is a string in some PipeWire versions and
    /// a boolean in others.
    fn is_virtual(&self) -> bool {
        if self
            .media_class()
            .is_some_and(|class| class.contains("/Virtual"))
        {
            return true;
        }

        match self.props().and_then(|props| props.node_virtual.as_ref()) {
            Some(serde_json::Value::Bool(value)) => *value,
            Some(serde_json::Value::String(value)) => value == "true",
            _ => false,
        }
    }

    /// Monitor sources replay a sink's output. PipeWire names them with a
    /// `.monitor` suffix, so this is a naming convention rather than a
    /// property check.
    fn is_monitor(&self) -> bool {
        self.node_name()
            .is_some_and(|name| name.ends_with(".monitor"))
    }

    /// A real capture device, as opposed to a virtual node or a monitor.
    fn is_hardware_source(&self) -> bool {
        self.media_class() == Some("Audio/Source") && !self.is_virtual() && !self.is_monitor()
    }
}

fn walk(objects: &[PwObject], source_name: &str) -> Option<SourceChain> {
    let source = objects
        .iter()
        .find(|object| object.is_node() && object.node_name() == Some(source_name))?;

    if !source.is_virtual() {
        return Some(SourceChain {
            physical: None,
            source_is_virtual: false,
        });
    }

    // The virtual source's links to a client sit downstream, so following
    // inputs only walks towards the hardware.
    let mut current_id = source.id;
    let mut visited = HashSet::from([current_id]);

    for _ in 0..MAX_HOPS {
        let Some(upstream) = upstream_node(objects, current_id, &visited) else {
            break;
        };
        if !visited.insert(upstream.id) {
            break;
        }
        if upstream.is_hardware_source() {
            return Some(SourceChain {
                physical: upstream.description().map(str::to_owned),
                source_is_virtual: true,
            });
        }
        current_id = upstream.id;
    }

    Some(SourceChain {
        physical: None,
        source_is_virtual: true,
    })
}

/// The node feeding `node_id`, preferring a hardware source over a monitor and
/// then over anything else.
fn upstream_node<'a>(
    objects: &'a [PwObject],
    node_id: u32,
    visited: &HashSet<u32>,
) -> Option<&'a PwObject> {
    let mut candidates: Vec<(u8, &PwObject)> = objects
        .iter()
        .filter(|object| object.is_link() && object.state() != Some("error"))
        .filter(|object| {
            object
                .info
                .as_ref()
                .and_then(|info| info.input_node_id)
                .is_some_and(|input| input == node_id)
        })
        .filter_map(|object| {
            let output_id = object.info.as_ref()?.output_node_id?;
            objects
                .iter()
                .find(|object| object.is_node() && object.id == output_id)
                .map(|node| (link_rank(object), node))
        })
        .filter(|(_, node)| !visited.contains(&node.id))
        .collect();

    // A stereo link contributes one edge per channel, so collapse per node
    // while keeping the most usable link state, then order by preference.
    candidates.sort_by_key(|(rank, node)| (node.id, *rank));
    candidates.dedup_by_key(|(_, node)| node.id);
    candidates.sort_by_key(|(rank, node)| {
        (
            !node.is_hardware_source(),
            node.is_monitor(),
            *rank,
            node.id,
        )
    });

    candidates.first().map(|(_, node)| *node)
}

/// Links that carry audio now are better evidence than paused ones, which can
/// outlive the audio they described.
fn link_rank(object: &PwObject) -> u8 {
    u8::from(object.state() != Some("active"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::{Value, json};

    const NODE: &str = "PipeWire:Interface:Node";
    const LINK: &str = "PipeWire:Interface:Link";

    fn graph(json: &str) -> Result<Vec<PwObject>> {
        Ok(serde_json::from_str(json)?)
    }

    fn parse(objects: Vec<Value>) -> Result<Vec<PwObject>> {
        Ok(serde_json::from_value(Value::Array(objects))?)
    }

    #[test]
    fn follows_a_virtual_source_to_its_microphone() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 58, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}},
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "node.description": "Easy Effects Source",
                "media.class": "Audio/Source/Virtual",
                "node.virtual": true}}},
              {"id": 181, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "ee_sie_echo_canceller"}}},
              {"id": 180, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "parec",
                "media.class": "Stream/Input/Audio"}}},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 58, "input-node-id": 181, "state": "active"}},
              {"id": 2, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 181, "input-node-id": 93, "state": "active"}},
              {"id": 3, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 93, "input-node-id": 180, "state": "active"}}
            ]"#,
        )?;

        let chain = walk(&objects, "easyeffects_source");

        assert_eq!(
            chain,
            Some(SourceChain {
                physical: Some("USB Condenser Microphone Mono".to_owned()),
                source_is_virtual: true,
            })
        );
        Ok(())
    }

    #[test]
    fn a_hardware_source_needs_no_walk() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 58, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}}
            ]"#,
        )?;

        assert_eq!(
            walk(&objects, "alsa_input.usb-mic"),
            Some(SourceChain {
                physical: None,
                source_is_virtual: false,
            })
        );
        Ok(())
    }

    #[test]
    fn an_unlinked_virtual_source_has_no_physical_device() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "node.description": "Easy Effects Source",
                "media.class": "Audio/Source/Virtual"}}}
            ]"#,
        )?;

        assert_eq!(
            walk(&objects, "easyeffects_source"),
            Some(SourceChain {
                physical: None,
                source_is_virtual: true,
            })
        );
        Ok(())
    }

    #[test]
    fn an_unknown_source_is_not_resolved() -> Result<()> {
        let objects = graph(r#"[]"#)?;

        assert_eq!(walk(&objects, "easyeffects_source"), None);
        Ok(())
    }

    #[test]
    fn a_link_cycle_terminates() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source/Virtual"}}},
              {"id": 200, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "ee_loop_a"}}},
              {"id": 201, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "ee_loop_b"}}},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 200, "input-node-id": 93}},
              {"id": 2, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 201, "input-node-id": 200}},
              {"id": 3, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 200, "input-node-id": 201}}
            ]"#,
        )?;

        assert_eq!(
            walk(&objects, "easyeffects_source"),
            Some(SourceChain {
                physical: None,
                source_is_virtual: true,
            })
        );
        Ok(())
    }

    #[test]
    fn a_hardware_source_wins_over_a_monitor() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source/Virtual"}}},
              {"id": 58, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}},
              {"id": 62, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_output.pci.analog-stereo.monitor",
                "node.description": "Monitor of Analog Stereo",
                "media.class": "Audio/Source"}}},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 62, "input-node-id": 93}},
              {"id": 2, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 58, "input-node-id": 93}}
            ]"#,
        )?;

        let chain = walk(&objects, "easyeffects_source");

        assert_eq!(
            chain.and_then(|chain| chain.physical),
            Some("USB Condenser Microphone Mono".to_owned())
        );
        Ok(())
    }

    #[test]
    fn stereo_links_do_not_duplicate_candidates() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source/Virtual"}}},
              {"id": 58, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 58, "input-node-id": 93}},
              {"id": 2, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 58, "input-node-id": 93}}
            ]"#,
        )?;

        let chain = walk(&objects, "easyeffects_source");

        assert_eq!(
            chain.and_then(|chain| chain.physical),
            Some("USB Condenser Microphone Mono".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_long_plugin_chain_still_resolves() -> Result<()> {
        let mut objects = vec![
            json!({"id": 1, "type": NODE, "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source/Virtual"}}}),
            json!({"id": 2, "type": NODE, "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}}),
        ];

        let hops = 12;
        let first_plugin = 100;
        let mut previous = 2;
        for offset in 0..hops {
            let plugin = first_plugin + offset;
            objects.push(json!({"id": plugin, "type": NODE, "info": {"props": {
                "node.name": format!("ee_plugin_{offset}")}}}));
            objects.push(json!({"id": 1000 + offset, "type": LINK, "info": {
                "output-node-id": previous, "input-node-id": plugin, "state": "active"}}));
            previous = plugin;
        }
        objects.push(json!({"id": 2000, "type": LINK, "info": {
            "output-node-id": previous, "input-node-id": 1, "state": "active"}}));

        let chain = walk(&parse(objects)?, "easyeffects_source");

        assert_eq!(
            chain.and_then(|chain| chain.physical),
            Some("USB Condenser Microphone Mono".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_string_virtual_property_is_understood() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 58, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}},
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source",
                "node.virtual": "true"}}},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 58, "input-node-id": 93}}
            ]"#,
        )?;

        let chain = walk(&objects, "easyeffects_source");

        assert_eq!(
            chain.and_then(|chain| chain.physical),
            Some("USB Condenser Microphone Mono".to_owned())
        );
        Ok(())
    }

    #[test]
    fn objects_without_info_and_dangling_links_are_ignored() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source/Virtual"}}},
              {"id": 94, "type": "PipeWire:Interface:Node", "info": null},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 4096, "input-node-id": 93}},
              {"id": 2, "type": "PipeWire:Interface:Link", "info": null}
            ]"#,
        )?;

        assert_eq!(
            walk(&objects, "easyeffects_source"),
            Some(SourceChain {
                physical: None,
                source_is_virtual: true,
            })
        );
        Ok(())
    }

    #[test]
    fn an_active_link_beats_a_paused_one() -> Result<()> {
        let objects = graph(
            r#"[
              {"id": 93, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "easyeffects_source",
                "media.class": "Audio/Source/Virtual"}}},
              {"id": 58, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.pci-analog-stereo",
                "node.description": "Ryzen HD Audio Controller Analog Stereo",
                "media.class": "Audio/Source"}}},
              {"id": 59, "type": "PipeWire:Interface:Node", "info": {"props": {
                "node.name": "alsa_input.usb-mic",
                "node.description": "USB Condenser Microphone Mono",
                "media.class": "Audio/Source"}}},
              {"id": 1, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 58, "input-node-id": 93, "state": "paused"}},
              {"id": 2, "type": "PipeWire:Interface:Link", "info": {
                "output-node-id": 59, "input-node-id": 93, "state": "active"}}
            ]"#,
        )?;

        let chain = walk(&objects, "easyeffects_source");

        assert_eq!(
            chain.and_then(|chain| chain.physical),
            Some("USB Condenser Microphone Mono".to_owned())
        );
        Ok(())
    }
}
