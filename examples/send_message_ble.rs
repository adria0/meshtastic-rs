//! Example: send_message_ble.rs
//!
//! This example demonstrates how to use the Meshtastic Rust API to send a message over
//! a Bluetooth Low Energy (BLE) connection and checking for its ACK.
//!
//! Usage:
//!    cargo run --example send_message_ble --features="bluetooth-le tokio" -- <DESTINATION_NODE | BROADCAST> <MESSAGE> [BLE_DEVICE_NAME]
//!
//! If the BLE_DEVICE_NAME is not provided, the example will scan and prompt to specify a device if multiple
//! are found.
//!
//! if the DESTINATION_NODE is not found the example will dump all destination nodes found.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::time::{Duration, Instant};

use meshtastic::Message;
use meshtastic::api::StreamApi;
use meshtastic::packet::{PacketDestination, PacketRouter};
use meshtastic::protobufs::mesh_packet::Priority;
use meshtastic::protobufs::{FromRadio, MeshPacket, PortNum};
use meshtastic::protobufs::{Routing, from_radio};
use meshtastic::protobufs::{mesh_packet, routing};
use meshtastic::types::{MeshChannel, NodeId};
use meshtastic::utils::generate_rand_id;
use meshtastic::utils::stream::{BleId, build_ble_stream};

const BROADCAST: u32 = 0xffffffff;

macro_rules! print_flush {
    ($($arg:tt)*) => {{
        use std::io::{Write, stdout};
        print!($($arg)*);
        stdout().flush().unwrap();
    }};
}

struct Router {
    sent: VecDeque<MeshPacket>,
    node_id: NodeId,
}

impl Router {
    fn new(node_id: NodeId) -> Self {
        Self {
            sent: VecDeque::new(),
            node_id,
        }
    }
}

impl PacketRouter<(), Infallible> for Router {
    fn handle_packet_from_radio(&mut self, _packet: FromRadio) -> Result<(), Infallible> {
        Ok(())
    }
    fn handle_mesh_packet(&mut self, packet: MeshPacket) -> Result<(), Infallible> {
        println!("Mesh packet sent..");
        self.sent.push_back(packet);
        Ok(())
    }
    fn source_node_id(&self) -> NodeId {
        self.node_id
    }
}

async fn get_ble_device() -> String {
    println!("Scanning devices 5s, will connect if only one device is found,...");
    let devices = meshtastic::utils::stream::available_ble_devices(Duration::from_secs(5))
        .await
        .expect("available_ble_devices failed");

    match devices.len() {
        0 => {
            panic!("No BLE devices found");
        }
        1 => devices[0]
            .name
            .clone()
            .expect("Device name should be present"),
        _ => {
            println!("Multiple BLE devices found: {:?}", devices);
            panic!("Please specify a device as last argument");
        }
    }
}

#[tokio::main]
async fn main() {
    let Some(to) = std::env::args().nth(1) else {
        panic!("First argument should be the destination node or BROADCAST");
    };
    let Some(msg) = std::env::args().nth(2) else {
        panic!("Second argument should be the message");
    };
    let ble_device = if let Some(ble_device) = std::env::args().nth(3) {
        ble_device
    } else {
        get_ble_device().await
    };

    // Initialize BLE stream
    // -----------------------------------------------------------------------
    println!("Connecting to {}", ble_device);
    let ble_stream = build_ble_stream(&BleId::from_name(&ble_device), Duration::from_secs(5))
        .await
        .expect("Unable to build BLE stream");

    let stream_api = StreamApi::new();
    let (mut packet_rx, stream_api) = stream_api.connect(ble_stream).await;
    let config_id = generate_rand_id();
    let mut stream_api = stream_api
        .configure(config_id)
        .await
        .expect("Unable to open stream api");

    // Retrieve all node names by processing incoming packets.
    // This also clears the BLE connection buffer to free up space,
    // ensuring there is room to send outgoing messages without issues.
    // -----------------------------------------------------------------------

    // My node
    let mut my_node_info = None;
    // Map of node names to NodeId
    let mut nodes: BTreeMap<_, _> = BTreeMap::new();
    print!("Emptying I/O buffer & getting other nodes info...");

    loop {
        tokio::select! {
            from_radio = packet_rx.recv() => {
                print_flush!(".");

                let from_radio = from_radio.expect("BLE stream closed");
                let Some(payload) = from_radio.payload_variant else {
                    continue
                };
                match payload {
                    // Check for information about my node
                    from_radio::PayloadVariant::MyInfo(node_info) => {
                        my_node_info = Some(node_info)
                    },
                    // Check for the data in NodeDB
                    from_radio::PayloadVariant::NodeInfo(node_info) => {
                        if let Some(user) = node_info.user {
                            nodes.insert(user.short_name, node_info.num);
                        }
                    },
                    _=> {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                break;
            }
        }
    }

    let my_node_info = my_node_info.expect("Failed to receive MyInfo");

    let to = if to == "BROADCAST" {
        BROADCAST
    } else if let Some(to) = nodes.get(&to) {
        *to
    } else {
        println!("\nAvailable nodes: {:?}", nodes.keys());
        panic!("Specified node '{to}' not found");
    };

    println!(
        "\nMy node id {}, destination node id {}",
        my_node_info.my_node_num, to
    );

    // Send a message
    // -----------------------------------------------------------------------
    print!("\nSending message...");

    let mut packet_router = Router::new(NodeId::new(my_node_info.my_node_num));
    stream_api
        .send_text(
            &mut packet_router,
            msg,
            PacketDestination::Node(NodeId::new(to)),
            true,
            MeshChannel::new(0).unwrap(),
        )
        .await
        .expect("Unable to send message");

    let sent_packet = packet_router.sent.pop_front().unwrap();

    // Wait for ACK (not available in Broadcast)
    // -----------------------------------------------------------------------
    print_flush!("Waiting for ACK (packet_id={})...", sent_packet.id);

    let start = Instant::now();
    loop {
        print_flush!(".");
        tokio::select! {
            from_radio = packet_rx.recv()  => {
                let from_radio = from_radio.expect("BLE stream closed");
                if let Some(from_radio::PayloadVariant::Packet(mesh_packet)) = from_radio.payload_variant
                    // Check mesh packet destination
                    && mesh_packet.to == my_node_info.my_node_num
                    // Check request id
                    && let Some(mesh_packet::PayloadVariant::Decoded(data)) = mesh_packet.payload_variant
                    && data.request_id == sent_packet.id
                    // Check that is a Routing app without error
                    && PortNum::try_from(data.portnum) == Ok(PortNum::RoutingApp)
                    && let Ok(Routing{ variant }) = Routing::decode(data.payload.as_slice())
                    && let Some(routing::Variant::ErrorReason(routing_error)) = variant
                {
                    if routing_error != routing::Error::None as i32 {
                        println!("Error in routing: {:?}", routing_error);
                        break;
                    }
                    if mesh_packet.from == to  {
                        // Normal ACK: if comes from the destination node
                        println!("got ACK from destination in {}s", start.elapsed().as_secs());
                        break;
                    } else if mesh_packet.from == my_node_info.my_node_num  && mesh_packet.priority == Priority::Ack.into() {
                        // Implicit ACK: my node heard another node rebroadcast my message
                        println!("got Implicit ACK in {}s", start.elapsed().as_secs());
                        if to == BROADCAST {
                            // In the case of BROADCAST, this is the only packet that we will recieve.
                            // (from documentation)
                            // The original sender listens to see if at least one node is rebroadcasting
                            // this packet (because naive flooding algorithm).
                            // If it hears that the odds (given typical LoRa topologies) the odds are very
                            // high that every node should eventually receive the message.
                            break;
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                if start.elapsed().as_secs() > 60 {
                    println!("ACK timeout");
                    break;
                }
            }
        }
    }

    let _ = stream_api.disconnect().await;
}
