use crate::json::{self, JsonValue};
use std::fs;

#[derive(Debug)]
pub struct ServerConfig {
    pub address: String,
    // Not read yet: routing on these lands in the Phase 3 (routing/locations) commit.
    #[allow(dead_code)]
    pub endpoints: Vec<String>,
}

#[derive(Debug)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

fn server_config_from_json(value: &JsonValue) -> Result<ServerConfig, String> {
    let address = value
        .get("address")
        .and_then(JsonValue::as_str)
        .ok_or("server entry missing string field 'address'")?
        .to_string();

    let endpoints = value
        .get("endpoints")
        .and_then(JsonValue::as_array)
        .ok_or("server entry missing array field 'endpoints'")?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "endpoint entries must be strings".to_string())
        })
        .collect::<Result<Vec<String>, String>>()?;

    Ok(ServerConfig { address, endpoints })
}

fn config_from_json(value: &JsonValue) -> Result<Config, String> {
    let servers = value
        .get("servers")
        .and_then(JsonValue::as_array)
        .ok_or("config missing array field 'servers'")?
        .iter()
        .map(server_config_from_json)
        .collect::<Result<Vec<ServerConfig>, String>>()?;

    Ok(Config { servers })
}

pub fn load_config(file_path: &str) -> Result<Config, String> {
    let config_str = fs::read_to_string(file_path)
        .map_err(|e| format!("failed to read configuration file '{}': {}", file_path, e))?;
    let value = json::parse(&config_str).map_err(|e| format!("invalid JSON config: {}", e))?;
    config_from_json(&value)
}
