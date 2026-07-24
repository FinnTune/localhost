use crate::json::{self, JsonValue};
use std::collections::HashMap;
use std::fs;

#[derive(Debug)]
pub struct Location {
    pub path: String,
    pub root: String,
    pub index: Option<String>,
    pub methods: Vec<String>,
    // Not read yet: directory listing lands in the Phase 8 commit.
    #[allow(dead_code)]
    pub autoindex: bool,
    /// Maps a file extension (no leading dot, e.g. "sh") to the interpreter
    /// binary that should execute matching scripts as CGI.
    pub cgi: HashMap<String, String>,
}

#[derive(Debug)]
pub struct ServerConfig {
    pub address: String,
    pub server_name: Option<String>,
    pub locations: Vec<Location>,
}

#[derive(Debug)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

fn location_from_json(value: &JsonValue) -> Result<Location, String> {
    let path = value
        .get("path")
        .and_then(JsonValue::as_str)
        .ok_or("location entry missing string field 'path'")?
        .to_string();

    let root = value
        .get("root")
        .and_then(JsonValue::as_str)
        .ok_or("location entry missing string field 'root'")?
        .to_string();

    let index = match value.get("index") {
        Some(v) => Some(
            v.as_str()
                .ok_or("location field 'index' must be a string")?
                .to_string(),
        ),
        None => None,
    };

    let methods = match value.get("methods") {
        Some(v) => v
            .as_array()
            .ok_or("location field 'methods' must be an array")?
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "method entries must be strings".to_string())
            })
            .collect::<Result<Vec<String>, String>>()?,
        None => vec!["GET".to_string()],
    };

    let autoindex = match value.get("autoindex") {
        Some(JsonValue::Bool(b)) => *b,
        Some(_) => return Err("location field 'autoindex' must be a boolean".to_string()),
        None => false,
    };

    let cgi = match value.get("cgi") {
        Some(JsonValue::Object(map)) => map
            .iter()
            .map(|(extension, interpreter)| {
                interpreter
                    .as_str()
                    .map(|path| (extension.clone(), path.to_string()))
                    .ok_or_else(|| "cgi interpreter paths must be strings".to_string())
            })
            .collect::<Result<HashMap<String, String>, String>>()?,
        Some(_) => return Err("location field 'cgi' must be an object".to_string()),
        None => HashMap::new(),
    };

    Ok(Location {
        path,
        root,
        index,
        methods,
        autoindex,
        cgi,
    })
}

fn server_config_from_json(value: &JsonValue) -> Result<ServerConfig, String> {
    let address = value
        .get("address")
        .and_then(JsonValue::as_str)
        .ok_or("server entry missing string field 'address'")?
        .to_string();

    let server_name = match value.get("server_name") {
        Some(v) => Some(
            v.as_str()
                .ok_or("server field 'server_name' must be a string")?
                .to_string(),
        ),
        None => None,
    };

    let locations = value
        .get("locations")
        .and_then(JsonValue::as_array)
        .ok_or("server entry missing array field 'locations'")?
        .iter()
        .map(location_from_json)
        .collect::<Result<Vec<Location>, String>>()?;

    Ok(ServerConfig {
        address,
        server_name,
        locations,
    })
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
