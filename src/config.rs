use crate::json::{self, JsonValue};
use std::collections::{HashMap, HashSet};
use std::fs;

/// Used both as the parser's hard safety ceiling on any request body
/// (src/http/request.rs) and as the default for a location's configurable
/// `client_max_body_size` below.
pub const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

const KNOWN_METHODS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"];

#[derive(Debug)]
pub struct Location {
    pub path: String,
    pub root: String,
    pub index: Option<String>,
    pub methods: Vec<String>,
    pub autoindex: bool,
    /// Maps a file extension (no leading dot, e.g. "sh") to the interpreter
    /// binary that should execute matching scripts as CGI.
    pub cgi: HashMap<String, String>,
    pub client_max_body_size: usize,
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

    if !path.starts_with('/') {
        return Err(format!("location path '{}' must start with '/'", path));
    }

    let root = value
        .get("root")
        .and_then(JsonValue::as_str)
        .ok_or("location entry missing string field 'root'")?
        .to_string();

    // Catches the single most common config mistake — a typo'd or
    // not-yet-created root — at startup instead of as a wall of runtime
    // 404s that give no hint the path itself is the problem.
    match fs::metadata(&root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "location '{}': root '{}' exists but is not a directory",
                path, root
            ))
        }
        Err(e) => {
            return Err(format!(
                "location '{}': root '{}' is not accessible: {}",
                path, root, e
            ))
        }
    }

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

    // A typo'd method (e.g. "GTE") would otherwise silently compile into a
    // location nothing can ever match, permanently 405ing every request.
    for method in &methods {
        if !KNOWN_METHODS.contains(&method.as_str()) {
            return Err(format!(
                "location '{}': unknown HTTP method '{}'",
                path, method
            ));
        }
    }

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

    let client_max_body_size = match value.get("client_max_body_size") {
        Some(JsonValue::Number(n)) if *n >= 0.0 => *n as usize,
        Some(_) => {
            return Err(
                "location field 'client_max_body_size' must be a non-negative number".to_string(),
            )
        }
        None => DEFAULT_MAX_BODY_SIZE,
    };

    Ok(Location {
        path,
        root,
        index,
        methods,
        autoindex,
        cgi,
        client_max_body_size,
    })
}

fn server_config_from_json(value: &JsonValue) -> Result<ServerConfig, String> {
    let address = value
        .get("address")
        .and_then(JsonValue::as_str)
        .ok_or("server entry missing string field 'address'")?
        .to_string();

    // Fail with a message that names the bad address, instead of the
    // generic bind error main.rs would otherwise surface much later.
    address
        .parse::<std::net::SocketAddr>()
        .map_err(|_| format!("server address '{}' is not a valid host:port", address))?;

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

    // router::select_server picks the *first* server block matching a given
    // (address, server_name) pair and never looks further, so a second
    // block with the identical pair (including two blocks with no
    // server_name at all) is dead config that can never be selected.
    let mut seen = HashSet::new();
    for server in &servers {
        let key = (server.address.clone(), server.server_name.clone());
        if !seen.insert(key) {
            return Err(format!(
                "duplicate server block for address '{}' and server_name {:?} — the second one can never be selected",
                server.address, server.server_name
            ));
        }
    }

    Ok(Config { servers })
}

pub fn load_config(file_path: &str) -> Result<Config, String> {
    let config_str = fs::read_to_string(file_path)
        .map_err(|e| format!("failed to read configuration file '{}': {}", file_path, e))?;
    let value = json::parse(&config_str).map_err(|e| format!("invalid JSON config: {}", e))?;
    config_from_json(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    static DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A real, existing directory for `root` to point at — validation
    /// checks the filesystem, so tests exercise it against real paths
    /// rather than mocking `fs::metadata`.
    fn temp_dir() -> std::path::PathBuf {
        let unique = DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "localhost_config_test_{}_{}",
            std::process::id(),
            unique
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_json(root: &str, extra_location_fields: &str, extra_server_fields: &str) -> String {
        format!(
            r#"{{"servers": [{{"address": "127.0.0.1:0", {extra_server}
                "locations": [{{"path": "/", "root": "{root}" {extra_loc}}}]}}]}}"#,
            root = root,
            extra_loc = extra_location_fields,
            extra_server = extra_server_fields,
        )
    }

    fn load(json_str: &str) -> Result<Config, String> {
        let value = json::parse(json_str).unwrap();
        config_from_json(&value)
    }

    #[test]
    fn accepts_a_minimal_valid_config() {
        let root = temp_dir();
        let json_str = config_json(&root.to_string_lossy(), "", "");
        assert!(load(&json_str).is_ok());
    }

    #[test]
    fn rejects_location_path_without_leading_slash() {
        let root = temp_dir();
        let json_str = format!(
            r#"{{"servers": [{{"address": "127.0.0.1:0",
                "locations": [{{"path": "about", "root": "{}"}}]}}]}}"#,
            root.to_string_lossy()
        );
        assert!(load(&json_str).unwrap_err().contains("must start with"));
    }

    #[test]
    fn rejects_unknown_http_method() {
        let root = temp_dir();
        let json_str = config_json(&root.to_string_lossy(), r#", "methods": ["GTE"]"#, "");
        assert!(load(&json_str).unwrap_err().contains("unknown HTTP method"));
    }

    #[test]
    fn rejects_missing_root_directory() {
        let json_str = config_json("/no/such/path/should/exist", "", "");
        assert!(load(&json_str).unwrap_err().contains("not accessible"));
    }

    #[test]
    fn rejects_root_that_is_a_file() {
        let root = temp_dir();
        let file_path = root.join("not_a_dir");
        fs::write(&file_path, b"x").unwrap();
        let json_str = config_json(&file_path.to_string_lossy(), "", "");
        assert!(load(&json_str).unwrap_err().contains("is not a directory"));
    }

    #[test]
    fn rejects_invalid_address() {
        let root = temp_dir();
        let json_str = format!(
            r#"{{"servers": [{{"address": "not-a-valid-address",
                "locations": [{{"path": "/", "root": "{}"}}]}}]}}"#,
            root.to_string_lossy()
        );
        assert!(load(&json_str)
            .unwrap_err()
            .contains("not a valid host:port"));
    }

    #[test]
    fn rejects_duplicate_server_blocks() {
        let root = temp_dir();
        let json_str = format!(
            r#"{{"servers": [
                {{"address": "127.0.0.1:0", "locations": [{{"path": "/", "root": "{root}"}}]}},
                {{"address": "127.0.0.1:0", "locations": [{{"path": "/", "root": "{root}"}}]}}
            ]}}"#,
            root = root.to_string_lossy()
        );
        assert!(load(&json_str)
            .unwrap_err()
            .contains("duplicate server block"));
    }
}
