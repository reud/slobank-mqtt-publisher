use libftd2xx::{self, FtdiCommon};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use thiserror::Error;
use clap::Arg;

#[derive(Error, Debug)]
pub enum SlobankPublisherError {
    #[error("FT245RL Device Not Found")]
    DeviceNotFoundError,
    #[error("line {}: my decode error", .linenum)]  // 付加的な情報を持つエラーメッセージの場合
    MyDecodeError { linenum: usize },
    
    #[error("my fuga error")] // 付加的な情報を持たないエラーメッセージの場合
    MyFugaError,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    let (topic, port, host) = parse_arguments();

    // MQTT setup
    let mqtt_options = MqttOptions::new("slobank_publisher", host, port);
    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options,10);

    let (tx,rx) = mpsc::channel::<u8>(100000);
    let states = Arc::new(Mutex::new([0u8, 0u8]));
    let stability_counter = Arc::new(Mutex::new(0));

    spawn_data_processor(rx, Arc::clone(&states), Arc::clone(&stability_counter), mqtt_client.clone(), topic.to_string());

    let device = Arc::new(Mutex::new(setup_ftdi()?)); 
    
    spawn_device_reader(tx.clone(), Arc::clone(&device));
    // Keep the MQTT event loop running
    while let Ok(_) = eventloop.poll().await {}

    Ok(())
}


fn parse_arguments() -> (String, u16, String) {
    // Parse command-line arguments
    let matches = clap::Command::new("slobank-publisher")
        .version("0.1.0")
        .author("reud")
        .about("Reads data from slot and publishes to MQTT")
        .arg(Arg::new("host")
            .short('h')
            .long("host")
            .num_args(1)
            .default_value("localhost")
            .help("MQTT broker host"))
        .arg(Arg::new("port")
            .short('p')
            .long("port")
            .num_args(1)
            .default_value("1883")
            .value_parser(clap::value_parser!(u16).range(1024..)) // Portはu16として検証
            .help("MQTT broker port"))
        .arg(Arg::new("topic")
            .short('t')
            .long("topic")
            .num_args(1)
            .default_value("slot")
            .help("MQTT topic to publish to"))
        .get_matches();

    let topic = matches.get_one::<String>("topic").unwrap().to_string();
    let port = matches.get_one::<u16>("port").unwrap();
    let host = matches.get_one::<String>("host").unwrap().to_string();

    (topic,*port,host)
}

fn setup_ftdi() -> Result<libftd2xx::Ftdi, Box<dyn std::error::Error>> {
    // FTDIデバイスの初期化
    let devices = libftd2xx::list_devices()?;

    // 0x6001 -> FT245RL product ID
    let target_device_info = devices.iter()
                .find(|&di| di.product_id == 0x6001)
                .ok_or_else(|| SlobankPublisherError::DeviceNotFoundError)?;
    
    let mut device = libftd2xx::Ftdi::with_serial_number(target_device_info.serial_number.as_str())?;
    
    // 0x00 -> Use All Bits, 0x01 -> BitBang Mode
    device.set_bit_mode(0x00, libftd2xx::BitMode::AsyncBitbang)?;

    Ok(device)
}

fn spawn_data_processor(mut rx: mpsc::Receiver<u8>, states: Arc<Mutex<[u8; 2]>>, stability_counter: Arc<Mutex<u64>>, mqtt_client: AsyncClient, topic: String) {
    tokio::spawn(async move {
        let mut counter = stability_counter.lock().await;
        while let Some(value) = rx.recv().await {
            let mut states = states.lock().await;

            states[0] = states[1];
            states[1] = value;

            if states[0] != states[1] {
                if *counter <= 3000 {
                    println!("Chattaring detected. Skip this callback: {},{}", states[0], states[1])
                }
                else {
                    if let Err(e) = mqtt_client
                        .publish(&topic, QoS::AtMostOnce, false, format!("{:?}", states[1]))
                        .await {
                        eprintln!("Failed to publish MQTT message: {}", e);
                    }
                    println!("value changed. {} -> {} (counter: {})", states[0], states[1], *counter);
                }
                *counter = 0; // Reset counter after publishing
            } else {
                *counter += 1;
            }
        }
        println!("Receiver task ended: Channel closed.");
    });
}

fn spawn_device_reader(tx: mpsc::Sender<u8>, device: Arc<Mutex<libftd2xx::Ftdi>>) {
    tokio::spawn(async move {
        loop {
            let mut buffer = vec![0u8; 4096];
            let mut device = device.lock().await; // デバイスをロックして操作
            match device.read(&mut buffer) {
                Ok(bytes_read) => {
                    for &byte in &buffer[..bytes_read] {
                        if let Err(e) = tx.send(byte).await {
                            eprintln!("Channel send failed: {:?}", e); // 詳細なエラーをログに記録
                            return; // エラー時にループを抜ける
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from device: {:?}", e); // デバイス読み取りエラー時にログ出力
                    return; // エラー時にループを抜ける
                }
            }
        }
    });
}
