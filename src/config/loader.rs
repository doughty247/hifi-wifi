use super::structs::Config;
use std::fs;
use std::path::Path;
use log::{info, warn};

const CONFIG_PATH: &str = "/etc/hifi-wifi/config.toml";

pub fn load_config() -> Config {
    if Path::new(CONFIG_PATH).exists() {
        match fs::read_to_string(CONFIG_PATH) {
            Ok(content) => match toml::from_str(&content) {
                Ok(config) => {
                    info!("Loaded configuration from {}", CONFIG_PATH);
                    return config;
                }
                Err(e) => {
                    warn!("Failed to parse config file: {}. Using defaults.", e);
                }
            },
            Err(e) => {
                warn!("Failed to read config file: {}. Using defaults.", e);
            }
        }
    } else {
        info!("No config file found at {}. Using defaults.", CONFIG_PATH);
    }
    
    Config::default()
}
