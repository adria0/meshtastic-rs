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
use std::io::Write;
use std::time::Duration;

use meshtastic::api::StreamApi;
use meshtastic::packet::{PacketDestination, PacketRouter};
use meshtastic::protobufs::from_radio::PayloadVariant;
use meshtastic::protobufs::{mesh_packet, Data, MyNodeInfo, User};
use meshtastic::protobufs::{FromRadio, MeshPacket, PortNum};
use meshtastic::types::{MeshChannel, NodeId};
use meshtastic::utils::generate_rand_id;
use meshtastic::utils::stream::{build_ble_stream, BleId};

use meshtastic::Message;

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
        self.sent.push_back(packet);
        Ok(())
    }
    fn source_node_id(&self) -> NodeId {
        self.node_id
    }
}

enum RecievedPacket {
    ConnectionClosed,
    RoutingApp(Data),
    MyInfo(MyNodeInfo),
    NodeInfo(NodeId, User),
    Other,
}
impl From<Option<FromRadio>> for RecievedPacket {
    fn from(from_radio: Option<FromRadio>) -> Self {
        use RecievedPacket::*;
        let Some(from_radio) = from_radio else {
            return ConnectionClosed;
        };
        let Some(payload) = from_radio.payload_variant else {
            return Other;
        };
        match payload {
            PayloadVariant::MyInfo(my_node_info) => MyInfo(my_node_info),
            PayloadVariant::NodeInfo(node_info) => {
                if let Some(user) = node_info.user {
                    NodeInfo(NodeId::new(node_info.num), user)
                } else {
                    Other
                }
            }
            PayloadVariant::Packet(recv_packet) => {
                let Some(pv) = recv_packet.payload_variant else {
                    return Other;
                };
                let mesh_packet::PayloadVariant::Decoded(data) = pv else {
                    return Other;
                };
                match PortNum::try_from(data.portnum) {
                    Ok(PortNum::RoutingApp) => RoutingApp(data),
                    Ok(PortNum::NodeinfoApp) => {
                        if let Ok(user) = User::decode(data.payload.as_slice()) {
                            NodeInfo(NodeId::new(recv_packet.from), user)
                        } else {
                            Other
                        }
                    }
                    _ => Other,
                }
            }
            _ => Other,
        }
    }
}

async fn get_ble_device() -> String {
    println!("Scanning devices 5s...");
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

    // Get MyInfo from the first message of stream
    // -----------------------------------------------------------------------
    let RecievedPacket::MyInfo(my_node_info) = RecievedPacket::from(packet_rx.recv().await) else {
        panic!("Failed to receive MyInfo");
    };

    println!("Got my node id {}", my_node_info.my_node_num);

    // Retrieve all node names by processing incoming packets.
    // This also clears the BLE connection buffer to free up space,
    // ensuring there is room to send outgoing messages without issues.
    // -----------------------------------------------------------------------

    let mut nodes: BTreeMap<_, _> = [(String::from("BROADCAST"), NodeId::new(u32::MAX))].into();

    print!("Emptying I/O buffer & getting other nodes info...");
    loop {
        tokio::select! {
            packet = packet_rx.recv() => {
                match RecievedPacket::from(packet).into() {
                    RecievedPacket::NodeInfo(node_id, node_info) => {
                        nodes.insert(node_info.short_name, node_id);
                    }
                    RecievedPacket::ConnectionClosed => {
                        panic!("Connection closed");
                    }
                    _ => {}
                }
                print!(".");
                std::io::stdout().flush().unwrap();
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                break;
            }
        }
    }

    let Some(to) = nodes.get(&to) else {
        println!("\nAvailable nodes: {:?}", nodes.keys());
        panic!("Specified node not found");
    };

    // Send a message
    // -----------------------------------------------------------------------
    print!("\nSending message...");

    let mut packet_router = Router::new(NodeId::new(my_node_info.my_node_num));
    stream_api
        .send_text(
            &mut packet_router,
            msg,
            PacketDestination::Node(*to),
            true,
            MeshChannel::default(),
        )
        .await
        .expect("Unable to send message");

    let sent_packet = packet_router.sent.pop_front().unwrap();

    println!(" sent.");

    // Wait for ACK
    // -----------------------------------------------------------------------
    print!("Waiting for ACK (packet_id={})...", sent_packet.id);
    std::io::stdout().flush().unwrap();

    loop {
        match packet_rx.recv().await.into() {
            RecievedPacket::RoutingApp(data) => {
                if data.portnum == PortNum::RoutingApp as i32 && data.request_id == sent_packet.id {
                    println!("got ACK");
                    break;
                }
            }
            RecievedPacket::ConnectionClosed => {
                panic!("Connection closed");
            }
            _ => {}
        }
    }
}
