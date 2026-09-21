//! The shrinking legacy OpenAPI fragment. Every path is now utoipa-generated
//! from handler annotations in calendar-server (IMPLEMENTATION_PLAN.md
//! stages 2-5); this module keeps only the shared info, security schemes and
//! global security until stage 5 folds them into the generated document.

/// The legacy hand-built fragment, now that every path is utoipa-generated in
/// calendar-server: only the shared info, security schemes and global
/// security remain for the merge in `openapi_json()`. Deleted once those move
/// onto the generated document (stage 5, IMPLEMENTATION_PLAN.md).
pub fn openapi_document() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Calendar Server API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Normalized calendar domain API; CalDAV is a separate first-class protocol.",
        },
        "paths": {},
        "components": {
            "securitySchemes": {
                "sessionCookie": {"type": "apiKey", "in": "cookie", "name": "session"},
                "bearerToken": {"type": "http", "scheme": "bearer"},
            }
        },
        "security": [{"sessionCookie": []}, {"bearerToken": []}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fragment's paths all moved to utoipa annotations in calendar-server
    /// (stages 2-4); only the shared info/security survive until stage 5.
    #[test]
    fn fragment_carries_only_info_and_security() {
        let doc = openapi_document();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["paths"].as_object().unwrap().len(), 0);
        assert!(doc["components"]["securitySchemes"]["sessionCookie"].is_object());
        assert!(doc["components"]["securitySchemes"]["bearerToken"].is_object());
        assert!(doc["security"].is_array());
    }
}
