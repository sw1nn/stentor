use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();

    println!("Available input devices:");
    println!("========================\n");

    match host.input_devices() {
        Ok(devices) => {
            for (i, device) in devices.enumerate() {
                match device.name() {
                    Ok(name) => {
                        println!("Device {}: {}", i + 1, name);

                        // Try to get default config
                        if let Ok(config) = device.default_input_config() {
                            println!("  Sample rate: {}", config.sample_rate().0);
                            println!("  Channels: {}", config.channels());
                            println!("  Sample format: {:?}", config.sample_format());
                        }
                        println!();
                    }
                    Err(e) => {
                        println!("Device {}: <error getting name: {}>", i + 1, e);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Error enumerating devices: {}", e);
        }
    }

    // Show default device
    println!("\nDefault input device:");
    println!("====================");
    match host.default_input_device() {
        Some(device) => {
            match device.name() {
                Ok(name) => println!("{}", name),
                Err(e) => println!("<error getting name: {}>", e),
            }
        }
        None => println!("No default input device"),
    }
}
