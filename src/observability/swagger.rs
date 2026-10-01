use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tracing::{debug, info};

/// Static API contract exposed for external tooling and human inspection.
///
/// The document intentionally stays small but explicit so it remains usable by
/// tools such as swagger-ui, generated clients, and dashboard monitoring.
#[allow(dead_code)]
pub fn openapi_document() -> serde_json::Value {
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "edgecontroller",
            "version": "1.0.0",
            "description": "Device controller API for hardware mapping, relay control, capture sessions, and controller health reporting."
        },
        "servers": [{
            "url": "http://localhost:8888",
            "description": "Local controller service"
        }],
        "paths": {
            "/health": {
                "get": {
                    "summary": "Simple service health probe",
                    "responses": {
                        "200": {"description": "Controller is reachable and responding"}
                    }
                }
            },
            "/status": {
                "get": {
                    "summary": "Operational status summary",
                    "responses": {
                        "200": {"description": "Current controller state"}
                    }
                }
            },
            "/metrics": {
                "get": {
                    "summary": "Prometheus metrics for monitoring",
                    "responses": {
                        "200": {"description": "Prometheus text format"}
                    }
                }
            },
            "/confirm": {
                "post": {
                    "summary": "Backend confirmation endpoint",
                    "responses": {
                        "200": {"description": "Confirmation accepted"}
                    }
                }
            },
            "/relay": {
                "post": {
                    "summary": "Relay control operation",
                    "responses": {
                        "200": {"description": "Command accepted"}
                    }
                }
            },
            "/ipl": {
                "post": {
                    "summary": "Firmware flash request",
                    "responses": {
                        "202": {"description": "Flash request accepted"}
                    }
                }
            }
        }
    })
}

pub async fn swagger_json() -> Response {
    debug!("swagger document requested");
    info!("serving OpenAPI contract");
    Json(openapi_document()).into_response()
}

pub async fn swagger_ui() -> impl IntoResponse {
    let page = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <title>edgecontroller API</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5.11.0/swagger-ui.css" />
    <style>body { margin: 0; padding: 0; }</style>
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5.11.0/swagger-ui-bundle.js"></script>
    <script>
      window.onload = () => {
        SwaggerUIBundle({
          url: '/swagger.json',
          dom_id: '#swagger-ui'
        });
      };
    </script>
  </body>
</html>"#;

    (StatusCode::OK, page).into_response()
}

#[cfg(test)]
mod tests {
    use super::openapi_document;

    #[test]
    fn openapi_document_has_expected_root_fields() {
        let doc = openapi_document();
        assert_eq!(doc["openapi"], "3.0.3");
        assert_eq!(doc["info"]["title"], "edgecontroller");
        assert!(doc["paths"].is_object());
    }
}
