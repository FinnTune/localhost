use crate::config::{Location, ServerConfig};

/// Longest-prefix match, nginx-style: among all locations whose path is a
/// prefix of the request path, the most specific (longest) one wins.
pub fn match_location<'a>(server: &'a ServerConfig, path: &str) -> Option<&'a Location> {
    server
        .locations
        .iter()
        .filter(|location| location_matches(path, &location.path))
        .max_by_key(|location| location.path.len())
}

fn location_matches(path: &str, location_path: &str) -> bool {
    if location_path == "/" {
        return true;
    }
    if !path.starts_with(location_path) {
        return false;
    }
    // Require an exact match or a following path separator, so "/about"
    // doesn't match a request for "/aboutus".
    path.len() == location_path.len() || path.as_bytes().get(location_path.len()) == Some(&b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_locations(paths: &[&str]) -> ServerConfig {
        ServerConfig {
            address: "127.0.0.1:0".to_string(),
            locations: paths
                .iter()
                .map(|p| Location {
                    path: p.to_string(),
                    root: "www".to_string(),
                    index: None,
                    methods: vec!["GET".to_string()],
                    autoindex: false,
                })
                .collect(),
        }
    }

    #[test]
    fn matches_exact_path() {
        let server = server_with_locations(&["/", "/about"]);
        let location = match_location(&server, "/about").unwrap();
        assert_eq!(location.path, "/about");
    }

    #[test]
    fn falls_back_to_root_for_unknown_path() {
        let server = server_with_locations(&["/", "/about"]);
        let location = match_location(&server, "/unknown").unwrap();
        assert_eq!(location.path, "/");
    }

    #[test]
    fn prefers_longest_matching_prefix() {
        let server = server_with_locations(&["/", "/about", "/about/team"]);
        let location = match_location(&server, "/about/team/alice").unwrap();
        assert_eq!(location.path, "/about/team");
    }

    #[test]
    fn does_not_match_partial_segment() {
        let server = server_with_locations(&["/about"]);
        let location = match_location(&server, "/aboutus");
        assert!(location.is_none());
    }

    #[test]
    fn returns_none_without_a_root_location() {
        let server = server_with_locations(&["/about"]);
        assert!(match_location(&server, "/contact").is_none());
    }
}
