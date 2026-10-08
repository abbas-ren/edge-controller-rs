use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::json;
use tracing::{debug, info};

fn json_request(schema: &str) -> serde_json::Value {
    json!({
        "required": true,
        "content": {
            "application/json": {
                "schema": {"$ref": format!("#/components/schemas/{schema}")}
            }
        }
    })
}

fn responses(status: &str, description: &str) -> serde_json::Value {
    json!({
        status: {"description": description},
        "400": {"$ref": "#/components/responses/BadRequest"},
        "401": {"$ref": "#/components/responses/Unauthorized"},
        "409": {"$ref": "#/components/responses/Conflict"},
        "500": {"$ref": "#/components/responses/InternalError"},
        "503": {"$ref": "#/components/responses/Unavailable"}
    })
}

fn json_response(description: &str, schema: &str) -> serde_json::Value {
    json!({
        "description": description,
        "content": {
            "application/json": {
                "schema": {"$ref": format!("#/components/schemas/{schema}")}
            }
        }
    })
}

fn post_operation(
    summary: &str,
    schema: &str,
    status: &str,
    description: &str,
) -> serde_json::Value {
    json!({
        "summary": summary,
        "requestBody": json_request(schema),
        "responses": responses(status, description)
    })
}

/// Return the complete OpenAPI contract exposed by the HTTP router.
pub fn openapi_document() -> serde_json::Value {
    let mut document = json!({
        "openapi": "3.0.3",
        "info": {
            "title": "edgecontroller",
            "version": "1.0.0",
            "description": "Device controller API for hardware mapping, relay control, capture sessions, and controller health reporting."
        },
        "security": [{}, {"bearerAuth": []}],
        "paths": {
            "/health": {
                "get": {
                    "summary": "Simple service health probe",
                    "responses": {
                        "200": json_response("Controller is reachable and responding", "HealthResponse"),
                        "401": {"$ref": "#/components/responses/Unauthorized"}
                    }
                }
            },
            "/ready": {
                "get": {
                    "summary": "Backend registration and shutdown readiness probe",
                    "responses": {
                        "200": json_response("Controller is registered and accepting work", "ReadinessResponse"),
                        "401": {"$ref": "#/components/responses/Unauthorized"},
                        "503": json_response("Controller is unconfirmed or shutting down", "ReadinessResponse")
                    }
                }
            },
            "/status": {
                "get": {
                    "summary": "Operational status summary",
                    "responses": {
                        "200": json_response("Current controller identity and enabled features", "StatusResponse"),
                        "401": {"$ref": "#/components/responses/Unauthorized"}
                    }
                }
            },
            "/metrics": {
                "get": {
                    "summary": "Prometheus metrics for monitoring",
                    "responses": {
                        "200": {
                            "description": "Prometheus exposition text",
                            "content": {"text/plain": {"schema": {"type": "string"}}}
                        },
                        "401": {"$ref": "#/components/responses/Unauthorized"}
                    }
                }
            },
            "/swagger.json": {
                "get": {
                    "summary": "OpenAPI contract",
                    "responses": responses("200", "OpenAPI 3.0 document")
                }
            },
            "/docs": {
                "get": {
                    "summary": "Swagger UI",
                    "responses": responses("200", "Interactive API documentation")
                }
            },
            "/confirmation": {
                "post": {
                    "summary": "Backend confirmation endpoint",
                    "requestBody": json_request("ConfirmationRequest"),
                    "responses": responses("200", "Controller identifier persisted")
                }
            },
            "/relay": {
                "post": post_operation("Set relay channel state", "RelayRequest", "200", "Relay state changed")
            },
            "/relay/status": {
                "post": post_operation("Read relay channel state", "RelayStatusRequest", "200", "Relay state returned")
            },
            "/relay/identity": {
                "post": post_operation("Update active relay USB identity", "RelayIdentityUpdateRequest", "200", "Relay identity updated")
            },
            "/relay/config": {
                "post": post_operation("Persist Gen3/Gen4 relay mapping", "RelayConfigRequest", "200", "Mapping persisted")
            },
            "/relay/delete": {
                "post": post_operation("Delete a relay mapping", "DeleteRequest", "200", "Mapping deleted")
            },
            "/devCon/delete": {
                "post": post_operation("Delete controller or board state", "DeleteRequest", "200", "State deleted")
            },
            "/mapping/entry": {
                "post": post_operation("Discover and persist a board mapping", "MappingEntryRequest", "200", "Mapping discovered")
            },
            "/reboot-device": {
                "post": post_operation("Power-cycle a mapped device", "RebootDeviceRequest", "200", "Device power-cycled")
            },
            "/ipl": {
                "post": post_operation("Start a firmware flash", "IplRequest", "202", "Flash job accepted")
            },
            "/ipl-mode": {
                "post": post_operation("Enter Gen3/Gen4 IPL mode", "IplModeRequest", "200", "IPL mode selected")
            },
            "/ipl-mode/default": {
                "post": post_operation("Restore default Gen3/Gen4 boot mode", "IplModeRequest", "200", "Default boot mode selected")
            },
            "/ipl/remove": {
                "post": post_operation("Remove a firmware package directory", "RemoveIplRequest", "200", "Package removed")
            },
            "/gen5/tty_entry": {
                "post": post_operation("Read a Gen5 TTY mapping", "Gen5TtyRequest", "200", "TTY mapping returned")
            },
            "/gen5/power": {
                "post": post_operation("Set Gen5 power state", "Gen5PowerRequest", "200", "Power state changed")
            },
            "/rtos/start": {
                "post": post_operation("Start an RTOS capture", "RtosStartRequest", "200", "Capture started")
            },
            "/rtos/end": {
                "post": {
                    "summary": "Stop and download an RTOS capture",
                    "requestBody": json_request("RtosEndRequest"),
                    "responses": {
                        "200": {
                            "description": "Captured bytes, possibly partial",
                            "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}}
                        },
                        "400": {"$ref": "#/components/responses/BadRequest"},
                        "401": {"$ref": "#/components/responses/Unauthorized"},
                        "409": {"$ref": "#/components/responses/Conflict"},
                        "500": {"$ref": "#/components/responses/InternalError"}
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Required when DEV_CONTROLLER_TOKEN is configured"
                }
            },
            "responses": {
                "BadRequest": {"description": "Invalid request", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorResponse"}}}},
                "Unauthorized": {"description": "Missing or invalid bearer token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorResponse"}}}},
                "Conflict": {"description": "Hardware operation conflicts with active work", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorResponse"}}}},
                "InternalError": {"description": "Hardware or persistence operation failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorResponse"}}}},
                "Unavailable": {"description": "Controller is shutting down", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorResponse"}}}}
            },
            "schemas": {
                "ErrorResponse": {"type": "object", "required": ["error"], "properties": {"error": {"type": "string"}}},
                "HealthResponse": {"type": "object", "required": ["OK"], "properties": {"OK": {"type": "boolean", "enum": [true]}}},
                "ReadinessResponse": {"type": "object", "required": ["ready", "registered", "shutting_down"], "properties": {"ready": {"type": "boolean"}, "registered": {"type": "boolean"}, "shutting_down": {"type": "boolean"}}},
                "FeatureStatus": {"type": "object", "required": ["gen3", "gen4", "gen5", "rtos"], "properties": {"gen3": {"type": "boolean"}, "gen4": {"type": "boolean"}, "gen5": {"type": "boolean"}, "rtos": {"type": "boolean"}}},
                "StatusResponse": {"type": "object", "required": ["status", "generation", "board_mac", "board_ip", "uid", "registered", "shutting_down", "deletion_requested", "usb_mapping_count", "gen5_mapping_count", "active_capture_count", "features"], "properties": {"status": {"type": "string", "enum": ["ok"]}, "generation": {"type": "integer", "enum": [3, 4, 5]}, "board_mac": {"type": "string"}, "board_ip": {"type": "string", "format": "ipv4"}, "uid": {"type": "string", "nullable": true}, "registered": {"type": "boolean"}, "shutting_down": {"type": "boolean"}, "deletion_requested": {"type": "boolean"}, "usb_mapping_count": {"type": "integer", "minimum": 0}, "gen5_mapping_count": {"type": "integer", "minimum": 0}, "active_capture_count": {"type": "integer", "minimum": 0}, "features": {"$ref": "#/components/schemas/FeatureStatus"}}},
                "ConfirmationRequest": {"type": "object", "required": ["controllerId"], "properties": {"controllerId": {"type": "string"}}},
                "RelayRequest": {"type": "object", "required": ["serial", "state", "channel"], "properties": {"serial": {"type": "string"}, "state": {"type": "string", "enum": ["on", "off"]}, "channel": {"type": "integer", "minimum": 0, "maximum": 7}}},
                "RelayStatusRequest": {"type": "object", "required": ["serial", "channel"], "properties": {"serial": {"type": "string"}, "channel": {"type": "integer", "minimum": 0, "maximum": 7}}},
                "RelayConfigRequest": {"type": "object", "required": ["mac", "serial", "channel", "gen"], "properties": {"mac": {"type": "string"}, "serial": {"type": "string"}, "channel": {"type": "integer", "minimum": 0, "maximum": 7}, "gen": {"type": "integer", "enum": [3, 4]}}},
                "DeleteRequest": {"type": "object", "properties": {"gen": {"type": "integer", "enum": [3, 4], "default": 4}, "uid": {"type": "string", "nullable": true}, "mac": {"type": "string", "nullable": true}, "serial": {"type": "string", "nullable": true}, "channel": {"type": "integer", "minimum": 0, "maximum": 7, "nullable": true}}},
                "MappingEntryRequest": {"type": "object", "required": ["mac", "gen"], "properties": {"mac": {"type": "string"}, "gen": {"type": "integer", "enum": [5]}}},
                "RebootDeviceRequest": {"type": "object", "required": ["power"], "properties": {"power": {"type": "string"}}},
                "VoltageLevel": {"type": "string", "enum": ["HIGH", "LOW"]},
                "RelayIdentityUpdateRequest": {"type": "object", "required": ["vidPid"], "properties": {"serialNumber": {"type": "string", "nullable": true}, "vidPid": {"type": "string", "pattern": "^[0-9A-Fa-f]{4}:[0-9A-Fa-f]{4}$"}}},
                "IplRequest": {"type": "object", "required": ["gen"], "properties": {"gen": {"type": "integer", "enum": [3, 4, 5]}, "gpio": {"type": "integer", "nullable": true}, "gpioDefaultLevel": {"allOf": [{"$ref": "#/components/schemas/VoltageLevel"}], "default": "LOW", "nullable": true}, "relayDefaultLevel": {"allOf": [{"$ref": "#/components/schemas/VoltageLevel"}], "default": "LOW", "nullable": true}, "mac": {"type": "string", "nullable": true}, "serial": {"type": "string", "nullable": true}, "channel": {"type": "integer", "minimum": 0, "maximum": 7, "nullable": true}, "path": {"type": "string", "nullable": true}, "uart": {"type": "string", "nullable": true}, "power": {"type": "string", "nullable": true}, "sdk_ver": {"type": "string", "nullable": true}}},
                "IplModeRequest": {"type": "object", "required": ["gpio", "mac", "serial", "channel"], "properties": {"gpio": {"type": "integer"}, "gpioDefaultLevel": {"allOf": [{"$ref": "#/components/schemas/VoltageLevel"}], "default": "LOW"}, "relayDefaultLevel": {"allOf": [{"$ref": "#/components/schemas/VoltageLevel"}], "default": "LOW"}, "mac": {"type": "string"}, "serial": {"type": "string"}, "channel": {"type": "integer", "minimum": 0, "maximum": 7}}},
                "RemoveIplRequest": {"type": "object", "required": ["path"], "properties": {"path": {"type": "string"}}},
                "Gen5TtyRequest": {"type": "object", "required": ["mac"], "properties": {"mac": {"type": "string"}}},
                "Gen5PowerRequest": {"type": "object", "required": ["state", "power"], "properties": {"state": {"type": "string", "enum": ["on", "off"]}, "power": {"type": "string"}}},
                "RtosStartRequest": {"type": "object", "required": ["gen"], "properties": {"gen": {"type": "integer", "enum": [4, 5]}, "mac": {"type": "string", "nullable": true}, "serial": {"type": "string", "nullable": true}, "channel": {"type": "integer", "minimum": 0, "maximum": 7, "nullable": true}, "rtos": {"type": "string", "nullable": true}}},
                "RtosEndRequest": {"type": "object", "required": ["gen"], "properties": {"gen": {"type": "integer", "enum": [4, 5]}, "mac": {"type": "string", "nullable": true}, "serial": {"type": "string", "nullable": true}, "channel": {"type": "integer", "minimum": 0, "maximum": 7, "nullable": true}, "rtos": {"type": "string", "nullable": true}}}
            }
        }
    });

    let paths = document["paths"]
        .as_object_mut()
        .expect("OpenAPI paths are an object");
    paths.insert(
        "/logs".into(),
        json!({"get": {
            "summary": "Read the bounded runtime log buffer",
            "responses": responses("200", "Runtime log entries and current capture level")
        }}),
    );
    paths.insert(
        "/logs/level".into(),
        json!({"put": {
            "summary": "Change the runtime tracing level",
            "requestBody": json_request("RuntimeLogLevelRequest"),
            "responses": responses("200", "Runtime tracing level updated")
        }}),
    );
    paths.insert(
        "/admin/control".into(),
        json!({
            "get": {
                "summary": "Read active and staged controller settings",
                "responses": responses("200", "Controller control-plane snapshot")
            },
            "patch": {
                "summary": "Update validated controller settings",
                "requestBody": json_request("ControlPatch"),
                "responses": responses("200", "Controller settings updated")
            },
            "post": {
                "summary": "Run an allowlisted controller action",
                "requestBody": json_request("ControlActionRequest"),
                "responses": responses("200", "Controller action completed")
            }
        }),
    );

    let schemas = document["components"]["schemas"]
        .as_object_mut()
        .expect("OpenAPI schemas are an object");
    schemas.insert(
        "RuntimeLogLevelRequest".into(),
        json!({"type": "object", "required": ["level"], "properties": {
            "level": {"type": "string", "enum": ["trace", "debug", "info", "warn", "error", "off"]}
        }}),
    );
    schemas.insert(
        "ControlPatch".into(),
        json!({"type": "object", "description": "Partial validated EdgeController control settings"}),
    );
    schemas.insert(
        "ControlActionRequest".into(),
        json!({"type": "object", "required": ["action"], "properties": {
            "action": {"type": "string", "enum": ["restart", "reloadMappings", "clearUsbMappings", "clearGen5Mappings", "clearUartMappings", "clearControllerUid"]}
        }}),
    );

    document
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

    (StatusCode::OK, Html(page)).into_response()
}

#[cfg(test)]
mod tests {
    use super::openapi_document;
    use serde_json::json;
    use std::collections::BTreeSet;

    #[test]
    fn openapi_document_has_expected_root_fields() {
        let doc = openapi_document();
        assert_eq!(doc["openapi"], "3.0.3");
        assert_eq!(doc["info"]["title"], "edgecontroller");
        assert!(doc["paths"].is_object());
    }

    #[test]
    fn openapi_document_covers_every_router_operation() {
        let doc = openapi_document();
        let documented = doc["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, item)| {
                item.as_object()
                    .unwrap()
                    .keys()
                    .map(move |method| (path.as_str(), method.as_str()))
            })
            .collect::<BTreeSet<_>>();
        let routed = BTreeSet::from([
            ("/admin/control", "get"),
            ("/admin/control", "patch"),
            ("/admin/control", "post"),
            ("/confirmation", "post"),
            ("/devCon/delete", "post"),
            ("/docs", "get"),
            ("/gen5/power", "post"),
            ("/gen5/tty_entry", "post"),
            ("/health", "get"),
            ("/ipl", "post"),
            ("/ipl-mode", "post"),
            ("/ipl-mode/default", "post"),
            ("/ipl/remove", "post"),
            ("/mapping/entry", "post"),
            ("/metrics", "get"),
            ("/logs", "get"),
            ("/logs/level", "put"),
            ("/ready", "get"),
            ("/reboot-device", "post"),
            ("/relay", "post"),
            ("/relay/config", "post"),
            ("/relay/delete", "post"),
            ("/relay/identity", "post"),
            ("/relay/status", "post"),
            ("/rtos/end", "post"),
            ("/rtos/start", "post"),
            ("/status", "get"),
            ("/swagger.json", "get"),
        ]);

        assert_eq!(documented, routed);
    }

    #[test]
    fn operational_responses_and_optional_authentication_are_documented() {
        let doc = openapi_document();

        assert_eq!(doc["security"], json!([{}, {"bearerAuth": []}]));
        for (path, schema) in [
            ("/health", "HealthResponse"),
            ("/ready", "ReadinessResponse"),
            ("/status", "StatusResponse"),
        ] {
            assert_eq!(
                doc["paths"][path]["get"]["responses"]["200"]["content"]["application/json"]
                    ["schema"]["$ref"],
                format!("#/components/schemas/{schema}")
            );
        }
    }
}
